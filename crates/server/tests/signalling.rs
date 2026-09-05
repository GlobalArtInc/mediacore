use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::livekit::{self, signal_request, signal_response};
use mediacore_server::{http, state::AppState};
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn spawn_server() -> SocketAddr {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();

    let state = AppState::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(state);

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    addr
}

fn token(identity: &str, room: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    let claims = Claims {
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
    };

    mediacore_auth::issue(API_SECRET, &claims).unwrap()
}

async fn connect(addr: SocketAddr, identity: &str, room: &str) -> Client {
    let url = format!(
        "ws://{addr}/rtc?access_token={}&protocol=17&sdk=test",
        token(identity, room)
    );
    let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
}

async fn next_signal(client: &mut Client) -> signal_response::Message {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .expect("signal did not arrive in time")
            .expect("socket closed")
            .expect("socket error");

        if let Message::Binary(data) = message {
            let response = livekit::SignalResponse::decode(data.as_ref()).unwrap();
            if let Some(message) = response.message {
                return message;
            }
        }
    }
}

#[tokio::test]
async fn join_is_answered_with_room_and_participant() {
    let addr = spawn_server().await;
    let mut alice = connect(addr, "alice", "demo").await;

    let join = match next_signal(&mut alice).await {
        signal_response::Message::Join(join) => join,
        other => panic!("expected a join response, got {other:?}"),
    };

    assert_eq!(join.room.unwrap().name, "demo");
    let participant = join.participant.unwrap();
    assert_eq!(participant.identity, "alice");
    assert!(participant.sid.starts_with("PA_"));
    assert!(join.other_participants.is_empty());

    let server = join.server_info.unwrap();
    assert_eq!(server.protocol, mediacore_proto::PROTOCOL_VERSION);
}

#[tokio::test]
async fn participants_learn_about_each_other() {
    let addr = spawn_server().await;
    let mut alice = connect(addr, "alice", "demo").await;
    let _ = next_signal(&mut alice).await;

    let mut bob = connect(addr, "bob", "demo").await;
    let join = match next_signal(&mut bob).await {
        signal_response::Message::Join(join) => join,
        other => panic!("expected a join response, got {other:?}"),
    };
    assert_eq!(join.other_participants.len(), 1);
    assert_eq!(join.other_participants[0].identity, "alice");

    match next_signal(&mut alice).await {
        signal_response::Message::Update(update) => {
            assert_eq!(update.participants[0].identity, "bob");
            assert_eq!(
                update.participants[0].state,
                livekit::participant_info::State::Joined as i32
            );
        }
        other => panic!("expected a participant update, got {other:?}"),
    }

    drop(bob);

    match next_signal(&mut alice).await {
        signal_response::Message::Update(update) => {
            assert_eq!(update.participants[0].identity, "bob");
            assert_eq!(
                update.participants[0].state,
                livekit::participant_info::State::Disconnected as i32
            );
        }
        other => panic!("expected a disconnect update, got {other:?}"),
    }
}

#[tokio::test]
async fn ping_is_answered_with_pong() {
    let addr = spawn_server().await;
    let mut alice = connect(addr, "alice", "demo").await;
    let _ = next_signal(&mut alice).await;

    let request = livekit::SignalRequest {
        message: Some(signal_request::Message::PingReq(livekit::Ping {
            timestamp: 1234,
            rtt: 0,
        })),
    };
    alice
        .send(Message::Binary(request.encode_to_vec().into()))
        .await
        .unwrap();

    match next_signal(&mut alice).await {
        signal_response::Message::PongResp(pong) => {
            assert_eq!(pong.last_ping_timestamp, 1234);
            assert!(pong.timestamp > 0);
        }
        other => panic!("expected a pong, got {other:?}"),
    }
}

#[tokio::test]
async fn connection_without_a_valid_token_is_refused() {
    let addr = spawn_server().await;
    let url = format!("ws://{addr}/rtc?access_token=not-a-token");

    assert!(tokio_tungstenite::connect_async(url).await.is_err());
}

#[tokio::test]
async fn validate_endpoint_reports_identity() {
    let addr = spawn_server().await;
    let path = format!("/rtc/validate?access_token={}", token("alice", "demo"));

    let (status, body) = http_get(addr, &path).await;
    assert_eq!(status, 200);
    assert_eq!(body, "alice");
}

async fn http_request(addr: SocketAddr, request: &str) -> String {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        socket.write_all(request.as_bytes()).await.expect("request");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        String::from_utf8_lossy(&raw).into_owned()
    };

    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("http response did not arrive in time")
}

#[tokio::test]
async fn a_browser_is_allowed_to_reach_the_signalling_endpoints() {
    let addr = spawn_server().await;

    let preflight = http_request(
        addr,
        &format!(
            "OPTIONS /rtc/validate HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://example.test\r\n\
Access-Control-Request-Method: GET\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
    .to_lowercase();

    assert!(
        preflight.starts_with("http/1.1 200"),
        "a preflight must be answered, got: {preflight}"
    );
    assert!(
        preflight.contains("access-control-allow-origin: *"),
        "the preflight carries no allow-origin header: {preflight}"
    );
    assert!(
        preflight.contains("get"),
        "the preflight does not allow GET: {preflight}"
    );

    let validate = http_request(
        addr,
        &format!(
            "GET /rtc/validate?access_token=nonsense HTTP/1.1\r\nHost: {addr}\r\n\
Origin: http://example.test\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
    .to_lowercase();

    assert!(
        validate.contains("access-control-allow-origin: *"),
        "a rejected token must still answer with cors headers, got: {validate}"
    );
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        socket.write_all(request.as_bytes()).await.expect("request");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        String::from_utf8(raw).expect("utf-8 response")
    };

    let response = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("http response did not arrive in time");

    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("http response without a body");
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("http status line")
        .parse()
        .expect("numeric http status");

    (status, body.to_owned())
}

#[tokio::test]
async fn room_state_is_visible_over_http() {
    let addr = spawn_server().await;
    let mut alice = connect(addr, "alice", "demo").await;
    let _ = next_signal(&mut alice).await;

    let (status, body) = http_get(addr, "/healthz").await;
    assert_eq!(status, 200);
    assert!(body.starts_with("rooms=1 participants=1 publishers=1 rtp_packets=0"));
    assert!(body.contains("ice=127.0.0.1:"));
}
