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

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mediacore_auth::Claims;
use mediacore_session::{DEFAULT_PAGE, MAX_PAGE, Recording, Session, StoreError, Work};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::twirp::{self, Error};

/// Every meeting a room has held.
pub const ROOM_SESSIONS_PATH: &str = "/api/rooms/{room}/sessions";

/// The meeting running in a room right now, if one is.
pub const ROOM_SESSION_PATH: &str = "/api/rooms/{room}/session";

/// One meeting by id, whatever room it was in.
pub const SESSION_PATH: &str = "/api/sessions/{id}";

/// Everything recorded during one meeting.
pub const SESSION_RECORDINGS_PATH: &str = "/api/sessions/{id}/recordings";

/// What an endpoint answers with, which is what the page has to describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// `{"sessions": [...], "total": n}`, newest first, one page at a time.
    List,
    /// `{"session": {...}}` or `{"session": null}`.
    Perhaps,
    /// `{"session": {...}}`, or a `not_found`.
    One,
    /// `{"recordings": [...]}`, each with the two pieces of work behind it.
    Recordings,
}

/// One endpoint, as this server serves it.
///
/// The same trick as `rooms::METHODS`: the router and the page at `/swagger`
/// read one table, so an endpoint cannot be documented one way and answer
/// another. The router takes only the paths from here -- axum wants a handler
/// per route and they have different shapes -- so a test walks this table
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

/// All four, in the order somebody reads them.
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
    Endpoint {
        path: SESSION_RECORDINGS_PATH,
        operation: "ListSessionRecordings",
        parameter: (
            "id",
            "The session id, as `SE_` and twelve hex digits. The same id a \
             `recording_ready` event carries in its `session` field.",
        ),
        summary: "The audio recorded during one meeting, and where it is.",
        description: "One entry per stretch of one participant's audio: where the object is, how long it \
             is, and where it sits inside the meeting. **This is what a consumer that missed a \
             `recording_ready` event asks instead** -- an event notifies, it is never the only \
             copy of a fact, so nothing that was announced is unavailable to somebody who was not \
             listening at the time. `objectKey` is null until the upload finishes, and `work` \
             says why: two rows, `upload` and `announce`, each with its state, how many times it \
             has been tried and what went wrong the last time. A meeting the caller's token may \
             not read is answered as one that does not exist, exactly as the meeting itself is.",
        answer: Answer::Recordings,
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
    if let Err(error) = reading_room(&state, &headers, &room) {
        return refused("ListRoomSessions", error);
    }

    let store = Arc::clone(state.registry.sessions());
    let offset = window.offset.unwrap_or(0);
    let limit = window.limit.unwrap_or(DEFAULT_PAGE).min(MAX_PAGE);

    match asked(move || store.of_room(&room, offset, limit)).await {
        Ok(page) => json(&Sessions {
            sessions: page.sessions,
            total: page.total,
        }),
        Err(error) => refused("ListRoomSessions", error),
    }
}

