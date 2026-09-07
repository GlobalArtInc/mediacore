//! What happened in a room, read back over a socket.
//!
//! The room tests cover when a meeting starts and stops. These cover the three
//! endpoints a backoffice actually calls: the shapes that come back, and the
//! line between a token that may read a meeting and one that may not.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::mediacore::{self, signal_response};
use mediacore_server::sessions::{self, Answer};
use mediacore_server::{http, state::AppState};
use prost::Message as _;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "standup";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A server whose meetings end the moment the room empties, and which notices
/// within a tick.
///
/// `end_grace: 0` is not the default -- it is the escape hatch for a
/// deployment that would rather a meeting end when the room empties than be
/// held open for whoever comes back. A test that used the default would spend
/// ninety seconds waiting for one.
async fn spawn_server() -> SocketAddr {
    spawn_server_with(|config| {
        config.room.end_grace = 0;
        config.room.sweep_interval_ms = 20;
    })
    .await
}

async fn spawn_server_with(adjust: impl FnOnce(&mut Config)) -> SocketAddr {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    // A directory of this server's own for session records: a test must
    // never write into the tree, and two servers sharing a store would
    // read each other's meetings back.
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();
    adjust(&mut config);

    let state = AppState::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(state);

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    addr
}

fn token(video: VideoGrant) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    mediacore_auth::issue(
        API_SECRET,
        &Claims {
            exp,
            iss: API_KEY.to_owned(),
            video,
            ..Claims::default()
        },
    )
    .unwrap()
}

fn lister() -> String {
    token(VideoGrant {
        room_list: Some(true),
        ..VideoGrant::default()
    })
}

fn admin(room: &str) -> String {
    token(VideoGrant {
        room_admin: Some(true),
        room: room.to_owned(),
        ..VideoGrant::default()
    })
}

fn creator() -> String {
    token(VideoGrant {
        room_create: Some(true),
        room: ROOM.to_owned(),
        ..VideoGrant::default()
    })
}

fn joiner(identity: &str, room: &str) -> String {
    joiner_carrying(identity, room, &[])
}

/// A join token with the platform above's own data on it.
///
/// Attributes are the only thing on a token this server does not read, which is
/// exactly what makes them the place an embedding platform puts its own
/// identifiers.
fn joiner_carrying(identity: &str, room: &str, attributes: &[(&str, &str)]) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    mediacore_auth::issue(
        API_SECRET,
        &Claims {
            exp,
            iss: API_KEY.to_owned(),
            sub: identity.to_owned(),
            name: identity.to_owned(),
            attributes: attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            video: VideoGrant {
                room_join: Some(true),
                room: room.to_owned(),
                ..VideoGrant::default()
            },
            ..Claims::default()
        },
    )
    .unwrap()
}

struct Answered {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Answered {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "the answer is not json ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn code(&self) -> String {
        self.json()
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn session(&self) -> Value {
        self.json()["session"].clone()
    }

    fn sessions(&self) -> Vec<Value> {
        self.json()["sessions"]
            .as_array()
            .expect("the answer carries no `sessions` array")
            .clone()
    }
}

async fn get(addr: SocketAddr, path: &str, authorization: Option<&str>) -> Answered {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let mut head = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
        if let Some(authorization) = authorization {
            head.push_str(&format!("Authorization: Bearer {authorization}\r\n"));
        }
        head.push_str("\r\n");

        socket.write_all(head.as_bytes()).await.expect("request");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        raw
    };

    let raw = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("the server did not answer in time");

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("http response without a body");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();

    Answered {
        status: head
            .split_whitespace()
            .nth(1)
            .expect("http status line")
            .parse()
            .expect("numeric http status"),
        content_type: head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_ascii_lowercase())
            })
            .unwrap_or_default(),
        body: raw[split + 4..].to_vec(),
    }
}

async fn join(addr: SocketAddr, identity: &str, room: &str) -> Client {
    join_with(addr, &joiner(identity, room)).await
}

async fn join_with(addr: SocketAddr, token: &str) -> Client {
    let url = format!("ws://{addr}/rtc?access_token={token}&protocol=17&sdk=test");
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("the join response did not arrive in time")
            .expect("socket closed")
            .expect("socket error");

        if let Message::Binary(data) = message {
            let response = mediacore::SignalResponse::decode(data.as_ref()).unwrap();
            if let Some(signal_response::Message::Join(_)) = response.message {
                return socket;
            }
        }
    }
}

