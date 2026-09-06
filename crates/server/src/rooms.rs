use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method as Verb};
use axum::response::{IntoResponse, Response};
use mediacore_auth::Claims;
use mediacore_proto::livekit;
use mediacore_room::{Adoption, Participant, ProfileUpdate, Room, RoomOptions};

use crate::state::AppState;
use crate::twirp::{self, Codec, Error};

/// Where LiveKit's server SDKs post. The service name is part of the path, so
/// this is not ours to choose: it is `/twirp/<proto package>.<service>/<Method>`.
pub const SERVICE_PATH: &str = "/twirp/livekit.RoomService/{method}";

/// The service name the descriptor pool knows, and half of the path above.
pub const SERVICE: &str = "livekit.RoomService";

/// The same path with no placeholder in it. One axum route serves every
/// method, but the OpenAPI document needs one path per method: a generated
/// client built from `{method}` would have a single function that takes the
/// method name as a string, which is not an API, it is a mailbox.
pub const PATH_PREFIX: &str = "/twirp/livekit.RoomService/";

/// The grant a token must carry for a method to be answered.
///
/// Three, and they are not interchangeable. The one that matters to whoever
/// mints tokens is that only `roomAdmin` is scoped: it is refused unless the
/// token's `room` claim names the room in the request, so an admin token for
/// one meeting cannot moderate another. `roomCreate` and `roomList` name no
/// room at all, which makes either of them a server-wide authority -- and
/// `roomCreate` also gates `DeleteRoom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Create,
    List,
    Admin,
}