/// The meeting running in a room now, if one is.
///
/// The only one of the four that asks nothing of the store: the meeting
/// happening now is in memory, on this node, which is why a poller watching for
/// a meeting to end gets an answer even while the store is away.
pub async fn current(
    State(state): State<AppState>,
    Path(room): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = reading_room(&state, &headers, &room) {
        return refused("GetRoomSession", error);
    }

    json(&Perhaps {
        session: state.registry.sessions().live(&room),
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
    let claims = match authenticate(&state, &headers) {
        Ok(claims) => claims,
        Err(error) => return refused("GetSession", error),
    };

    let store = Arc::clone(state.registry.sessions());
    let wanted = id.clone();
    let found = asked(move || store.get(&wanted)).await;

    match found {
        Ok(session) => match session.filter(|session| reads(&claims, &session.room)) {
            Some(session) => json(&Perhaps {
                session: Some(session),
            }),
            None => refused(
                "GetSession",
                Error::not_found(format!("there is no session `{id}`")),
            ),
        },
        Err(error) => refused("GetSession", error),
    }
}

/// Everything recorded during one meeting.
///
/// Not paged, unlike a room's history: the number of recordings a meeting has
/// is the number of people who spoke at it, and a meeting with a thousand of
/// those is not a meeting.
pub async fn recordings(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let claims = match authenticate(&state, &headers) {
        Ok(claims) => claims,
        Err(error) => return refused("ListSessionRecordings", error),
    };

    let store = Arc::clone(state.registry.sessions());
    let wanted = id.clone();
    // One trip to the blocking pool for all three reads, and two queries for
    // the recordings and their work rather than one per recording: a ten-person
    // meeting has at least ten recordings, and a backoffice opening it should
    // not cost twenty-one round trips.
    let found = asked(move || {
        let Some(session) = store.get(&wanted)? else {
            return Ok(None);
        };

        // A store that cannot keep recordings has none, and "none" is the true
        // answer to what was recorded at this meeting -- so it is an empty list
        // and not a 501. The refusal matters where somebody tries to *queue*
        // work on a directory of files; it has no business surfacing on a read
        // that a backoffice makes about every meeting it shows.
        Ok(Some((
            session,
            allowing_none(store.recordings(&wanted))?,
            allowing_none(store.meeting_work(&wanted))?,
        )))
    })
    .await;

    // Word for word what `one` answers for an id that names nothing, because a
    // token scoped to another room being told that some id is real but
    // forbidden is all it needs to walk the id space and learn who met where.
    let missing = || Error::not_found(format!("there is no session `{id}`"));

    match found {
        Ok(Some((session, recordings, work))) if reads(&claims, &session.room) => {
            json(&Recordings {
                recordings: recordings
                    .into_iter()
                    .map(|recording| Made {
                        work: work
                            .iter()
                            .filter(|piece| piece.recording == recording.id)
                            .cloned()
                            .collect(),
                        recording,
                    })
                    .collect(),
            })
        }
        Ok(_) => refused("ListSessionRecordings", missing()),
        Err(error) => refused("ListSessionRecordings", error),
    }
}

#[derive(Serialize)]
struct Recordings {
    recordings: Vec<Made>,
}

/// One recording as this endpoint answers with it: the row, and the two pieces
/// of work behind it flattened onto nothing.
///
/// The work is on the recording rather than in a list of its own because the
/// question it answers is always about one file -- "this one has no object key,
/// why" -- and pairing them up in the caller is work every caller would do the
/// same way.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Made {
    #[serde(flatten)]
    recording: Recording,
    work: Vec<Work>,
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

/// Turns "this store does not keep recordings" into "there are none".
fn allowing_none<T: Default>(outcome: Result<T, StoreError>) -> Result<T, StoreError> {
    match outcome {
        Err(StoreError::Unsupported(_)) => Ok(T::default()),
        other => other,
    }
}

/// Runs a read against the store on a thread that is allowed to wait.
///
/// The store waits on a disk or on a database. A handler that did that on a
/// runtime thread would hold up every other request this server is serving --
/// and against Postgres it would not merely be slow: the blocking client drives
/// a runtime of its own, and a runtime cannot be started inside a runtime.
async fn asked<T: Send + 'static>(
    read: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, Error> {
    match tokio::task::spawn_blocking(read).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(refusal(error)),
        Err(error) => Err(Error::internal(format!(
            "the session read did not finish: {error}"
        ))),
    }
}

/// What a store's refusal looks like to a caller.
///
/// A store that is away is `unavailable` and not `internal`, because the two
/// mean different things to whoever wrote the client: this one is worth trying
/// again in a minute, and the server is otherwise working -- the meeting the
/// caller is asking about is very likely still happening.
fn refusal(error: StoreError) -> Error {
    match error {
        StoreError::Unavailable(_) => Error::unavailable(error.to_string()),
        StoreError::Unsupported(_) => Error::unimplemented(error.to_string()),
        StoreError::Nonsense(_) => Error::internal(error.to_string()),
    }
}

/// Turns a refusal into the same error body the room service answers with,
/// saying in the log which read it was.
fn refused(operation: &'static str, error: Error) -> Response {
    tracing::info!(
        %operation,
        code = error.code.as_str(),
        message = %error.message,
        "session read refused"
    );
    error.into_response()
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
                [
                    ROOM_SESSIONS_PATH,
                    ROOM_SESSION_PATH,
                    SESSION_PATH,
                    SESSION_RECORDINGS_PATH
                ]
                .contains(&endpoint.path),
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
            4,
            "an endpoint was added or removed and the page's table was not told"
        );
    }
}
