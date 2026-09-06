use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::mediacore::{self, signal_request, signal_response};
use mediacore_server::rooms::{self, Grant, Support};
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
    spawn_server_with(|_| {}).await
}

async fn spawn_server_with(adjust: impl FnOnce(&mut Config)) -> SocketAddr {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
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

fn creator() -> String {
    token(VideoGrant {
        room_create: Some(true),
        ..VideoGrant::default()
    })
}

/// A `roomCreate` token that also says which room it is for. That scope is
/// what lets it settle a room somebody already walked into.
fn scoped_creator(room: &str) -> String {
    token(VideoGrant {
        room_create: Some(true),
        room: room.to_owned(),
        ..VideoGrant::default()
    })
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

struct Answer {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
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
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn identities(&self) -> Vec<String> {
        self.list("participants", "identity")
    }

    fn room_names(&self) -> Vec<String> {
        self.list("rooms", "name")
    }

    fn list(&self, field: &str, key: &str) -> Vec<String> {
        self.json()[field]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry[key].as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn message(&self) -> String {
        self.json()
            .get("msg")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

async fn call(
    addr: SocketAddr,
    method: &str,
    content_type: &str,
    authorization: Option<&str>,
    body: &[u8],
) -> Answer {
    call_with_verb(addr, "POST", method, content_type, authorization, body).await
}

async fn call_with_verb(
    addr: SocketAddr,
    verb: &str,
    method: &str,
    content_type: &str,
    authorization: Option<&str>,
    body: &[u8],
) -> Answer {
    call_at(
        addr,
        rooms::PATH_PREFIX,
        verb,
        method,
        content_type,
        authorization,
        body,
    )
    .await
}

/// The same call, under whichever of the service's two names the caller picks.
///
/// The prefix is a parameter rather than a constant so that the compatibility
/// alias is exercised by the same helper, and therefore by the same
/// assertions, as the canonical path.
async fn call_at(
    addr: SocketAddr,
    prefix: &str,
    verb: &str,
    method: &str,
    content_type: &str,
    authorization: Option<&str>,
    body: &[u8],
) -> Answer {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let mut head = format!(
            "{verb} {prefix}{method} HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(authorization) = authorization {
            head.push_str(&format!("Authorization: Bearer {authorization}\r\n"));
        }
        head.push_str("\r\n");

        socket.write_all(head.as_bytes()).await.expect("request");
        socket.write_all(body).await.expect("request body");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        raw
    };

    let raw = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("the room service did not answer in time");

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("http response without a body");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();

    Answer {
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

async fn post_json(
    addr: SocketAddr,
    method: &str,
    authorization: &str,
    body: serde_json::Value,
) -> Answer {
    call(
        addr,
        method,
        "application/json",
        Some(authorization),
        body.to_string().as_bytes(),
    )
    .await
}

async fn connect(addr: SocketAddr, identity: &str, room: &str) -> Client {
    let url = format!(
        "ws://{addr}/rtc?access_token={}&protocol=17&sdk=test",
        joiner(identity, room)
    );
    let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
}

async fn next_signal(client: &mut Client) -> signal_response::Message {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("signal did not arrive in time")
            .expect("socket closed")
            .expect("socket error");

        if let Message::Binary(data) = message {
            let response = mediacore::SignalResponse::decode(data.as_ref()).unwrap();
            if let Some(message) = response.message {
                return message;
            }
        }
    }
}

async fn join(addr: SocketAddr, identity: &str, room: &str) -> (Client, mediacore::JoinResponse) {
    let mut client = connect(addr, identity, room).await;
    match next_signal(&mut client).await {
        signal_response::Message::Join(join) => (client, join),
        other => panic!("expected a join response, got {other:?}"),
    }
}

async fn publish(client: &mut Client, cid: &str) -> mediacore::TrackInfo {
    let request = mediacore::SignalRequest {
        message: Some(signal_request::Message::AddTrack(
            mediacore::AddTrackRequest {
                cid: cid.to_owned(),
                name: cid.to_owned(),
                r#type: mediacore::TrackType::Audio as i32,
                source: mediacore::TrackSource::Microphone as i32,
                ..Default::default()
            },
        )),
    };

    client
        .send(Message::Binary(request.encode_to_vec().into()))
        .await
        .expect("add track");

    loop {
        if let signal_response::Message::TrackPublished(published) = next_signal(client).await {
            return published.track.expect("the published track");
        }
    }
}

// The wire

#[tokio::test]
async fn a_room_is_created_and_then_listed_over_json() {
    let addr = spawn_server().await;

    let created = post_json(
        addr,
        "CreateRoom",
        &creator(),
        serde_json::json!({"name": "standup", "emptyTimeout": 60, "maxParticipants": 4}),
    )
    .await;

    assert_eq!(created.status, 200);
    assert_eq!(created.content_type, "application/json");

    let room = created.json();
    assert_eq!(room["name"], "standup");
    assert_eq!(
        room["emptyTimeout"], 60,
        "protobuf json names fields in lowerCamelCase, and a server SDK reads no other spelling"
    );
    assert_eq!(room["maxParticipants"], 4);
    assert!(
        room["creationTime"].is_string(),
        "a 64-bit integer travels as a json string, and an SDK that parses it as a number loses \
         precision: {room}"
    );
    assert!(
        room.get("numParticipants").is_none(),
        "protobuf json leaves a default value out entirely, it does not send a zero: {room}"
    );

    let listed = post_json(addr, "ListRooms", &lister(), serde_json::json!({})).await;
    assert_eq!(listed.status, 200);
    assert_eq!(listed.json()["rooms"][0]["name"], "standup");
}

#[tokio::test]
async fn the_same_call_in_protobuf_is_answered_in_protobuf() {
    let addr = spawn_server().await;

    let request = mediacore::CreateRoomRequest {
        name: "board".to_owned(),
        max_participants: 8,
        ..Default::default()
    };
    let created = call(
        addr,
        "CreateRoom",
        "application/protobuf",
        Some(&creator()),
        &request.encode_to_vec(),
    )
    .await;

    assert_eq!(created.status, 200);
    assert_eq!(created.content_type, "application/protobuf");

    let room = mediacore::Room::decode(created.body.as_slice()).expect("a protobuf room");
    assert_eq!(room.name, "board");
    assert_eq!(room.max_participants, 8);
}

#[tokio::test]
async fn a_filtered_list_returns_only_the_rooms_it_names() {
    let addr = spawn_server().await;
    let creator = creator();

    for name in ["standup", "board", "retro"] {
        post_json(
            addr,
            "CreateRoom",
            &creator,
            serde_json::json!({"name": name}),
        )
        .await;
    }

    let all = post_json(addr, "ListRooms", &lister(), serde_json::json!({})).await;
    let names = all.room_names();
    assert_eq!(
        names,
        ["board", "retro", "standup"],
        "rooms live in a hash map, so the listing sorts them: a backoffice that renders them in \
         the order they arrive would reshuffle its table on every refresh"
    );

    let some = post_json(
        addr,
        "ListRooms",
        &lister(),
        serde_json::json!({"names": ["retro", "nowhere"]}),
    )
    .await;
    let names = some.room_names();
    assert_eq!(names, ["retro"]);
}

#[tokio::test]
async fn an_unknown_method_is_a_route_that_does_not_exist() {
    let addr = spawn_server().await;

    let answer = post_json(addr, "DestroyEverything", &creator(), serde_json::json!({})).await;

    assert_eq!(answer.status, 404);
    assert_eq!(answer.code(), "bad_route");
    assert!(
        answer.message().contains("DestroyEverything"),
        "the message has to name the method, or a caller cannot tell a typo from an outage: {}",
        answer.message()
    );
}

#[tokio::test]
async fn a_content_type_twirp_does_not_speak_is_a_route_that_does_not_exist() {
    let addr = spawn_server().await;

    let answer = call(
        addr,
        "ListRooms",
        "text/plain",
        Some(&lister()),
        b"{}".as_slice(),
    )
    .await;

    assert_eq!(answer.status, 404);
    assert_eq!(answer.code(), "bad_route");
    assert_eq!(
        answer.content_type, "application/json",
        "a twirp error is json whatever the request was, because a caller we could not understand \
         cannot be assumed to read protobuf either"
    );
}

#[tokio::test]
async fn a_call_that_is_not_a_post_is_a_route_that_does_not_exist() {
    let addr = spawn_server().await;

    let answer = call_with_verb(
        addr,
        "GET",
        "ListRooms",
        "application/json",
        Some(&lister()),
        b"".as_slice(),
    )
    .await;

    assert_eq!(answer.status, 404);
    assert_eq!(answer.code(), "bad_route");
}

#[tokio::test]
async fn a_body_that_is_not_what_it_claims_to_be_is_malformed() {
    let addr = spawn_server().await;

    let broken_json = call(
        addr,
        "CreateRoom",
        "application/json",
        Some(&creator()),
        b"{\"name\": ".as_slice(),
    )
    .await;
    assert_eq!(broken_json.status, 400);
    assert_eq!(broken_json.code(), "malformed");

    let broken_protobuf = call(
        addr,
        "CreateRoom",
        "application/protobuf",
        Some(&creator()),
        b"\xff\xff\xff\xff".as_slice(),
    )
    .await;
    assert_eq!(broken_protobuf.status, 400);
    assert_eq!(broken_protobuf.code(), "malformed");
}

#[tokio::test]
async fn a_room_without_a_name_is_a_bad_argument() {
    let addr = spawn_server().await;

    let answer = post_json(addr, "CreateRoom", &creator(), serde_json::json!({})).await;

    assert_eq!(answer.status, 400);
    assert_eq!(answer.code(), "invalid_argument");
}

#[tokio::test]
async fn the_methods_that_need_several_nodes_say_they_are_not_implemented() {
    let addr = spawn_server().await;
    let admin = admin("standup");

    for (method, body) in [
        (
            "ForwardParticipant",
            serde_json::json!({"room": "standup", "identity": "alice", "destinationRoom": "board"}),
        ),
        (
            "MoveParticipant",
            serde_json::json!({"room": "standup", "identity": "alice", "destinationRoom": "board"}),
        ),
        (
            "PerformRpc",
            serde_json::json!({"room": "standup", "destinationIdentity": "alice", "method": "hi"}),
        ),
        (
            "SendData",
            serde_json::json!({"room": "standup", "data": "aGk="}),
        ),
    ] {
        let answer = post_json(addr, method, &admin, body).await;

        assert_eq!(
            answer.status, 501,
            "{method} answered {} instead of saying plainly that it is not implemented",
            answer.status
        );
        assert_eq!(answer.code(), "unimplemented");
        assert!(
            !answer.message().is_empty(),
            "{method} refused without saying why"
        );
    }
}

#[tokio::test]
async fn a_browser_may_preflight_a_room_service_call_that_carries_a_token() {
    let addr = spawn_server().await;
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "OPTIONS {}ListRooms HTTP/1.1\r\nHost: {addr}\r\n\
             Origin: http://backoffice.test\r\nAccess-Control-Request-Method: POST\r\n\
             Access-Control-Request-Headers: authorization,content-type\r\n\
             Connection: close\r\n\r\n",
            rooms::PATH_PREFIX
        );
        socket.write_all(request.as_bytes()).await.expect("request");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        String::from_utf8_lossy(&raw).to_ascii_lowercase()
    };

    let preflight = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("the preflight was never answered");

    assert!(preflight.starts_with("http/1.1 200"), "{preflight}");
    assert!(
        preflight.contains("access-control-allow-headers: authorization"),
        "a `*` in allow-headers does not cover Authorization -- the Fetch standard excludes it -- \
         so a browser would never send the call at all: {preflight}"
    );
}

// The authorization boundary

#[tokio::test]
async fn a_call_with_no_token_at_all_is_unauthenticated() {
    let addr = spawn_server().await;

    let answer = call(
        addr,
        "ListRooms",
        "application/json",
        None,
        b"{}".as_slice(),
    )
    .await;

    assert_eq!(answer.status, 401);
    assert_eq!(answer.code(), "unauthenticated");
}

#[tokio::test]
async fn a_server_api_token_carries_no_identity_and_is_still_accepted() {
    let addr = spawn_server().await;
    let creator = creator();
    let payload = creator.split('.').nth(1).unwrap();
    let payload =
        String::from_utf8(base64_decode(payload).expect("the token payload is base64url")).unwrap();

    assert!(
        !payload.contains("\"sub\""),
        "this test is meant to send the token a LiveKit server SDK sends, which has no identity \
         on it: {payload}"
    );

    let created = post_json(
        addr,
        "CreateRoom",
        &creator,
        serde_json::json!({"name": "standup"}),
    )
    .await;

    assert_eq!(
        created.status, 200,
        "LiveKit's server SDKs refuse to put an identity on anything but a join token, so a room \
         service that insists on one refuses every one of them"
    );
}

fn base64_decode(value: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in value.bytes() {
        let index = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            _ => return None,
        };

        buffer = (buffer << 6) | u32::from(index);
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    Some(out)
}

#[tokio::test]
async fn a_token_we_did_not_sign_is_unauthenticated() {
    let addr = spawn_server().await;
    let forged = mediacore_auth::issue(
        "a-secret-that-is-not-ours-at-all",
        &Claims {
            exp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 600,
            iss: API_KEY.to_owned(),
            sub: "intruder".to_owned(),
            video: VideoGrant {
                room_create: Some(true),
                room_list: Some(true),
                room_admin: Some(true),
                ..VideoGrant::default()
            },
            ..Claims::default()
        },
    )
    .unwrap();

    let answer = post_json(addr, "ListRooms", &forged, serde_json::json!({})).await;

    assert_eq!(answer.status, 401);
    assert_eq!(
        answer.code(),
        "unauthenticated",
        "a token can claim every grant there is; it is the signature that decides"
    );
}

#[tokio::test]
async fn a_join_token_cannot_run_the_server() {
    let addr = spawn_server().await;
    let creator = creator();
    post_json(
        addr,
        "CreateRoom",
        &creator,
        serde_json::json!({"name": "standup"}),
    )
    .await;

    let joiner = joiner("alice", "standup");

    for (method, body) in [
        ("CreateRoom", serde_json::json!({"name": "mine"})),
        ("DeleteRoom", serde_json::json!({"room": "standup"})),
        ("ListRooms", serde_json::json!({})),
        ("ListParticipants", serde_json::json!({"room": "standup"})),
        (
            "RemoveParticipant",
            serde_json::json!({"room": "standup", "identity": "bob"}),
        ),
        (
            "UpdateRoomMetadata",
            serde_json::json!({"room": "standup", "metadata": "mine now"}),
        ),
    ] {
        let answer = post_json(addr, method, &joiner, body).await;

        assert_eq!(
            answer.status, 403,
            "a token that only lets someone into the meeting was allowed to call {method}: this \
             is every participant holding a master key for the server"
        );
        assert_eq!(answer.code(), "permission_denied");
    }

    let listed = post_json(addr, "ListRooms", &lister(), serde_json::json!({})).await;
    assert_eq!(
        listed.json()["rooms"].as_array().unwrap().len(),
        1,
        "the room the join token tried to delete is gone"
    );
}

#[tokio::test]
async fn an_admin_of_one_room_is_refused_on_another() {
    let addr = spawn_server().await;
    let creator = creator();
    for name in ["standup", "payroll"] {
        post_json(
            addr,
            "CreateRoom",
            &creator,
            serde_json::json!({"name": name}),
        )
        .await;
    }

    let (_alice, _) = join(addr, "alice", "payroll").await;
    let intruder = admin("standup");

    for (method, body) in [
        ("ListParticipants", serde_json::json!({"room": "payroll"})),
        (
            "GetParticipant",
            serde_json::json!({"room": "payroll", "identity": "alice"}),
        ),
        (
            "RemoveParticipant",
            serde_json::json!({"room": "payroll", "identity": "alice"}),
        ),
        (
            "UpdateRoomMetadata",
            serde_json::json!({"room": "payroll", "metadata": "x"}),
        ),
        (
            "UpdateParticipant",
            serde_json::json!({"room": "payroll", "identity": "alice", "name": "nobody"}),
        ),
        (
            "MutePublishedTrack",
            serde_json::json!({"room": "payroll", "identity": "alice", "trackSid": "TR_x", "muted": true}),
        ),
        (
            "UpdateSubscriptions",
            serde_json::json!({"room": "payroll", "identity": "alice", "trackSids": [], "subscribe": false}),
        ),
    ] {
        let answer = post_json(addr, method, &intruder, body).await;

        assert_eq!(
            answer.status, 403,
            "an admin token naming `standup` reached `payroll` through {method}: the room claim \
             is the only thing that keeps one meeting's moderator out of another's"
        );
        assert_eq!(answer.code(), "permission_denied");
        assert!(
            answer.message().contains("payroll"),
            "the refusal should name the room it refused: {}",
            answer.message()
        );
    }
}

#[tokio::test]
async fn one_grant_does_not_stand_in_for_another() {
    let addr = spawn_server().await;

    let listing_with_a_creator =
        post_json(addr, "ListRooms", &creator(), serde_json::json!({})).await;
    assert_eq!(listing_with_a_creator.status, 403);

    let creating_with_a_lister = post_json(
        addr,
        "CreateRoom",
        &lister(),
        serde_json::json!({"name": "standup"}),
    )
    .await;
    assert_eq!(creating_with_a_lister.status, 403);

    let administering_with_a_creator = post_json(
        addr,
        "ListParticipants",
        &creator(),
        serde_json::json!({"room": "standup"}),
    )
    .await;
    assert_eq!(
        administering_with_a_creator.status, 403,
        "roomCreate builds rooms, it does not moderate the people inside them"
    );
}

// The methods, against a real registry

#[tokio::test]
async fn deleting_a_room_disconnects_the_people_in_it() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;
    let (mut bob, _) = join(addr, "bob", "standup").await;
    let _ = next_signal(&mut alice).await;

    let deleted = post_json(
        addr,
        "DeleteRoom",
        &creator(),
        serde_json::json!({"room": "standup"}),
    )
    .await;
    assert_eq!(deleted.status, 200);
    assert_eq!(deleted.json(), serde_json::json!({}));

    for (name, client) in [("alice", &mut alice), ("bob", &mut bob)] {
        loop {
            match next_signal(client).await {
                signal_response::Message::Leave(leave) => {
                    assert_eq!(
                        leave.reason,
                        mediacore::DisconnectReason::RoomDeleted as i32,
                        "{name} was cut off without being told the room was deleted, so their \
                         client cannot tell this from a network failure and will try to reconnect"
                    );
                    assert!(!leave.can_reconnect);
                    break;
                }
                _ => continue,
            }
        }
    }

    let listed = post_json(addr, "ListRooms", &lister(), serde_json::json!({})).await;
    assert!(listed.json()["rooms"].as_array().is_none_or(Vec::is_empty));
}

#[tokio::test]
async fn deleting_a_room_that_is_already_gone_is_not_a_failure() {
    let addr = spawn_server().await;

    let answer = post_json(
        addr,
        "DeleteRoom",
        &creator(),
        serde_json::json!({"room": "never-existed"}),
    )
    .await;

    assert_eq!(
        answer.status, 200,
        "a backoffice retrying a delete after a timeout must not be told its own cleanup failed"
    );
}

#[tokio::test]
async fn a_created_room_keeps_the_settings_it_was_created_with() {
    let addr = spawn_server().await;
    let creator = creator();

    post_json(
        addr,
        "CreateRoom",
        &creator,
        serde_json::json!({"name": "standup", "maxParticipants": 2, "metadata": "first"}),
    )
    .await;
    let again = post_json(
        addr,
        "CreateRoom",
        &creator,
        serde_json::json!({"name": "standup", "maxParticipants": 9, "metadata": "second"}),
    )
    .await;

    assert_eq!(again.status, 200);
    assert_eq!(
        again.json()["maxParticipants"],
        2,
        "a retried create rewrote a room people may already be sitting in"
    );
    assert_eq!(again.json()["metadata"], "first");
}

#[tokio::test]
async fn creating_a_room_the_first_arrival_made_settles_it() {
    let addr = spawn_server().await;
    let (mut alice, join) = join(addr, "alice", "standup").await;
    assert_eq!(join.room.unwrap().max_participants, 0);

    let created = post_json(
        addr,
        "CreateRoom",
        &scoped_creator("standup"),
        serde_json::json!({"name": "standup", "maxParticipants": 5, "metadata": "the daily"}),
    )
    .await;

    assert_eq!(created.json()["maxParticipants"], 5);
    assert_eq!(
        created.json()["numParticipants"],
        1,
        "the room alice is in is the room that was created, not a second one beside it"
    );

    loop {
        if let signal_response::Message::RoomUpdate(update) = next_signal(&mut alice).await {
            assert_eq!(update.room.unwrap().metadata, "the daily");
            break;
        }
    }
}

/// `roomCreate` carries no room name, so one backoffice token is an authority
/// over every room on the server. Settling a room an arrival opened is a
/// rewrite of the settings and the session policy of a room people are already
/// sitting in, and that needs a token that says which room it means.
#[tokio::test]
async fn an_unscoped_create_leaves_a_room_with_people_in_it_alone() {
    let addr = spawn_server().await;
    let (_alice, _join) = join(addr, "alice", "standup").await;

    let answered = post_json(
        addr,
        "CreateRoom",
        &creator(),
        serde_json::json!({
            "name": "standup",
            "maxParticipants": 5,
            "metadata": r#"{"sessionPolicy":{"minHumans":2,"endGraceMs":1000}}"#,
        }),
    )
    .await;

    assert_eq!(answered.status, 200, "LiveKit's CreateRoom is idempotent");
    assert_eq!(
        answered.json()["maxParticipants"],
        serde_json::Value::Null,
        "a token that names no room re-pointed the settings of a meeting that was already \
         running; the answer carries the room as it is, which is how a caller sees that"
    );
    assert_eq!(answered.json()["metadata"], serde_json::Value::Null);
    assert_eq!(answered.json()["numParticipants"], 1);
}

/// `CreateRoomRequest` carries fields for things this server does not do. A
/// caller asking for automatic recording and getting a 200 believes the
/// meeting is being recorded, and nothing later tells them otherwise.
#[tokio::test]
async fn a_room_that_asks_for_something_we_cannot_do_is_refused() {
    let addr = spawn_server().await;

    let recorded = post_json(
        addr,
        "CreateRoom",
        &creator(),
        serde_json::json!({
            "name": "board-meeting",
            "egress": {"room": {"roomName": "board-meeting", "layout": "grid"}},
        }),
    )
    .await;

    assert_eq!(recorded.status, 501);
    assert_eq!(recorded.code(), "unimplemented");
    assert!(
        recorded.message().contains("egress"),
        "the refusal has to name the field, or a caller cannot tell which half of their \
         request we could not do: {}",
        recorded.message()
    );
    assert!(
        post_json(
            addr,
            "ListRooms",
            &lister(),
            serde_json::json!({"names": ["board-meeting"]}),
        )
        .await
        .room_names()
        .is_empty(),
        "a refused create still made the room, so the caller has a meeting nobody is recording"
    );

    for field in [
        serde_json::json!({"name": "r1", "agents": [{"agentName": "notetaker"}]}),
        serde_json::json!({"name": "r2", "roomPreset": "webinar"}),
        serde_json::json!({"name": "r3", "replayEnabled": true}),
        serde_json::json!({"name": "r4", "tags": {"team": "platform"}}),
    ] {
        let answered = post_json(addr, "CreateRoom", &creator(), field.clone()).await;
        assert_eq!(
            answered.code(),
            "unimplemented",
            "accepted and dropped: {field}"
        );
    }
}

#[tokio::test]
async fn a_room_service_call_reports_both_connections_of_one_person() {
    let addr = spawn_server().await;
    let (_first, first_join) = join(addr, "alice", "standup").await;
    let (_second, second_join) = join(addr, "alice", "standup").await;

    assert_eq!(first_join.participant.unwrap().identity, "alice");
    assert_eq!(second_join.participant.unwrap().identity, "alice#2");

    let listed = post_json(
        addr,
        "ListParticipants",
        &admin("standup"),
        serde_json::json!({"room": "standup"}),
    )
    .await;

    let identities = listed.identities();
    assert_eq!(
        identities,
        ["alice", "alice#2"],
        "the listing reports the identity each connection actually has, which is the only name a \
         caller can use to address one of them"
    );
}

#[tokio::test]
async fn removing_a_person_takes_every_connection_they_have() {
    let addr = spawn_server().await;
    let (mut first, _) = join(addr, "alice", "standup").await;
    let (mut second, _) = join(addr, "alice", "standup").await;
    let (_bob, _) = join(addr, "bob", "standup").await;

    let removed = post_json(
        addr,
        "RemoveParticipant",
        &admin("standup"),
        serde_json::json!({"room": "standup", "identity": "alice"}),
    )
    .await;
    assert_eq!(removed.status, 200);

    for (name, client) in [
        ("the first tab", &mut first),
        ("the second tab", &mut second),
    ] {
        loop {
            if let signal_response::Message::Leave(leave) = next_signal(client).await {
                assert_eq!(
                    leave.reason,
                    mediacore::DisconnectReason::ParticipantRemoved as i32,
                    "{name} stayed in the room after a moderator removed the person holding it"
                );
                break;
            }
        }
    }

    let listed = post_json(
        addr,
        "ListParticipants",
        &admin("standup"),
        serde_json::json!({"room": "standup"}),
    )
    .await;
    let identities = listed.identities();
    assert_eq!(identities, ["bob"]);
}

#[tokio::test]
async fn removing_a_suffixed_identity_takes_only_that_connection() {
    let addr = spawn_server().await;
    let (mut first, _) = join(addr, "alice", "standup").await;
    let (mut second, _) = join(addr, "alice", "standup").await;

    post_json(
        addr,
        "RemoveParticipant",
        &admin("standup"),
        serde_json::json!({"room": "standup", "identity": "alice#2"}),
    )
    .await;

    loop {
        if let signal_response::Message::Leave(_) = next_signal(&mut second).await {
            break;
        }
    }

    let listed = post_json(
        addr,
        "ListParticipants",
        &admin("standup"),
        serde_json::json!({"room": "standup"}),
    )
    .await;
    let identities = listed.identities();
    assert_eq!(
        identities,
        ["alice"],
        "naming one connection has to mean one connection, or a caller has no way to remove a \
         single tab"
    );

    let update = loop {
        match next_signal(&mut first).await {
            signal_response::Message::Update(update) => break update,
            _ => continue,
        }
    };
    assert_eq!(update.participants[0].identity, "alice#2");
}

#[tokio::test]
async fn asking_for_somebody_who_is_not_there_is_a_not_found() {
    let addr = spawn_server().await;
    let (_alice, _) = join(addr, "alice", "standup").await;
    let admin = admin("standup");

    let missing = post_json(
        addr,
        "GetParticipant",
        &admin,
        serde_json::json!({"room": "standup", "identity": "nobody"}),
    )
    .await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code(), "not_found");

    let no_room = post_json(
        addr,
        "ListParticipants",
        &admin,
        serde_json::json!({"room": "standup-2"}),
    )
    .await;
    assert_eq!(
        no_room.status, 403,
        "an admin token for `standup` has no business asking about `standup-2`, whether or not it \
         exists"
    );

    let found = post_json(
        addr,
        "GetParticipant",
        &admin,
        serde_json::json!({"room": "standup", "identity": "alice"}),
    )
    .await;
    assert_eq!(found.status, 200);
    assert_eq!(found.json()["identity"], "alice");
    assert_eq!(found.json()["name"], "alice");
}

#[tokio::test]
async fn updating_a_participant_tells_the_room_who_they_are_now() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;
    let (mut bob, _) = join(addr, "bob", "standup").await;
    let _ = next_signal(&mut alice).await;

    let updated = post_json(
        addr,
        "UpdateParticipant",
        &admin("standup"),
        serde_json::json!({
            "room": "standup",
            "identity": "alice",
            "name": "Alice Liddell",
            "metadata": "{\"seat\":\"chair\"}",
            "attributes": {"role": "host"},
        }),
    )
    .await;

    assert_eq!(updated.status, 200);
    assert_eq!(updated.json()["name"], "Alice Liddell");
    assert_eq!(updated.json()["attributes"]["role"], "host");

    for (who, client) in [("alice herself", &mut alice), ("bob", &mut bob)] {
        loop {
            if let signal_response::Message::Update(update) = next_signal(client).await
                && update.participants[0].identity == "alice"
                && update.participants[0].name == "Alice Liddell"
            {
                assert_eq!(
                    update.participants[0].metadata, "{\"seat\":\"chair\"}",
                    "{who} was not told about the change"
                );
                break;
            }
        }
    }
}

