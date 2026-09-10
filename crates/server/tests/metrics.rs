//! What a collector and a healthcheck get from a server that is really running.
//!
//! The unit tests beside the module render a snapshot somebody made up; these
//! are the two things only a whole server can show: that the health line a
//! compose healthcheck curls has not changed shape, and that the two endpoints
//! answering it are one reading of one node rather than two readings taken a
//! moment apart.
//!
//! The node under test here is the one most deployments run -- no recorder, no
//! event delivery, one replica -- which is also the node with the most `None`s
//! in the gathering.

use std::collections::HashMap;
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

/// Named like a meeting somebody would actually create, because half of what
/// these tests check is that this string never reaches the scrape.
const ROOM: &str = "board-pay-review";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What a freshly started node answers, copied from what it answered before
/// any of this was written.
///
/// Two fields cannot be predicted -- the store is a temporary directory and the
/// ice port is whatever the kernel handed this run -- so they are taken out and
/// everything else is compared as one string. Held here as well as in the unit
/// test on purpose: this one came off a socket, and it is what a compose
/// healthcheck and three other test files are really reading.
const HEALTH_LINE: &str = "rooms=0 participants=0 publishers=0 rtp_packets=0 rtp_bytes=0 \
                           tracks=0 subscriptions=0 forwarded_packets=0 forwarded_bytes=0 \
                           bwe=off allocated_subscribers=0 session_store=<store> sessions=0 \
                           live_sessions=0 resident_sessions=0 owed_sessions=0 \
                           unwritten_sessions=0 events=0 owed_events=0 undelivered_events=0 \
                           unwritten_events=0 recording=off unwritten_recordings=0 \
                           dropped_audio_packets=0 abandoned_recordings=0 \
                           stuck_recording_work=0 ice=<ice>";

/// The server, and the handle a drain needs to reach inside.
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

struct Answer {
    status: u16,
    headers: String,
    body: String,
}

async fn get(addr: SocketAddr, path: &str) -> Answer {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.unwrap();
    let response = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a response with a body");

    Answer {
        status: head
            .split_whitespace()
            .nth(1)
            .expect("a status line")
            .parse()
            .expect("a numeric status"),
        headers: head.to_lowercase(),
        body: body.to_owned(),
    }
}

/// The health line, split the way its readers split it.
fn fields(line: &str) -> HashMap<&str, &str> {
    line.split(' ')
        .map(|field| {
            field
                .split_once('=')
                .unwrap_or_else(|| panic!("`{field}` is not `name=value`"))
        })
        .collect()
}

/// The samples out of an exposition, by name. The strict reading of the whole
/// document is in `metrics_tests.rs`; what is wanted here is the numbers.
fn samples(exposition: &str) -> HashMap<&str, u64> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(name, value)| Some((name.split('{').next()?, value.parse().ok()?)))
        .collect()
}

/// A field somebody added to the line and to nothing else is a number no alert
/// will ever see; a field taken away is a healthcheck and three test files that
/// stop parsing. Either way this fails first.
#[tokio::test]
async fn the_health_line_is_the_line_it_has_always_been() {
    let (addr, _state) = spawn().await;
    let health = get(addr, "/healthz").await;

    assert_eq!(health.status, 200);
    assert_eq!(predictable(&health.body), HEALTH_LINE);
    assert!(!health.body.ends_with('\n'), "the line grew a newline");
    assert!(health.body.contains(" ice=127.0.0.1:"));
    assert!(fields(&health.body)["session_store"].ends_with(":up"));
}

