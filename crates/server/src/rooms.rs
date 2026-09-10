use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method as Verb, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use mediacore_auth::Claims;
use mediacore_proto::mediacore;
use mediacore_room::{Adoption, Participant, ProfileUpdate, Room, RoomOptions};

use crate::data::distribute;
use crate::relay;
use crate::state::AppState;
use crate::twirp::{self, Codec, Error};

/// Where a caller posts. Twirp derives the path from the schema rather than
/// letting anyone choose it -- `/twirp/<proto package>.<service>/<Method>` --
/// so this reads `mediacore` because `build.rs` renames the package on the
/// descriptors, and for no other reason.
pub const SERVICE_PATH: &str = "/twirp/mediacore.RoomService/{method}";

/// The service name the descriptor pool knows, and half of the path above.
pub const SERVICE: &str = "mediacore.RoomService";

/// The same path with no placeholder in it. One axum route serves every
/// method, but the OpenAPI document needs one path per method: a generated
/// client built from `{method}` would have a single function that takes the
/// method name as a string, which is not an API, it is a mailbox.
pub const PATH_PREFIX: &str = "/twirp/mediacore.RoomService/";

/// What the same service was called before the package was renamed.
///
/// LiveKit's server SDKs build this path inside generated code, from the
/// package their own `.proto` declares, and MCN's backoffice points at us
/// without a line changed. Renaming the package broke nothing on the
/// signalling wire -- protobuf sends field numbers, not names -- but the Twirp
/// URL is the one place the package was ever visible outside this process, so
/// it is the one place the old name has to survive.
///
/// It is an alias, not a second implementation: [`handler`] serves both routes
/// and reads the same [`METHODS`] table for both, so the two cannot come to
/// disagree about which methods exist or what they need. The only thing the
/// path decides is what a `bad_route` body calls the service.
pub const ALIAS_SERVICE: &str = "livekit.RoomService";

/// The alias route, registered when `twirp.livekit_alias` is on.
pub const ALIAS_PATH: &str = "/twirp/livekit.RoomService/{method}";

/// The alias path with no placeholder, which is how [`handler`] tells the two
/// routes apart.
pub const ALIAS_PREFIX: &str = "/twirp/livekit.RoomService/";

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

/// One method of `mediacore.RoomService`, as this server implements it.
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

/// What this server does with `mediacore.RoomService`, method by method.
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
        support: Support::Served(Grant::Admin),
        requires: &["room"],
        note: "Delivered over the participants' own data channels, so a participant whose \
               channel has not opened yet receives nothing and is not waited for. The payload \
               is wrapped in a `UserPacket` with no sender, which is what makes it arrive as \
               a message from the room rather than from somebody in it. \
               `destinationSids` is refused: a name in `destinationIdentities` is a person, \
               so `alice` reaches every connection alice has, `alice#2` included.",
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
        "stored, broadcast to the room, read for a `sessionPolicy`, and kept on the record of \
         every meeting held here",
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

/// The one entry point for every RoomService method, under either name.
///
/// Twirp routes on the path and the content type together, so a call that gets
/// either wrong is `bad_route` rather than a 404 with no body or a 405 -- an
/// SDK reads the code out of the body, and an empty body reads as a server
/// that fell over rather than as a method that does not exist.
///
/// [`SERVICE_PATH`] and [`ALIAS_PATH`] both arrive here. Nothing downstream
/// knows which one was used except the text of a `bad_route`, and that is the
/// point: one dispatcher, one grant table, one set of error bodies, so the
/// compatibility route cannot rot into something that behaves differently from
/// the canonical one.
pub async fn handler(
    State(state): State<AppState>,
    Path(method): Path<String>,
    verb: Verb,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let service = service_for(uri.path());

    // Whoever holds the room answers about it. A call that landed on the wrong
    // node would otherwise be told, correctly for this process and wrongly for
    // the cluster, that the room is empty -- which is worse than an error,
    // because nobody doubts it.
    if let Some(answer) = forwarded(&state, service, &method, &headers, &body).await {
        return answer;
    }

    if let Some(answer) = gathered(&state, service, &method, &headers, &body).await {
        return answer;
    }

    match serve(&state, service, &method, &verb, &headers, &body) {
        Ok(response) => response,
        Err(error) => {
            tracing::info!(
                %service,
                %method,
                code = error.code.as_str(),
                message = %error.message,
                "room service call refused"
            );
            error.into_response()
        }
    }
}

