use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use mediacore_proto::livekit::{self, signal_request, signal_response};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::ClientError;
use crate::transport::Command;

pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Default)]
struct State {
    participants: HashMap<String, livekit::ParticipantInfo>,
    closed: Option<livekit::DisconnectReason>,
}

pub(crate) enum Outgoing {
    Frame(Box<livekit::SignalRequest>),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct SignalHandle {
    outgoing: mpsc::UnboundedSender<Outgoing>,
    events: broadcast::Sender<signal_response::Message>,
    state: Arc<Mutex<State>>,
}

impl SignalHandle {
    pub(crate) fn send(&self, message: signal_request::Message) -> Result<(), ClientError> {
        self.outgoing
            .send(Outgoing::Frame(Box::new(livekit::SignalRequest {
                message: Some(message),
            })))
            .map_err(|_| ClientError::Closed)
    }

    pub(crate) async fn flush(&self, timeout: Duration) -> Result<(), ClientError> {
        let (reply, flushed) = oneshot::channel();
        self.outgoing
            .send(Outgoing::Flush(reply))
            .map_err(|_| ClientError::Closed)?;

        tokio::time::timeout(timeout, flushed)
            .await
            .map_err(|_| ClientError::Timeout("the signal socket to flush"))?
            .map_err(|_| ClientError::Closed)
    }

    pub(crate) fn subscribe(&self) -> SignalStream {
        SignalStream {
            inner: self.events.subscribe(),
        }
    }

    pub(crate) fn participants(&self) -> Vec<livekit::ParticipantInfo> {
        let state = self.state.lock().expect("signal state poisoned");
        let mut participants: Vec<_> = state.participants.values().cloned().collect();
        participants.sort_by(|a, b| a.identity.cmp(&b.identity));
        participants
    }

    pub(crate) fn disconnect_reason(&self) -> Option<livekit::DisconnectReason> {
        self.state.lock().expect("signal state poisoned").closed
    }
}

pub struct SignalStream {
    inner: broadcast::Receiver<signal_response::Message>,
}

impl SignalStream {
    pub async fn next(
        &mut self,
        timeout: Duration,
    ) -> Result<signal_response::Message, ClientError> {
        match tokio::time::timeout(timeout, self.inner.recv()).await {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(broadcast::error::RecvError::Closed)) => Err(ClientError::Closed),
            Ok(Err(broadcast::error::RecvError::Lagged(missed))) => {
                Err(ClientError::Lagged(missed))
            }
            Err(_) => Err(ClientError::Timeout("a signal response")),
        }
    }

    pub async fn wait_for<T, F>(
        &mut self,
        timeout: Duration,
        mut matches: F,
    ) -> Result<T, ClientError>
    where
        F: FnMut(&signal_response::Message) -> Option<T>,
    {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(ClientError::Timeout("a matching signal response"));
            }

            let message = self.next(left).await?;
            if let Some(found) = matches(&message) {
                return Ok(found);
            }
            if let signal_response::Message::Leave(leave) = &message {
                return Err(ClientError::Disconnected(
                    livekit::DisconnectReason::try_from(leave.reason)
                        .unwrap_or(livekit::DisconnectReason::UnknownReason),
                ));
            }
        }
    }
}

pub(crate) struct Signalling {
    pub(crate) handle: SignalHandle,
    pub(crate) reader: tokio::task::JoinHandle<()>,
    pub(crate) writer: tokio::task::JoinHandle<()>,
}

pub(crate) fn start(socket: Socket, transport: mpsc::Sender<Command>, buffer: usize) -> Signalling {
    let (sink, stream) = socket.split();
    let (outgoing, outbox) = mpsc::unbounded_channel();
    let (events, _) = broadcast::channel(buffer);
    let state = Arc::new(Mutex::new(State::default()));

    let handle = SignalHandle {
        outgoing,
        events: events.clone(),
        state: state.clone(),
    };

    let writer = tokio::spawn(write_loop(sink, outbox));
    let reader = tokio::spawn(read_loop(stream, events, state, transport));

    Signalling {
        handle,
        reader,
        writer,
    }
}

async fn write_loop(
    mut sink: SplitSink<Socket, Message>,
    mut outbox: mpsc::UnboundedReceiver<Outgoing>,
) {
    while let Some(outgoing) = outbox.recv().await {
        match outgoing {
            Outgoing::Frame(request) => {
                let frame = Message::Binary(Bytes::from(request.encode_to_vec()));
                if let Err(error) = sink.send(frame).await {
                    tracing::debug!(%error, "signal socket write failed");
                    break;
                }
            }
            Outgoing::Flush(reply) => {
                if let Err(error) = sink.flush().await {
                    tracing::debug!(%error, "signal socket flush failed");
                    break;
                }
                let _ = reply.send(());
            }
        }
    }

    let _ = sink.close().await;
}

