//! Reading back what happened in a room, after it happened.
//!
//! Three endpoints, plain JSON over `GET`, and deliberately not part of
//! `mediacore.RoomService`. LiveKit has no session record, so there is no
//! method here that an SDK generated from its `.proto` could call and nothing
//! to be compatible with; inventing a Twirp method for it would mean a POST
//! with an empty body to read a list, which is a worse API than the one every
//! backoffice already knows how to call. The shapes are the ones MCN's own
//! conference service answers to its backoffice, which is why they are these
//! shapes and not others.
//!
//! What is shared with the room service is the way a failure looks. One server
//! answering two different error bodies would have a caller writing two error
//! paths, so a refusal here is the same `{"code", "msg"}` object with the same
//! status mapping as a Twirp error, produced by the same type.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mediacore_auth::Claims;
use mediacore_session::{DEFAULT_PAGE, MAX_PAGE, Session};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::twirp::{self, Error};

/// Every meeting a room has held.
pub const ROOM_SESSIONS_PATH: &str = "/api/rooms/{room}/sessions";

/// The meeting running in a room right now, if one is.
pub const ROOM_SESSION_PATH: &str = "/api/rooms/{room}/session";

/// One meeting by id, whatever room it was in.
pub const SESSION_PATH: &str = "/api/sessions/{id}";

/// What an endpoint answers with, which is what the page has to describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// `{"sessions": [...], "total": n}`, newest first, one page at a time.
    List,
    /// `{"session": {...}}` or `{"session": null}`.
    Perhaps,
    /// `{"session": {...}}`, or a `not_found`.
    One,
}

/// One endpoint, as this server serves it.
///
/// The same trick as `rooms::METHODS`: the router and the page at `/swagger`
/// read one table, so an endpoint cannot be documented one way and answer
/// another. The router takes only the paths from here -- axum wants a handler
/// per route and the three have different shapes -- so a test walks this table
/// and calls every path in it, which is what closes the gap.
#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    pub path: &'static str,
    pub operation: &'static str,
    /// The `{name}` in the path, and what it holds.
    pub parameter: (&'static str, &'static str),
    pub summary: &'static str,
    pub description: &'static str,
    pub answer: Answer,
}

/// The three of them, in the order somebody reads them.
pub const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        path: ROOM_SESSIONS_PATH,
        operation: "ListRoomSessions",
        parameter: ("room", "The room's name, as a join token spells it."),
        summary: "Every meeting a room has held.",
        description: "Newest first, one page at a time: `limit` (100 by default, 1000 at most) and \
             `offset`, with `total` on the answer so a caller knows what it has not seen. Plural \
             on purpose: a room created over the room service outlives its meetings and holds one \
             after another, so the history of a room is a list -- and a room a service has been \
             meeting in for two years has thousands, which is why the answer is a page and not \
             the history. A room that only ever existed because somebody walked into it is gone, \
             and its meetings are still here: this reads the record, not the registry, so a room \
             that no longer exists still answers.",
        answer: Answer::List,
    },
    Endpoint {
        path: ROOM_SESSION_PATH,
        operation: "GetRoomSession",
        parameter: ("room", "The room's name, as a join token spells it."),
        summary: "The meeting running in a room now, or nothing.",
        description: "`session` is null when no meeting is running, which is an answer and not a failure, \
             so this is 200 either way and a poller has one case to handle. A meeting is still \
             running while its grace period lasts: everybody may have left, and anybody who \
             comes back before it expires rejoins the meeting they were in rather than starting \
             a new one.",
        answer: Answer::Perhaps,
    },
    Endpoint {
        path: SESSION_PATH,
        operation: "GetSession",
        parameter: (
            "id",
            "The session id, as `SE_` and twelve hex digits. It appears in the server log beside \
             everything that happened in that meeting.",
        ),
        summary: "One meeting, whatever room it was in.",
        description: "The id is what a recording, a transcript or a billing line hangs off later, because \
             those belong to a meeting and not to a room. A meeting the caller's token may not \
             read is answered as one that does not exist: telling a token scoped to one room \
             that a given id is real, only forbidden, turns this into a way of enumerating \
             everyone else's meetings.",
        answer: Answer::One,
    },
];