/// Which of the two names the caller used.
///
/// A `bad_route` has to name the service the caller actually posted to. Told
/// that `mediacore.RoomService` has no method `ListRoom`, somebody who posted
/// to `/twirp/livekit.RoomService/ListRoom` goes looking for a path they never
/// used, and the typo they made stays invisible behind a name they have never
/// seen.
fn service_for(path: &str) -> &'static str {
    if path.starts_with(ALIAS_PREFIX) {
        ALIAS_SERVICE
    } else {
        SERVICE
    }
}

fn serve(
    state: &AppState,
    service: &str,
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
        "SendData" => encode(codec, send_data(state, &claims, decode(codec, body)?)?),
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
            None => Error::bad_route(format!("{service} has no method `{unknown}`")),
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
    request: mediacore::CreateRoomRequest,
) -> Result<mediacore::Room, Error> {
    let mediacore::CreateRoomRequest {
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
            record: policy.record,
        },
        adoption,
    );

    Ok(room.to_proto())
}

fn list_rooms(
    state: &AppState,
    claims: &Claims,
    request: mediacore::ListRoomsRequest,
) -> Result<mediacore::ListRoomsResponse, Error> {
    ensure_list(claims)?;

    let mut rooms: Vec<mediacore::Room> = state
        .registry
        .rooms()
        .into_iter()
        .filter(|room| request.names.is_empty() || request.names.contains(&room.name))
        .map(|room| room.to_proto())
        .collect();

    rooms.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(mediacore::ListRoomsResponse { rooms })
}

fn delete_room(
    state: &AppState,
    claims: &Claims,
    request: mediacore::DeleteRoomRequest,
) -> Result<mediacore::DeleteRoomResponse, Error> {
    require(&request.room, "room")?;
    ensure_create(claims)?;

    match state.registry.delete(&request.room) {
        Some(room) => tracing::info!(room = %room.name, sid = %room.sid, "room deleted"),
        None => tracing::debug!(
            room = %request.room,
            "nothing to delete, the room is already gone"
        ),
    }

    Ok(mediacore::DeleteRoomResponse {})
}

fn list_participants(
    state: &AppState,
    claims: &Claims,
    request: mediacore::ListParticipantsRequest,
) -> Result<mediacore::ListParticipantsResponse, Error> {
    require(&request.room, "room")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;

    // Arrival order, and identity where two arrived in the same millisecond:
    // participants live in a hash map, and a roster that reshuffles itself
    // between two refreshes is worse than one in an order nobody chose.
    let mut participants = room.participants();
    participants
        .sort_by_key(|participant| (participant.joined_at_ms, participant.identity.clone()));

    Ok(mediacore::ListParticipantsResponse {
        participants: participants
            .iter()
            .map(|participant| participant.to_proto(mediacore::participant_info::State::Active))
            .collect(),
    })
}

fn get_participant(
    state: &AppState,
    claims: &Claims,
    request: mediacore::RoomParticipantIdentity,
) -> Result<mediacore::ParticipantInfo, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let participant = participant_of(&room, &request.identity)?;

    Ok(participant.to_proto(mediacore::participant_info::State::Active))
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
    request: mediacore::RoomParticipantIdentity,
) -> Result<mediacore::RemoveParticipantResponse, Error> {
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

    Ok(mediacore::RemoveParticipantResponse {})
}

