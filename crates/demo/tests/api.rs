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
    assert_eq!(
        claims.video.can_publish_sources,
        vec!["microphone", "camera", "screen_share", "screen_share_audio"],
        "the demo mints the only token it can publish with, so the grant is the whole permission"
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
