mod error;
mod media;
mod signal;
mod track;
mod transport;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use mediacore_proto::livekit::{self, signal_request, signal_response};
use str0m::media::Mid;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub use crate::error::ClientError;
pub use crate::media::{Codec, MediaProfile};
pub use crate::signal::SignalStream;
pub use crate::track::{TrackKind, TrackSpec};
pub use crate::transport::TrackStats;
pub use mediacore_proto::livekit as proto;

use crate::signal::SignalHandle;
use crate::transport::Command;

const COMMAND_QUEUE: usize = 64;

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub sdk: String,
    pub version: String,
    pub protocol: i32,
    pub bind: IpAddr,
    pub connect_timeout: Duration,
    pub response_timeout: Duration,
    pub event_buffer: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            sdk: "mediacore-rust".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: mediacore_proto::PROTOCOL_VERSION,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            connect_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(10),
            event_buffer: 256,
        }
    }
}

pub struct Client {
    signal: SignalHandle,
    commands: mpsc::Sender<Command>,
    join: livekit::JoinResponse,
    options: ClientOptions,
    local_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Client {
    pub async fn connect(url: &str, token: &str) -> Result<Self, ClientError> {
        Self::connect_with(url, token, ClientOptions::default()).await
    }

    pub async fn connect_with(
        url: &str,
        token: &str,
        options: ClientOptions,
    ) -> Result<Self, ClientError> {
        let target =
            signal::signal_url(url, token, options.protocol, &options.sdk, &options.version)?;

        let connect = tokio_tungstenite::connect_async(target);
        let (socket, _) = tokio::time::timeout(options.connect_timeout, connect)
            .await
            .map_err(|_| ClientError::Timeout("the signalling handshake"))??;

        let (commands, inbox) = mpsc::channel(COMMAND_QUEUE);
        let signalling = signal::start(socket, commands.clone(), options.event_buffer);
        let mut responses = signalling.handle.subscribe();

        let join = responses
            .wait_for(options.response_timeout, |message| match message {
                signal_response::Message::Join(join) => Some(join.clone()),
                _ => None,
            })
            .await?;

        let (local_addr, transport) =
            transport::start(options.bind, inbox, signalling.handle.clone()).await?;

        let ping_interval = match join.ping_interval {
            interval if interval > 0 => Duration::from_secs(interval as u64),
            _ => Duration::from_secs(15),
        };
        let ping = signal::ping_loop(signalling.handle.clone(), ping_interval);

        Ok(Self {
            signal: signalling.handle,
            commands,
            join,
            options,
            local_addr,
            tasks: vec![signalling.reader, signalling.writer, transport, ping],
        })
    }

    pub fn join(&self) -> &livekit::JoinResponse {
        &self.join
    }

    pub fn identity(&self) -> &str {
        self.join
            .participant
            .as_ref()
            .map(|participant| participant.identity.as_str())
            .unwrap_or_default()
    }

    pub fn sid(&self) -> &str {
        self.join
            .participant
            .as_ref()
            .map(|participant| participant.sid.as_str())
            .unwrap_or_default()
    }

    pub fn participants(&self) -> Vec<livekit::ParticipantInfo> {
        self.signal.participants()
    }

    pub fn disconnect_reason(&self) -> Option<livekit::DisconnectReason> {
        self.signal.disconnect_reason()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn responses(&self) -> SignalStream {
        self.signal.subscribe()
    }

    pub fn send(&self, message: signal_request::Message) -> Result<(), ClientError> {
        self.signal.send(message)
    }

    pub async fn publish(&self, spec: TrackSpec) -> Result<PublishedTrack, ClientError> {
        let mut responses = self.signal.subscribe();
        self.signal
            .send(signal_request::Message::AddTrack(spec.to_request()))?;

        let cid = spec.cid.clone();
        let info = responses
            .wait_for(self.options.response_timeout, |message| match message {
                signal_response::Message::TrackPublished(published) if published.cid == cid => {
                    Some(published.track.clone())
                }
                _ => None,
            })
            .await?;

        let (reply, negotiated) = oneshot::channel();
        self.command(Command::Publish {
            kind: spec.kind.media_kind(),
            stream_id: spec.stream_id.clone(),
            track_id: spec.cid.clone(),
            profile: spec.profile,
            reply,
        })
        .await?;

        let mid = tokio::time::timeout(self.options.response_timeout, negotiated)
            .await
            .map_err(|_| ClientError::Timeout("the publisher answer"))?
            .map_err(|_| ClientError::TransportGone)??;

        Ok(PublishedTrack {
            spec,
            info,
            mid,
            commands: self.commands.clone(),
            response_timeout: self.options.response_timeout,
        })
    }

    pub async fn wait_connected(&self, timeout: Duration) -> Result<(), ClientError> {
        let (reply, connected) = oneshot::channel();
        self.command(Command::Connected(reply)).await?;

        tokio::time::timeout(timeout, connected)
            .await
            .map_err(|_| ClientError::Timeout("the publisher transport to connect"))?
            .map_err(|_| ClientError::TransportGone)
    }

    pub async fn leave(mut self) -> Result<(), ClientError> {
        let leave = signal_request::Message::Leave(livekit::LeaveRequest {
            reason: livekit::DisconnectReason::ClientInitiated as i32,
            action: livekit::leave_request::Action::Disconnect as i32,
            ..livekit::LeaveRequest::default()
        });
        let sent = self.signal.send(leave);
        let flushed = self.signal.flush(Duration::from_secs(1)).await;

        let (reply, closed) = oneshot::channel();
        if self.command(Command::Close(reply)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(1), closed).await;
        }

        self.stop();
        sent.and(flushed)
    }

    async fn command(&self, command: Command) -> Result<(), ClientError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| ClientError::TransportGone)
    }

    fn stop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("identity", &self.identity())
            .field("sid", &self.sid())
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct PublishedTrack {
    spec: TrackSpec,
    info: Option<livekit::TrackInfo>,
    mid: Mid,
    commands: mpsc::Sender<Command>,
    response_timeout: Duration,
}

impl std::fmt::Debug for PublishedTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedTrack")
            .field("cid", &self.cid())
            .field("sid", &self.sid())
            .field("mid", &self.mid)
            .finish()
    }
}

impl PublishedTrack {
    pub fn spec(&self) -> &TrackSpec {
        &self.spec
    }

    pub fn info(&self) -> Option<&livekit::TrackInfo> {
        self.info.as_ref()
    }

    pub fn cid(&self) -> &str {
        &self.spec.cid
    }

    pub fn sid(&self) -> &str {
        self.info
            .as_ref()
            .map(|info| info.sid.as_str())
            .unwrap_or_default()
    }

    pub fn mid(&self) -> String {
        self.mid.to_string()
    }

    pub async fn start(&self) -> Result<(), ClientError> {
        self.command(Command::Start(self.mid)).await
    }

    pub async fn stop(&self) -> Result<(), ClientError> {
        self.command(Command::Stop(self.mid)).await
    }

    pub async fn stats(&self) -> Result<TrackStats, ClientError> {
        let (reply, stats) = oneshot::channel();
        self.command(Command::Stats(self.mid, reply)).await?;

        tokio::time::timeout(self.response_timeout, stats)
            .await
            .map_err(|_| ClientError::Timeout("track statistics"))?
            .map_err(|_| ClientError::TransportGone)
            .map(Option::unwrap_or_default)
    }

    async fn command(&self, command: Command) -> Result<(), ClientError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| ClientError::TransportGone)
    }
}
