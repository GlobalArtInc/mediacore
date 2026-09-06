//! What a real server announces, and what it costs the people in the room.
//!
//! The events crate covers delivery against a socket in isolation. These cover
//! the two things only a whole server can show: that a room somebody actually
//! joined over a WebSocket produces the events a consumer is waiting for, and
//! that a webhook endpoint which accepts connections and never answers costs
//! that person nothing at all.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::mediacore::{self, signal_response};
use mediacore_server::{http, state::AppState};
use prost::Message as _;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "standup";

/// An endpoint that accepts the connection, reads the request and then says
/// nothing at all. Not a refused connection: a refusal comes back in
/// microseconds and would prove nothing about waiting.
const SILENT: u16 = 0;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Somebody else's HTTP endpoint, recording what it was posted.
struct Endpoint {
    url: String,
    status: Arc<AtomicU16>,
    bodies: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicUsize>,
}

impl Endpoint {
    async fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a socket");
        let addr: SocketAddr = listener.local_addr().unwrap();

        let endpoint = Self {
            url: format!("http://{addr}/hook"),
            status: Arc::new(AtomicU16::new(status)),
            bodies: Arc::new(Mutex::new(Vec::new())),
            connections: Arc::new(AtomicUsize::new(0)),
        };

        let status = Arc::clone(&endpoint.status);
        let bodies = Arc::clone(&endpoint.bodies);
        let connections = Arc::clone(&endpoint.connections);

        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(serve(socket, Arc::clone(&status), Arc::clone(&bodies)));
            }
        });

        endpoint
    }

    /// The `event` field of everything posted so far, in order.
    fn events(&self) -> Vec<String> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .map(|body| body["event"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

async fn serve(mut socket: TcpStream, status: Arc<AtomicU16>, bodies: Arc<Mutex<Vec<Value>>>) {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];

    let split = loop {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
        }
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break split;
        }
    };

    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let length: usize = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);

    let mut body = raw[split + 4..].to_vec();
    while body.len() < length {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&buffer[..read]),
        }
    }

    if let Ok(parsed) = serde_json::from_slice::<Value>(&body) {
        bodies.lock().unwrap().push(parsed);
    }

    let status = status.load(Ordering::Relaxed);
    if status == SILENT {
        std::future::pending::<()>().await;
        return;
    }

    let _ = socket
        .write_all(
            format!("HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
}

fn test_config(adjust: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    // A directory each, for the same reason: two servers sharing a store would
    // read each other's meetings, and two sharing an outbox would deliver each
    // other's events.
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();
    config.events.directory = tempfile::TempDir::new().unwrap().keep();
    adjust(&mut config);

    config
}

async fn spawn_server_with(adjust: impl FnOnce(&mut Config)) -> SocketAddr {
    let state = AppState::start(test_config(adjust)).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(state);

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    addr
}

fn joiner(identity: &str, room: &str) -> String {
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

async fn join(addr: SocketAddr, identity: &str, room: &str) -> Client {
    let url = format!(
        "ws://{addr}/rtc?access_token={}&protocol=17&sdk=test",
        joiner(identity, room)
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

async fn healthz(addr: SocketAddr) -> HashMap<String, String> {
    let mut socket = TcpStream::connect(addr).await.expect("connect");
    socket
        .write_all(
            format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("request");

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.expect("response");
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();

    body.split_whitespace()
        .filter_map(|field| field.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// Waits for something to become true rather than sleeping long enough to be
/// safe. Takes an async condition, because some of what is waited for here is
/// another request to the server.
async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        if ready().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("{what}");
}

/// One person joining a room and closing their laptop, as a consumer sees it.
///
/// The whole point of the shape being LiveKit's is that a consumer already
/// listening to a LiveKit deployment does not have to be changed, so what is
/// asserted here is the names and the fields, not that something arrived.
#[tokio::test]
async fn a_real_room_announces_itself_to_a_webhook_endpoint() {
    let endpoint = Endpoint::start(200).await;
    let url = endpoint.url.clone();
    let addr = spawn_server_with(move |config| {
        config.room.end_grace = 0;
        config.room.sweep_interval_ms = 20;
        config.events.webhooks.api_key = API_KEY.to_owned();
        config.events.webhooks.urls = vec![url];
        config.events.attempts = 3;
        config.events.timeout_ms = 500;
        config.events.backoff_ms = 10;
        config.events.max_backoff_ms = 20;
    })
    .await;

    let alice = join(addr, "alice", ROOM).await;
    until("the arrival was never announced", async || {
        endpoint.events().contains(&"participant_joined".to_owned())
    })
    .await;

    let joined = endpoint
        .bodies()
        .into_iter()
        .find(|body| body["event"] == "participant_joined")
        .expect("a participant_joined");
    assert_eq!(joined["room"]["name"], ROOM);
    assert_eq!(joined["participant"]["identity"], "alice");
    assert!(
        joined["id"].as_str().unwrap_or_default().starts_with("EV_"),
        "no event id, so a consumer that throws duplicates away by id cannot: {}",
        joined["id"]
    );

    drop(alice);

    until(
        "the departure and the end of the meeting were never announced",
        async || {
            let seen = endpoint.events();
            seen.contains(&"participant_left".to_owned())
                && seen.contains(&"session_finished".to_owned())
        },
    )
    .await;

    let seen = endpoint.events();
    for expected in [
        "room_started",
        "participant_joined",
        "session_started",
        "participant_left",
        "room_finished",
        "session_finished",
    ] {
        assert!(
            seen.contains(&expected.to_owned()),
            "`{expected}` never reached the endpoint; what did was {seen:?}"
        );
    }

    // The order a consumer can rely on: a room before the people in it, a
    // meeting before it ends.
    let position = |name: &str| seen.iter().position(|event| event == name).unwrap();
    assert!(
        position("room_started") < position("participant_joined"),
        "somebody arrived in a room the consumer had not been told about: {seen:?}"
    );
    assert!(
        position("session_started") < position("session_finished"),
        "a meeting ended before it began: {seen:?}"
    );

    let ended = endpoint
        .bodies()
        .into_iter()
        .find(|body| body["event"] == "session_finished")
        .expect("a session_finished");
    assert_eq!(
        ended["session"]["room"], ROOM,
        "a meeting outlives the room it was in, so the room's name has to travel inside it"
    );
    assert!(
        ended["session"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("SE_"),
        "the meeting cannot be looked up afterwards, which is the one thing a consumer that \
         missed an event has left"
    );
}

/// The rule the whole subsystem is built around, measured against a real
/// server: a webhook endpoint that accepts connections and never answers must
/// cost somebody joining a room nothing.
///
/// Every setting here is chosen to make a failure obvious. The endpoint is a
/// black hole, its timeout is five seconds and it gets five attempts thirty
/// seconds apart -- so anything on the join path that waited for a delivery
/// would show up as seconds per join rather than milliseconds.
#[tokio::test]
async fn joining_is_not_slowed_down_by_an_endpoint_that_never_answers() {
    let endpoint = Endpoint::start(SILENT).await;
    let url = endpoint.url.clone();
    let addr = spawn_server_with(move |config| {
        config.events.webhooks.api_key = API_KEY.to_owned();
        config.events.webhooks.urls = vec![url];
        config.events.attempts = 5;
        config.events.timeout_ms = 5_000;
        config.events.backoff_ms = 30_000;
        config.events.max_backoff_ms = 30_000;
    })
    .await;

    let mut sockets = Vec::new();
    let started = Instant::now();
    for index in 0..8 {
        sockets.push(join(addr, &format!("person-{index}"), ROOM).await);
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "eight people took {elapsed:?} to get into a room while a webhook endpoint was hanging. \
         A delivery is being awaited somewhere on the join path, and the room is the one thing \
         that must never wait for one"
    );

    // Waited for rather than read straight away. Joining costs no I/O at all
    // now -- the meeting record is written by a thread of its own -- so eight
    // people are in the room in less time than it takes the delivery worker to
    // open a socket, and reading this immediately would fail the guard on a
    // server that is behaving exactly as it should.
    until(
        "the endpoint was never even tried, so nothing here was ever waiting on it and this test \
         proves nothing",
        async || endpoint.connections() > 0,
    )
    .await;

    drop(sockets);
}

/// What an operator is told. A destination that is behind shows up as events
/// owed; one that has been given up on shows up as events nobody will ever
/// get, and there is nowhere else that number appears.
#[tokio::test]
async fn healthz_says_how_many_events_are_owed_and_how_many_are_lost() {
    let endpoint = Endpoint::start(500).await;
    let url = endpoint.url.clone();
    let addr = spawn_server_with(move |config| {
        config.events.webhooks.api_key = API_KEY.to_owned();
        config.events.webhooks.urls = vec![url];
        config.events.attempts = 2;
        config.events.timeout_ms = 300;
        config.events.backoff_ms = 10;
        config.events.max_backoff_ms = 20;
    })
    .await;

    let health = healthz(addr).await;
    for field in [
        "events",
        "owed_events",
        "undelivered_events",
        "unwritten_events",
    ] {
        assert_eq!(
            health.get(field).map(String::as_str),
            Some("0"),
            "/healthz does not carry `{field}`, so nothing an operator watches would notice a \
             consumer going deaf: {health:?}"
        );
    }

    let _alice = join(addr, "alice", ROOM).await;

    until("no event was ever given up on", async || {
        healthz(addr)
            .await
            .get("undelivered_events")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            > 0
    })
    .await;

    let health = healthz(addr).await;
    assert!(
        health["events"].parse::<u64>().unwrap() >= 3,
        "a room was made, somebody joined it and a meeting started, and /healthz counted \
         {} events",
        health["events"]
    );
}

/// A broker written into the config of a binary that has no producer in it.
///
/// Refused, not ignored. An operator who wrote that down believes their bus is
/// being fed, and a bus that is silently not being fed is the exact failure
/// every other decision in this subsystem is arranged to prevent.
#[cfg(not(feature = "kafka"))]
#[tokio::test]
async fn a_broker_named_on_a_build_with_no_producer_is_refused_at_startup() {
    let config = test_config(|config| {
        config.events.kafka.brokers = "kafka1:9093".to_owned();
    });

    let Err(error) = AppState::start(config).await else {
        panic!(
            "a server with no kafka producer started against a configured broker, and every \
             event it emits for that topic goes nowhere"
        );
    };

    let said = format!("{error:#}");
    assert!(
        said.contains("kafka") && said.contains("feature"),
        "the refusal does not say what is wrong or how to fix it: {said}"
    );
}