#[tokio::test]
async fn revoking_publishing_takes_the_track_away() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;
    let track = publish(&mut alice, "cid-alice").await;

    let updated = post_json(
        addr,
        "UpdateParticipant",
        &admin("standup"),
        serde_json::json!({
            "room": "standup",
            "identity": "alice",
            "permission": {"canSubscribe": true, "canPublish": false},
        }),
    )
    .await;

    assert_eq!(updated.status, 200);
    assert_eq!(
        updated.json()["permission"]["canPublish"],
        serde_json::Value::Null
    );
    assert!(
        updated.json()["tracks"].is_null(),
        "a participant who may not publish must not still be listed as publishing: {}",
        updated.json()
    );

    loop {
        if let signal_response::Message::TrackUnpublished(unpublished) =
            next_signal(&mut alice).await
        {
            assert_eq!(
                unpublished.track_sid, track.sid,
                "alice's own client was never told to stop sending, so it keeps the camera on and \
                 the room keeps paying for it"
            );
            break;
        }
    }
}

#[tokio::test]
async fn muting_a_track_tells_the_publisher_and_the_room() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;
    let (mut bob, _) = join(addr, "bob", "standup").await;
    let track = publish(&mut alice, "cid-alice").await;

    let muted = post_json(
        addr,
        "MutePublishedTrack",
        &admin("standup"),
        serde_json::json!({
            "room": "standup",
            "identity": "alice",
            "trackSid": track.sid,
            "muted": true,
        }),
    )
    .await;

    assert_eq!(muted.status, 200);
    assert_eq!(muted.json()["track"]["sid"], track.sid);
    assert_eq!(muted.json()["track"]["muted"], true);

    loop {
        if let signal_response::Message::Mute(mute) = next_signal(&mut alice).await {
            assert_eq!(
                mute.sid, track.sid,
                "a mute the publisher is never told about only stops the room from seeing a \
                 camera that is still on"
            );
            assert!(mute.muted);
            break;
        }
    }

    loop {
        if let signal_response::Message::Update(update) = next_signal(&mut bob).await
            && update.participants[0].identity == "alice"
            && update.participants[0]
                .tracks
                .iter()
                .any(|published| published.sid == track.sid && published.muted)
        {
            break;
        }
    }
}

