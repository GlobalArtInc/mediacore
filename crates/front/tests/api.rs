use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use mediacore_front::config::{Config, TOKEN_TTL};
use mediacore_front::http;
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
        oidc: None,
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

/// The head of a `POST`, for the one thing about a minted token that is not in
/// its body.
async fn post_head(addr: SocketAddr, path: &str, body: &str) -> String {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
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
    assert!(body.contains("<title>mediacore</title>"));
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
        "the lobby cannot name the room a `/r/` link actually reaches"
    );
    assert_eq!(
        value["maxRoomLength"], 64,
        "the room field cannot bind itself to the length this server accepts"
    );
    assert!(
        value["roomError"].is_null(),
        "a page opened at `/` names no room, so there is nothing to refuse"
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
    assert_eq!(
        claims.video.can_publish_sources,
        vec!["microphone", "camera", "screen_share", "screen_share_audio"],
        "the front end mints the only token it can publish with, so the grant is the whole \
         permission"
    );
    assert!(
        claims
            .video
            .can_publish_sources
            .contains(&"screen_share".to_owned()),
        "the server would refuse the screen share the page offers"
    );
    assert!(
        !claims
            .video
            .can_publish_sources
            .iter()
            .any(|source| source == "data" || source == "unknown"),
        "the grant names a source nothing on the page publishes"
    );
    assert!(claims.iss == API_KEY);
}