/// The line with the two fields nothing can predict written as themselves.
fn predictable(line: &str) -> String {
    line.split(' ')
        .map(|field| match field.split_once('=') {
            Some(("session_store", _)) => "session_store=<store>",
            Some(("ice", _)) => "ice=<ice>",
            _ => field,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A scrape that is not told which format it is reading is parsed by
/// guesswork, and the version is part of the type rather than a detail of it.
#[tokio::test]
async fn the_scrape_says_what_it_is() {
    let (addr, _state) = spawn().await;
    let metrics = get(addr, "/metrics").await;

    assert_eq!(metrics.status, 200);
    assert!(
        metrics
            .headers
            .contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
        "{}",
        metrics.headers
    );
}

/// A collector reaches this with whatever it was configured with, which is
/// nothing: there is no meeting token a `ServiceMonitor` could carry. What
/// makes that safe is that the answer is counts and this node's own name, so
/// the scrape must not need a grant.
#[tokio::test]
async fn a_scrape_needs_no_token() {
    let (addr, _state) = spawn().await;

    assert_eq!(get(addr, "/metrics").await.status, 200);
    assert_eq!(get(addr, "/healthz").await.status, 200);
    assert_eq!(
        get(addr, "/api/cluster").await.status,
        401,
        "the endpoint that does name rooms stopped asking for a token"
    );
}

/// The two endpoints on a node with no recorder, no delivery and no company:
/// every quantity gathered through a `None`, and both renderings of it agreeing.
#[tokio::test]
async fn both_endpoints_answer_one_reading() {
    let (addr, _state) = spawn().await;
    let line = get(addr, "/healthz").await.body;
    let exposition = get(addr, "/metrics").await.body;

    let line = fields(&line);
    let samples = samples(&exposition);

    assert_eq!(line["rooms"], samples["mediacore_rooms"].to_string());
    assert_eq!(
        line["stuck_recording_work"],
        samples["mediacore_stuck_recording_work"].to_string()
    );
    assert_eq!(
        line["session_store"].ends_with(":up"),
        samples["mediacore_session_store_up"] == 1
    );
    assert_eq!(line["recording"], "off");
    assert_eq!(samples["mediacore_recorder_enabled"], 0);
    assert_eq!(samples["mediacore_recorder_running"], 0);
    assert_eq!(samples["mediacore_owed_events"], 0);
    assert_eq!(samples["mediacore_undelivered_events_total"], 0);
    assert_eq!(samples["mediacore_cluster_joined"], 0);
    assert_eq!(samples["mediacore_cluster_nodes"], 0);
    assert_eq!(samples["mediacore_draining"], 0);
}

/// The rule the module was written around. A room name in a label is
/// unbounded cardinality and, worse, the names of the meetings written into a
/// database that keeps them for ever -- so the gauges move and the name of the
/// room does not appear.
#[tokio::test]
async fn a_room_moves_the_gauges_and_never_reaches_a_label() {
    let (addr, _state) = spawn().await;
    let _alice = join(addr, "alice", ROOM).await;

    let exposition = get(addr, "/metrics").await.body;
    let samples = samples(&exposition);

    assert_eq!(samples["mediacore_rooms"], 1);
    assert_eq!(samples["mediacore_participants"], 1);
    assert_eq!(samples["mediacore_live_sessions"], 1);
    assert!(
        !exposition.contains(ROOM),
        "the name of a room is in the exposition"
    );
    assert!(
        !exposition.contains("alice"),
        "the name of a participant is in the exposition"
    );
}

/// A node on its way out still answers both, and says so in the one of them
/// that has somewhere to say it. `/healthz` stays exactly as it was, because a
/// health check that went red during a drain would have the orchestrator kill
/// the process mid-flush.
#[tokio::test]
async fn a_draining_node_answers_both() {
    let (addr, state) = spawn().await;
    assert!(state.begin_draining());

    let health = get(addr, "/healthz").await;
    let metrics = get(addr, "/metrics").await;

    assert_eq!(health.status, 200);
    assert!(health.body.starts_with("rooms="));
    assert!(!health.body.contains("draining"));

    assert_eq!(metrics.status, 200);
    assert_eq!(samples(&metrics.body)["mediacore_draining"], 1);
}

/// This node's own name, version and region, and the only labels in the
/// document.
#[tokio::test]
async fn the_only_labels_name_this_node() {
    let (addr, state) = spawn().await;
    let exposition = get(addr, "/metrics").await.body;

    let info = exposition
        .lines()
        .find(|line| line.starts_with("mediacore_build_info"))
        .expect("the build info");

    assert!(info.contains(&format!(r#"node="{}""#, state.cluster.id())));
    assert!(info.contains(&format!(r#"version="{}""#, mediacore_proto::SERVER_VERSION)));
    assert!(info.ends_with(" 1"));

    for line in exposition.lines().filter(|line| !line.starts_with('#')) {
        assert!(
            line.starts_with("mediacore_build_info") || !line.contains('{'),
            "`{line}` carries labels"
        );
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