#[tokio::test]
async fn muting_a_track_that_is_not_theirs_is_a_not_found() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;
    let _ = publish(&mut alice, "cid-alice").await;

    let answer = post_json(
        addr,
        "MutePublishedTrack",
        &admin("standup"),
        serde_json::json!({
            "room": "standup",
            "identity": "alice",
            "trackSid": "TR_ANOTHERS",
            "muted": true,
        }),
    )
    .await;

    assert_eq!(answer.status, 404);
    assert_eq!(answer.code(), "not_found");
}

#[tokio::test]
async fn room_metadata_reaches_everyone_in_the_room() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;

    let updated = post_json(
        addr,
        "UpdateRoomMetadata",
        &admin("standup"),
        serde_json::json!({"room": "standup", "metadata": "{\"title\":\"Standup\"}"}),
    )
    .await;

    assert_eq!(updated.status, 200);
    assert_eq!(updated.json()["metadata"], "{\"title\":\"Standup\"}");

    loop {
        if let signal_response::Message::RoomUpdate(update) = next_signal(&mut alice).await {
            assert_eq!(update.room.unwrap().metadata, "{\"title\":\"Standup\"}");
            break;
        }
    }
}

#[tokio::test]
async fn a_subscription_can_be_dropped_and_taken_back_from_outside() {
    let addr = spawn_server().await;
    let (mut alice, _) = join(addr, "alice", "standup").await;
    let (_bob, _) = join(addr, "bob", "standup").await;
    let track = publish(&mut alice, "cid-alice").await;
    let admin = admin("standup");

    let unknown = post_json(
        addr,
        "UpdateSubscriptions",
        &admin,
        serde_json::json!({
            "room": "standup",
            "identity": "bob",
            "trackSids": [track.sid, "TR_NOWHERE"],
            "subscribe": true,
        }),
    )
    .await;
    assert_eq!(unknown.status, 404);
    assert_eq!(
        unknown.code(),
        "not_found",
        "the whole request is checked before any of it is applied, so a caller is never left \
         guessing which half landed"
    );

    let own_track = post_json(
        addr,
        "UpdateSubscriptions",
        &admin,
        serde_json::json!({
            "room": "standup",
            "identity": "alice",
            "trackSids": [track.sid],
            "subscribe": true,
        }),
    )
    .await;
    assert_eq!(own_track.status, 400);
    assert_eq!(own_track.code(), "invalid_argument");

    let dropped = post_json(
        addr,
        "UpdateSubscriptions",
        &admin,
        serde_json::json!({
            "room": "standup",
            "identity": "bob",
            "trackSids": [track.sid],
            "subscribe": false,
        }),
    )
    .await;
    assert_eq!(dropped.status, 200);

    let taken_back = post_json(
        addr,
        "UpdateSubscriptions",
        &admin,
        serde_json::json!({
            "room": "standup",
            "identity": "bob",
            "trackSids": [track.sid],
            "subscribe": true,
        }),
    )
    .await;
    assert_eq!(taken_back.status, 200);
}

