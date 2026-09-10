use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use mediacore_auth::{Claims, VideoGrant, turn};
use mediacore_proto::mediacore::{self, signal_request, signal_response};
use mediacore_proto::{PROTOCOL_VERSION, SERVER_VERSION};
use mediacore_room::{Joined, ParticipantKind, ParticipantSpec, ProfileUpdate};
use prost::Message as _;
use serde::Deserialize;
use tokio::time::Instant;

use crate::peer::PublisherSession;
use crate::relay;
use crate::state::AppState;
use crate::subscriber::SubscriberSession;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
const TIMEOUT_INTERVAL: Duration = Duration::from_secs(5);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
const LAYER_INTERVAL: Duration = Duration::from_millis(250);
const PING_TIMEOUT_SECONDS: i32 = 60;
const PING_INTERVAL_SECONDS: i32 = 15;

#[derive(Debug, Deserialize)]
pub struct SignalQuery {
    /// The pass, when the client puts it in the address.
    ///
    /// Optional because a client is equally entitled to put it in an
    /// `Authorization: Bearer` header, and the newer server SDKs only do that.
    /// A connection carrying neither is refused by `token_from`, so this being
    /// absent is not the same as being allowed in without one.
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub protocol: Option<i32>,
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub reconnect: Option<String>,
    #[serde(default)]
    pub sid: Option<String>,
    #[serde(default)]
    pub auto_subscribe: Option<String>,
}

impl SignalQuery {
    pub fn resumes(&self) -> Option<String> {
        self.reconnect.as_ref()?;
        self.sid.clone().filter(|sid| !sid.is_empty())
    }

    pub fn auto_subscribe(&self) -> bool {
        match self.auto_subscribe.as_deref() {
            None => true,
            Some(value) => matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ),
        }
    }
}

pub async fn handler(
    State(state): State<AppState>,
    Query(query): Query<SignalQuery>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let token = match token_from(&query, &headers) {
        Some(token) => token,
        None => {
            tracing::info!("rejecting a signal connection that carries no pass");
            return (
                StatusCode::UNAUTHORIZED,
                "no pass on the connection: put it in `access_token` or in an `Authorization: \
                 Bearer` header"
                    .to_owned(),
            )
                .into_response();
        }
    };

    let claims = match mediacore_auth::verify_join(token, &state.config.keys) {
        Ok(claims) => claims,
        Err(error) => {
            tracing::info!(%error, "rejecting signal connection");
            return (StatusCode::UNAUTHORIZED, error.to_string()).into_response();
        }
    };

    // Where this room lives. A node that is alone answers itself for every
    // room, so this costs a lookup in an empty view and nothing else.
    let raw = raw.unwrap_or_default();
    if let Some(owner) = elsewhere(&state, &claims.video.room, &raw) {
        let url = relay::inner_url(&owner.signal_url, &raw);
        let room = claims.video.room.clone();
        let node = owner.id.clone();
        let pass = token.to_owned();

        tracing::info!(%room, holder = %node, "carrying a connection to the node that holds this room");

        return upgrade.on_upgrade(move |socket| async move {
            if let Err(error) = relay::carry(socket, url, pass).await {
                tracing::warn!(%room, holder = %node, %error, "a connection could not be carried");
            }
        });
    }

    upgrade.on_upgrade(move |socket| session(state, claims, query, socket))
}

/// The node holding this room, when it is not this one.
///
/// `None` means serve it here, which covers a node on its own, a room this node
/// already holds, a room the cluster scores to this node, and a connection that
/// has been carried once already. That last one is the loop guard: a marked
/// connection is served or refused where it lands, never passed on again, so a
/// view two nodes briefly disagree about costs one wrong hop rather than a ring.
fn elsewhere(state: &AppState, room: &str, raw_query: &str) -> Option<mediacore_session::Node> {
    if !state.cluster.joined() || relay::already_relayed(raw_query) {
        return None;
    }

    let view = state.cluster.view();
    let owner = view.should_hold(room)?;

    match owner.id == state.cluster.id() {
        true => None,
        false => Some(owner.clone()),
    }
}