impl Grant {
    /// The claim as it is spelled in the token, which is also what the page
    /// calls the security scheme -- so the authorize dialog reads as the list
    /// of grants a caller might hold rather than as one anonymous `bearer`.
    pub const fn claim(self) -> &'static str {
        match self {
            Self::Create => "roomCreate",
            Self::List => "roomList",
            Self::Admin => "roomAdmin",
        }
    }

    /// Whether the token's `room` claim has to name the room in the request.
    pub const fn scoped(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// Whether this server answers a method, and on what terms.
#[derive(Debug, Clone, Copy)]
pub enum Support {
    /// Answered, to a token carrying this grant.
    Served(Grant),
    /// Refused with `unimplemented`, and why. No grant is checked: the refusal
    /// comes before authorization, so every token this server signed gets the
    /// same 501 and the reason is the whole answer.
    Refused(&'static str),
}

/// One method of `livekit.RoomService`, as this server implements it.
#[derive(Debug, Clone, Copy)]
pub struct Method {
    pub name: &'static str,
    pub support: Support,
    /// Fields the server will not guess at, in the spelling the JSON uses.
    /// Absent or empty, and the call comes back `invalid_argument` naming the
    /// field rather than doing something to a room nobody meant.
    pub requires: &'static [&'static str],
    /// What this server does that LiveKit's own comment on the method does
    /// not say -- and it is only ever the difference. Everything the `.proto`
    /// already explains is read off the descriptors.
    pub note: &'static str,
}

/// What this server does with `livekit.RoomService`, method by method.
///
/// The `.proto` describes the wire. It does not say which of these methods
/// this server actually answers, which grant each one insists on, or which
/// fields it refuses to guess at, and those are exactly the things a caller
/// discovers at three in the morning. So they are written once, here:
/// `serve` reads this table to tell a method it refuses from a method that
/// does not exist, and `openapi` renders it onto the page at `/swagger`. A
/// method cannot be documented as implemented and answer 501, because the same
/// row decides both.
pub const METHODS: &[Method] = &[
    Method {
        name: "CreateRoom",
        support: Support::Served(Grant::Create),
        requires: &["name"],
        note: "Settles a room an arrival opened first, but only for a token whose `room` claim \
               names it: `roomCreate` is unscoped, and rewriting the settings of a room people \
               are already sitting in should be asked of a token that says which room it means. \
               Without that scope an existing room comes back untouched with its own metadata, \
               which is how a caller sees that nothing was applied.",
    },
    Method {
        name: "ListRooms",
        support: Support::Served(Grant::List),
        requires: &[],
        note: "Unscoped, as on LiveKit: one `roomList` token sees every room on the server and \
               the whole of every record, metadata included.",
    },
    Method {
        name: "DeleteRoom",
        support: Support::Served(Grant::Create),
        requires: &["room"],
        note: "Disconnects everybody in the room with `ROOM_DELETED` and then forgets it. \
               Deleting a room that is already gone is not a failure. `roomCreate` is unscoped, \
               so one token can end any meeting in progress on this server.",
    },
    Method {
        name: "ListParticipants",
        support: Support::Served(Grant::Admin),
        requires: &["room"],
        note: "Arrival order, and identity where two arrived in the same millisecond, so a \
               roster does not reshuffle between two refreshes. The identities reported are the \
               effective ones, suffix included: a second connection under one name is `alice#2`.",
    },
    Method {
        name: "GetParticipant",
        support: Support::Served(Grant::Admin),
        requires: &["room", "identity"],
        note: "Matches one connection exactly. Ask for `alice` and you get the connection \
               called `alice`, never `alice#2`.",
    },
    Method {
        name: "RemoveParticipant",
        support: Support::Served(Grant::Admin),
        requires: &["room", "identity"],
        note: "Names a person, not a connection: removing `alice` removes `alice`, `alice#2` \
               and `alice#3` together. Leaving one of somebody's two tabs in the room, still \
               audible, is the wrong answer to a moderator's button. Name the suffixed identity \
               to remove exactly one.",
    },
    Method {
        name: "MutePublishedTrack",
        support: Support::Served(Grant::Admin),
        requires: &["room", "identity", "trackSid"],
        note: "Reaches the publisher's own client as well as the room's roster, and stops the \
               forwarder, so a muted track is silent rather than merely marked.",
    },
    Method {
        name: "UpdateParticipant",
        support: Support::Served(Grant::Admin),
        requires: &["room", "identity"],
        note: "Revoking `canPublish` unpublishes the tracks it forbids and tears down their \
               forwarders; revoking `canSubscribe` drops what they were listening to. Turning \
               somebody hidden tells the room they disconnected, because that is the only \
               update a LiveKit client removes a participant from its roster for. An update \
               that changes nothing announces nothing.",
    },
    Method {
        name: "UpdateSubscriptions",
        support: Support::Served(Grant::Admin),
        requires: &["room", "identity"],
        note: "Writes intent; the participant's own session acts on it within a few seconds. \
               The whole request is checked before any of it is applied, because a call that \
               half worked leaves the caller with no way to know which half. An unsubscribe is \
               remembered as a refusal, so the automatic subscriber does not undo it.",
    },
    Method {
        name: "SendData",
        support: Support::Refused(
            "SendData needs a data channel, and mediacore does not carry data packets yet",
        ),
        requires: &[],
        note: "",
    },
    Method {
        name: "UpdateRoomMetadata",
        support: Support::Served(Grant::Admin),
        requires: &["room"],
        note: "Broadcasts the new metadata to everyone in the room, and re-reads the \
               `sessionPolicy` out of it, so a policy can arrive after the room did. Metadata \
               carrying no policy leaves the existing one alone rather than dropping it.",
    },
    Method {
        name: "ForwardParticipant",
        support: Support::Refused(
            "ForwardParticipant moves a participant between nodes, and mediacore is a single node",
        ),
        requires: &[],
        note: "",
    },
    Method {
        name: "MoveParticipant",
        support: Support::Refused(
            "MoveParticipant moves a participant between nodes, and mediacore is a single node",
        ),
        requires: &[],
        note: "",
    },
    Method {
        name: "PerformRpc",
        support: Support::Refused(
            "PerformRpc calls a method on an agent over a data channel, and mediacore has neither",
        ),
        requires: &[],
        note: "",
    },
];

/// The fields of `CreateRoomRequest` this server acts on, and what each does.
///
/// Three lists rather than a paragraph, because the page is rendered from them
/// and a test checks that together they name every field the descriptor has.
/// A field LiveKit adds to the request therefore fails a test until somebody
/// decides which of the three it belongs in -- and that decision is the one
/// that gets skipped, at the cost of whoever believed the page.
pub const CREATE_HONOURED: &[(&str, &str)] = &[
    ("name", "the room, and the one field with no default"),
    (
        "emptyTimeout",
        "seconds a room nobody ever joined is kept before it is collected",
    ),
    (
        "departureTimeout",
        "seconds a room is kept after the last person leaves",
    ),
    (
        "maxParticipants",
        "how many connections the room admits before it turns people away",
    ),
    (
        "metadata",
        "stored, broadcast to the room, and read for a `sessionPolicy`",
    ),
];

/// Accepted, stored nowhere, and acted on by nothing.
///
/// The line between this list and the one below it: a promise about the
/// meeting -- a recording that will exist, an agent that will join, a tag that
/// can be read back -- must not be answered as though it were kept. A hint
/// about which node to place the room on, or how long to buffer playout, costs
/// nothing to ignore on a server with one node and no jitter buffer of its
/// own, and refusing it would turn a working LiveKit client into a broken one
/// over a field it did not mean.
pub const CREATE_IGNORED: &[(&str, &str)] = &[
    ("nodeId", "there is one node, and it is this one"),
    (
        "minPlayoutDelay",
        "playout delay is the client's, and nothing here sets it",
    ),
    (
        "maxPlayoutDelay",
        "playout delay is the client's, and nothing here sets it",
    ),
    (
        "syncStreams",
        "audio and video are forwarded as they arrive, with no stream synchronisation to turn on",
    ),
];

/// Refused with `unimplemented`, naming every one of them, creating nothing.
pub const CREATE_REFUSED: &[(&str, &str)] = &[
    (
        "egress",
        "there is no recorder to start, and a meeting nobody is recording must not answer as \
         though somebody is",
    ),
    ("agents", "there is no agent dispatch"),
    (
        "roomPreset",
        "there are no presets, so the settings one carries would be silently missing",
    ),
    (
        "replayEnabled",
        "nothing is recorded, so there is nothing to replay",
    ),
    (
        "tags",
        "they are not stored, and nothing would read them back",
    ),
];

/// The reason the table gives for refusing a field, for the message a caller
/// reads.
fn refused_because(field: &str) -> &'static str {
    CREATE_REFUSED
        .iter()
        .find(|(name, _)| *name == field)
        .map_or("mediacore does not implement it", |(_, reason)| *reason)
}