// The compatibility alias

/// The room the alias calls below name, so that a scoped `roomAdmin` token can
/// be minted for it.
const ALIAS_ROOM: &str = "standup";

/// A token carrying exactly one grant, scoped where the grant is scoped.
fn token_for(grant: Grant, room: &str) -> String {
    match grant {
        Grant::Create => scoped_creator(room),
        Grant::List => lister(),
        Grant::Admin => admin(room),
    }
}

/// A request body carrying exactly the fields the table says the method
/// insists on, so the call gets past argument checking and as far as the parts
/// the two paths share.
fn required_body(method: &rooms::Method) -> serde_json::Value {
    let mut body = serde_json::Map::new();

    for field in method.requires {
        let value = match *field {
            "name" | "room" => ALIAS_ROOM,
            "identity" => "alice",
            "trackSid" => "TR_nothing",
            other => panic!(
                "rooms::METHODS says `{}` requires `{other}`, and this test does not know what a \
                 plausible value for it looks like -- teach it here rather than dropping the field",
                method.name
            ),
        };
        body.insert((*field).to_owned(), serde_json::json!(value));
    }

    serde_json::Value::Object(body)
}

/// The top-level fields of a successful answer, which is as much of the body
/// as two independent servers can be expected to agree on: room ids and
/// creation times are theirs alone.
fn shape(answer: &Answer) -> Vec<String> {
    if answer.status != 200 {
        return Vec::new();
    }

    let mut fields = answer
        .json()
        .as_object()
        .map(|body| body.keys().cloned().collect::<Vec<String>>())
        .unwrap_or_default();
    fields.sort();
    fields
}

