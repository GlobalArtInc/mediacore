//! Stopping this node without losing what was happening on it.
//!
//! A deploy is the operation that hits this path, and it hits it every time.
//! What these cover is the order: the node stops being sent arrivals before it
//! disconnects anybody, everybody is told why, and the meetings they were at
//! end as meetings somebody watched end rather than being found unfinished
//! afterwards by a reaper.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::mediacore::{self, signal_response};
use mediacore_server::{http, state::AppState};
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "standup";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The server, and the handle on it that a stop needs.
///
/// `spawn_server` elsewhere hands back only an address, which is all a caller
/// needs; a drain is the one thing that has to reach inside, because the whole
/// point is what the process does to itself.
async fn spawn() -> (SocketAddr, AppState) {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();

    let state = AppState::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(state.clone());

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    (addr, state)
}

/// What `main`'s drain does, without the signal: this is the sequence under
/// test, and a test that sent a real SIGTERM would be stopping the test runner.
fn drain(state: &AppState) {
    assert!(state.begin_draining(), "this is the first drain");

    for room in state.registry.rooms() {
        state
            .registry
            .close(&room.name, mediacore::DisconnectReason::ServerShutdown);
    }
}

async fn join(addr: SocketAddr, identity: &str, room: &str) -> Client {
    let url = format!(
        "ws://{addr}/rtc?access_token={}&protocol=17&sdk=test",
        identity_token(identity, room)
    );
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

fn identity_token(identity: &str, room: &str) -> String {
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
            video: VideoGrant {
                room_join: Some(true),
                room: room.to_owned(),
                can_publish: Some(true),
                can_subscribe: Some(true),
                ..VideoGrant::default()
            },
            ..Claims::default()
        },
    )
    .unwrap()
}

/// The first thing a socket is handed after the drain begins.
async fn next_leave(socket: &mut Client) -> mediacore::LeaveRequest {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .expect("no leave arrived before the deadline")
            .expect("socket closed without a leave")
            .expect("socket error");

        if let Message::Binary(data) = message {
            let response = mediacore::SignalResponse::decode(data.as_ref()).unwrap();
            if let Some(signal_response::Message::Leave(leave)) = response.message {
                return leave;
            }
        }
    }
}

async fn status(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.unwrap();
    let response = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a response with a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .expect("a status line")
        .parse()
        .expect("a numeric status");

    (code, body.to_owned())
}

/// A load balancer reads a status code and nothing else, so that is what says
/// whether this node should be sent anybody.
#[tokio::test]
async fn readiness_turns_away_before_anybody_is_disconnected() {
    let (addr, state) = spawn().await;

    let (code, body) = status(addr, "/readyz").await;
    assert_eq!(code, 200);
    assert_eq!(body.trim(), "ready");

    drain(&state);

    let (code, body) = status(addr, "/readyz").await;
    assert_eq!(code, 503, "a draining node is still being sent arrivals");
    assert_eq!(body.trim(), "draining");
}

/// Draining is not being unhealthy. The node is finishing what it holds, and a
/// health check that went red would have the orchestrator kill it mid-flush.
#[tokio::test]
async fn a_draining_node_is_still_healthy() {
    let (addr, state) = spawn().await;

    let (before, before_body) = status(addr, "/healthz").await;
    drain(&state);
    let (after, after_body) = status(addr, "/healthz").await;

    assert_eq!(before, 200);
    assert_eq!(after, 200, "a draining node reported itself unhealthy");
    assert!(
        before_body.starts_with("rooms=") && after_body.starts_with("rooms="),
        "the health line changed shape; a compose healthcheck and three test files parse it"
    );
}

/// Everybody is told, and told something their client can act on: `Reconnect`
/// sends it back through the load balancer, which by now has been told not to
/// send it here.
#[tokio::test]
async fn everybody_in_a_room_is_told_the_server_is_stopping() {
    let (addr, state) = spawn().await;
    let mut alice = join(addr, "alice", ROOM).await;

    drain(&state);

    let leave = next_leave(&mut alice).await;
    assert_eq!(
        leave.reason,
        mediacore::DisconnectReason::ServerShutdown as i32,
        "somebody was cut off without being told the server was stopping"
    );
}

/// The arrival that was already in flight when the drain began. Refused before
/// the join, because a node in the middle of closing a meeting must not open
/// one.
#[tokio::test]
async fn an_arrival_during_the_drain_is_turned_away_and_told_to_come_back() {
    let (addr, state) = spawn().await;
    drain(&state);

    let url = format!(
        "ws://{addr}/rtc?access_token={}&protocol=17&sdk=test",
        identity_token("bob", ROOM)
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let leave = next_leave(&mut socket).await;

    assert_eq!(
        leave.reason,
        mediacore::DisconnectReason::ServerShutdown as i32
    );
    assert!(
        leave.can_reconnect,
        "an arrival turned away by a node that is stopping was told never to come back"
    );
    assert_eq!(
        state.registry.rooms().len(),
        0,
        "the drain opened a room for somebody it was refusing"
    );
}

/// The point of the whole thing: the meeting ends as one somebody watched end,
/// at the time it really stopped, rather than being found unfinished later and
/// recorded as a restart with an end time nobody observed.
#[tokio::test]
async fn the_meeting_ends_as_one_somebody_watched_end() {
    let (addr, state) = spawn().await;
    let _alice = join(addr, "alice", ROOM).await;

    let live = state
        .registry
        .sessions()
        .live(ROOM)
        .expect("a meeting is running");

    drain(&state);

    assert!(
        state.registry.sessions().live(ROOM).is_none(),
        "the meeting is still open after the node let go of its rooms"
    );

    let page = state.registry.sessions().of_room(ROOM, 0, 10).unwrap();
    let record = page
        .sessions
        .iter()
        .find(|session| session.id == live.id)
        .expect("the meeting is in the record");

    assert_eq!(
        record.ended_by.map(|ending| ending.as_str()),
        Some("closed"),
        "the meeting was not recorded as one that was closed"
    );
    assert!(record.ended_at_ms.is_some());
}

/// A second signal must not start a second walk: it would close meetings the
/// first walk had already ended.
#[tokio::test]
async fn a_second_stop_signal_changes_nothing() {
    let (_addr, state) = spawn().await;

    assert!(state.begin_draining());
    assert!(
        !state.begin_draining(),
        "a second signal began a second drain"
    );
    assert!(state.draining());
}

/// The half the first version of this got wrong. Telling somebody the server
/// is stopping is only half an answer: the client also reads whether it may
/// come back, and `Disconnect` with `can_reconnect: false` means never. A
/// rolling deploy would empty every meeting in the cluster permanently.
#[tokio::test]
async fn nobody_is_told_never_to_come_back() {
    let (addr, state) = spawn().await;
    let mut alice = join(addr, "alice", ROOM).await;

    drain(&state);

    let leave = next_leave(&mut alice).await;
    assert!(
        leave.can_reconnect,
        "a rolling deploy told somebody in a meeting never to come back"
    );
    assert_eq!(
        leave.action,
        mediacore::leave_request::Action::Reconnect as i32,
        "the client reads `action`, and only RECONNECT sends it back through the load balancer"
    );
}
