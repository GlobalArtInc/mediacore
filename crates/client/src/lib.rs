mod error;
mod events;
mod media;
mod receiver;
mod signal;
mod simulcast;
mod subscriber;
mod subscription;
mod track;
mod transport;
mod udp;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use mediacore_proto::mediacore::{self, signal_request, signal_response};
use str0m::media::{Mid, Rid};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub use crate::error::ClientError;
pub use crate::events::{ClientEvent, EventStream, RemoteTrack};
pub use crate::media::{
    Codec, LAYER_TAG_TRAILER, MediaProfile, SyntheticPacket, SyntheticSource, layer_tag,
};
pub use crate::receiver::{
    DEFAULT_PAYLOAD_WINDOW, DEFAULT_SWITCH_HISTORY, LayerId, LayerReport, MAX_LAYERS,
    MAX_TRACKED_MISSING, ReceivedPacket, Span, Switch, TrackReport,
};
pub use crate::signal::SignalStream;
pub use crate::simulcast::{
    LayerSpec, MAX_RID_LEN, RID_FULL, RID_HALF, RID_QUARTER, default_layers,
};
pub use crate::subscription::{SubscriptionRequest, TrackSettings};
pub use crate::track::{TrackKind, TrackSpec};
pub use crate::transport::{LayerStats, TrackStats};
pub use mediacore_proto::mediacore as proto;

use crate::signal::SignalHandle;
use crate::subscriber::SubCommand;
use crate::transport::{Command, PublishSource};

const COMMAND_QUEUE: usize = 64;
const REPORT_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub sdk: String,
    pub version: String,
    pub protocol: i32,
    pub bind: IpAddr,
    pub connect_timeout: Duration,
    pub response_timeout: Duration,
    pub event_buffer: usize,
    pub payload_window: usize,
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
            payload_window: DEFAULT_PAYLOAD_WINDOW,
        }
    }
}

pub struct Client {
    signal: SignalHandle,
    commands: mpsc::Sender<Command>,
    subscriber: mpsc::Sender<SubCommand>,
    join: mediacore::JoinResponse,
    options: ClientOptions,
    local_addr: SocketAddr,
    subscriber_addr: SocketAddr,
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
        let (subscriber, subscriber_inbox) = mpsc::channel(COMMAND_QUEUE);
        let signalling = signal::start(
            socket,
            commands.clone(),
            subscriber.clone(),
            options.event_buffer,
        );
        let mut responses = signalling.responses;

        let join = responses
            .wait_for(options.response_timeout, |message| match message {
                signal_response::Message::Join(join) => Some(join.clone()),
                _ => None,
            })
            .await?;

        let (local_addr, transport) =
            transport::start(options.bind, inbox, signalling.handle.clone()).await?;

        let (subscriber_addr, subscriber_task) = subscriber::start(
            options.bind,
            options.payload_window,
            subscriber_inbox,
            signalling.handle.clone(),
        )
        .await?;

        let ping_interval = match join.ping_interval {
            interval if interval > 0 => Duration::from_secs(interval as u64),
            _ => Duration::from_secs(15),
        };
        let ping = signal::ping_loop(signalling.handle.clone(), ping_interval);