/// The reason the table gives for refusing a method, if it refuses it.
fn refusal(method: &str) -> Option<&'static str> {
    METHODS
        .iter()
        .find(|entry| entry.name == method)
        .and_then(|entry| match entry.support {
            Support::Refused(reason) => Some(reason),
            Support::Served(_) => None,
        })
}

/// The one entry point for every RoomService method.
///
/// Twirp routes on the path and the content type together, so a call that gets
/// either wrong is `bad_route` rather than a 404 with no body or a 405 -- an
/// SDK reads the code out of the body, and an empty body reads as a server
/// that fell over rather than as a method that does not exist.
pub async fn handler(
    State(state): State<AppState>,
    Path(method): Path<String>,
    verb: Verb,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match serve(&state, &method, &verb, &headers, &body) {
        Ok(response) => response,
        Err(error) => {
            tracing::info!(
                %method,
                code = error.code.as_str(),
                message = %error.message,
                "room service call refused"
            );
            error.into_response()
        }
    }
}

fn serve(
    state: &AppState,
    method: &str,
    verb: &Verb,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, Error> {
    if verb != Verb::POST {
        return Err(Error::bad_route(format!(
            "unsupported method {verb}, every Twirp call is a POST"
        )));
    }

    let codec = Codec::from_headers(headers)?;
    let claims = mediacore_auth::verify(twirp::bearer(headers)?, &state.config.keys)
        .map_err(|error| Error::unauthenticated(error.to_string()))?;

    match method {
        "CreateRoom" => encode(codec, create_room(state, &claims, decode(codec, body)?)?),
        "ListRooms" => encode(codec, list_rooms(state, &claims, decode(codec, body)?)?),
        "DeleteRoom" => encode(codec, delete_room(state, &claims, decode(codec, body)?)?),
        "ListParticipants" => encode(
            codec,
            list_participants(state, &claims, decode(codec, body)?)?,
        ),
        "GetParticipant" => encode(
            codec,
            get_participant(state, &claims, decode(codec, body)?)?,
        ),
        "RemoveParticipant" => encode(
            codec,
            remove_participant(state, &claims, decode(codec, body)?)?,
        ),
        "MutePublishedTrack" => encode(codec, mute_track(state, &claims, decode(codec, body)?)?),
        "UpdateParticipant" => encode(
            codec,
            update_participant(state, &claims, decode(codec, body)?)?,
        ),
        "UpdateSubscriptions" => encode(
            codec,
            update_subscriptions(state, &claims, decode(codec, body)?)?,
        ),
        "UpdateRoomMetadata" => encode(
            codec,
            update_room_metadata(state, &claims, decode(codec, body)?)?,
        ),
        // Everything else is either a method the table says we refuse, or a
        // method that does not exist. Reading the reason out of `METHODS`
        // rather than matching on the names again is what keeps the page and
        // the server from disagreeing about which is which.
        unknown => Err(match refusal(unknown) {
            Some(reason) => Error::unimplemented(reason),
            None => Error::bad_route(format!("livekit.RoomService has no method `{unknown}`")),
        }),
    }
}

/// Creates a room, or settles one an arrival opened first.
///
/// Adoption is what closes the trap of `auto_create`: a client can open
/// `standup` a second before the backoffice calls `CreateRoom`, and the room
/// that arrival made carries none of the settings anybody chose. But
/// `roomCreate` names no room -- one backoffice token is an authority over
/// every room on the server -- so rewriting the settings and the session
/// policy of a room that already has people in it is asked of a token that
/// also names the room in its `room` claim. Without that scope the existing
/// room comes back untouched, exactly as LiveKit's own `CreateRoom` returns
/// it, and the metadata in the answer is the room's, so a caller can see that
/// what it sent was not applied.
fn create_room(
    state: &AppState,
    claims: &Claims,
    request: livekit::CreateRoomRequest,
) -> Result<livekit::Room, Error> {
    let livekit::CreateRoomRequest {
        name,
        room_preset,
        empty_timeout,
        departure_timeout,
        max_participants,
        node_id: _,
        metadata,
        tags,
        egress,
        min_playout_delay: _,
        max_playout_delay: _,
        sync_streams: _,
        replay_enabled,
        agents,
    } = request;

    require(&name, "name")?;
    ensure_create(claims)?;

    let unsupported = [
        ("egress", egress.is_some()),
        ("agents", !agents.is_empty()),
        ("roomPreset", !room_preset.is_empty()),
        ("replayEnabled", replay_enabled),
        ("tags", !tags.is_empty()),
    ]
    .into_iter()
    .filter(|(_, asked)| *asked)
    .map(|(field, _)| format!("{field} ({})", refused_because(field)))
    .collect::<Vec<String>>();
    if !unsupported.is_empty() {
        return Err(Error::unimplemented(format!(
            "mediacore cannot honour {}, and will not answer as if it had",
            unsupported.join("; ")
        )));
    }

    let adoption = if claims.video.room == name {
        Adoption::Always
    } else {
        Adoption::WhenEmpty
    };

    let policy = SessionPolicy::read(&metadata);
    let room = state.registry.create(
        &name,
        RoomOptions {
            metadata,
            empty_timeout: chosen(empty_timeout),
            departure_timeout: chosen(departure_timeout),
            max_participants: chosen(max_participants),
            min_humans: policy.min_humans,
            end_grace: policy.end_grace,
        },
        adoption,
    );

    Ok(room.to_proto())
}

fn list_rooms(
    state: &AppState,
    claims: &Claims,
    request: livekit::ListRoomsRequest,
) -> Result<livekit::ListRoomsResponse, Error> {
    ensure_list(claims)?;

    let mut rooms: Vec<livekit::Room> = state
        .registry
        .rooms()
        .into_iter()
        .filter(|room| request.names.is_empty() || request.names.contains(&room.name))
        .map(|room| room.to_proto())
        .collect();

    rooms.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(livekit::ListRoomsResponse { rooms })
}

fn delete_room(
    state: &AppState,
    claims: &Claims,
    request: livekit::DeleteRoomRequest,
) -> Result<livekit::DeleteRoomResponse, Error> {
    require(&request.room, "room")?;
    ensure_create(claims)?;

    match state.registry.delete(&request.room) {
        Some(room) => tracing::info!(room = %room.name, sid = %room.sid, "room deleted"),
        None => tracing::debug!(
            room = %request.room,
            "nothing to delete, the room is already gone"
        ),
    }

    Ok(livekit::DeleteRoomResponse {})
}

fn list_participants(
    state: &AppState,
    claims: &Claims,
    request: livekit::ListParticipantsRequest,
) -> Result<livekit::ListParticipantsResponse, Error> {
    require(&request.room, "room")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;

    // Arrival order, and identity where two arrived in the same millisecond:
    // participants live in a hash map, and a roster that reshuffles itself
    // between two refreshes is worse than one in an order nobody chose.
    let mut participants = room.participants();
    participants
        .sort_by_key(|participant| (participant.joined_at_ms, participant.identity.clone()));

    Ok(livekit::ListParticipantsResponse {
        participants: participants
            .iter()
            .map(|participant| participant.to_proto(livekit::participant_info::State::Active))
            .collect(),
    })
}

fn get_participant(
    state: &AppState,
    claims: &Claims,
    request: livekit::RoomParticipantIdentity,
) -> Result<livekit::ParticipantInfo, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let participant = participant_of(&room, &request.identity)?;

    Ok(participant.to_proto(livekit::participant_info::State::Active))
}

