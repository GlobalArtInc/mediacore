use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use mediacore_demo::config::{Config, TOKEN_TTL};
use mediacore_demo::http;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "demo";
const WS_URL: &str = "wss://media.example.test";

fn config() -> Config {
    Config {
        api_key: API_KEY.to_owned(),
        api_secret: API_SECRET.to_owned(),
        ws_url: WS_URL.to_owned(),
        room: ROOM.to_owned(),
        port: 0,
        token_ttl: TOKEN_TTL,
    }
}

fn keys() -> HashMap<String, String> {
    HashMap::from([(API_KEY.to_owned(), API_SECRET.to_owned())])
}

async fn spawn() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(config());

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    addr
}

async fn request(addr: SocketAddr, head: &str, body: &str) -> (u16, String) {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "{head} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
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

async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    request(addr, &format!("GET {path}"), "").await
}

async fn head_of(addr: SocketAddr, path: &str) -> String {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        socket.write_all(request.as_bytes()).await.expect("request");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        String::from_utf8_lossy(&raw).into_owned()
    };

    let response = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("http response did not arrive in time");

    response
        .split_once("\r\n\r\n")
        .expect("http response without a body")
        .0
        .to_owned()
}

async fn mint(addr: SocketAddr, body: &str) -> (u16, serde_json::Value) {
    let (status, raw) = request(addr, "POST /api/token", body).await;
    let value = serde_json::from_str(&raw).unwrap_or_else(|_| panic!("json response, got {raw}"));
    (status, value)
}