        Ok(Self {
            signal: signalling.handle,
            commands,
            subscriber,
            join,
            options,
            local_addr,
            subscriber_addr,
            tasks: vec![
                signalling.reader,
                signalling.writer,
                transport,
                subscriber_task,
                ping,
            ],
        })
    }

    pub fn join(&self) -> &mediacore::JoinResponse {
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

    pub fn participants(&self) -> Vec<mediacore::ParticipantInfo> {
        self.signal.participants()
    }

    pub fn disconnect_reason(&self) -> Option<mediacore::DisconnectReason> {
        self.signal.disconnect_reason()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn subscriber_addr(&self) -> SocketAddr {
        self.subscriber_addr
    }

    pub fn responses(&self) -> SignalStream {
        self.signal.subscribe()
    }

    pub fn events(&self) -> EventStream {
        self.signal.events()
    }

    pub fn remote_tracks(&self) -> Vec<RemoteTrack> {
        self.signal.remote_tracks()
    }

    pub fn send(&self, message: signal_request::Message) -> Result<(), ClientError> {
        self.signal.send(message)
    }

    pub async fn publish(&self, spec: TrackSpec) -> Result<PublishedTrack, ClientError> {
        simulcast::validate_layers(&spec.layers)?;

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
            sources: publish_sources(&spec),
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

    pub async fn unpublish(&self, track: &PublishedTrack) -> Result<(), ClientError> {
        let (reply, done) = oneshot::channel();
        self.command(Command::Unpublish {
            mid: track.mid,
            reply,
        })
        .await?;

        tokio::time::timeout(self.options.response_timeout, done)
            .await
            .map_err(|_| ClientError::Timeout("the unpublish answer"))?
            .map_err(|_| ClientError::TransportGone)?
    }

    pub async fn wait_connected(&self, timeout: Duration) -> Result<(), ClientError> {
        let (reply, connected) = oneshot::channel();
        self.command(Command::Connected(reply)).await?;

        tokio::time::timeout(timeout, connected)
            .await
            .map_err(|_| ClientError::Timeout("the publisher transport to connect"))?
            .map_err(|_| ClientError::TransportGone)
    }

    pub async fn wait_subscribed(&self, timeout: Duration) -> Result<(), ClientError> {
        let (reply, connected) = oneshot::channel();
        self.subscriber_command(SubCommand::Connected(reply))
            .await?;

        tokio::time::timeout(timeout, connected)
            .await
            .map_err(|_| ClientError::Timeout("the subscriber transport to connect"))?
            .map_err(|_| ClientError::TransportGone)
    }

    pub fn update_subscription(&self, request: &SubscriptionRequest) -> Result<(), ClientError> {
        self.signal.send(request.to_message())
    }

    pub fn subscribe<I, S>(&self, track_sids: I) -> Result<(), ClientError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.update_subscription(&SubscriptionRequest::subscribe(track_sids))
    }

    pub fn unsubscribe<I, S>(&self, track_sids: I) -> Result<(), ClientError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.update_subscription(&SubscriptionRequest::unsubscribe(track_sids))
    }

    pub fn update_track_settings(&self, settings: &TrackSettings) -> Result<(), ClientError> {
        self.signal.send(settings.to_message())
    }

    pub fn request_quality<I, S>(
        &self,
        track_sids: I,
        quality: mediacore::VideoQuality,
    ) -> Result<(), ClientError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.update_track_settings(&TrackSettings::for_tracks(track_sids).with_quality(quality))
    }

    pub fn request_video_size<I, S>(
        &self,
        track_sids: I,
        width: u32,
        height: u32,
    ) -> Result<(), ClientError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.update_track_settings(&TrackSettings::for_tracks(track_sids).with_size(width, height))
    }

    pub async fn received(&self) -> Result<Vec<TrackReport>, ClientError> {
        let (reply, reports) = oneshot::channel();
        self.subscriber_command(SubCommand::Reports(reply)).await?;

        tokio::time::timeout(self.options.response_timeout, reports)
            .await
            .map_err(|_| ClientError::Timeout("the received track reports"))?
            .map_err(|_| ClientError::TransportGone)
    }

    pub async fn received_track(
        &self,
        track_sid: &str,
    ) -> Result<Option<TrackReport>, ClientError> {
        Ok(self
            .received()
            .await?
            .into_iter()
            .find(|report| report.track_sid == track_sid))
    }

    pub async fn wait_for_packets(
        &self,
        track_sid: &str,
        packets: u64,
        timeout: Duration,
    ) -> Result<TrackReport, ClientError> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if let Some(report) = self.received_track(track_sid).await?
                && report.packets >= packets
            {
                return Ok(report);
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(ClientError::Timeout("media on a subscribed track"));
            }

            tokio::time::sleep(REPORT_POLL_INTERVAL).await;
        }
    }

    pub async fn leave(mut self) -> Result<(), ClientError> {
        let leave = signal_request::Message::Leave(mediacore::LeaveRequest {
            reason: mediacore::DisconnectReason::ClientInitiated as i32,
            action: mediacore::leave_request::Action::Disconnect as i32,
            ..mediacore::LeaveRequest::default()
        });
        let sent = self.signal.send(leave);
        let flushed = self.signal.flush(Duration::from_secs(1)).await;

        let (reply, closed) = oneshot::channel();
        if self.command(Command::Close(reply)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(1), closed).await;
        }

        let (reply, closed) = oneshot::channel();
        if self
            .subscriber_command(SubCommand::Close(reply))
            .await
            .is_ok()
        {
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

    async fn subscriber_command(&self, command: SubCommand) -> Result<(), ClientError> {
        self.subscriber
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

fn publish_sources(spec: &TrackSpec) -> Vec<PublishSource> {
    if spec.layers.is_empty() {
        return vec![PublishSource {
            rid: None,
            profile: spec.profile,
        }];
    }

    spec.layers
        .iter()
        .map(|layer| PublishSource {
            rid: Some(Rid::from(layer.rid.as_str())),
            profile: layer.profile,
        })
        .collect()
}

pub struct PublishedTrack {
    spec: TrackSpec,
    info: Option<mediacore::TrackInfo>,
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

    pub fn info(&self) -> Option<&mediacore::TrackInfo> {
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

    pub fn layers(&self) -> &[LayerSpec] {
        &self.spec.layers
    }

    pub fn rids(&self) -> Vec<&str> {
        self.spec.rids()
    }

    pub async fn start(&self) -> Result<(), ClientError> {
        self.command(Command::Start(self.mid, None)).await
    }

    pub async fn stop(&self) -> Result<(), ClientError> {
        self.command(Command::Stop(self.mid, None)).await
    }

    pub async fn start_layer(&self, rid: &str) -> Result<(), ClientError> {
        self.command(Command::Start(self.mid, Some(self.rid(rid)?)))
            .await
    }

    pub async fn stop_layer(&self, rid: &str) -> Result<(), ClientError> {
        self.command(Command::Stop(self.mid, Some(self.rid(rid)?)))
            .await
    }

    pub async fn stats(&self) -> Result<TrackStats, ClientError> {
        let mut total = TrackStats::default();
        for layer in self.layer_stats().await? {
            total.merge(&layer.stats);
        }

        Ok(total)
    }

    pub async fn layer_stats(&self) -> Result<Vec<LayerStats>, ClientError> {
        let (reply, stats) = oneshot::channel();
        self.command(Command::Stats(self.mid, reply)).await?;

        tokio::time::timeout(self.response_timeout, stats)
            .await
            .map_err(|_| ClientError::Timeout("track statistics"))?
            .map_err(|_| ClientError::TransportGone)
    }

    pub async fn stats_of(&self, rid: &str) -> Result<TrackStats, ClientError> {
        let wanted = self.rid(rid)?.to_string();

        self.layer_stats()
            .await?
            .into_iter()
            .find(|layer| layer.rid.as_deref() == Some(wanted.as_str()))
            .map(|layer| layer.stats)
            .ok_or_else(|| ClientError::UnknownLayer(rid.to_owned()))
    }

    fn rid(&self, rid: &str) -> Result<Rid, ClientError> {
        self.spec
            .layer(rid)
            .map(|layer| Rid::from(layer.rid.as_str()))
            .ok_or_else(|| ClientError::UnknownLayer(rid.to_owned()))
    }

    async fn command(&self, command: Command) -> Result<(), ClientError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| ClientError::TransportGone)
    }
}