/// How much of a room's history one call asks for.
///
/// Both optional, because the caller that wants the last meeting and the
/// caller that wants the year are the same endpoint. `limit` is capped rather
/// than refused when it is too large: a backoffice asking for everything gets
/// as much as this server will build in one answer, plus the `total` that
/// tells it there is more.
#[derive(Debug, Default, Deserialize)]
pub struct Window {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// One page of a room's meetings, newest first.
pub async fn of_room(
    State(state): State<AppState>,
    Path(room): Path<String>,
    Query(window): Query<Window>,
    headers: HeaderMap,
) -> Response {
    answer("ListRoomSessions", || {
        reading_room(&state, &headers, &room)?;

        let page = state.registry.sessions().of_room(
            &room,
            window.offset.unwrap_or(0),
            window.limit.unwrap_or(DEFAULT_PAGE).min(MAX_PAGE),
        );

        Ok(json(&Sessions {
            sessions: page.sessions,
            total: page.total,
        }))
    })
}

/// The meeting running in a room now, if one is.
pub async fn current(
    State(state): State<AppState>,
    Path(room): Path<String>,
    headers: HeaderMap,
) -> Response {
    answer("GetRoomSession", || {
        reading_room(&state, &headers, &room)?;

        Ok(json(&Perhaps {
            session: state.registry.sessions().live(&room),
        }))
    })
}

/// One meeting by id.
///
/// The lookup happens before the authorization check, because the record is
/// what says which room the meeting was in and there is nothing to scope a
/// `roomAdmin` token against until it has been read. Nothing about it reaches
/// the caller until they have been allowed to see it.
pub async fn one(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    answer("GetSession", || {
        let claims = authenticate(&state, &headers)?;
        let session = state
            .registry
            .sessions()
            .get(&id)
            .filter(|session| reads(&claims, &session.room))
            .ok_or_else(|| Error::not_found(format!("there is no session `{id}`")))?;

        Ok(json(&Perhaps {
            session: Some(session),
        }))
    })
}

#[derive(Serialize)]
struct Sessions {
    sessions: Vec<Session>,
    /// How many meetings the room has held, of which this page is a part. A
    /// caller handed a hundred meetings cannot otherwise tell whether that is
    /// the whole history or the first hundredth of it.
    total: usize,
}

#[derive(Serialize)]
struct Perhaps {
    session: Option<Session>,
}

/// Runs a read and turns a refusal into the same error body the room service
/// answers with, saying in the log which read it was.
fn answer(operation: &'static str, read: impl FnOnce() -> Result<Response, Error>) -> Response {
    match read() {
        Ok(response) => response,
        Err(error) => {
            tracing::info!(
                %operation,
                code = error.code.as_str(),
                message = %error.message,
                "session read refused"
            );
            error.into_response()
        }
    }
}

fn json<T: Serialize>(body: &T) -> Response {
    match serde_json::to_vec(body) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        // A record we built ourselves out of strings and integers does not
        // fail to serialise. If one ever does, the caller gets the same error
        // body as every other failure rather than a half-written list.
        Err(error) => Error::internal(format!("the session record could not be encoded: {error}"))
            .into_response(),
    }
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Claims, Error> {
    mediacore_auth::verify(twirp::bearer(headers)?, &state.config.keys)
        .map_err(|error| Error::unauthenticated(error.to_string()))
}

/// The check for the two endpoints that name a room in their path.
///
/// A refusal here is `permission_denied` and not a `not_found`, unlike the one
/// on a session id: the caller supplied the room name, so being told they may
/// not read it tells them nothing they did not already know, and being told
/// "no such room" when the answer is "not your room" sends somebody debugging
/// a token into the room service instead.
fn reading_room(state: &AppState, headers: &HeaderMap, room: &str) -> Result<(), Error> {
    let claims = authenticate(state, headers)?;

    if reads(&claims, room) {
        return Ok(());
    }

    Err(Error::permission_denied(format!(
        "this token carries neither roomList nor roomAdmin over room `{room}`"
    )))
}

/// Who may read what happened in a room.
///
/// Two grants, and the split is the one `rooms.rs` already makes. `roomList`
/// is this server's unscoped read authority -- it sees every room and the
/// whole of every room record already, metadata included -- so it reads every
/// meeting; a service that watches the whole server holds one of those.
/// `roomAdmin` is the only grant here that is scoped by room, so it reads the
/// meetings of the room its token names and no others, which is what lets a
/// backoffice hand out a token for one meeting and show that meeting's history
/// without handing over the run of the server.
///
/// `roomCreate` is not on the list. It makes rooms and deletes them; it has
/// never been a way to look inside one, and `ListParticipants` refuses it for
/// the same reason.
fn reads(claims: &Claims, room: &str) -> bool {
    if claims.video.room_list.unwrap_or(false) {
        return true;
    }

    claims.video.room_admin.unwrap_or(false) && claims.video.room == room
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
    fn an_admin_token_reads_the_history_of_its_own_room_only() {
        let admin = claims(VideoGrant {
            room_admin: Some(true),
            room: "standup".to_owned(),
            ..VideoGrant::default()
        });

        assert!(reads(&admin, "standup"));
        assert!(
            !reads(&admin, "payroll"),
            "a token minted for one meeting read another meeting's attendance list"
        );
    }

    #[test]
    fn the_unscoped_read_grant_reads_every_room() {
        let lister = claims(VideoGrant {
            room_list: Some(true),
            ..VideoGrant::default()
        });

        assert!(reads(&lister, "standup"));
        assert!(
            reads(&lister, "payroll"),
            "roomList already returns every room and the whole of every room record; refusing it \
             the meetings would be a lock on a door with no wall"
        );
    }

    #[test]
    fn making_rooms_and_joining_them_are_not_ways_of_reading_them() {
        let creator = claims(VideoGrant {
            room_create: Some(true),
            room: "standup".to_owned(),
            ..VideoGrant::default()
        });
        let joiner = claims(VideoGrant {
            room_join: Some(true),
            room: "standup".to_owned(),
            ..VideoGrant::default()
        });

        assert!(
            !reads(&creator, "standup"),
            "roomCreate builds rooms and deletes them; it has never been a way to look inside one"
        );
        assert!(
            !reads(&joiner, "standup"),
            "roomJoin is the grant everybody in the meeting holds, and the attendance list is not \
             theirs to read"
        );
    }

    #[test]
    fn every_endpoint_on_the_page_is_one_this_module_serves() {
        for endpoint in ENDPOINTS {
            assert!(
                [ROOM_SESSIONS_PATH, ROOM_SESSION_PATH, SESSION_PATH].contains(&endpoint.path),
                "`{}` is documented at a path this module does not serve",
                endpoint.path
            );
            assert!(
                endpoint
                    .path
                    .contains(&format!("{{{}}}", endpoint.parameter.0)),
                "`{}` documents a `{}` parameter that its path does not carry",
                endpoint.operation,
                endpoint.parameter.0
            );
        }

        assert_eq!(
            ENDPOINTS.len(),
            3,
            "an endpoint was added or removed and the page's table was not told"
        );
    }
}