/// Takes a person out of a room, every connection of theirs at once.
///
/// A caller who names `155592` means the person, and a person can hold more
/// than one connection here, because a second one is given `155592#2` rather
/// than displacing the first. Removing one of two and leaving the other is the
/// wrong answer to a moderator's button: the person is still in the room, still
/// audible. A caller who wants one connection names the suffixed identity that
/// `ListParticipants` reported, and gets exactly that one.
fn remove_participant(
    state: &AppState,
    claims: &Claims,
    request: livekit::RoomParticipantIdentity,
) -> Result<livekit::RemoveParticipantResponse, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let connections = room.participants_of(&request.identity);

    if connections.is_empty() {
        return Err(missing_participant(&room, &request.identity));
    }

    for connection in connections {
        state
            .registry
            .remove_participant(&request.room, &connection.identity);
    }

    Ok(livekit::RemoveParticipantResponse {})
}

fn mute_track(
    state: &AppState,
    claims: &Claims,
    request: livekit::MuteRoomTrackRequest,
) -> Result<livekit::MuteRoomTrackResponse, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    require(&request.track_sid, "track_sid")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let participant = participant_of(&room, &request.identity)?;
    let track = room
        .set_track_muted(&participant.sid, &request.track_sid, request.muted)
        .ok_or_else(|| {
            Error::not_found(format!(
                "`{}` is not publishing track `{}`",
                request.identity, request.track_sid
            ))
        })?;

    state
        .router
        .set_publisher_muted(&request.track_sid, request.muted);
    participant.send(livekit::signal_response::Message::Mute(
        livekit::MuteTrackRequest {
            sid: request.track_sid.clone(),
            muted: request.muted,
        },
    ));

    tracing::info!(
        room = %request.room,
        identity = %request.identity,
        track = %request.track_sid,
        muted = request.muted,
        "track muted by the room service"
    );

    Ok(livekit::MuteRoomTrackResponse {
        track: Some(track.to_proto()),
    })
}