/// Waits for the sweep to notice something, rather than sleeping for a fixed
/// time and hoping. A test that sleeps long enough to be safe is a test that
/// is slow on every machine to be safe on the slowest one.
async fn until(mut ready: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while tokio::time::Instant::now() < deadline {
        if ready().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("the server never got there");
}

#[tokio::test]
async fn a_meeting_can_be_watched_while_it_runs_and_read_after_it_ends() {
    let addr = spawn_server().await;

    let running = get(addr, &room_path("session"), Some(&lister())).await;
    assert_eq!(running.status, 200);
    assert!(
        running.session().is_null(),
        "a room nobody has been in reported a meeting in progress"
    );
    assert!(
        get(addr, &room_path("sessions"), Some(&lister()))
            .await
            .sessions()
            .is_empty()
    );

    let alice = join(addr, "alice", ROOM).await;

    let running = get(addr, &room_path("session"), Some(&lister())).await;
    assert_eq!(running.status, 200);
    assert!(
        running.content_type.starts_with("application/json"),
        "the answer came back as `{}`",
        running.content_type
    );

    let session = running.session();
    let id = session["id"]
        .as_str()
        .expect("a running meeting with no id on it")
        .to_owned();
    assert!(id.starts_with("SE_"), "the session id is `{id}`");
    assert_eq!(session["room"], ROOM);
    assert!(
        session["endedAtMs"].is_null(),
        "a meeting that is happening reported an end time"
    );
    assert_eq!(
        session["participants"][0]["identity"], "alice",
        "the meeting in progress does not say who is at it"
    );
    assert!(session["participants"][0]["leftAtMs"].is_null());

    // Alice's browser goes away. The room goes with her -- nobody created it
    // -- and the record of what she did there does not.
    drop(alice);

    until(async || {
        get(addr, &room_path("session"), Some(&lister()))
            .await
            .session()
            .is_null()
    })
    .await;

    let held = get(addr, &room_path("sessions"), Some(&lister())).await;
    assert_eq!(held.status, 200);
    let sessions = held.sessions();
    assert_eq!(
        sessions.len(),
        1,
        "the room is gone and so is every trace that anybody met in it"
    );
    assert_eq!(sessions[0]["id"], id.as_str());
    assert!(
        sessions[0]["endedAtMs"].is_number(),
        "a meeting nobody is at has no end time"
    );
    assert_eq!(sessions[0]["endedBy"], "grace");
    assert!(
        sessions[0]["participants"][0]["leftAtMs"].is_number(),
        "alice's browser is closed and the record says she is still in the meeting"
    );

    let one = get(addr, &format!("/api/sessions/{id}"), Some(&lister())).await;
    assert_eq!(one.status, 200);
    assert_eq!(
        one.session()["id"],
        id.as_str(),
        "a meeting cannot be read by the id every other record will refer to it by"
    );
}

/// What replaced the account column: the platform above's own data reaching the
/// record over a real socket and coming back out of a real endpoint.
///
/// The unit tests prove the store keeps it. This proves the whole path -- the
/// token, the roster, the record, the JSON a backoffice parses -- because that
/// is the path an embedding platform actually uses, and every step of it had a
/// place where somebody could have decided to read the data or to drop it.
#[tokio::test]
async fn a_participants_own_attributes_come_back_on_the_attendance_list() {
    let addr = spawn_server().await;
    let alice = join_with(
        addr,
        &joiner_carrying(
            "alice",
            ROOM,
            &[("accountId", "AC_9271"), ("booking", "BK_44")],
        ),
    )
    .await;

    let session = get(addr, &room_path("session"), Some(&lister()))
        .await
        .session();
    let attendance = &session["participants"][0];

    assert_eq!(attendance["identity"], "alice");
    assert_eq!(
        attendance["attributes"]["accountId"], "AC_9271",
        "an identifier the platform above put on the token did not reach the record, so it has \
         nowhere to hang its own meaning off this meeting"
    );
    assert_eq!(
        attendance["attributes"]["booking"], "BK_44",
        "only one of the attributes came back, so this server is choosing between them -- and it \
         has no business knowing what any of them are"
    );
    assert!(
        attendance.get("accountId").is_none(),
        "the account column is gone and nothing may quietly answer with it again"
    );

    drop(alice);
}

/// Finding G, at the endpoint. A room a service has been meeting in for two
/// years has thousands of records, and the answer to "list them" cannot be all
/// of them in one response built out of thousands of files with the lock every
/// arrival on this server needs.
#[tokio::test]
async fn a_rooms_history_comes_back_a_page_at_a_time() {
    let addr = spawn_server().await;
    let held = 6;

    for round in 0..held {
        let person = join(addr, &format!("person-{round}"), ROOM).await;
        until(async || {
            !get(addr, &room_path("session"), Some(&lister()))
                .await
                .session()
                .is_null()
        })
        .await;
        drop(person);
        until(async || {
            get(addr, &room_path("session"), Some(&lister()))
                .await
                .session()
                .is_null()
        })
        .await;
    }

    let whole = get(addr, &room_path("sessions"), Some(&lister())).await;
    assert_eq!(
        whole.sessions().len(),
        held,
        "the room held {held} meetings"
    );
    assert_eq!(
        whole.json()["total"],
        held,
        "the answer does not say how many meetings there were, so a caller handed a full page \
         cannot tell whether it has the history or the first slice of it"
    );

    let page = get(
        addr,
        &format!("{}?limit=2&offset=1", room_path("sessions")),
        Some(&lister()),
    )
    .await;
    assert_eq!(page.status, 200);
    assert_eq!(
        page.sessions().len(),
        2,
        "the endpoint answered with the whole history however small a page was asked for"
    );
    assert_eq!(
        page.json()["total"],
        held,
        "a page has to carry the total, or paging cannot be stopped"
    );
    assert_eq!(
        page.sessions()[0]["id"],
        whole.sessions()[1]["id"],
        "`offset=1` did not skip the newest meeting; the page is not a window on the same list"
    );

    let past_the_end = get(
        addr,
        &format!("{}?offset=100", room_path("sessions")),
        Some(&lister()),
    )
    .await;
    assert!(
        past_the_end.sessions().is_empty(),
        "reading past the end of a room's history is an empty page and not a failure"
    );
    assert_eq!(past_the_end.json()["total"], held);
}

/// The room service is the other half of the answer, and it should disagree
/// with none of this: the room is gone from `ListRooms` and the meeting it
/// held is still there to be asked about.
#[tokio::test]
async fn a_room_that_no_longer_exists_still_answers_for_its_meetings() {
    let addr = spawn_server().await;
    let alice = join(addr, "alice", ROOM).await;
    drop(alice);

    until(async || {
        get(addr, &room_path("session"), Some(&lister()))
            .await
            .session()
            .is_null()
    })
    .await;

    let rooms = get(addr, "/api/rooms/standup/sessions", Some(&lister())).await;
    assert_eq!(rooms.sessions().len(), 1);
    assert_eq!(rooms.sessions()[0]["room"], ROOM);
}

#[tokio::test]
async fn a_room_that_never_met_answers_with_nothing_rather_than_a_failure() {
    let addr = spawn_server().await;

    let current = get(addr, "/api/rooms/never/session", Some(&lister())).await;
    assert_eq!(
        current.status, 200,
        "no meeting in progress is an answer, not a failure; a poller that has to tell a 404 \
         meaning `nothing yet` from a 404 meaning `wrong url` will get it wrong"
    );
    assert!(current.session().is_null());

    let held = get(addr, "/api/rooms/never/sessions", Some(&lister())).await;
    assert_eq!(held.status, 200);
    assert!(held.sessions().is_empty());

    let missing = get(addr, "/api/sessions/SE_000000000000", Some(&lister())).await;
    assert_eq!(
        missing.status, 404,
        "an id that names nothing is the one case that is a failure: the caller is holding an id \
         from somewhere and needs to know it is no good"
    );
    assert_eq!(missing.code(), "not_found");
}

#[tokio::test]
async fn reading_a_meeting_takes_a_grant_that_reads_rooms() {
    let addr = spawn_server().await;
    let _alice = join(addr, "alice", ROOM).await;

    let anonymous = get(addr, &room_path("sessions"), None).await;
    assert_eq!(anonymous.status, 401);
    assert_eq!(anonymous.code(), "unauthenticated");

    for (name, refused) in [
        ("roomJoin", joiner("alice", ROOM)),
        ("roomCreate", creator()),
    ] {
        let answer = get(addr, &room_path("sessions"), Some(&refused)).await;
        assert_eq!(
            answer.status, 403,
            "a `{name}` token read the attendance list of a meeting"
        );
        assert_eq!(answer.code(), "permission_denied");
    }

    assert_eq!(
        get(addr, &room_path("sessions"), Some(&lister()))
            .await
            .status,
        200,
        "roomList already returns every room and the whole of every room record; it has to read \
         the meetings too or a backoffice cannot use one token for both"
    );
    assert_eq!(
        get(addr, &room_path("sessions"), Some(&admin(ROOM)))
            .await
            .status,
        200,
        "roomAdmin is the only grant here that is scoped to a room, so it is the one a backoffice \
         hands out for a single meeting"
    );

    let elsewhere = get(addr, &room_path("sessions"), Some(&admin("payroll"))).await;
    assert_eq!(
        elsewhere.status, 403,
        "an admin token minted for one meeting read another meeting's attendance list"
    );
    assert_eq!(elsewhere.code(), "permission_denied");
    assert_eq!(
        get(addr, &room_path("session"), Some(&admin("payroll")))
            .await
            .status,
        403
    );
}

/// The one endpoint that names no room until it has been read, and therefore
/// the one that could be turned into a way of enumerating everybody else's
/// meetings.
#[tokio::test]
async fn a_meeting_a_token_may_not_read_is_a_meeting_that_does_not_exist() {
    let addr = spawn_server().await;
    let _alice = join(addr, "alice", ROOM).await;

    let id = get(addr, &room_path("session"), Some(&lister()))
        .await
        .session()["id"]
        .as_str()
        .expect("a meeting is running")
        .to_owned();

    let own = get(addr, &format!("/api/sessions/{id}"), Some(&admin(ROOM))).await;
    assert_eq!(
        own.status, 200,
        "an admin of the room the meeting happened in cannot read the meeting"
    );

    let stranger = get(
        addr,
        &format!("/api/sessions/{id}"),
        Some(&admin("payroll")),
    )
    .await;
    assert_eq!(
        stranger.status, 404,
        "a token scoped to another room was told this id is real but forbidden, which is all it \
         needs to walk the id space and learn who met where"
    );
    assert_eq!(stranger.code(), "not_found");

    let invented = get(
        addr,
        "/api/sessions/SE_000000000000",
        Some(&admin("payroll")),
    )
    .await;
    assert_eq!(stranger.status, invented.status);
    assert_eq!(
        String::from_utf8_lossy(&stranger.body),
        String::from_utf8_lossy(&invented.body).replace("SE_000000000000", &id),
        "a real meeting and an invented id answer differently, and the difference is the oracle: \
         the only thing that may vary between the two is the id the caller sent in"
    );
}

/// The recordings of a meeting are the meeting's, and the same token reads
/// both or neither.
///
/// The list is what a consumer that missed a `recording_ready` event asks
/// instead, so it has to be a read anybody who may read the meeting can make --
/// and a read nobody else can, including as a way of finding out that an id is
/// real.
#[tokio::test]
async fn a_meetings_recordings_are_read_by_whoever_may_read_the_meeting() {
    let addr = spawn_server().await;
    let _alice = join(addr, "alice", ROOM).await;

    let id = get(addr, &room_path("session"), Some(&lister()))
        .await
        .session()["id"]
        .as_str()
        .expect("a meeting is running")
        .to_owned();
    let path = format!("/api/sessions/{id}/recordings");

    let own = get(addr, &path, Some(&admin(ROOM))).await;
    assert_eq!(own.status, 200);
    assert_eq!(
        own.json()["recordings"],
        serde_json::json!([]),
        "this server keeps its meetings in a directory, so it cannot record and has nothing to \
         list -- and `nothing was recorded` is an answer rather than a failure, or a backoffice \
         showing every meeting would have to special-case a deployment that records nothing"
    );

    let stranger = get(addr, &path, Some(&admin("payroll"))).await;
    assert_eq!(
        stranger.status, 404,
        "a token scoped to another room was told this meeting is real, which is all it needs to \
         walk the id space"
    );
    assert_eq!(stranger.code(), "not_found");

    let anonymous = get(addr, &path, None).await;
    assert_eq!(anonymous.status, 401);
}

/// The page at `/swagger` lists these endpoints out of `sessions::ENDPOINTS`.
/// A row in that table that this server does not serve is a documented
/// endpoint that answers 404, which is worse than an undocumented one.
#[tokio::test]
async fn every_endpoint_the_page_lists_is_one_this_server_answers() {
    let addr = spawn_server().await;
    let _alice = join(addr, "alice", ROOM).await;

    let id = get(addr, &room_path("session"), Some(&lister()))
        .await
        .session()["id"]
        .as_str()
        .expect("a meeting is running")
        .to_owned();

    for endpoint in sessions::ENDPOINTS {
        let value = match endpoint.parameter.0 {
            "room" => ROOM,
            "id" => &id,
            other => panic!("`{other}` is a path parameter no test knows how to fill in"),
        };
        let path = endpoint
            .path
            .replace(&format!("{{{}}}", endpoint.parameter.0), value);

        let answer = get(addr, &path, Some(&lister())).await;
        assert_eq!(
            answer.status, 200,
            "the page documents `{path}` and this server answers {}",
            answer.status
        );

        match endpoint.answer {
            Answer::List => assert!(answer.json()["sessions"].is_array()),
            Answer::Perhaps | Answer::One => assert!(
                answer.json().get("session").is_some(),
                "`{path}` is documented as returning a `session` and returned {}",
                answer.json()
            ),
            // Empty here, because this server keeps its meetings in a
            // directory and a directory cannot be an upload queue. "Nothing was
            // recorded" is the answer to what a meeting recorded, and a
            // backoffice that shows every meeting must not have to special-case
            // a 501 for the ones held on a deployment that records nothing.
            Answer::Recordings => assert!(
                answer.json()["recordings"].is_array(),
                "`{path}` is documented as returning a list of recordings and returned {}",
                answer.json()
            ),
        }
    }
}

fn room_path(tail: &str) -> String {
    format!("/api/rooms/{ROOM}/{tail}")
}
