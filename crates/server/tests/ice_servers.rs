//! What a client is told about relays when it joins.
//!
//! The field this exercises used to go out empty on every join, so every client
//! built an `RTCConfiguration` with no ice servers on it and gathered host
//! candidates only -- a coturn running next door was never tried once, and the
//! failure looked exactly like a call that could not find a path.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::mediacore::{self, signal_response};
use mediacore_server::{http, state::AppState};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const TURN_SECRET: &str = "coturn-shared-secret";

/// A relay with a user in its database, and a plain reflexive lookup beside it.
const FIXED_PAIR: &str = r#"
rtc:
  ice_servers:
    - urls:
        - stun:stun.example.test:3478
    - urls:
        - turn:turn.example.test:3478?transport=udp
        - turns:turn.example.test:5349?transport=tcp
      username: mediacore
      credential: relay-password
"#;

/// A coturn started with `use-auth-secret`, which mints per join. The secret is
/// `TURN_SECRET`, written out again because a raw string interpolates nothing
/// and yaml is easier to read whole than escaped.
const SHARED_SECRET: &str = r#"
rtc:
  ice_servers:
    - urls:
        - turn:turn.example.test:3478
      secret: coturn-shared-secret
      ttl_seconds: 3600
"#;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn spawn_server(rtc: &str) -> SocketAddr {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n{rtc}")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();

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

async fn join_room(addr: SocketAddr, identity: &str, room: &str) -> mediacore::JoinResponse {
    let url = format!(
        "ws://{addr}/rtc?access_token={}&protocol=17&sdk=test",
        token(identity, room)
    );
    let (mut socket, _): (Client, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    loop {
        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("the join answer did not arrive in time")
            .expect("socket closed")
            .expect("socket error");

        if let Message::Binary(data) = message {
            let response = mediacore::SignalResponse::decode(data.as_ref()).unwrap();
            match response.message {
                Some(signal_response::Message::Join(join)) => return join,
                Some(other) => panic!("expected a join response, got {other:?}"),
                None => continue,
            }
        }
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn a_client_is_told_about_the_relays_this_deployment_has() {
    let addr = spawn_server(FIXED_PAIR).await;

    let join = join_room(addr, "alice", "demo").await;

    assert_eq!(
        join.ice_servers.len(),
        2,
        "a client that is handed nothing gathers host candidates only, and a relay next door is \
         never tried"
    );

    assert_eq!(
        join.ice_servers[0].urls,
        vec!["stun:stun.example.test:3478"]
    );
    assert!(
        join.ice_servers[0].username.is_empty() && join.ice_servers[0].credential.is_empty(),
        "stun has nothing to authenticate"
    );

    let relay = &join.ice_servers[1];
    assert_eq!(
        relay.urls,
        vec![
            "turn:turn.example.test:3478?transport=udp",
            "turns:turn.example.test:5349?transport=tcp"
        ],
        "one relay reachable two ways has to stay one entry, in the order it was written: a \
         client tries them from the top"
    );
    assert_eq!(relay.username, "mediacore");
    assert_eq!(relay.credential, "relay-password");
}

/// The shared-secret path, which is the one a deployment of any size uses. The
/// credential is checked the way coturn checks it -- recomputed from the secret
/// over the whole username it was handed -- rather than compared against
/// whatever the server happened to produce.
#[tokio::test]
async fn a_relay_holding_a_secret_gets_a_credential_minted_for_the_person_joining() {
    let addr = spawn_server(SHARED_SECRET).await;

    let asked_at = now_seconds();
    let join = join_room(addr, "alice", "demo").await;

    let relay = join
        .ice_servers
        .first()
        .expect("the relay this deployment configured");

    let (expiry, identity) = relay
        .username
        .split_once(':')
        .expect("coturn reads the expiry out of the username, so there has to be one");
    let expiry: u64 = expiry.parse().expect("the expiry is a unix time");

    assert!(
        expiry > asked_at,
        "a credential that has already expired when it reaches the client is a relay that \
         refuses every allocation: minted for {expiry}, asked for at {asked_at}"
    );
    assert!(
        expiry <= asked_at + 3_600 + 5,
        "the credential outlives the ttl this deployment asked for: {expiry} against {asked_at}"
    );
    assert_eq!(
        identity, "alice",
        "the identity is what makes a relay's own logs and per-user limits mean anything"
    );

    assert_eq!(
        relay.credential,
        mediacore_auth::turn::credential_for(TURN_SECRET, &relay.username),
        "coturn recomputes the credential from the secret over the username it was handed, and \
         refuses the allocation if it does not match"
    );

    let bob = join_room(addr, "bob", "demo").await;
    assert_ne!(
        bob.ice_servers[0].username, relay.username,
        "one credential for everybody costs the relay any way of telling who is using it"
    );
}

/// Nothing is invented for a deployment that configured no relay: an empty list
/// is the answer, and a client then reaches this server directly exactly as it
/// did before any of this existed.
#[tokio::test]
async fn a_deployment_with_no_relay_is_answered_with_an_empty_list() {
    let addr = spawn_server("").await;

    let join = join_room(addr, "alice", "demo").await;

    assert!(join.ice_servers.is_empty());
}