fn update_participant(
    state: &AppState,
    claims: &Claims,
    request: livekit::UpdateParticipantRequest,
) -> Result<livekit::ParticipantInfo, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let update = ProfileUpdate {
        name: chosen_text(request.name),
        metadata: chosen_text(request.metadata),
        attributes: request.attributes,
        permission: request.permission,
    };

    let updated = room
        .update_participant(&request.identity, update)
        .ok_or_else(|| missing_participant(&room, &request.identity))?;

    // Taking a track out of the room does not stop it. The publisher is still
    // connected -- they were demoted, not disconnected -- so their RTP keeps
    // arriving and the router keeps forwarding it to every sink it already
    // has. `peer.rs::unpublish`, the path a client takes when it unpublishes
    // for itself, drops the route here; so does this one.
    for track_sid in &updated.unpublished {
        let subscribers = state.router.drop_track(track_sid);

        tracing::info!(
            room = %request.room,
            identity = %request.identity,
            track = %track_sid,
            subscribers,
            "tore down every forwarder of a track the participant may no longer publish"
        );
    }

    Ok(updated
        .participant
        .to_proto(livekit::participant_info::State::Active))
}

/// Subscribes or unsubscribes somebody else's session on their behalf.
///
/// The check runs over the whole request before any of it is applied, because
/// a call that half worked leaves the caller with no way to know which half.
/// What it changes is intent; the participant's own session notices on its next
/// reconcile, within a few seconds, and negotiates from there.
fn update_subscriptions(
    state: &AppState,
    claims: &Claims,
    request: livekit::UpdateSubscriptionsRequest,
) -> Result<livekit::UpdateSubscriptionsResponse, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let participant = participant_of(&room, &request.identity)?;
    let update = livekit::UpdateSubscription {
        track_sids: request.track_sids,
        subscribe: request.subscribe,
        participant_tracks: request.participant_tracks,
    };

    if update.subscribe {
        let mut wanted = update.track_sids.clone();
        for tracks in &update.participant_tracks {
            wanted.extend(tracks.track_sids.iter().cloned());
        }
        wanted.sort();
        wanted.dedup();

        let missing = wanted
            .iter()
            .filter(|track_sid| room.track(track_sid).is_none())
            .cloned()
            .collect::<Vec<String>>();
        if !missing.is_empty() {
            return Err(Error::not_found(format!(
                "room `{}` has no track {}",
                room.name,
                missing.join(", ")
            )));
        }

        let refused = wanted
            .iter()
            .filter(|track_sid| !room.can_subscribe(&participant.sid, track_sid))
            .cloned()
            .collect::<Vec<String>>();
        if !refused.is_empty() {
            return Err(Error::invalid_argument(format!(
                "`{}` may not subscribe to {}",
                request.identity,
                refused.join(", ")
            )));
        }
    }

    room.update_subscriptions(&participant.sid, &update);

    Ok(livekit::UpdateSubscriptionsResponse {})
}