/// The alias is the same service under an older name, not a second one.
///
/// Every method in the table, under every grant a caller might hold and under
/// none at all, asked of both paths. The two servers are driven through the
/// same sequence of calls, so their registries are in the same state at every
/// step and any difference in an answer is the routing and nothing else.
///
/// This is the half of the API a paying integration depends on: LiveKit's
/// server SDKs build `/twirp/livekit.RoomService/<Method>` in generated code,
/// so a divergence here is a backoffice that stops working against a server
/// whose own tests all pass.
#[tokio::test]
async fn the_alias_answers_every_method_exactly_as_the_canonical_path_does() {
    let canonical = spawn_server().await;
    let aliased = spawn_server().await;

    for method in rooms::METHODS {
        let body = required_body(method).to_string();

        for holder in [
            None,
            Some(Grant::Create),
            Some(Grant::List),
            Some(Grant::Admin),
        ] {
            let token = holder.map(|grant| token_for(grant, ALIAS_ROOM));
            let held = holder.map_or("no token", Grant::claim);

            let straight = call_at(
                canonical,
                rooms::PATH_PREFIX,
                "POST",
                method.name,
                "application/json",
                token.as_deref(),
                body.as_bytes(),
            )
            .await;
            let through_alias = call_at(
                aliased,
                rooms::ALIAS_PREFIX,
                "POST",
                method.name,
                "application/json",
                token.as_deref(),
                body.as_bytes(),
            )
            .await;

            assert_eq!(
                straight.status,
                through_alias.status,
                "`{}` with {held} answered {} on `{}` and {} on `{}`: an SDK reads the status \
                 before it reads the body, so the two paths disagree about whether the call even \
                 failed",
                method.name,
                straight.status,
                rooms::SERVICE,
                through_alias.status,
                rooms::ALIAS_SERVICE
            );
            assert_ne!(
                through_alias.code(),
                "bad_route",
                "`{}` is in rooms::METHODS and `{}` does not route it, so a LiveKit SDK calling \
                 a method this server implements is told the method does not exist",
                method.name,
                rooms::ALIAS_SERVICE
            );
            assert_eq!(
                straight.code(),
                through_alias.code(),
                "`{}` with {held} came back as `{}` on `{}` and `{}` on `{}`, and a caller turns \
                 that code into a typed error",
                method.name,
                straight.code(),
                rooms::SERVICE,
                through_alias.code(),
                rooms::ALIAS_SERVICE
            );
            assert_eq!(
                straight.message(),
                through_alias.message(),
                "`{}` with {held} explained itself differently on the two paths, so whichever \
                 name a caller uses decides what they are told went wrong",
                method.name
            );
            assert_eq!(
                shape(&straight),
                shape(&through_alias),
                "`{}` with {held} answered with different fields on the two paths, and a \
                 generated client is built out of exactly those fields",
                method.name
            );
        }
    }
}