async fn session(state: AppState, claims: Claims, query: SignalQuery, socket: WebSocket) {
    let room_name = claims.video.room.clone();
    let requested = claims.identity().to_owned();
    let (tx, mut rx) = mediacore_room::signal_channel();

    if query.reconnect.is_some() {
        tracing::debug!(
            identity = %requested,
            "resume is not implemented, rejoining as a new session in place of the one named"
        );
    }

    let spec = ParticipantSpec {
        identity: requested.clone(),
        name: claims.display_name().to_owned(),
        metadata: claims.metadata.clone(),
        // Carried through untouched and never read. Whatever the platform that
        // minted this token needs on the record of the meeting is in here, and
        // what any of it means is its business -- see
        // `mediacore_session::Attendance::attributes`.
        attributes: claims.attributes.clone(),
        permission: permission_from(&claims.video),
        kind: kind_from(&claims),
        replaces: query.resumes(),
    };

    // A node on its way out takes nobody new. Refused here rather than at the
    // socket, and with a reason the client acts on: `Reconnect` sends it back
    // through the load balancer, which by now has had `/readyz` say 503 and
    // will hand it a node that is staying. Refusing after the join would mean
    // opening a meeting this node is in the middle of closing.
    if state.draining() {
        tracing::info!(room = %room_name, identity = %requested, "refusing an arrival while draining");
        let mut socket = socket;
        let _ = write(
            &mut socket,
            mediacore_room::reconnect(mediacore::DisconnectReason::ServerShutdown),
        )
        .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    let joined = match state.registry.join(&room_name, spec, tx) {
        Ok(joined) => joined,
        Err(error) => {
            tracing::info!(%error, room = %room_name, identity = %requested, "join rejected");
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
    let identity = joined.participant.identity.clone();
    tracing::info!(
        room = %room_name,
        %identity,
        %requested,
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

    let mut publisher = PublisherSession::new(&state, &sid);
    let mut subscriber = SubscriberSession::new(query.auto_subscribe());
    let mut last_seen = Instant::now();
    let mut timeouts = tokio::time::interval(TIMEOUT_INTERVAL);
    timeouts.tick().await;
    let mut reconciles = tokio::time::interval(RECONCILE_INTERVAL);
    reconciles.tick().await;
    let mut layers = tokio::time::interval(LAYER_INTERVAL);
    layers.tick().await;
    let mut allocations = tokio::time::interval(state.config.video.bwe.allocation_interval());
    allocations.tick().await;

    subscriber.on_room_changed(&state, &joined).await;

    loop {
        tokio::select! {
            event = publisher.next_event() => {
                let Some(event) = event else { break };

                if !publisher.on_event(&state, &joined, event) {
                    let _ = send(
                        &mut sink,
                        mediacore_room::leave(mediacore::DisconnectReason::MediaFailure),
                    )
                    .await;
                    break;
                }

                publisher.apply_pending(&state, &joined).await;
            }
            event = subscriber.next_event() => {
                let Some(event) = event else { break };

                if !subscriber.on_event(&state, &joined, event) {
                    let _ = send(
                        &mut sink,
                        mediacore_room::leave(mediacore::DisconnectReason::MediaFailure),
                    )
                    .await;
                    break;
                }
            }
            outgoing = rx.recv() => {
                let Some(response) = outgoing else { break };
                let Some(message) = response.message else { continue };
                let closing = matches!(message, signal_response::Message::Leave(_));
                let room_changed = matches!(
                    message,
                    signal_response::Message::Update(_)
                        | signal_response::Message::SubscriptionPermissionUpdate(_)
                );

                if send(&mut sink, message).await.is_err() {
                    break;
                }
                if closing {
                    break;
                }
                if room_changed {
                    subscriber.on_room_changed(&state, &joined).await;
                }
            }
            incoming = stream.next() => {
                last_seen = Instant::now();

                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        match mediacore::SignalRequest::decode(data.as_ref()) {
                            Ok(request) => {
                                let alive = handle_request(
                                    &state,
                                    &joined,
                                    &mut publisher,
                                    &mut subscriber,
                                    request,
                                )
                                .await;

                                if !alive {
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
            _ = layers.tick() => {
                publisher.tick(&state, &joined).await;
            }
            _ = allocations.tick() => {
                subscriber.tick(&state, &joined).await;
            }
            _ = reconciles.tick() => {
                subscriber.on_room_changed(&state, &joined).await;
            }
            _ = timeouts.tick() => {
                if last_seen.elapsed() >= CLIENT_TIMEOUT {
                    tracing::info!(%identity, %sid, "no signal traffic, closing the session");
                    let _ = send(
                        &mut sink,
                        mediacore_room::leave(mediacore::DisconnectReason::ConnectionTimeout),
                    )
                    .await;
                    break;
                }
            }
        }
    }

    subscriber.close(&state, &joined);
    publisher.close(&state, &sid);
    state.registry.leave(&room_name, &sid);
    tracing::info!(room = %room_name, %identity, %sid, "participant left");
}

async fn handle_request(
    state: &AppState,
    joined: &Joined,
    publisher: &mut PublisherSession,
    subscriber: &mut SubscriberSession,
    request: mediacore::SignalRequest,
) -> bool {
    let Some(message) = request.message else {
        return true;
    };

    match message {
        signal_request::Message::AddTrack(add_track) => {
            add_track_request(joined, add_track);
            true
        }
        signal_request::Message::Offer(offer) => {
            publisher.on_offer(state, joined, offer).await;
            true
        }
        signal_request::Message::Answer(answer) => {
            subscriber.on_answer(state, joined, answer).await;
            true
        }
        signal_request::Message::Trickle(trickle) => {
            if trickle.target == mediacore::SignalTarget::Subscriber as i32 {
                subscriber.on_trickle(joined, trickle).await;
            } else {
                publisher.on_trickle(joined, trickle).await;
            }
            true
        }
        signal_request::Message::Subscription(update) => {
            subscription_request(state, joined, subscriber, update).await;
            true
        }
        signal_request::Message::TrackSetting(settings) => {
            subscriber.on_track_settings(state, joined, &settings).await;
            true
        }
        signal_request::Message::Mute(mute) => {
            mute_request(state, joined, mute);
            true
        }
        signal_request::Message::PingReq(ping) => {
            joined
                .participant
                .send(signal_response::Message::PongResp(mediacore::Pong {
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
        signal_request::Message::SubscriptionPermission(permission) => {
            subscription_permission_request(joined, permission);
            true
        }
        signal_request::Message::UpdateMetadata(update) => {
            update_metadata_request(state, joined, update);
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

fn subscription_permission_request(joined: &Joined, request: mediacore::SubscriptionPermission) {
    let Some(cut) = joined
        .room
        .set_subscription_permission(&joined.participant.sid, &request)
    else {
        tracing::debug!(
            identity = %joined.participant.identity,
            "a subscription permission arrived from a session the room no longer holds"
        );
        return;
    };

    tracing::info!(
        identity = %joined.participant.identity,
        all_participants = request.all_participants,
        named = request.track_permissions.len(),
        cut,
        "the publisher restricted who may subscribe"
    );
}

fn update_metadata_request(
    state: &AppState,
    joined: &Joined,
    request: mediacore::UpdateParticipantMetadata,
) {
    if !joined.participant.can_update_metadata() {
        refuse_metadata(
            joined,
            request,
            mediacore::request_response::Reason::NotAllowed,
            "this participant may not change their own name, metadata or attributes".to_owned(),
        );
        return;
    }

    let update = ProfileUpdate {
        name: (!request.name.is_empty()).then(|| request.name.clone()),
        metadata: (!request.metadata.is_empty()).then(|| request.metadata.clone()),
        attributes: request.attributes.clone(),
        permission: None,
    };

    let limit = state.config.sessions.max_opaque_bytes();
    let bytes = joined.participant.profile_bytes_with(&update);

    if bytes > limit {
        refuse_metadata(
            joined,
            request,
            mediacore::request_response::Reason::LimitExceeded,
            format!(
                "a participant profile is limited to {limit} bytes and this one would be {bytes}"
            ),
        );
        return;
    }

    if joined
        .room
        .update_participant(&joined.participant.identity, update)
        .is_none()
    {
        refuse_metadata(
            joined,
            request,
            mediacore::request_response::Reason::NotFound,
            "this session is no longer in the room".to_owned(),
        );
        return;
    }

    joined
        .participant
        .send(signal_response::Message::RequestResponse(
            mediacore::RequestResponse {
                request_id: request.request_id,
                reason: mediacore::request_response::Reason::Ok as i32,
                request: Some(mediacore::request_response::Request::UpdateMetadata(
                    request,
                )),
                ..Default::default()
            },
        ));
}

fn refuse_metadata(
    joined: &Joined,
    request: mediacore::UpdateParticipantMetadata,
    reason: mediacore::request_response::Reason,
    message: String,
) {
    tracing::info!(
        identity = %joined.participant.identity,
        ?reason,
        "refusing a profile update"
    );

    joined
        .participant
        .send(signal_response::Message::RequestResponse(
            mediacore::RequestResponse {
                request_id: request.request_id,
                reason: reason as i32,
                message,
                request: Some(mediacore::request_response::Request::UpdateMetadata(
                    request,
                )),
            },
        ));
}

fn add_track_request(joined: &Joined, request: mediacore::AddTrackRequest) {
    match joined.room.publish_track(&joined.participant.sid, &request) {
        Ok(track) => {
            tracing::info!(
                identity = %joined.participant.identity,
                track = %track.sid,
                cid = %track.cid,
                kind = ?track.kind,
                "track published"
            );

            joined
                .participant
                .send(signal_response::Message::TrackPublished(
                    mediacore::TrackPublishedResponse {
                        cid: request.cid.clone(),
                        track: Some(track.to_proto()),
                    },
                ));
        }
        Err(error) => {
            tracing::info!(
                identity = %joined.participant.identity,
                cid = %request.cid,
                %error,
                "refusing to publish a track"
            );

            joined
                .participant
                .send(signal_response::Message::RequestResponse(
                    mediacore::RequestResponse {
                        reason: error.reason() as i32,
                        message: error.to_string(),
                        request: Some(mediacore::request_response::Request::AddTrack(request)),
                        ..Default::default()
                    },
                ));
        }
    }
}

fn mute_request(state: &AppState, joined: &Joined, request: mediacore::MuteTrackRequest) {
    if joined
        .room
        .set_track_muted(&joined.participant.sid, &request.sid, request.muted)
        .is_none()
    {
        tracing::debug!(
            identity = %joined.participant.identity,
            track = %request.sid,
            "cannot mute a track that is not published"
        );
        return;
    }

    state
        .router
        .set_publisher_muted(&request.sid, request.muted);
}

async fn subscription_request(
    state: &AppState,
    joined: &Joined,
    subscriber: &mut SubscriberSession,
    update: mediacore::UpdateSubscription,
) {
    let refused = joined
        .room
        .update_subscriptions(&joined.participant.sid, &update);

    for track_sid in refused {
        let error = match joined.room.track(&track_sid) {
            Some(_) => mediacore::SubscriptionError::SeUnknown,
            None => mediacore::SubscriptionError::SeTrackNotfound,
        };

        tracing::debug!(
            identity = %joined.participant.identity,
            track = %track_sid,
            ?error,
            "refusing a subscription request"
        );

        joined
            .participant
            .send(signal_response::Message::SubscriptionResponse(
                mediacore::SubscriptionResponse {
                    track_sid,
                    err: error as i32,
                },
            ));
    }

    subscriber.reconcile(state, joined).await;
}

fn request_name(message: &signal_request::Message) -> &'static str {
    match message {
        signal_request::Message::SyncState(_) => "sync_state",
        _ => "other",
    }
}

fn build_join_response(state: &AppState, joined: &Joined) -> mediacore::JoinResponse {
    mediacore::JoinResponse {
        room: Some(joined.room.to_proto()),
        participant: Some(
            joined
                .participant
                .to_proto(mediacore::participant_info::State::Joined),
        ),
        other_participants: joined.others.clone(),
        ice_servers: ice_servers(state, &joined.participant.identity),
        server_version: SERVER_VERSION.to_owned(),
        subscriber_primary: false,
        ping_timeout: PING_TIMEOUT_SECONDS,
        ping_interval: PING_INTERVAL_SECONDS,
        server_info: Some(mediacore::ServerInfo {
            edition: mediacore::server_info::Edition::Standard as i32,
            version: SERVER_VERSION.to_owned(),
            protocol: PROTOCOL_VERSION,
            region: state.config.region.clone(),
            node_id: state.config.node_id.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Where this client should look for a way through, and what to say when it
/// gets there.
///
/// Without this the field went out empty, every client built an
/// `RTCConfiguration` with no ice servers on it, and a coturn running perfectly
/// well next door was never tried once -- there was nothing in the answer to
/// tell a client it existed.
///
/// **Minted per join, not per configuration.** An entry holding a coturn's
/// shared secret gets a credential made here and now, carrying this
/// participant's identity and an expiry: a credential that leaks is good for
/// bandwidth through one relay under one name until it runs out, coturn's logs
/// and its per-user limits say who was holding an allocation, and the secret
/// itself never leaves this process. An entry holding a fixed pair is passed
/// through as it stands, because that pair is what its relay knows.
///
/// [`mediacore::ReconnectResponse`] carries this same field and nothing in this
/// tree builds one yet. Whoever implements resume fills it from here as well,
/// or a client that resumes comes back with the relay it joined with dropped.
fn ice_servers(state: &AppState, identity: &str) -> Vec<mediacore::IceServer> {
    let now = now_ms().max(0) as u64 / 1_000;

    state
        .config
        .rtc
        .ice_servers
        .iter()
        .map(|server| {
            let (username, credential) = if server.mints_credentials() {
                let minted = turn::mint(&server.secret, identity, now, server.credential_ttl());
                (minted.username, minted.credential)
            } else {
                (server.username.clone(), server.credential.clone())
            };

            mediacore::IceServer {
                urls: server.urls.clone(),
                username,
                credential,
            }
        })
        .collect()
}

fn permission_from(grant: &VideoGrant) -> mediacore::ParticipantPermission {
    mediacore::ParticipantPermission {
        can_subscribe: grant.can_subscribe(),
        can_publish: grant.can_publish(),
        can_publish_data: grant.can_publish_data(),
        can_publish_sources: publish_sources(grant),
        can_update_metadata: grant.can_update_own_metadata(),
        hidden: grant.hidden(),
        ..Default::default()
    }
}

/// The pass on a signal connection, from wherever this client put it.
///
/// Browsers put it in the address, because a browser cannot set a header on a
/// websocket handshake. Every server SDK used to do the same and the newer ones
/// no longer do: they send `Authorization: Bearer` on both the handshake and
/// the validate request, and a server reading only the query answers 400 to
/// every one of them. Both are read, the header first, because a client that
/// sent both meant the one it went out of its way to set.
pub fn token_from<'a>(query: &'a SignalQuery, headers: &'a HeaderMap) -> Option<&'a str> {
    if let Ok(token) = crate::twirp::bearer(headers) {
        return Some(token);
    }

    Some(query.access_token.as_str()).filter(|token| !token.is_empty())
}

fn kind_from(claims: &Claims) -> ParticipantKind {
    match claims.kind.trim().to_ascii_lowercase().as_str() {
        "egress" | "recorder" => return ParticipantKind::Egress,
        "sip" => return ParticipantKind::Sip,
        "agent" => return ParticipantKind::Agent,
        _ => {}
    }

    if claims.video.recorder.unwrap_or(false) {
        ParticipantKind::Egress
    } else if claims.video.agent.unwrap_or(false) {
        ParticipantKind::Agent
    } else {
        ParticipantKind::Standard
    }
}

const UNRECOGNISED_SOURCE: i32 = -1;

fn publish_sources(grant: &VideoGrant) -> Vec<i32> {
    grant
        .can_publish_sources
        .iter()
        .map(|source| match source.as_str() {
            "camera" => mediacore::TrackSource::Camera as i32,
            "microphone" => mediacore::TrackSource::Microphone as i32,
            "screen_share" => mediacore::TrackSource::ScreenShare as i32,
            "screen_share_audio" => mediacore::TrackSource::ScreenShareAudio as i32,
            unknown => {
                tracing::warn!(
                    source = %unknown,
                    "unknown publish source in token, keeping the restriction"
                );
                UNRECOGNISED_SOURCE
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
    mediacore::SignalResponse {
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

#[cfg(test)]
#[path = "signal_tests.rs"]
mod tests;