fn update_room_metadata(
    state: &AppState,
    claims: &Claims,
    request: livekit::UpdateRoomMetadataRequest,
) -> Result<livekit::Room, Error> {
    require(&request.room, "room")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let policy = SessionPolicy::read(&request.metadata);

    room.set_session_policy(policy.min_humans, policy.end_grace);
    room.set_metadata(&request.metadata);

    Ok(room.to_proto())
}

/// When a session ends, read out of the room metadata.
///
/// `CreateRoomRequest` has no field for either number -- LiveKit has no such
/// policy -- so the only way one can reach us through an unmodified server SDK
/// is inside the metadata the caller already sends. MCN's backoffice keeps a
/// room record shaped `{"sessionPolicy": {"minHumans": 2, "endGraceMs": 90000}}`
/// and that is the shape read here. Metadata that says nothing about a session
/// leaves the policy to the server configuration, and metadata that is not
/// JSON at all is left entirely alone: it is the caller's opaque blob and it
/// round-trips unchanged either way.
#[derive(Debug, Default, PartialEq, Eq)]
struct SessionPolicy {
    min_humans: Option<u32>,
    end_grace: Option<u32>,
}

impl SessionPolicy {
    fn read(metadata: &str) -> Self {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(metadata) else {
            return Self::default();
        };
        let Some(policy) = record.get("sessionPolicy") else {
            return Self::default();
        };

        Self {
            min_humans: policy
                .get("minHumans")
                .and_then(serde_json::Value::as_u64)
                .map(saturating),
            end_grace: policy
                .get("endGraceMs")
                .and_then(serde_json::Value::as_u64)
                .map(|milliseconds| saturating(milliseconds.div_ceil(1_000))),
        }
    }
}

