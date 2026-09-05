use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use mediacore_auth::{Claims, VideoGrant};
use mediacore_proto::livekit::{self, signal_request, signal_response};
use mediacore_proto::{PROTOCOL_VERSION, SERVER_VERSION};
use mediacore_room::{Joined, ParticipantSpec};
use prost::Message as _;
use serde::Deserialize;
use tokio::time::Instant;

use crate::state::AppState;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
const TIMEOUT_INTERVAL: Duration = Duration::from_secs(5);
const PING_TIMEOUT_SECONDS: i32 = 60;
const PING_INTERVAL_SECONDS: i32 = 15;

#[derive(Debug, Deserialize)]
pub struct SignalQuery {
    pub access_token: String,
    #[serde(default)]
    pub protocol: Option<i32>,
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub reconnect: Option<String>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(query): Query<SignalQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let claims = match mediacore_auth::verify_join(&query.access_token, &state.config.keys) {
        Ok(claims) => claims,
        Err(error) => {
            tracing::info!(%error, "rejecting signal connection");
            return (StatusCode::UNAUTHORIZED, error.to_string()).into_response();
        }
    };

    upgrade.on_upgrade(move |socket| session(state, claims, query, socket))
}

async fn session(state: AppState, claims: Claims, query: SignalQuery, socket: WebSocket) {
    let room_name = claims.video.room.clone();
    let identity = claims.identity().to_owned();
    let (tx, mut rx) = mediacore_room::signal_channel();

    if query.reconnect.is_some() {
        tracing::debug!(%identity, "resume is not implemented, joining as a new session");
    }

    let spec = ParticipantSpec {
        identity: identity.clone(),
        name: claims.display_name().to_owned(),
        metadata: claims.metadata.clone(),
        attributes: claims.attributes.clone(),
        permission: permission_from(&claims.video),
    };

    let joined = match state.registry.join(&room_name, spec, tx) {
        Ok(joined) => joined,
        Err(error) => {
            tracing::info!(%error, room = %room_name, %identity, "join rejected");
            let mut socket = socket;
            let _ = write(
                &mut socket,
                mediacore_room::leave(error.disconnect_reason()),
            )
            .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let sid = joined.participant.sid.clone();
    tracing::info!(
        room = %room_name,
        %identity,
        %sid,
        sdk = query.sdk.as_deref().unwrap_or("unknown"),
        sdk_version = query.version.as_deref().unwrap_or("unknown"),
        client_protocol = query.protocol.unwrap_or_default(),
        "participant joined"
    );

    let (mut sink, mut stream) = socket.split();
    let join = signal_response::Message::Join(build_join_response(&state, &joined));

    if send(&mut sink, join).await.is_err() {
        state.registry.leave(&room_name, &sid);
        return;
    }

    let mut last_seen = Instant::now();
    let mut timeouts = tokio::time::interval(TIMEOUT_INTERVAL);
    timeouts.tick().await;

    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(response) = outgoing else { break };
                let Some(message) = response.message else { continue };
                let closing = matches!(message, signal_response::Message::Leave(_));

                if send(&mut sink, message).await.is_err() {
                    break;
                }
                if closing {
                    break;
                }
            }
            incoming = stream.next() => {
                last_seen = Instant::now();

                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        match livekit::SignalRequest::decode(data.as_ref()) {
                            Ok(request) => {
                                if !handle_request(&joined, request) {
                                    break;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%error, %identity, "undecodable signal request");
                            }
                        }
                    }
                    Some(Ok(Message::Text(_))) => {
                        tracing::warn!(%identity, "json signalling is not supported, use protobuf");
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::debug!(%error, %identity, "signal socket error");
                        break;
                    }
                }
            }
            _ = timeouts.tick() => {
                if last_seen.elapsed() >= CLIENT_TIMEOUT {
                    tracing::info!(%identity, %sid, "no signal traffic, closing the session");
                    let _ = send(
                        &mut sink,
                        mediacore_room::leave(livekit::DisconnectReason::ConnectionTimeout),
                    )
                    .await;
                    break;
                }
            }
        }
    }

    state.registry.leave(&room_name, &sid);
    tracing::info!(room = %room_name, %identity, %sid, "participant left");
}