async fn read_loop(
    mut stream: SplitStream<Socket>,
    events: broadcast::Sender<signal_response::Message>,
    state: Arc<Mutex<State>>,
    transport: mpsc::Sender<Command>,
) {
    while let Some(frame) = stream.next().await {
        let data = match frame {
            Ok(Message::Binary(data)) => data,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(%error, "signal socket read failed");
                break;
            }
        };

        let response = match livekit::SignalResponse::decode(data.as_ref()) {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, "undecodable signal response");
                continue;
            }
        };

        let Some(message) = response.message else {
            continue;
        };

        let closing = dispatch(&message, &state, &transport).await;
        let _ = events.send(message);

        if closing {
            break;
        }
    }

    let mut state = state.lock().expect("signal state poisoned");
    if state.closed.is_none() {
        state.closed = Some(livekit::DisconnectReason::UnknownReason);
    }
}

async fn dispatch(
    message: &signal_response::Message,
    state: &Arc<Mutex<State>>,
    transport: &mpsc::Sender<Command>,
) -> bool {
    match message {
        signal_response::Message::Join(join) => {
            let mut state = state.lock().expect("signal state poisoned");
            for participant in &join.other_participants {
                state
                    .participants
                    .insert(participant.identity.clone(), participant.clone());
            }
        }
        signal_response::Message::Update(update) => {
            let mut state = state.lock().expect("signal state poisoned");
            for participant in &update.participants {
                if participant.state == livekit::participant_info::State::Disconnected as i32 {
                    state.participants.remove(&participant.identity);
                } else {
                    state
                        .participants
                        .insert(participant.identity.clone(), participant.clone());
                }
            }
        }
        signal_response::Message::Answer(answer) => {
            let _ = transport.send(Command::Answer(answer.sdp.clone())).await;
        }
        signal_response::Message::Trickle(trickle) => {
            if trickle.target == livekit::SignalTarget::Publisher as i32 {
                let _ = transport
                    .send(Command::RemoteCandidate(trickle.candidate_init.clone()))
                    .await;
            }
        }
        signal_response::Message::Leave(leave) => {
            let mut state = state.lock().expect("signal state poisoned");
            state.closed = Some(
                livekit::DisconnectReason::try_from(leave.reason)
                    .unwrap_or(livekit::DisconnectReason::UnknownReason),
            );
            return true;
        }
        _ => {}
    }

    false
}

pub(crate) fn ping_loop(handle: SignalHandle, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let now = now_ms();
            let ping = signal_request::Message::PingReq(livekit::Ping {
                timestamp: now,
                rtt: 0,
            });
            if handle.send(ping).is_err() {
                break;
            }
        }
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) fn signal_url(
    base: &str,
    token: &str,
    protocol: i32,
    sdk: &str,
    version: &str,
) -> Result<String, ClientError> {
    let base = base.trim_end_matches('/');

    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| ClientError::InvalidUrl(base.to_owned()))?;

    let scheme = match scheme {
        "ws" | "http" => "ws",
        "wss" | "https" => "wss",
        _ => return Err(ClientError::InvalidUrl(base.to_owned())),
    };

    if rest.is_empty() {
        return Err(ClientError::InvalidUrl(base.to_owned()));
    }

    let rest = rest.strip_suffix("/rtc").unwrap_or(rest);

    Ok(format!(
        "{scheme}://{rest}/rtc?access_token={}&protocol={protocol}&sdk={}&version={}&auto_subscribe=1",
        escape(token),
        escape(sdk),
        escape(version),
    ))
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                escaped.push(byte as char)
            }
            other => escaped.push_str(&format!("%{other:02X}")),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_websocket_url_is_built_from_an_http_base() {
        let url = signal_url("http://127.0.0.1:7880", "abc.def", 17, "mediacore", "0.1.0").unwrap();

        assert_eq!(
            url,
            "ws://127.0.0.1:7880/rtc?access_token=abc.def&protocol=17&sdk=mediacore&version=0.1.0&auto_subscribe=1"
        );
    }

    #[test]
    fn tls_schemes_map_to_wss() {
        let url = signal_url("https://sfu.example/", "t", 17, "mediacore", "0.1.0").unwrap();
        assert!(url.starts_with("wss://sfu.example/rtc?"));
    }

    #[test]
    fn an_explicit_rtc_path_is_not_repeated() {
        let url = signal_url("ws://host:1/rtc", "t", 17, "mediacore", "0.1.0").unwrap();
        assert!(url.starts_with("ws://host:1/rtc?"));
    }

    #[test]
    fn query_values_are_percent_encoded() {
        let url = signal_url("ws://host:1", "t", 17, "media core/1", "0.1.0").unwrap();
        assert!(url.contains("sdk=media%20core%2F1"), "{url}");
    }

    #[test]
    fn a_url_without_a_scheme_is_rejected() {
        let error = signal_url("127.0.0.1:7880", "t", 17, "mediacore", "0.1.0").unwrap_err();
        assert!(matches!(error, ClientError::InvalidUrl(_)));
    }
}