fn saturating(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A zero in a protobuf request is a field the caller left out.
///
/// Proto3 cannot tell "zero" from "unset" on a scalar, and LiveKit's own
/// server reads these the same way, so `emptyTimeout: 0` means "whatever the
/// server's default is" rather than "close this room the instant it empties".
/// The consequence is that a zero timeout cannot be asked for over this API at
/// all; it has to come from the configuration file.
fn chosen(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

/// The same rule for a string field: empty means the caller did not mention it.
fn chosen_text(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn require(value: &str, field: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(Error::invalid_argument(format!(
            "`{field}` is required and was empty"
        )));
    }

    Ok(())
}

fn room_of(state: &AppState, name: &str) -> Result<Arc<Room>, Error> {
    state
        .registry
        .room(name)
        .ok_or_else(|| Error::not_found(format!("room `{name}` does not exist")))
}

fn participant_of(room: &Room, identity: &str) -> Result<Arc<Participant>, Error> {
    room.participant(identity)
        .ok_or_else(|| missing_participant(room, identity))
}

fn missing_participant(room: &Room, identity: &str) -> Error {
    Error::not_found(format!(
        "`{identity}` is not in room `{}`",
        room.name.as_str()
    ))
}

/// `roomCreate`, which LiveKit also wants for `DeleteRoom`: whoever may bring a
/// room into being is whoever may take it away again.
fn ensure_create(claims: &Claims) -> Result<(), Error> {
    if claims.video.room_create.unwrap_or(false) {
        return Ok(());
    }

    Err(Error::permission_denied(
        "this token does not carry the roomCreate grant",
    ))
}

fn ensure_list(claims: &Claims) -> Result<(), Error> {
    if claims.video.room_list.unwrap_or(false) {
        return Ok(());
    }

    Err(Error::permission_denied(
        "this token does not carry the roomList grant",
    ))
}

/// `roomAdmin`, and only over the room the token names.
///
/// The room claim is the whole point of the grant. Without the second half an
/// admin token minted for one meeting would moderate, mute and empty every
/// other room on the server, which is the difference between a moderator
/// button and a master key.
fn ensure_admin(claims: &Claims, room: &str) -> Result<(), Error> {
    if !claims.video.room_admin.unwrap_or(false) {
        return Err(Error::permission_denied(
            "this token does not carry the roomAdmin grant",
        ));
    }

    if claims.video.room != room {
        return Err(Error::permission_denied(format!(
            "this token is an admin of room `{}`, not of `{room}`",
            claims.video.room
        )));
    }

    Ok(())
}

fn decode<T>(codec: Codec, body: &Bytes) -> Result<T, Error>
where
    T: prost::Message + Default + serde::de::DeserializeOwned,
{
    twirp::decode(codec, body)
}

fn encode<T>(codec: Codec, message: T) -> Result<Response, Error>
where
    T: prost::Message + serde::Serialize,
{
    twirp::encode(codec, &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediacore_auth::VideoGrant;

    fn claims(video: VideoGrant) -> Claims {
        Claims {
            video,
            ..Claims::default()
        }
    }

    #[test]
    fn an_admin_token_is_an_admin_of_one_room_only() {
        let admin = claims(VideoGrant {
            room_admin: Some(true),
            room: "standup".to_owned(),
            ..VideoGrant::default()
        });

        assert!(ensure_admin(&admin, "standup").is_ok());
        assert_eq!(
            ensure_admin(&admin, "payroll").unwrap_err().code,
            twirp::Code::PermissionDenied,
            "an admin token that works on any room is a master key for the whole server"
        );
    }

    #[test]
    fn a_join_token_administers_nothing() {
        let joiner = claims(VideoGrant {
            room_join: Some(true),
            room: "standup".to_owned(),
            ..VideoGrant::default()
        });

        assert!(
            ensure_admin(&joiner, "standup").is_err(),
            "roomJoin is the grant everyone in the meeting holds; it must not delete the meeting"
        );
        assert!(ensure_create(&joiner).is_err());
        assert!(ensure_list(&joiner).is_err());
    }

    #[test]
    fn the_grants_do_not_stand_in_for_each_other() {
        let creator = claims(VideoGrant {
            room_create: Some(true),
            ..VideoGrant::default()
        });
        let lister = claims(VideoGrant {
            room_list: Some(true),
            ..VideoGrant::default()
        });

        assert!(ensure_create(&creator).is_ok());
        assert!(ensure_list(&creator).is_err());
        assert!(ensure_list(&lister).is_ok());
        assert!(ensure_create(&lister).is_err());
    }

    #[test]
    fn mcns_room_record_carries_a_session_policy() {
        let policy = SessionPolicy::read(
            r#"{"sessionPolicy":{"minHumans":2,"endGraceMs":90000},"recordingEnabled":true}"#,
        );

        assert_eq!(policy.min_humans, Some(2));
        assert_eq!(
            policy.end_grace,
            Some(90),
            "the grace arrives in milliseconds and the room counts seconds"
        );
    }

    #[test]
    fn metadata_that_says_nothing_about_a_session_leaves_the_policy_alone() {
        assert_eq!(SessionPolicy::read(""), SessionPolicy::default());
        assert_eq!(
            SessionPolicy::read("a note from the backoffice"),
            SessionPolicy::default(),
            "metadata is an opaque blob to everyone but its owner, and most of it is not JSON"
        );
        assert_eq!(
            SessionPolicy::read(r#"{"title":"Standup"}"#),
            SessionPolicy::default()
        );
        assert_eq!(
            SessionPolicy::read(r#"{"sessionPolicy":{"minHumans":2}}"#),
            SessionPolicy {
                min_humans: Some(2),
                end_grace: None,
            },
            "half a policy leaves the other half to the server configuration"
        );
    }

    #[test]
    fn a_grace_that_does_not_divide_into_seconds_rounds_up() {
        assert_eq!(
            SessionPolicy::read(r#"{"sessionPolicy":{"endGraceMs":1500}}"#).end_grace,
            Some(2),
            "rounding down would end a session earlier than the caller asked for"
        );
        assert_eq!(
            SessionPolicy::read(r#"{"sessionPolicy":{"endGraceMs":1}}"#).end_grace,
            Some(1),
            "a grace shorter than a second is still a grace, not none at all"
        );
    }
}