/// A join token is a bearer credential with six hours in it, and the response
/// that carries it says so. No cache stores a `POST` response unasked, so this
/// is a header nothing is currently relying on -- which is the reason to set it
/// now rather than the reason to leave it out: the rule that keeps it safe
/// lives in somebody else's software, and a proxy in front is somebody else's
/// software.
#[tokio::test]
async fn a_minted_token_is_never_something_to_keep_a_copy_of() {
    let addr = spawn().await;
    let head = post_head(addr, "/api/token", r#"{"name":"Alice"}"#)
        .await
        .to_lowercase();

    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(
        head.contains("cache-control: no-store"),
        "the response carrying a join token does not say not to keep a copy of it: {head}"
    );
}

#[tokio::test]
async fn the_capture_module_is_served_as_its_own_module() {
    let addr = spawn().await;

    let (status, media) = get(addr, "/media.js").await;
    assert_eq!(status, 200, "/media.js is not served");
    assert!(
        media.contains("export function createMedia("),
        "the interface has nothing to import"
    );

    let head = head_of(addr, "/media.js").await.to_lowercase();
    assert!(
        head.contains("content-type: text/javascript"),
        "/media.js is not served as a module: {head}"
    );
    assert!(
        head.contains("content-security-policy: default-src 'none';"),
        "/media.js was served without a policy"
    );
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

/// The endpoint mints for anybody who asks, so what it can address is the
/// whole of its blast radius. Two rooms are inside it: the configured one, and
/// a name under it. Nothing a caller sends reaches a third.
#[tokio::test]
async fn a_caller_cannot_name_a_room_outside_the_configured_namespace() {
    let addr = spawn().await;

    for requested in ["board", "demo", "..", "ops.private", "-", "_", "Demo"] {
        let (status, body) = mint(
            addr,
            &format!(r#"{{"name":"Mallory","room":"{requested}"}}"#),
        )
        .await;
        assert_eq!(status, 200, "`{requested}` was refused outright");

        let claims = mediacore_auth::verify_join(body["token"].as_str().unwrap(), &keys()).unwrap();
        let room = &claims.video.room;
        assert!(
            room == ROOM || room.starts_with(&format!("{ROOM}.")),
            "`{requested}` reached `{room}`, a room this front end does not own"
        );
        assert_eq!(
            body["room"], *room,
            "the page is told a room it was not given"
        );
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

/// The complaint this answers, in the owner's words: typed a room name,
/// pressed F5, and ended up back on the front page. A room only survives a
/// reload if it has a URL, and a URL only survives one if the server answers
/// at it.
#[tokio::test]
async fn a_room_has_a_url_of_its_own() {
    let addr = spawn().await;

    for path in ["/r/standup", "/r/demo", "/r/a.b-c_d", "/r/not%20a%20room"] {
        let (status, page) = get(addr, path).await;
        assert_eq!(
            status, 200,
            "{path} is not served, so reloading a room lands on nothing"
        );
        assert!(
            page.contains(r#"id="join-form""#),
            "{path} answered with something that is not the lobby"
        );
        assert!(
            page.contains(r#"src="/app.js""#) && page.contains(r#"href="/app.css""#),
            "{path} serves a page whose assets are addressed relative to it, so one directory \
             down they would 404"
        );
    }

    let (_, js) = get(addr, "/app.js").await;
    assert!(
        js.contains("addressedRoom(location.pathname, location.search)"),
        "nothing reads the room out of the url the page was opened at"
    );

    let (_, room) = get(addr, "/room.js").await;
    assert!(
        room.contains(&format!(
            r#"export const ROOM_PATH_PREFIX = "{}";"#,
            http::ROOM_PATH_PREFIX
        )),
        "the page writes room urls under a prefix this server does not answer at, so every link \
         it hands out would 404"
    );
}

/// A room's URL is the thing somebody pastes into a chat window, so what it
/// carries is what they hand over. It carries the room. The token is minted
/// per load, lives in memory for the length of the connection, and is written
/// neither into the address bar nor into storage.
#[tokio::test]
async fn the_rooms_url_carries_the_room_and_nothing_else() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, room) = get(addr, "/room.js").await;

    let path = body_of(&room, "export function roomPath(room, search) {");
    for private in ["token", "identity", "name", "prefs"] {
        assert!(
            !path.contains(private),
            "the url a room is addressed by is built out of `{private}`: {path}"
        );
    }

    assert_eq!(
        js.matches("history.replaceState(").count(),
        2,
        "the page writes the address bar somewhere other than the two places that know what a \
         room's url is: the correction of a legacy link, and entering a room"
    );
    assert!(
        js.contains(r#"history.replaceState(null, "", arrival.canonical);"#)
            && js.contains(r#"history.replaceState(null, "", path);"#),
        "a url is being assembled at the call site rather than by `roomPath`"
    );
    assert!(
        js.contains("const path = roomPath(room, location.search);"),
        "the address the page writes is not the room's own url"
    );
    assert!(
        !js.contains("history.pushState("),
        "the room is pushed onto the history stack, so Back leaves the url naming the lobby \
         while the page is still in the room and nothing here listens for `popstate`"
    );
}

/// The lobby asks two questions now. The room is one of them, it is bound to
/// the same length the server enforces, and it is dead until the client has
/// loaded for the same reason the name field is.
#[tokio::test]
async fn the_lobby_asks_which_room() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        page.contains(r#"<label for="join-room">Room</label>"#),
        "the room field has no label, so nothing says what the box is for"
    );

    let field = page
        .split_once(r#"id="join-room""#)
        .expect("the lobby has a room field")
        .1
        .split_once("/>")
        .expect("the room field is closed")
        .0;
    assert!(
        field.contains("disabled"),
        "the room field takes a room before the client that would join it has loaded"
    );
    assert!(
        field.contains(r#"maxlength="64""#),
        "the room field is not bound to the length this server accepts: {field}"
    );

    assert!(
        js.contains("mintToken(name, el.joinRoom.value)"),
        "joining ignores the room in the field"
    );
    assert!(
        js.contains("offeredRoom(arrival.room, prefs.room, settings.room)"),
        "the room the lobby offers is no longer the link first, then what this browser last \
         joined, then the server's own"
    );
    assert!(
        js.contains("el.joinRoom.maxLength = settings.maxRoomLength;"),
        "a room out of storage or out of a url could sit in the field at a length the server \
         would refuse"
    );
}

/// A link that was mangled on the way -- a space, a slash, a name too long --
/// is answered in the lobby with the reason, and the name it named is left in
/// the field where it can be corrected. Not a 404: somebody who was sent a
/// broken link cannot edit the link, and a dead end tells them nothing.
#[tokio::test]
async fn a_room_name_that_cannot_be_one_is_refused_in_the_lobby() {
    let addr = spawn().await;

    let (status, body) = get(addr, "/api/config?room=not%20a%20room").await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        value["roomError"], "a room name may only contain letters, digits, `-`, `_` and `.`",
        "the lobby is not told why the room in the url cannot be one"
    );

    let long = "a".repeat(64);
    let (_, body) = get(addr, &format!("/api/config?room={long}")).await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        value["roomError"]
            .as_str()
            .expect("a reason")
            .contains("at most"),
        "a name that only becomes too long once it is composed is accepted by the lobby and \
         refused after the microphone has been opened"
    );

    for good in ["standup", "demo", "a.b-c_d"] {
        let (_, body) = get(addr, &format!("/api/config?room={good}")).await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            value["roomError"].is_null(),
            "`{good}` is a room this server would mint for and the lobby refuses it"
        );
    }

    let (_, body) = get(addr, "/api/config").await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        value["roomError"].is_null(),
        "arriving at `/` with no room named is not a mistake"
    );

    let (_, js) = get(addr, "/app.js").await;
    assert!(
        js.contains("loadSettings(arrival.room)") && js.contains("if (settings.roomError) {"),
        "the page never asks whether the room in its url can be one, or never says so"
    );
    assert!(
        body_of(&js, "function onRoomTyped() {").contains("hide(el.lobbyError)"),
        "a refusal of what the link named outlives the correction of it, so the lobby goes on \
         arguing with a name nobody is asking for while somebody types the one they want"
    );
}

/// The round trip, from both ends. `/` and `/r/demo` are the same room, or
/// pressing F5 on the room everybody lands in would quietly move them into
/// `demo.demo`; and the same address twice is the same room with a new token
/// rather than a token that was kept.
#[tokio::test]
async fn reloading_a_rooms_url_comes_back_to_the_same_room() {
    let addr = spawn().await;

    let (_, bare) = mint(addr, r#"{"name":"Alice"}"#).await;
    let (_, named) = mint(addr, &format!(r#"{{"name":"Alice","room":"{ROOM}"}}"#)).await;
    assert_eq!(
        bare["room"], named["room"],
        "`/` and `/r/{ROOM}` are two different rooms, so reloading the page everybody lands on \
         moves them out of it"
    );

    let (_, first) = mint(addr, r#"{"name":"Alice","room":"standup"}"#).await;
    let (_, again) = mint(addr, r#"{"name":"Alice","room":"standup"}"#).await;
    assert_eq!(first["room"], "demo.standup");
    assert_eq!(
        again["room"], first["room"],
        "the same url reached a different room the second time"
    );
    assert_ne!(
        first["token"], again["token"],
        "a token was reused across loads instead of minted per load"
    );

    let (_, js) = get(addr, "/app.js").await;
    let enter = body_of(&js, "function enterRoom(credentials) {");
    assert!(
        enter.contains("plainRoom(") && enter.contains("credentials.room,"),
        "the room the page names is not the room the token came back for: {enter}"
    );
    assert!(
        enter.contains("showAddress(address);"),
        "the address bar is left naming whatever the tab was opened at"
    );
}

/// Four tabs, four rooms, and a tab strip that squeezes each of them to two
/// centimetres. The room goes first in the title, where that much is still
/// readable, and the roster header carries the name a person says rather than
/// the composed one the SFU knows -- the footer already carries that, beside
/// the socket and the identity, for whoever is reading `ListRooms` at the same
/// time.
#[tokio::test]
async fn a_tab_says_which_room_it_is() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, room) = get(addr, "/room.js").await;

    assert!(
        page.contains("<title>mediacore</title>"),
        "the page ships with a title that is not the product's name"
    );
    assert!(
        room.contains("`${room.trim()} · ${TITLE}`"),
        "the room is not the first thing in the tab's name, so four of them are four identical \
         tabs"
    );
    assert!(
        js.contains("document.title = roomTitle(address);"),
        "joining a room does not name the tab after it"
    );
    assert!(
        js.contains("el.roomName.textContent = address;"),
        "the roster header no longer carries the room this tab is in"
    );
    assert!(
        js.contains(
            "el.footUrl.textContent = `${credentials.url} - ${credentials.room} - ${credentials.identity}`;"
        ),
        "the composed room the sfu knows is nowhere on the page, so nothing connects what is on \
         screen to what `ListRooms` returns"
    );
}

#[tokio::test]
async fn the_address_module_is_served_as_its_own_module() {
    let addr = spawn().await;

    let (status, room) = get(addr, "/room.js").await;
    assert_eq!(status, 200, "/room.js is not served");
    assert!(
        room.contains("export function addressedRoom(")
            && room.contains("export function roomPath("),
        "the interface has nothing to import"
    );

    let head = head_of(addr, "/room.js").await.to_lowercase();
    assert!(
        head.contains("content-type: text/javascript"),
        "/room.js is not served as a module: {head}"
    );
    assert!(
        head.contains("content-security-policy: default-src 'none';"),
        "/room.js was served without a policy"
    );
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
            head.contains("script-src 'self' 'wasm-unsafe-eval';"),
            "{path} would still allow a third-party script, or would refuse the denoiser"
        );
        assert!(
            !head.contains("'unsafe-eval';") && !head.contains("'unsafe-inline'"),
            "{path} bought WebAssembly with a general eval: {head}"
        );
        assert!(head.contains("x-content-type-options: nosniff"));
    }
}

#[tokio::test]
async fn the_denoiser_is_served_from_this_origin_with_its_version_in_the_path() {
    let addr = spawn().await;

    for path in [http::RNNOISE_WASM_PATH, http::RNNOISE_SIMD_WASM_PATH] {
        assert!(
            path.contains(http::RNNOISE_VERSION),
            "{path} does not name the build it serves, so an upgrade could be answered from cache"
        );

        let head = head_of(addr, path).await.to_lowercase();
        assert!(
            head.starts_with("http/1.1 200"),
            "{path} is not served: {head}"
        );
        assert!(
            head.contains("content-type: application/wasm"),
            "{path} is not served as WebAssembly, and no browser will compile it: {head}"
        );
        assert!(
            head.contains("immutable"),
            "{path} is not cacheable, so every join would fetch it again: {head}"
        );
    }

    let head = head_of(addr, http::RNNOISE_WORKLET_PATH)
        .await
        .to_lowercase();
    assert!(head.starts_with("http/1.1 200"));
    assert!(head.contains("content-type: text/javascript"));
    assert!(head.contains("immutable"));

    let (status, worklet) = get(addr, http::RNNOISE_WORKLET_PATH).await;
    assert_eq!(status, 200);
    assert!(
        worklet.contains("registerProcessor"),
        "the vendored worklet registers no processor, so the node could never be constructed"
    );
    assert!(
        worklet.contains("@sapphi-red/web-noise-suppressor/rnnoise"),
        "the vendored worklet does not register the name denoise.js constructs"
    );
    assert!(
        !worklet.contains("sourceMappingURL"),
        "the worklet points at a source map this server does not serve"
    );
}

#[tokio::test]
async fn the_probe_worklet_is_served_and_registers_both_ends_of_the_measurement() {
    let addr = spawn().await;

    let (status, probe) = get(addr, http::DENOISE_PROBE_PATH).await;
    assert_eq!(status, 200, "{} is not served", http::DENOISE_PROBE_PATH);

    for processor in ["mediacore/denoise-tap", "mediacore/denoise-probe"] {
        assert!(
            probe.contains(processor),
            "the probe module does not register {processor}, so one end of the measurement is \
             missing and the cost cannot be attributed to the denoiser"
        );
    }

    let (status, gate) = get(addr, http::DENOISE_GATE_PATH).await;
    assert_eq!(status, 200, "{} is not served", http::DENOISE_GATE_PATH);
    assert!(
        gate.contains("@sapphi-red/web-noise-suppressor/rnnoise"),
        "the gate does not name the processor it wraps, so nothing would be gated and the node \
         would publish silence until its wasm compiled"
    );

    for path in [http::DENOISE_PROBE_PATH, http::DENOISE_GATE_PATH] {
        let head = head_of(addr, path).await.to_lowercase();
        assert!(
            head.contains("content-type: text/javascript"),
            "addModule refuses anything that is not javascript: {head}"
        );
    }
}

/// The notice has to be reachable from a running deployment, not merely present
/// in the repository: that is what Apache-2.0 4(d) and the MIT permission
/// notice ask for.
#[tokio::test]
async fn the_licence_notice_is_served_and_the_page_links_to_it() {
    let addr = spawn().await;

    let (status, notice) = get(addr, http::NOTICE_PATH).await;
    assert_eq!(status, 200, "{} is not served", http::NOTICE_PATH);

    for project in [
        "livekit-client",
        "@sapphi-red/web-noise-suppressor",
        "@shiguredo/rnnoise-wasm",
        "xiph/rnnoise",
    ] {
        assert!(
            notice.contains(project),
            "{project} is served from this origin and the notice does not name it"
        );
    }
    assert!(
        notice.contains("Apache License") && notice.contains("MIT License"),
        "the notice names projects whose licences it does not reproduce"
    );

    let head = head_of(addr, http::NOTICE_PATH).await.to_lowercase();
    assert!(
        head.contains("content-type: text/plain"),
        "the notice would be downloaded rather than read: {head}"
    );

    let (_, index) = get(addr, "/").await;
    assert!(
        index.contains(http::NOTICE_PATH),
        "nothing on the page links to the notice, so nobody running this ever sees it"
    );
}

#[tokio::test]
async fn the_page_never_reaches_a_third_party_for_the_denoiser() {
    let addr = spawn().await;

    for path in [
        "/app.js",
        "/denoise.js",
        http::DENOISE_GATE_PATH,
        http::DENOISE_PROBE_PATH,
    ] {
        let (status, module) = get(addr, path).await;
        assert_eq!(status, 200);
        for host in ["cdn.jsdelivr.net", "unpkg.com", "cdn.mezon.ai", "https://"] {
            assert!(
                !module.contains(host),
                "{path} names {host}; nothing in a running deployment may leave this origin"
            );
        }
    }

    let (_, app) = get(addr, "/app.js").await;
    for path in [
        http::RNNOISE_WASM_PATH,
        http::RNNOISE_SIMD_WASM_PATH,
        http::RNNOISE_WORKLET_PATH,
        http::DENOISE_GATE_PATH,
        http::DENOISE_PROBE_PATH,
    ] {
        assert!(
            app.contains(path),
            "the page does not load {path} from this server"
        );
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

const README: &str = include_str!("../README.md");

fn quoted_after(haystack: &str, marker: &str) -> Vec<String> {
    haystack
        .match_indices(marker)
        .filter_map(|(index, found)| {
            let rest = &haystack[index + found.len()..];
            rest.split_once('"').map(|(value, _)| value.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn every_module_the_page_imports_is_served() {
    let addr = spawn().await;
    let (_, app) = get(addr, "/app.js").await;
    let (_, readout) = get(addr, "/readout.js").await;

    let mut imports = quoted_after(&app, "from \"");
    imports.extend(quoted_after(&readout, "from \""));
    assert!(
        imports.contains(&"/bench.js".to_owned()) && imports.contains(&"/readout.js".to_owned()),
        "the page no longer imports the bench modules: {imports:?}"
    );

    for path in imports {
        let (status, body) = get(addr, &path).await;
        assert_eq!(status, 200, "{path} is imported but not served");
        assert!(!body.is_empty(), "{path} is served empty");

        let head = head_of(addr, &path).await.to_lowercase();
        assert!(
            head.contains("content-type: text/javascript"),
            "{path} is not served as a module: {head}"
        );
    }
}

#[tokio::test]
async fn the_page_carries_a_node_for_every_element_the_script_looks_up() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    let ids = quoted_after(&js, "getElementById(\"");
    assert!(ids.len() > 10, "the element table was not found: {ids:?}");

    for id in ids {
        assert!(
            page.contains(&format!("id=\"{id}\"")),
            "app.js looks up `{id}`, which the page does not contain"
        );
    }
}

#[tokio::test]
async fn the_camera_is_off_until_it_is_asked_for() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        page.contains(r#"<input id="with-video" name="with-video" type="checkbox" disabled />"#),
        "the camera checkbox is missing, or ships already ticked"
    );
    assert!(
        js.contains(r#"query.get("video") === "1""#),
        "nothing turns the camera on from the query string"
    );
    assert!(
        js.contains("const withVideo = el.withVideo.checked;"),
        "joining does not consult the camera checkbox"
    );

    let connect = js
        .split_once("async function connect(")
        .expect("connect is defined")
        .1
        .split_once("\nasync function")
        .expect("connect is followed by another function")
        .0;
    assert!(
        !connect.contains("createLocalVideoTrack"),
        "joining opens the camera whether or not it was asked for"
    );
}

#[tokio::test]
async fn the_camera_is_published_with_three_simulcast_rungs() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    for rung in ["h180", "h360", "h720"] {
        assert!(
            js.contains(rung),
            "the capture ladder does not mention {rung}"
        );
    }
    assert!(js.contains("simulcast: wantsSimulcast"));
    assert!(
        js.contains(r#"videoCodec: "vp8""#),
        "the page would offer a codec the sfu does not forward"
    );
    assert!(
        js.contains("adaptiveStream: false"),
        "with adaptive streaming on, the client picks layers and the bench measures the client"
    );
}

#[tokio::test]
async fn the_bench_reads_the_browsers_own_numbers() {
    let addr = spawn().await;
    let (_, bench) = get(addr, "/bench.js").await;

    for field in [
        "availableIncomingBitrate",
        "availableOutgoingBitrate",
        "bytesReceived",
        "packetsLost",
        "frameHeight",
        "framesPerSecond",
    ] {
        assert!(bench.contains(field), "the bench never reads {field}");
    }
    assert!(
        bench.contains("streamState"),
        "the page cannot show a track the server paused for bandwidth"
    );
}

#[tokio::test]
async fn the_bench_measures_video_apart_from_audio() {
    let addr = spawn().await;
    let (_, bench) = get(addr, "/bench.js").await;
    let (_, app) = get(addr, "/app.js").await;
    let (_, page) = get(addr, "/").await;

    assert!(
        bench.contains(r#"entry.kind !== kind"#),
        "the bench totals every inbound stream together, so audio hides video that stopped"
    );
    assert!(
        bench.contains("link.video ="),
        "the bench never measures the video rate the whole milestone is about"
    );
    assert!(
        bench.contains("NO_INCOMING_ESTIMATE"),
        "a browser that reports no incoming estimate is indistinguishable from a broken one"
    );
    assert!(
        app.contains("el.video") && page.contains(r#"id="link-video""#),
        "the video rate is measured but never shown"
    );
}

#[tokio::test]
async fn a_number_the_browser_did_not_report_is_not_shown_as_a_zero() {
    let addr = spawn().await;
    let (_, bench) = get(addr, "/bench.js").await;
    let (_, readout) = get(addr, "/readout.js").await;

    for (path, module) in [("/bench.js", &bench), ("/readout.js", &readout)] {
        assert!(
            !module.contains("|| 0"),
            "{path} substitutes a zero for a value the browser did not report"
        );
    }

    assert!(
        readout.contains(r#"node.dataset.missing = "true""#),
        "a missing value is not marked as missing"
    );
    assert!(
        readout.contains("node.title = reason"),
        "a missing value does not say why it is missing"
    );
}

#[tokio::test]
async fn the_readme_says_how_to_constrain_the_link() {
    for needle in ["dnctl", "pfctl", "tc qdisc", "netem", "7882"] {
        assert!(
            README.contains(needle),
            "the bench instructions never mention {needle}"
        );
    }
}

#[tokio::test]
async fn the_element_table_names_each_node_once() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    let table = js
        .split_once("const el = {")
        .expect("the element table is defined")
        .1
        .split_once("\n};")
        .expect("the element table is closed")
        .0;

    let mut keys: Vec<&str> = table
        .lines()
        .filter_map(|line| line.trim().split_once(": document.getElementById"))
        .map(|(key, _)| key)
        .collect();

    let named = keys.len();
    keys.sort_unstable();
    keys.dedup();

    assert!(named > 10, "the element table was not found: {keys:?}");
    assert_eq!(
        named,
        keys.len(),
        "two entries in the element table share a key, so one node silently replaces the other \
         and the control that reads it acts on the wrong element"
    );
}

#[tokio::test]
async fn the_room_is_a_roster_a_stage_and_a_bar() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, css) = get(addr, "/app.css").await;

    for id in [
        "participants",
        "cards",
        "share",
        "share-frame",
        "audio-menu",
    ] {
        assert!(
            page.contains(&format!("id=\"{id}\"")),
            "the room has no `{id}`, so the layout the brief asked for is not there"
        );
    }

    for control in [
        "mute",
        "audio-settings",
        "mic-select",
        "noise",
        "share-screen",
        "leave",
    ] {
        assert!(
            page.contains(&format!("id=\"{control}\"")),
            "the control bar is missing `{control}`"
        );
    }

    let bar = page
        .split_once(r#"<div class="bar""#)
        .expect("the control bar is in the page")
        .1;
    for control in [
        "id=\"mute\"",
        "id=\"noise\"",
        "id=\"share-screen\"",
        "id=\"leave\"",
    ] {
        assert!(
            bar.contains(control),
            "{control} is in the page but not in the control bar"
        );
    }

    assert!(
        css.contains(".roster"),
        "the persistent list of who is present is unstyled"
    );
    assert!(css.contains(".bar"), "the control bar is unstyled");
}

#[tokio::test]
async fn who_is_speaking_is_measured_from_the_audio() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, css) = get(addr, "/app.css").await;

    assert!(
        js.contains("createAnalyser()") && js.contains("getByteTimeDomainData"),
        "the speaking indicator no longer measures the audio itself"
    );
    assert!(
        !js.contains("ActiveSpeakersChanged"),
        "the page waits for a speakers message this server does not send, so the ring would \
         stay dark for ever"
    );
    assert!(
        js.contains(r#"media.on("microphonetrack""#),
        "restarting the capture replaces the MediaStreamTrack, and a meter left on the old one \
         would report silence for ever"
    );
    assert!(
        css.contains(r#"[data-speaking="true"]"#),
        "nothing lights up around whoever is speaking"
    );
}

#[tokio::test]
async fn a_participant_the_page_cannot_measure_does_not_look_fine() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, css) = get(addr, "/app.css").await;

    assert!(
        js.contains("function unknownState("),
        "every participant is reported as either speaking or listening, including the ones \
         nothing on this page can measure"
    );

    for unmeasurable in ["not subscribed", "level unknown", "no microphone"] {
        assert!(
            js.contains(unmeasurable),
            "a participant who is `{unmeasurable}` is not distinguished from a healthy one"
        );
    }

    assert!(
        css.contains(r#"[data-state="unknown"]"#),
        "an unknown participant is styled exactly like a fine one"
    );
}

#[tokio::test]
async fn every_control_states_what_it_will_do_and_what_happened() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    for pair in [
        ("Mute", "Unmute"),
        ("Share screen", "Stop sharing"),
        ("Start camera", "Stop camera"),
    ] {
        assert!(
            js.contains(pair.0) && js.contains(pair.1),
            "`{}` never becomes `{}`, so the button does not say what pressing it will do",
            pair.0,
            pair.1
        );
    }

    assert!(
        page.contains(r#"id="notice""#) && js.contains("function showNotice("),
        "nothing tells the person what just happened"
    );
    assert!(
        js.contains(r#"result.outcome === "cancelled""#),
        "a cancelled screen picker is reported as a failure, or not at all"
    );
    assert!(
        js.contains("fail(el.roomError, result.reason)"),
        "a failed screen share never says what failed"
    );
    assert!(
        js.contains("microphoneMessage") || js.contains("opened.reason"),
        "a refused microphone does not say what to do about it"
    );
}

#[tokio::test]
async fn the_page_says_when_the_browser_reported_nothing() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        !js.contains("|| 0"),
        "app.js substitutes a zero for a value the browser did not report"
    );
    assert!(
        js.contains("reported === null"),
        "a processing flag the browser does not report is painted as though it were off"
    );
    for sentence in [
        "does not report whether it applied this",
        "does not report which device it opened",
    ] {
        assert!(
            js.contains(sentence),
            "the page shows a value instead of saying `{sentence}`"
        );
    }
}

#[tokio::test]
async fn the_capture_module_drives_every_audio_control() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        js.contains(r#"from "/media.js""#) && js.contains("createMedia(lk, {"),
        "the interface does not use the capture module at all"
    );

    for call in [
        "media.openMicrophone()",
        "media.publishMicrophone()",
        "media.toggleMuted()",
        "media.setProcessing(",
        "media.setNoiseEngine(",
        "media.prepareDenoiser()",
        "media.selectMicrophone(",
        "media.startScreenShare(",
        "media.stopScreenShare()",
    ] {
        assert!(js.contains(call), "nothing on the page calls {call}");
    }

    assert!(
        !js.contains("createLocalAudioTrack"),
        "the page opens its own microphone behind the capture module's back, so muting, the \
         device choice and the processing switches would act on different tracks"
    );
    assert!(
        js.contains("function onCycleNoise()") && js.contains("function applyNoiseEngine("),
        "the noise suppression control is not wired to anything"
    );
}

#[tokio::test]
async fn the_bench_view_survives_the_redesign() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    for id in [
        "bench",
        "bench-toggle",
        "tiles",
        "link-estimate",
        "link-arriving",
        "link-video",
        "link-rtt",
        "link-sending",
    ] {
        assert!(
            page.contains(&format!("id=\"{id}\"")),
            "the bench lost `{id}`, and with it the way the bandwidth work is judged"
        );
    }

    assert!(
        js.contains(r#"query.get("bench") === "1""#),
        "the bench cannot be opened from the address bar"
    );
    assert!(
        js.contains("function onBenchToggle()"),
        "the bench cannot be opened from the page"
    );
    assert!(
        js.contains("createBench(room)") && js.contains("tiles.sync("),
        "the bench is in the markup but nothing feeds it"
    );
}

#[tokio::test]
async fn a_video_track_is_never_attached_in_two_places_at_once() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    let render = js
        .split_once("\nfunction render(")
        .expect("render is defined")
        .1
        .split_once("\nfunction ")
        .expect("render is followed by another function")
        .0;

    assert!(
        render.contains("if (showBench) {") && render.contains("tiles.sync("),
        "render no longer chooses between the stage and the bench"
    );
    assert!(
        render.contains("clearCards();") && render.contains("tiles.clear();"),
        "the stage and the bench can hold the same track at once, and whichever detaches first \
         tears the video out of the other"
    );
}

#[tokio::test]
async fn every_control_is_a_real_control() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, css) = get(addr, "/app.css").await;

    assert!(
        !page.contains("onclick="),
        "the page carries an inline handler, which the content security policy blocks anyway"
    );
    assert!(
        !page.contains(r#"tabindex="-1""#),
        "a control was taken out of the tab order"
    );
    assert!(
        !page.contains(r#"role="button""#),
        "something that is not a button is pretending to be one, so it answers a click but not \
         a keypress"
    );
    assert!(
        page.contains(r#"aria-expanded="false""#) && page.contains(r#"aria-pressed="false""#),
        "the toggles do not report their state to a screen reader"
    );
    assert!(
        css.contains(":focus-visible"),
        "a keyboard user cannot see what is focused"
    );
    assert!(
        css.contains("prefers-reduced-motion"),
        "the speaking animation cannot be turned off"
    );
}

#[tokio::test]
async fn every_icon_the_script_swaps_in_exists_in_the_page() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    let icons = quoted_after(&js, "\"#icon-");
    assert!(!icons.is_empty(), "no icon is ever swapped");

    for icon in icons {
        assert!(
            page.contains(&format!("id=\"icon-{icon}\"")),
            "app.js swaps in `#icon-{icon}`, which the page does not define"
        );
    }
}

fn body_of<'a>(js: &'a str, signature: &str) -> &'a str {
    js.split_once(signature)
        .unwrap_or_else(|| panic!("{signature} is defined"))
        .1
        .split_once("\n}\n")
        .unwrap_or_else(|| panic!("{signature} is closed"))
        .0
}

fn rule_of<'a>(css: &'a str, selector: &str) -> &'a str {
    css.split_once(&format!("{selector} {{"))
        .unwrap_or_else(|| panic!("the stylesheet has no rule for {selector}"))
        .1
        .split_once('}')
        .expect("the rule is closed")
        .0
}

#[tokio::test]
async fn the_shared_screen_can_be_taken_fullscreen_by_button_by_double_click_and_left_by_escape() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    let button = page
        .split_once(r#"id="share-fullscreen""#)
        .expect("the shared screen carries no fullscreen control")
        .1
        .split_once('>')
        .expect("the control is closed")
        .0;
    assert!(
        button.contains(r#"type="button""#) && button.contains(r#"aria-pressed="false""#),
        "the fullscreen control is not a real toggle a keyboard and a screen reader can work: \
         {button}"
    );

    let share = page
        .split_once(r#"<section id="share""#)
        .expect("the stage has a share section")
        .1
        .split_once("</section>")
        .expect("the share section is closed")
        .0;
    assert!(
        share.contains(r#"id="share-fullscreen""#),
        "the fullscreen control is somewhere other than on the tile it acts on"
    );

    assert!(
        js.contains(r#"el.shareFullscreen.addEventListener("click", onToggleFullscreen)"#),
        "the fullscreen button does nothing"
    );
    assert!(
        js.contains(r#"el.shareFrame.addEventListener("dblclick", onToggleFullscreen)"#),
        "double-clicking the video does not take it fullscreen"
    );

    let control = body_of(&js, "\nfunction paintFullscreenControl(");
    for state in ["Exit fullscreen", "Fullscreen"] {
        assert!(
            control.contains(state),
            "the control never says `{state}`, so it does not say which state it will put you in"
        );
    }
    assert!(
        control.contains(r#"el.shareFullscreen.setAttribute("aria-pressed", String(active))"#)
            && control.contains(r#""aria-label","#),
        "the control's state never reaches a screen reader: {control}"
    );

    let escape = body_of(&js, "\nfunction onKeydown(");
    assert!(
        escape.contains(r#"event.key !== "Escape""#) && escape.contains("leaveFullscreen()"),
        "Escape does not leave the fullscreen this page drew itself, which no browser will \
         leave for it"
    );
}

#[tokio::test]
async fn the_call_controls_stay_reachable_while_fullscreen() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, css) = get(addr, "/app.css").await;

    assert!(
        js.contains("await el.room.requestFullscreen()"),
        "fullscreen is asked for on something smaller than the room, so muting and leaving go \
         off the screen with everything else"
    );

    let inside = page
        .split_once(r#"id="room""#)
        .expect("the room element is in the page")
        .1;
    let bar = inside
        .find(r#"<div class="bar""#)
        .expect("the control bar is in the page");
    let sink = inside
        .find(r#"id="audio""#)
        .expect("the audio sink follows the room");
    assert!(
        bar < sink,
        "the control bar is outside the element that goes fullscreen"
    );

    for line in css.lines().filter(|line| line.contains("data-fullscreen")) {
        for gone in [".bar", ".dock", "#notice"] {
            assert!(
                !line.contains(gone),
                "fullscreen hides `{gone}`, and leaving the call from inside it is exactly what \
                 somebody will try: {line}"
            );
        }
    }
}

#[tokio::test]
async fn a_browser_that_will_not_go_fullscreen_still_fills_the_window() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, css) = get(addr, "/app.css").await;
    let (status, stage) = get(addr, "/stage.js").await;

    assert_eq!(status, 200, "the stage module is not served");
    assert!(
        stage.contains("fullscreenPlan") && stage.contains("mode: \"fallback\""),
        "nothing decides what to do with a browser that refuses fullscreen"
    );

    let enter = body_of(&js, "\nasync function enterFullscreen(");
    assert!(
        enter.contains("fullscreenPlan(fullscreenSupport())")
            && enter.contains("fillTheWindow(plan.reason)")
            && enter.contains("afterRefusal(error)")
            && enter.contains("afterRequest(document.fullscreenElement === el.room)"),
        "a browser with no Fullscreen API, one that refuses the request, and one that takes the \
         request and stays where it is do not all land on the path that fills the window: {enter}"
    );

    let filled = rule_of(&css, r#"#room[data-fullscreen="fallback"]"#);
    assert!(
        filled.contains("position: fixed") && filled.contains("inset: 0"),
        "the fallback does not actually fill the window: {filled}"
    );
}

#[tokio::test]
async fn what_arrived_is_shown_in_the_corner_of_the_shared_screen() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, css) = get(addr, "/app.css").await;
    let (_, bench) = get(addr, "/bench.js").await;

    assert!(
        page.contains(r#"<p id="share-stats" class="share-stats" hidden></p>"#),
        "the shared screen carries no readout, or ships one with something already in it"
    );

    let corner = rule_of(&css, ".share-stats");
    assert!(
        corner.contains("position: absolute"),
        "the readout is not in a corner of the picture: {corner}"
    );

    let paint = body_of(&js, "\nfunction paintShareStats(");
    assert!(
        paint.contains("shareBadge(reading, formatBitrate)"),
        "the corner does not read the browser's own numbers"
    );
    assert!(
        paint.contains("badge.text === null") && paint.contains("hideShareStats()"),
        "a browser that reported nothing still gets a badge drawn over the picture: {paint}"
    );

    assert!(
        bench.contains("trackIdentifier") && bench.contains("mediaSourceId"),
        "the bench cannot tell one local track's outbound stream from another's, so a screen \
         share's bitrate would move when a camera beside it did"
    );
}

#[tokio::test]
async fn a_share_that_ends_under_a_viewer_does_not_leave_a_dead_rectangle() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    let announce = body_of(&js, "\nfunction announceShareChange(");
    assert!(
        announce.contains("leaveFullscreen()") && announce.contains("showNotice("),
        "a share that ends while somebody is watching it fullscreen leaves them in fullscreen, \
         or leaves without a word: {announce}"
    );
    assert!(
        announce.contains(r#"step.outcome === "switched""#),
        "the viewer is moved to the other share in silence"
    );

    let render = body_of(&js, "\nfunction render(");
    assert!(
        render.contains("announceShareChange(step)"),
        "nothing notices a share appearing or ending"
    );

    let bench = body_of(&js, "\nfunction onBenchToggle(");
    assert!(
        bench.contains("leaveFullscreen()"),
        "opening the bench takes the video off the stage without leaving the fullscreen that \
         was showing it"
    );
}

#[tokio::test]
async fn the_shared_screen_is_never_displaced_by_whoever_is_speaking() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, stage) = get(addr, "/stage.js").await;

    for name in ["screenShares(", "nextShare("] {
        let body = if name == "nextShare(" {
            body_of(&stage, "\nexport function nextShare(")
        } else {
            body_of(&js, "\nfunction screenShares(")
        };
        assert!(
            !body.contains("speaking"),
            "what the stage shows depends on who is talking, which is the one thing sharing a \
             screen exists to override: {name}"
        );
    }

    let sample = body_of(&js, "\nfunction sample(");
    assert!(
        !sample.contains("Share"),
        "the speaking meter reaches into the stage: {sample}"
    );

    let render = body_of(&js, "\nfunction render(");
    assert!(
        render.contains("nextShare(pinnedShare, shares,"),
        "the stage no longer keeps the share it was holding"
    );
}

#[tokio::test]
async fn a_link_that_dropped_is_not_reported_as_somebody_ending_their_share() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, stage) = get(addr, "/stage.js").await;
    let (_, css) = get(addr, "/app.css").await;

    let render = body_of(&js, "\nfunction render(");
    assert!(
        render.contains("link: linkState()"),
        "the stage decides a share has ended without knowing whether the room is still there, \
         so a reconnect empties the participant list and the page blames whoever was sharing"
    );

    let link = body_of(&js, "\nfunction linkState(");
    for state in ["Connected", "Disconnected"] {
        assert!(
            link.contains(state),
            "a reconnecting room is not told apart from one that ended: {link}"
        );
    }

    let next = body_of(&stage, "\nexport function nextShare(");
    assert!(
        next.contains("SHARE_GRACE_MS") && next.contains(r#"outcome: "waiting""#)
            || next.contains(r#"return waitingOn("#),
        "an empty share list is concluded from at once, so a publisher swapping a window for a \
         screen throws every viewer out of fullscreen: {next}"
    );
    assert!(
        next.contains("entry.identity") && next.contains("held.identity"),
        "shares are told apart only by track, so the same person starting a second capture \
         reads as one person stopping and another beginning"
    );

    let announce = body_of(&js, "\nfunction announceShareChange(");
    let ended = announce
        .split_once(r#"step.outcome === "ended""#)
        .expect("nothing handles a room that ended")
        .1;
    assert!(
        !ended.contains("showNotice("),
        "the room ending is announced as a person stopping their share: {ended}"
    );

    for rule in css
        .split('}')
        .filter(|rule| rule.contains("data-fullscreen") && rule.contains("display: none"))
    {
        assert!(
            !rule.contains(".share-live"),
            "fullscreen hides the one line that survives it, which is where who is speaking and \
             whether the link is up are said: {rule}"
        );
    }
    let live = rule_of(&css, ".share-live");
    assert!(
        live.contains("display: flex"),
        "the stage carries no line for who is speaking: {live}"
    );
}

#[tokio::test]
async fn two_screens_shared_at_once_are_a_choice_and_not_a_race() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    let share = page
        .split_once(r#"<section id="share""#)
        .expect("the stage has a share section")
        .1
        .split_once("</section>")
        .expect("the share section is closed")
        .0;
    assert!(
        share.contains(r#"id="share-tabs""#) && share.contains(r#"role="group""#),
        "two people sharing at once are not offered as a choice"
    );

    let tabs = body_of(&js, "\nfunction paintShareTabs(");
    assert!(
        tabs.contains("el.shareTabs.hidden = shares.length < 2"),
        "the chooser is shown for a single share, or hidden for two"
    );
    assert!(
        tabs.contains(r#"button.type = "button""#)
            && tabs.contains(r#"setAttribute("aria-pressed""#),
        "the chooser is not a keyboard-reachable control that says which screen is on the stage"
    );
    assert!(
        tabs.contains("key: entry.key,") && tabs.contains("label: shareOwner(entry),"),
        "choosing a screen does not pin it, so the next render can move the stage under the \
         person who chose"
    );
}

#[tokio::test]
async fn nothing_of_the_fullscreen_view_reaches_a_room_with_no_share() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        page.contains(r#"<section id="share" class="share" aria-label="Shared screen" hidden>"#),
        "the share section no longer starts hidden, so an audio-only room shows an empty stage"
    );
    assert!(
        page.contains(r#"<div id="room" class="shell" data-fullscreen="off" hidden>"#),
        "the room does not start out of fullscreen"
    );

    let toggle = body_of(&js, "\nfunction onToggleFullscreen(");
    assert!(
        toggle.contains("if (!stageShare) return;"),
        "fullscreen can be entered with nothing on the stage: {toggle}"
    );

    let leave = body_of(&js, "\nasync function leave(");
    assert!(
        leave.contains("leaveFullscreen()"),
        "leaving the room leaves the browser in fullscreen"
    );
}

#[tokio::test]
async fn a_tile_you_can_press_is_a_control_and_not_a_picture_that_answers_a_click() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    let build = body_of(&js, "\nfunction buildCard(");
    assert!(
        build.contains(r#"document.createElement("button")"#)
            && build.contains(r#"press.type = "button""#)
            && build.contains(r#"press.addEventListener("click""#),
        "the tile answers a click without being a button, so a keyboard reaches every control on \
         this page except the one that chooses what fills the stage: {build}"
    );
    assert!(
        !build.contains("node.addEventListener"),
        "the press is bound to the list item rather than to the button inside it, so Enter and \
         Space do nothing at all: {build}"
    );

    let paint = body_of(&js, "\nfunction paintCard(");
    assert!(
        paint.contains(r#"card.press.setAttribute("aria-pressed", String(middle))"#),
        "a tile never says whether it is the one in the middle: {paint}"
    );
    for name in ["Exit focus on ${who}", "Focus ${who}"] {
        assert!(
            paint.contains(name),
            "a tile never reads `{name}`, so the one already in the middle is a control that \
             cannot say what pressing it will do"
        );
    }

    assert!(
        page.contains(r#"id="share-focus""#),
        "the shared screen has no control of its own, so the one tile on the stage that answers \
         a double click rather than a click cannot be chosen at all"
    );

    let focus = body_of(&js, "\nfunction paintFocusControl(");
    for name in [
        "Exit focus and show everybody again",
        "Focus the shared screen",
    ] {
        assert!(
            focus.contains(name),
            "the shared screen's control never reads `{name}`"
        );
    }
    assert!(
        focus.contains(r#"el.shareFocus.setAttribute("aria-pressed", String(held))"#),
        "the shared screen's control never reports its state to a screen reader: {focus}"
    );
}

#[tokio::test]
async fn the_middle_fills_the_stage_and_the_strip_is_the_only_thing_that_scrolls() {
    let addr = spawn().await;
    let (_, css) = get(addr, "/app.css").await;

    let stage = rule_of(&css, r#".stage[data-layout="focus"]"#);
    assert!(
        stage.contains("grid-template-columns: minmax(0, 1fr) 15rem"),
        "the middle and the strip are not a wide column and a narrow one beside it: {stage}"
    );
    assert!(
        stage.contains("overflow: hidden"),
        "the stage scrolls in this layout, so the tile somebody asked to look at scrolls out of \
         the window and takes the strip with it: {stage}"
    );

    for strip in [
        r#".stage[data-layout="focus"][data-main="person"] .reel"#,
        r#".stage[data-layout="focus"][data-main="share"] .cards"#,
    ] {
        let rule = rule_of(&css, strip);
        assert!(
            rule.contains("overflow-y: auto") && rule.contains("min-height: 0"),
            "{strip} does not scroll on its own, so a room with more people than fit is a room \
             whose last few are simply not there: {rule}"
        );
    }

    let reel = rule_of(&css, "\n.reel");
    assert!(
        reel.contains("display: contents"),
        "the wrapper this one layout needs is a box in the other three as well, so the grid and \
         the share view are laid out by rules that were never written for them: {reel}"
    );
}

#[tokio::test]
async fn a_window_too_narrow_for_a_column_beside_the_picture_puts_the_strip_underneath() {
    let addr = spawn().await;
    let (_, css) = get(addr, "/app.css").await;

    let narrow = css
        .split_once("@media (max-width: 64rem) {")
        .expect("the page no longer adapts to a window too narrow for two columns on the stage")
        .1;
    let focus = narrow
        .split_once(r#".stage[data-layout="focus"] {"#)
        .expect("the focus layout is not adapted for a narrow window")
        .1
        .split_once('}')
        .expect("the rule is closed")
        .0;
    assert!(
        focus.contains("grid-template-columns: minmax(0, 1fr)")
            && focus.contains("grid-template-rows: minmax(0, 1fr) auto"),
        "a phone keeps the strip in a column beside the picture, which leaves neither of them \
         wide enough to be worth looking at: {focus}"
    );
    assert!(
        narrow.contains("overflow-x: auto"),
        "the strip along the bottom does not scroll sideways, so everybody past the third person \
         is off the edge of the screen with no way to reach them"
    );

    for owner in ["body", ".page", ".shell", ".main"] {
        let rule = rule_of(&css, owner);
        assert!(
            !rule.contains("overflow-x"),
            "`{owner}` scrolls sideways, and a page that scrolls sideways on a phone is a page \
             half of whose controls are off the edge of it: {rule}"
        );
    }
}

#[tokio::test]
async fn putting_a_person_in_the_middle_does_not_throw_the_shared_screen_away() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, stage) = get(addr, "/stage.js").await;

    let plan = body_of(&stage, "\nexport function stagePlan(");
    assert!(
        plan.contains(r#"strip.push({ kind: "share", key: null })"#),
        "a screen somebody is sharing has nowhere to go once a person is in the middle, so \
         choosing a face ends the share for the one person who chose it: {plan}"
    );
    assert!(
        !plan.contains("speaking"),
        "what fills the stage depends on who is talking, which is the one thing both a shared \
         screen and a tile somebody pressed exist to override: {plan}"
    );

    let render = body_of(&js, "\nfunction render(");
    assert!(
        render.contains("stagePlan(stageFocus,") && render.contains("stageFocus = plan.focus;"),
        "what fills the stage is decided somewhere `node --test` cannot argue with it, or is \
         worked out and then forgotten between renders: {render}"
    );
    assert!(
        render.contains("layoutStage(plan)") && render.contains("syncCards(entries, plan)"),
        "the plan is worked out and then not laid out: {render}"
    );

    let layout = body_of(&js, "\nfunction layoutStage(");
    assert!(
        !layout.contains("el.share"),
        "the shared screen is moved in the document to get it into the strip, and a video \
         element taken out of the document is a video the browser is entitled to pause: {layout}"
    );
}

#[tokio::test]
async fn the_tile_in_the_middle_is_the_same_tile_that_was_in_the_strip() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;
    let (_, css) = get(addr, "/app.css").await;

    let sync = body_of(&js, "\nfunction syncCards(");
    assert!(
        sync.contains("el.spotlight.replaceChildren(cards.get(middle).node)"),
        "the middle is a second tile built beside the one in the strip, so one camera is \
         attached in two places and whichever detaches first tears the picture out of the \
         other: {sync}"
    );
    assert!(
        sync.contains("standing.focus()"),
        "moving the tile takes it out of the document and the keyboard's focus with it, so \
         pressing a tile sends the next Tab back to the top of the page: {sync}"
    );

    let speaking = rule_of(&css, r#".card-tile[data-speaking="true"]"#);
    assert!(
        speaking.contains("outline-color"),
        "a tile in the strip no longer shows that its person is talking, which is how somebody \
         knows which one is worth pressing: {speaking}"
    );

    let bench = body_of(&js, "\nfunction onBenchToggle(");
    assert!(
        !bench.contains("stageFocus"),
        "opening the bench forgets what somebody put in the middle, so closing it again leaves \
         them looking at a grid they will have to rebuild by hand: {bench}"
    );
}

#[tokio::test]
async fn escape_leaves_whatever_is_on_top_before_whatever_is_underneath_it() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    let escape = body_of(&js, "\nfunction onKeydown(");
    let menu = escape
        .find("el.audioMenu.hidden")
        .expect("Escape no longer closes the audio menu");
    let full = escape
        .find(r#"fullscreen !== "off""#)
        .expect("Escape no longer leaves the fullscreen this page drew itself");
    let middle = escape
        .find("stageFocus")
        .expect("Escape does not leave the middle at all, so a keyboard has no way out of it");

    assert!(
        menu < full && full < middle,
        "Escape reaches past a menu that is standing open, or lets go of the tile in the middle \
         while a shared screen is filling the screen over the top of it: {escape}"
    );
    assert!(
        escape[full..middle].contains("return;"),
        "one press of Escape both leaves fullscreen and lets go of the middle, so somebody \
         coming out of a picture finds the room rearranged behind it as well: {escape}"
    );
}

#[tokio::test]
async fn the_preferences_module_is_served_as_its_own_module() {
    let addr = spawn().await;

    let (status, prefs) = get(addr, "/prefs.js").await;
    assert_eq!(status, 200, "/prefs.js is not served");
    assert!(
        prefs.contains("export function readPrefs(")
            && prefs.contains("export function writePrefs("),
        "the interface has nothing to import"
    );

    let head = head_of(addr, "/prefs.js").await.to_lowercase();
    assert!(
        head.contains("content-type: text/javascript"),
        "/prefs.js is not served as a module: {head}"
    );
    assert!(
        head.contains("content-security-policy: default-src 'none';"),
        "/prefs.js was served without a policy"
    );
}

/// A display name and a device id are preferences about one browser. A token,
/// an identity and the room they were minted for are credentials, they expire,
/// and a copy of one on disk outlives the call it was issued for. The list of
/// what may be written is a single line in `prefs.js` for exactly this reason,
/// and the write side of the page goes through it or through nothing.
#[tokio::test]
async fn nothing_the_server_minted_is_ever_written_to_this_browser() {
    let addr = spawn().await;
    let (_, prefs) = get(addr, "/prefs.js").await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        prefs.contains(
            r#"const FIELDS = ["name", "room", "camera", "noise", "microphone", "processing"];"#
        ),
        "the list of what may be stored has changed; every field on it is written to disk verbatim"
    );

    assert_eq!(
        js.matches("writePrefs(").count(),
        1,
        "the page writes storage from more than one place, so what it stores can no longer be \
         read off one function"
    );
    assert!(
        js.contains("const written = writePrefs(store, prefs);"),
        "the one write is not the one in `remember`"
    );

    for value in ["credentials.token", "credentials.identity", "payload.token"] {
        assert!(
            !js.contains(&format!("remember({{ {value}")),
            "{value} is minted for one call and is being remembered between them"
        );
    }
}

/// One key, read in one place, written in one place. A scattering of keys
/// written from wherever is a storage surface nobody can audit, and this page's
/// is meant to be as easy to read as the settings it holds.
#[tokio::test]
async fn everything_remembered_lives_under_one_versioned_key() {
    let addr = spawn().await;
    let (_, prefs) = get(addr, "/prefs.js").await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        prefs.contains(r#"export const PREFS_KEY = "mediacore.front.v1";"#),
        "the storage key is gone, or is no longer versioned"
    );
    assert_eq!(
        prefs.matches("PREFS_KEY").count(),
        3,
        "prefs.js names its key somewhere other than its declaration, its read and its write"
    );
    assert!(
        !js.contains("localStorage"),
        "app.js reaches for localStorage itself, so a value is being stored outside the one blob \
         and outside the try/catch that keeps a private window from white-screening the page"
    );
    assert!(
        js.contains("readPrefs(store, ON_OFFER)")
            && js.contains(
                "const ON_OFFER = { engines: NOISE_ENGINES, processing: DIRECT_PROCESSING_KEYS };"
            ),
        "the page validates stored values against something other than what the capture module \
         actually offers"
    );
}

/// Every read and every write, including the accessor itself: a browser told to
/// block site data throws on `window.localStorage` before a method is called,
/// and a page that white-screens in a private window is worse than one that
/// forgets.
#[tokio::test]
async fn every_touch_of_storage_is_wrapped() {
    let addr = spawn().await;
    let (_, prefs) = get(addr, "/prefs.js").await;

    for function in [
        "export function storageOf(scope) {",
        "export function readPrefs(store, offer) {",
        "export function writePrefs(store, prefs) {",
    ] {
        let body = body_of(&prefs, function);
        assert!(
            body.contains("try {") && body.contains("catch"),
            "{function} can throw out of the page: {body}"
        );
    }

    assert!(
        body_of(&prefs, "export function storageOf(scope) {").contains(".localStorage"),
        "the accessor is read outside the try, which is the one place a blocked browser throws"
    );
}

/// A remembered choice is not truer than a chosen one. It goes in through the
/// same calls the controls make, so it meets the same probe, the same attach
/// and the same fallbacks -- including a machine that cannot run our denoiser.
#[tokio::test]
async fn a_remembered_choice_is_applied_through_the_controls_own_calls() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    let restore = body_of(&js, "async function restoreChoices() {");
    for call in [
        "media.setNoiseEngine(prefs.noise)",
        "media.setProcessing(prefs.processing)",
    ] {
        assert!(
            restore.contains(call),
            "the restore sets a capture setting behind the capture module's back: {restore}"
        );
    }

    assert!(
        js.contains("media.prepareDenoiser().then(restoreChoices)"),
        "the remembered engine is applied without waiting for the probe, so two of them race for \
         the same wasm and a refusal can land after the choice"
    );

    let microphone = body_of(&js, "async function restoreMicrophone() {");
    assert!(
        microphone.contains("chooseMicrophone(prefs.microphone, devices.list)")
            && microphone.contains("media.selectMicrophone(choice.deviceId)"),
        "a remembered device id is asked for without checking whether the browser can still see \
         it: {microphone}"
    );
    assert!(
        microphone.contains("arrivalNotice = MICROPHONE_GONE"),
        "a microphone that is gone is swapped for another one silently: {microphone}"
    );
    assert!(
        body_of(&js, "function enterRoom(credentials) {").contains("showNotice(arrivalNotice)"),
        "what the page found out before there was a room to say it in is never said"
    );
}

#[tokio::test]
async fn the_lobby_comes_back_the_way_it_was_left() {
    let addr = spawn().await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        js.contains("el.name.value = prefs.name.slice(0, settings.maxNameLength);"),
        "the name is not put back, or is put back at a length this server would refuse"
    );
    assert!(
        js.contains("if (prefs.name && !settings.signIn) {"),
        "a remembered name is put back into a field that is not there, on a deployment where the \
         name is the provider's to give"
    );
    assert!(
        js.contains(r#"query.has("video")"#) && js.contains("prefs.camera === true"),
        "the camera box is not put back, or a link that names the camera no longer outranks it"
    );

    for setting in [
        "remember(settings.signIn ? { camera: withVideo } : { name, camera: withVideo });",
        "remember({ noise: engine });",
        "remember({ microphone: chosen === SYSTEM_DEFAULT ? null : chosen });",
        "remember({ processing: media.state().processing });",
    ] {
        assert!(
            js.contains(setting),
            "`{setting}` is gone, so that setting is set again on every visit"
        );
    }
}

/// Ours is the default, which is only safe because every way out of it still
/// ends on the browser's own suppressor. These are those ways out. A change
/// that makes the default look better by softening one of them is the change
/// this test exists to fail.
#[tokio::test]
async fn our_denoiser_is_the_default_and_every_way_out_of_it_lands_on_the_browsers() {
    let addr = spawn().await;
    let (_, media) = get(addr, "/media.js").await;

    assert!(
        media.contains("export const DEFAULT_NOISE_ENGINE = NOISE_MEDIACORE;"),
        "the page no longer starts on our denoiser"
    );
    assert_eq!(
        media.matches("noiseEngine = NOISE_BROWSER;").count(),
        2,
        "the two places a denoiser that cannot be had gives way to the browser's -- the probe \
         answering no, and the node refusing to attach as the microphone opens -- are no longer \
         both there"
    );
    assert!(
        media.contains(r#"switchNoiseEngine(NOISE_BROWSER, "denoiser-overloaded")"#),
        "a render thread that cannot keep up no longer puts the browser's suppressor back"
    );
    assert!(
        !media.contains("setTimeout") && !media.contains("setInterval"),
        "something in the capture module is on a timer, and the only thing a timer could be for \
         here is putting our denoiser back after a machine gave up on it -- a suppressor that \
         switches on and off mid-sentence is worse than either state"
    );
}

/// With signing in switched on the lobby stops asking who you are, because the
/// answer is no longer the browser's to give. This is that shape, asserted
/// against the markup and the script this binary actually serves -- the
/// behaviour over real sockets is in `tests/oidc.rs`.
#[tokio::test]
async fn the_lobby_asks_for_a_name_only_when_nobody_has_signed_it_in() {
    let addr = spawn().await;
    let (_, page) = get(addr, "/").await;
    let (_, js) = get(addr, "/app.js").await;

    assert!(
        page.contains(r#"<div class="field" id="name-field">"#),
        "the name field cannot be taken out of the lobby in one piece, so a deployment that \
         signs people in would show a label with no box under it"
    );
    assert!(
        js.contains("el.nameField.hidden = true;") && js.contains("el.name.value = \"\";"),
        "the name field stays in the lobby with a session open, and whatever is typed in it is \
         thrown away by the server"
    );

    assert!(
        js.contains("const body = settings.signIn ? { room } : { name, room };"),
        "the page still tells the server who it is when the server already knows, which is a \
         field somebody will one day believe"
    );
    assert!(
        js.contains("if (!settings.signIn && !name) {"),
        "the lobby refuses to join for want of a name it is not asking for"
    );
    assert!(
        js.contains("firstQuestion().focus()"),
        "the cursor is put in a box that is not there"
    );

    assert!(
        page.contains(r#"id="signed-in-name"#),
        "nothing on the page says which account is about to join"
    );
    assert!(
        page.contains(&format!(
            r#"id="sign-out" href="{}""#,
            mediacore_front::auth::LOGOUT_PATH
        )),
        "the sign-out link does not go where this server answers"
    );

    let (_, css) = get(addr, "/app.css").await;
    assert!(
        css.contains("[hidden] {\n  display: none !important;\n}"),
        "`hidden` is set on elements this stylesheet gives a `display` to, and without a rule \
         that outranks the user agent's, setting it does nothing at all"
    );
}

/// Nothing about the page changes when nobody has to sign in, which is the
/// whole of the switch's promise.
#[tokio::test]
async fn a_deployment_that_signs_nobody_in_is_the_page_it_always_was() {
    let addr = spawn().await;

    let (status, page) = get(addr, "/r/standup").await;
    assert_eq!(status, 200);
    assert!(page.contains(r#"id="name""#));

    let (status, body) = get(addr, "/api/config").await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        value["signIn"], false,
        "the lobby is told to hide the name field on a deployment with no sign-in"
    );

    let (status, body) = mint(addr, r#"{"name":"Alice","room":"standup"}"#).await;
    assert_eq!(
        status, 200,
        "the token endpoint asks for a session on a deployment that has none: {body}"
    );
}