/// A grant is checked once, so the alias cannot be the loose door.
#[tokio::test]
async fn the_alias_refuses_a_token_the_canonical_path_would_refuse() {
    let addr = spawn_server().await;

    for method in rooms::METHODS {
        let Support::Served(wanted) = method.support else {
            continue;
        };
        let body = required_body(method).to_string();

        for grant in [Grant::Create, Grant::List, Grant::Admin] {
            if grant == wanted {
                continue;
            }

            let answered = call_at(
                addr,
                rooms::ALIAS_PREFIX,
                "POST",
                method.name,
                "application/json",
                Some(&token_for(grant, ALIAS_ROOM)),
                body.as_bytes(),
            )
            .await;

            assert!(
                !answered.body.is_empty(),
                "`{}` is not routed under `{}` at all: the alias is gone, and a LiveKit SDK \
                 pointed here gets a bare 404 with nothing in it to read",
                method.name,
                rooms::ALIAS_SERVICE
            );
            assert_eq!(
                answered.code(),
                "permission_denied",
                "`{}` wants `{}` and a `{}` token got through the alias: the old path would be a \
                 way around the grants, and it is the path every LiveKit SDK uses",
                method.name,
                wanted.claim(),
                grant.claim()
            );
        }

        if wanted.scoped() {
            let elsewhere = call_at(
                addr,
                rooms::ALIAS_PREFIX,
                "POST",
                method.name,
                "application/json",
                Some(&token_for(wanted, "payroll")),
                body.as_bytes(),
            )
            .await;

            assert_eq!(
                elsewhere.code(),
                "permission_denied",
                "`{}` is scoped by the token's `room` claim and a token for `payroll` moderated \
                 `{ALIAS_ROOM}` through the alias, which makes every admin token a master key",
                method.name
            );
        }
    }
}