#[tokio::test]
async fn the_page_is_served() {
    let addr = spawn().await;

    let (status, body) = get(addr, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("<title>mediacore demo</title>"));
    assert!(body.contains(r#"id="join-form""#));
    assert!(body.contains(r#"src="/app.js""#));

    let (status, css) = get(addr, "/app.css").await;
    assert_eq!(status, 200);
    assert!(css.contains(".participants"));

    let (status, js) = get(addr, "/app.js").await;
    assert_eq!(status, 200);
    assert!(js.contains(http::SDK_PATH));
}

#[tokio::test]
async fn the_page_never_sees_the_secret() {
    let addr = spawn().await;

    for path in ["/", "/app.css", "/app.js", "/api/config"] {
        let (_, body) = get(addr, path).await;
        assert!(
            !body.contains(API_SECRET),
            "{path} leaked the api secret to the browser"
        );
    }

    let (_, token) = mint(addr, r#"{"name":"Alice"}"#).await;
    assert!(!token.to_string().contains(API_SECRET));
}

#[tokio::test]
async fn public_config_carries_the_room_and_socket_url() {
    let addr = spawn().await;
    let (status, body) = get(addr, "/api/config").await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(status, 200);
    assert_eq!(value["room"], ROOM);
    assert_eq!(value["wsUrl"], WS_URL);
    assert_eq!(
        value["roomNamespaceSeparator"], ".",
        "the lobby cannot name the room a `?room=` link actually reaches"
    );
}

#[tokio::test]
async fn a_token_is_issued_that_the_server_accepts() {
    let addr = spawn().await;
    let (status, body) = mint(addr, r#"{"name":"  Alice  "}"#).await;
    assert_eq!(status, 200);

    assert_eq!(body["room"], ROOM);
    assert_eq!(body["url"], WS_URL);
    assert_eq!(body["name"], "Alice");

    let token = body["token"].as_str().expect("a token string");
    let claims = mediacore_auth::verify_join(token, &keys()).expect("the sfu accepts the token");

    assert_eq!(claims.identity(), body["identity"].as_str().unwrap());
    assert_eq!(claims.display_name(), "Alice");
    assert_eq!(claims.video.room, ROOM);
    assert!(claims.video.room_join());
    assert!(claims.video.can_publish());
    assert!(claims.video.can_subscribe());
    assert_eq!(claims.video.can_publish_sources, vec!["microphone"]);
    assert!(claims.iss == API_KEY);
}

#[tokio::test]
async fn a_requested_room_is_namespaced_under_the_configured_one() {
    let addr = spawn().await;
    let (status, body) = mint(addr, r#"{"name":"Alice","room":"standup"}"#).await;
    assert_eq!(status, 200);

    let token = body["token"].as_str().unwrap();
    let claims = mediacore_auth::verify_join(token, &keys()).unwrap();
    assert_eq!(claims.video.room, "demo.standup");
    assert_eq!(body["room"], "demo.standup");
}

#[tokio::test]
async fn a_caller_cannot_name_a_room_outside_the_demo_namespace() {
    let addr = spawn().await;

    for requested in ["board", "demo", "..", "ops.private", "-", "_"] {
        let (status, body) = mint(
            addr,
            &format!(r#"{{"name":"Mallory","room":"{requested}"}}"#),
        )
        .await;
        assert_eq!(status, 200, "`{requested}` was refused outright");

        let claims = mediacore_auth::verify_join(body["token"].as_str().unwrap(), &keys()).unwrap();
        assert_eq!(
            claims.video.room,
            format!("{ROOM}.{requested}"),
            "`{requested}` reached a room the demo does not own"
        );
        assert!(claims.video.room.starts_with(&format!("{ROOM}.")));
        assert_ne!(claims.video.room, requested);
    }
}

#[tokio::test]
async fn a_room_that_would_not_fit_the_namespace_is_refused() {
    let addr = spawn().await;
    let requested = "a".repeat(64);
    let (status, body) = mint(addr, &format!(r#"{{"name":"Alice","room":"{requested}"}}"#)).await;

    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("at most"));
}

#[tokio::test]
async fn the_client_sdk_is_served_from_this_origin() {
    let addr = spawn().await;

    let (status, js) = get(addr, "/app.js").await;
    assert_eq!(status, 200);
    assert!(
        !js.contains("cdn.jsdelivr.net"),
        "the page still fetches the sdk from a third-party cdn"
    );

    let (status, sdk) = get(addr, http::SDK_PATH).await;
    assert_eq!(status, 200, "the vendored sdk is not served");
    assert!(sdk.contains("class Room"));
    assert!(sdk.contains("ConnectionState"));

    let head = head_of(addr, http::SDK_PATH).await.to_lowercase();
    assert!(head.contains("content-type: text/javascript"));
    assert!(head.contains("immutable"));
}

#[tokio::test]
async fn every_response_carries_a_content_security_policy() {
    let addr = spawn().await;

    for path in ["/", "/app.js", "/app.css", "/api/config"] {
        let head = head_of(addr, path).await.to_lowercase();
        assert!(
            head.contains("content-security-policy: default-src 'none';"),
            "{path} was served without a policy"
        );
        assert!(
            head.contains(&format!("connect-src 'self' {}", WS_URL.to_lowercase())),
            "{path} does not allow the signalling socket"
        );
        assert!(
            head.contains("script-src 'self';"),
            "{path} would still allow a third-party script"
        );
        assert!(head.contains("x-content-type-options: nosniff"));
    }
}

#[tokio::test]
async fn the_page_reports_the_states_a_dropped_socket_produces() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        js.contains("ConnectionState.SignalReconnecting"),
        "a dropped signalling socket would leave the page claiming the call is live"
    );
    assert!(js.contains("cannot resume"));
}

#[tokio::test]
async fn the_page_arms_the_audio_context_inside_the_click() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    let join = js
        .split_once("async function onJoin(")
        .expect("onJoin is defined")
        .1;
    let gesture = join.split_once("await").expect("onJoin awaits something").0;

    assert!(
        gesture.contains("ensureAudioContext()"),
        "the audio context is only created after an await, outside the user gesture"
    );
}

#[tokio::test]
async fn the_join_form_starts_disabled() {
    let addr = spawn().await;

    let (_, page) = get(addr, "/").await;
    assert!(
        page.contains(r#"<button id="join" type="submit" disabled>"#),
        "the join button accepts a click before the sdk has loaded"
    );

    let (_, js) = get(addr, "/app.js").await;
    assert!(
        js.contains("function ready()"),
        "nothing re-enables the form once the sdk is loaded"
    );
}

#[tokio::test]
async fn colliding_display_names_are_disambiguated() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        js.contains("function labelsFor("),
        "two people called Alice would be indistinguishable in the participant list"
    );
    assert!(js.contains("shortIdentity("));
}

#[tokio::test]
async fn an_unusable_room_name_is_refused() {
    let addr = spawn().await;
    let (status, body) = mint(addr, r#"{"name":"Alice","room":"not a room"}"#).await;

    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("room name"));
}

#[tokio::test]
async fn an_empty_name_is_refused() {
    let addr = spawn().await;

    for body in [r#"{"name":""}"#, r#"{"name":"   "}"#, "{}"] {
        let (status, response) = mint(addr, body).await;
        assert_eq!(status, 400, "{body} was accepted");
        assert_eq!(response["error"], "a name is required");
        assert!(response["token"].is_null());
    }
}

#[tokio::test]
async fn an_oversized_name_is_refused() {
    let addr = spawn().await;
    let name = "a".repeat(41);
    let (status, body) = mint(addr, &format!(r#"{{"name":"{name}"}}"#)).await;

    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("at most 40"));
}

#[tokio::test]
async fn a_malformed_body_is_refused() {
    let addr = spawn().await;
    let (status, body) = mint(addr, "not json").await;

    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("json"));
}

#[tokio::test]
async fn two_people_with_one_name_get_two_identities() {
    let addr = spawn().await;

    let (_, first) = mint(addr, r#"{"name":"Alice"}"#).await;
    let (_, second) = mint(addr, r#"{"name":"Alice"}"#).await;

    let first_identity = first["identity"].as_str().unwrap();
    let second_identity = second["identity"].as_str().unwrap();

    assert_ne!(first_identity, second_identity);
    assert!(first_identity.starts_with("alice-"));
    assert!(second_identity.starts_with("alice-"));
    assert_eq!(first["name"], second["name"]);

    let first_claims =
        mediacore_auth::verify_join(first["token"].as_str().unwrap(), &keys()).unwrap();
    let second_claims =
        mediacore_auth::verify_join(second["token"].as_str().unwrap(), &keys()).unwrap();
    assert_ne!(first_claims.identity(), second_claims.identity());
    assert_eq!(first_claims.display_name(), second_claims.display_name());
}