fn mute_track(
    state: &AppState,
    claims: &Claims,
    request: mediacore::MuteRoomTrackRequest,
) -> Result<mediacore::MuteRoomTrackResponse, Error> {
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
    participant.send(mediacore::signal_response::Message::Mute(
        mediacore::MuteTrackRequest {
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

    Ok(mediacore::MuteRoomTrackResponse {
        track: Some(track.to_proto()),
    })
}

fn update_participant(
    state: &AppState,
    claims: &Claims,
    request: mediacore::UpdateParticipantRequest,
) -> Result<mediacore::ParticipantInfo, Error> {
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
        .to_proto(mediacore::participant_info::State::Active))
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
    request: mediacore::UpdateSubscriptionsRequest,
) -> Result<mediacore::UpdateSubscriptionsResponse, Error> {
    require(&request.room, "room")?;
    require(&request.identity, "identity")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let participant = participant_of(&room, &request.identity)?;
    let update = mediacore::UpdateSubscription {
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

    Ok(mediacore::UpdateSubscriptionsResponse {})
}

#[allow(deprecated)]
fn send_data(
    state: &AppState,
    claims: &Claims,
    request: mediacore::SendDataRequest,
) -> Result<mediacore::SendDataResponse, Error> {
    require(&request.room, "room")?;
    ensure_admin(claims, &request.room)?;

    if !request.destination_sids.is_empty() {
        return Err(Error::unimplemented(
            "destinationSids names connections and mediacore will not answer as if it had \
             honoured it; name the people in destinationIdentities instead",
        ));
    }

    if request.data.len() > state.config.data.max_packet_bytes as usize {
        return Err(Error::invalid_argument(format!(
            "data is {} bytes, over the {} this server carries in one packet",
            request.data.len(),
            state.config.data.max_packet_bytes
        )));
    }

    let room = room_of(state, &request.room)?;
    let named = !request.destination_identities.is_empty();
    let packet = mediacore::DataPacket {
        kind: request.kind,
        destination_identities: request.destination_identities,
        value: Some(mediacore::data_packet::Value::User(mediacore::UserPacket {
            payload: request.data,
            topic: request.topic,
            nonce: request.nonce,
            ..Default::default()
        })),
        ..Default::default()
    };

    let delivered = distribute(&state.data, &room, &packet, None);

    if named && delivered == 0 {
        return Err(Error::not_found(format!(
            "room `{}` has nobody listening under the identities named",
            request.room
        )));
    }

    tracing::info!(room = %request.room, delivered, "the room service sent a data packet");

    Ok(mediacore::SendDataResponse {})
}

fn update_room_metadata(
    state: &AppState,
    claims: &Claims,
    request: mediacore::UpdateRoomMetadataRequest,
) -> Result<mediacore::Room, Error> {
    require(&request.room, "room")?;
    ensure_admin(claims, &request.room)?;

    let room = room_of(state, &request.room)?;
    let policy = SessionPolicy::read(&request.metadata);

    state
        .registry
        .set_session_policy(&room, policy.min_humans, policy.end_grace, policy.record);
    room.set_metadata(&request.metadata);

    Ok(room.to_proto())
}

/// When a session ends and whether it is recorded, read out of the room
/// metadata.
///
/// `CreateRoomRequest` has no field for any of these -- LiveKit has no such
/// policy -- so the only way one can reach us through an unmodified server SDK
/// is inside the metadata the caller already sends. MCN's backoffice keeps a
/// room record shaped `{"sessionPolicy": {"minHumans": 2, "endGraceMs": 90000}}`
/// and that is the shape read here. Metadata that says nothing about a session
/// leaves the policy to the server configuration, and metadata that is not
/// JSON at all is left entirely alone: it is the caller's opaque blob and it
/// round-trips unchanged either way.
///
/// `record` lives beside `minHumans` rather than in a block of its own because
/// the two are one decision. Recording follows the meeting: there is no second
/// threshold saying how many people make a recording, so a room where everybody
/// sits together and joins from one account sets `minHumans` to one and is
/// recorded like any other. A rule that could not be set to one is a rule that
/// loses meetings, which is what happened next door.
#[derive(Debug, Default, PartialEq, Eq)]
struct SessionPolicy {
    min_humans: Option<u32>,
    end_grace: Option<u32>,
    record: Option<bool>,
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
            record: policy.get("record").and_then(serde_json::Value::as_bool),
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
#[path = "rooms_tests.rs"]
mod tests;

/// The room one call is about, when it is about exactly one.
///
/// Only the shapes that name a room are read, and only the field is taken --
/// the body itself goes over untouched, so nothing here has to keep up with
/// the rest of the request. `ListRooms` is absent on purpose: it names a set,
/// and answering it needs every node rather than one.
fn room_named(codec: Codec, method: &str, body: &Bytes) -> Option<String> {
    let named = |room: String| (!room.is_empty()).then_some(room);

    match method {
        "CreateRoom" => named(twirp::decode::<mediacore::CreateRoomRequest>(codec, body).ok()?.name),
        "DeleteRoom" => named(twirp::decode::<mediacore::DeleteRoomRequest>(codec, body).ok()?.room),
        "ListParticipants" => {
            named(twirp::decode::<mediacore::ListParticipantsRequest>(codec, body).ok()?.room)
        }
        "GetParticipant" => {
            named(twirp::decode::<mediacore::RoomParticipantIdentity>(codec, body).ok()?.room)
        }
        "RemoveParticipant" => {
            named(twirp::decode::<mediacore::RoomParticipantIdentity>(codec, body).ok()?.room)
        }
        "MutePublishedTrack" => {
            named(twirp::decode::<mediacore::MuteRoomTrackRequest>(codec, body).ok()?.room)
        }
        "UpdateParticipant" => {
            named(twirp::decode::<mediacore::UpdateParticipantRequest>(codec, body).ok()?.room)
        }
        "UpdateSubscriptions" => {
            named(twirp::decode::<mediacore::UpdateSubscriptionsRequest>(codec, body).ok()?.room)
        }
        "SendData" => named(twirp::decode::<mediacore::SendDataRequest>(codec, body).ok()?.room),
        "UpdateRoomMetadata" => {
            named(twirp::decode::<mediacore::UpdateRoomMetadataRequest>(codec, body).ok()?.room)
        }
        _ => None,
    }
}

/// The answer from the node that holds this room, or `None` to serve it here.
///
/// `None` covers a node on its own, a call about a room this node holds, a call
/// that names no room, and a call that has already been forwarded once. That
/// last one is the loop guard, and it is the same rule the signalling relay
/// uses: a forwarded call is answered or refused where it lands.
///
/// A `CreateRoom` for a room nobody holds is forwarded to whichever node the
/// cluster scores highest for that name, so the room is made where the next
/// arrival will be carried, rather than here and then relayed away.
async fn forwarded(
    state: &AppState,
    service: &str,
    method: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Option<Response> {
    if !state.cluster.joined() || headers.contains_key(relay::RELAY_HEADER) {
        return None;
    }

    let codec = Codec::from_headers(headers).ok()?;
    let room = room_named(codec, method, body)?;
    let view = state.cluster.view();
    let owner = view.should_hold(&room)?;

    if owner.id == state.cluster.id() {
        return None;
    }

    let token = twirp::bearer(headers).ok()?;
    let outcome = relay::forward(
        &owner.signal_url,
        service,
        method,
        codec.content_type(),
        token,
        body.clone(),
    )
    .await;

    match outcome {
        Ok((status, content_type, answer)) => {
            tracing::debug!(%room, holder = %owner.id, %method, status, "a room service call was answered elsewhere");

            Some(
                (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    [(header::CONTENT_TYPE, content_type)],
                    answer,
                )
                    .into_response(),
            )
        }
        Err(error) => {
            tracing::warn!(%room, holder = %owner.id, %method, %error, "a room service call could not be forwarded");

            Some(Error::unavailable(error).into_response())
        }
    }
}

/// `ListRooms` across the whole cluster.
///
/// The one call that is about no room in particular, so it cannot be forwarded
/// to an owner: it is asked of everybody and the answers are put together. A
/// backoffice that saw only the rooms of whichever node the load balancer
/// picked would show a third of the meetings on a cluster of three and have no
/// way of telling.
///
/// Each node is asked with the same mark a forwarded call carries, so it
/// answers for itself and asks nobody -- one round, never a storm. A node that
/// does not answer is left out and said so in the log rather than failing the
/// call: a partial list with a line in the log is more use to whoever is
/// looking than an error, and the alternative is one sick node making the
/// cluster unreadable.
async fn gathered(
    state: &AppState,
    service: &str,
    method: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Option<Response> {
    if method != "ListRooms" || !state.cluster.joined() || headers.contains_key(relay::RELAY_HEADER)
    {
        return None;
    }

    let codec = Codec::from_headers(headers).ok()?;
    let token = twirp::bearer(headers).ok()?.to_owned();
    let view = state.cluster.view();

    let elsewhere: Vec<(String, String)> = view
        .alive()
        .filter(|node| node.id != state.cluster.id())
        .map(|node| (node.id.clone(), node.signal_url.clone()))
        .collect();

    // Nobody else to ask. Serving it here is the same answer and one fewer
    // decode, so the ordinary single-node case pays nothing for this.
    if elsewhere.is_empty() {
        return None;
    }

    let mut rooms = match serve(state, service, method, &Verb::POST, headers, body) {
        Ok(_) => local_rooms(state, headers, body)?,
        Err(error) => return Some(error.into_response()),
    };

    let asked = elsewhere.iter().map(|(id, url)| {
        let token = token.clone();
        let body = body.clone();

        async move {
            let answer =
                relay::forward(url, service, method, codec.content_type(), &token, body).await;

            (id.clone(), answer)
        }
    });

    for (id, answer) in futures_util::future::join_all(asked).await {
        match answer {
            Ok((200, _, bytes)) => {
                match twirp::decode::<mediacore::ListRoomsResponse>(codec, &bytes) {
                    Ok(theirs) => rooms.extend(theirs.rooms),
                    Err(error) => tracing::warn!(node = %id, message = %error.message, "a node answered ListRooms with something this one could not read"),
                }
            }
            Ok((status, _, _)) => {
                tracing::warn!(node = %id, status, "a node refused ListRooms; its rooms are missing from this answer")
            }
            Err(error) => {
                tracing::warn!(node = %id, %error, "a node did not answer ListRooms; its rooms are missing from this answer")
            }
        }
    }

    // One name can only be in one place -- a room belongs to the node holding
    // it -- but a node whose view is a beat stale can publish a name another
    // has taken over, and the same room twice in a list is a backoffice showing
    // a meeting twice.
    rooms.sort_by(|left, right| left.name.cmp(&right.name));
    rooms.dedup_by(|left, right| left.name == right.name);

    Some(encode(codec, mediacore::ListRoomsResponse { rooms }).unwrap_or_else(|error| error.into_response()))
}

/// This node's own answer to `ListRooms`, decoded back out of the response it
/// would have sent. Going through `serve` rather than around it keeps the
/// filtering and the grant check in exactly one place.
fn local_rooms(state: &AppState, headers: &HeaderMap, body: &Bytes) -> Option<Vec<mediacore::Room>> {
    let codec = Codec::from_headers(headers).ok()?;
    let claims = mediacore_auth::verify(twirp::bearer(headers).ok()?, &state.config.keys).ok()?;
    let request = twirp::decode::<mediacore::ListRoomsRequest>(codec, body).ok()?;

    list_rooms(state, &claims, request).ok().map(|answer| answer.rooms)
}