/// A `bad_route` names the service the caller used, not the other one.
#[tokio::test]
async fn a_bad_route_names_the_service_the_caller_posted_to() {
    let addr = spawn_server().await;

    let straight = post_json(addr, "DestroyEverything", &creator(), serde_json::json!({})).await;
    let aliased = call_at(
        addr,
        rooms::ALIAS_PREFIX,
        "POST",
        "DestroyEverything",
        "application/json",
        Some(&creator()),
        b"{}".as_slice(),
    )
    .await;

    assert_eq!(straight.status, aliased.status);
    assert_eq!(straight.code(), "bad_route");
    assert_eq!(aliased.code(), "bad_route");

    assert!(
        straight.message().contains(rooms::SERVICE)
            && !straight.message().contains(rooms::ALIAS_SERVICE),
        "a call to `{}` was refused in the name of the other service: {}",
        rooms::SERVICE,
        straight.message()
    );
    assert!(
        aliased.message().contains(rooms::ALIAS_SERVICE)
            && !aliased.message().contains(rooms::SERVICE),
        "somebody who posted to `{}` is told about `{}`, and goes looking for a path they never \
         used while their typo stays invisible: {}",
        rooms::ALIAS_SERVICE,
        rooms::SERVICE,
        aliased.message()
    );
}

/// What turning the alias off actually costs.
#[tokio::test]
async fn the_alias_can_be_retired_and_takes_the_livekit_sdks_with_it() {
    let addr = spawn_server_with(|config| config.twirp.livekit_alias = false).await;

    let straight = post_json(addr, "ListRooms", &lister(), serde_json::json!({})).await;
    assert_eq!(
        straight.status, 200,
        "retiring the old name must not disturb the service's own path"
    );

    let aliased = call_at(
        addr,
        rooms::ALIAS_PREFIX,
        "POST",
        "ListRooms",
        "application/json",
        Some(&lister()),
        b"{}".as_slice(),
    )
    .await;

    assert_eq!(aliased.status, 404);
    assert!(
        aliased.body.is_empty(),
        "the route is gone rather than refusing, so there is no Twirp handler left to give the \
         caller a code -- an SDK sees a bare 404 and reports a transport failure rather than a \
         method that does not exist. That is what turning this off costs, and why it is on."
    );
}