fn handle_request(joined: &Joined, request: livekit::SignalRequest) -> bool {
    let Some(message) = request.message else {
        return true;
    };

    match message {
        signal_request::Message::PingReq(ping) => {
            joined
                .participant
                .send(signal_response::Message::PongResp(livekit::Pong {
                    last_ping_timestamp: ping.timestamp,
                    timestamp: now_ms(),
                }));
            true
        }
        signal_request::Message::Ping(_) => {
            joined
                .participant
                .send(signal_response::Message::Pong(now_ms()));
            true
        }
        signal_request::Message::Leave(_) => false,
        other => {
            tracing::debug!(
                identity = %joined.participant.identity,
                request = request_name(&other),
                "signal request is not implemented yet"
            );
            true
        }
    }
}

fn request_name(message: &signal_request::Message) -> &'static str {
    match message {
        signal_request::Message::Offer(_) => "offer",
        signal_request::Message::Answer(_) => "answer",
        signal_request::Message::Trickle(_) => "trickle",
        signal_request::Message::AddTrack(_) => "add_track",
        signal_request::Message::Mute(_) => "mute",
        signal_request::Message::Subscription(_) => "subscription",
        signal_request::Message::TrackSetting(_) => "track_setting",
        signal_request::Message::SyncState(_) => "sync_state",
        signal_request::Message::UpdateMetadata(_) => "update_metadata",
        _ => "other",
    }
}

fn build_join_response(state: &AppState, joined: &Joined) -> livekit::JoinResponse {
    livekit::JoinResponse {
        room: Some(joined.room.to_proto()),
        participant: Some(
            joined
                .participant
                .to_proto(livekit::participant_info::State::Joined),
        ),
        other_participants: joined.others.clone(),
        server_version: SERVER_VERSION.to_owned(),
        subscriber_primary: true,
        ping_timeout: PING_TIMEOUT_SECONDS,
        ping_interval: PING_INTERVAL_SECONDS,
        server_info: Some(livekit::ServerInfo {
            edition: livekit::server_info::Edition::Standard as i32,
            version: SERVER_VERSION.to_owned(),
            protocol: PROTOCOL_VERSION,
            region: state.config.region.clone(),
            node_id: state.config.node_id.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn permission_from(grant: &VideoGrant) -> livekit::ParticipantPermission {
    livekit::ParticipantPermission {
        can_subscribe: grant.can_subscribe(),
        can_publish: grant.can_publish(),
        can_publish_data: grant.can_publish_data(),
        can_publish_sources: publish_sources(grant),
        can_update_metadata: grant.can_update_own_metadata(),
        hidden: grant.hidden(),
        ..Default::default()
    }
}

fn publish_sources(grant: &VideoGrant) -> Vec<i32> {
    grant
        .can_publish_sources
        .iter()
        .filter_map(|source| match source.as_str() {
            "camera" => Some(livekit::TrackSource::Camera as i32),
            "microphone" => Some(livekit::TrackSource::Microphone as i32),
            "screen_share" => Some(livekit::TrackSource::ScreenShare as i32),
            "screen_share_audio" => Some(livekit::TrackSource::ScreenShareAudio as i32),
            unknown => {
                tracing::warn!(source = %unknown, "unknown publish source in token");
                None
            }
        })
        .collect()
}

async fn send(
    sink: &mut SplitSink<WebSocket, Message>,
    message: signal_response::Message,
) -> Result<(), axum::Error> {
    sink.send(Message::Binary(encode(message))).await
}

async fn write(
    socket: &mut WebSocket,
    message: signal_response::Message,
) -> Result<(), axum::Error> {
    socket.send(Message::Binary(encode(message))).await
}

fn encode(message: signal_response::Message) -> bytes::Bytes {
    livekit::SignalResponse {
        message: Some(message),
    }
    .encode_to_vec()
    .into()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}
