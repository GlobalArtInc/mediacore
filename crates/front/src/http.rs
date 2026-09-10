use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mediacore_auth::{AuthError, Claims, VideoGrant};
use serde::{Deserialize, Serialize};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth;
use crate::config::Config;
use crate::log::quoted;
use crate::name::{NameError, RoomError, clean_name, identity_for, room_for};
use crate::session::Session;
use crate::state::{Front, Shared};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
const MEDIA_JS: &str = include_str!("../assets/media.js");
const BENCH_JS: &str = include_str!("../assets/bench.js");
const READOUT_JS: &str = include_str!("../assets/readout.js");
const STAGE_JS: &str = include_str!("../assets/stage.js");
const ROOM_JS: &str = include_str!("../assets/room.js");
const CHAT_JS: &str = include_str!("../assets/chat.js");
const PASS_JS: &str = include_str!("../assets/pass.js");
const PREFS_JS: &str = include_str!("../assets/prefs.js");
const VOICE_JS: &str = include_str!("../assets/voice.js");
const LEVEL_METER_JS: &str = include_str!("../assets/level-meter.js");
const DENOISE_JS: &str = include_str!("../assets/denoise.js");
const DENOISE_GATE_JS: &str = include_str!("../assets/denoise-gate.js");
const DENOISE_PROBE_JS: &str = include_str!("../assets/denoise-probe.js");
const NOTICE_TXT: &str = include_str!("../assets/vendor/NOTICE.txt");
const SDK_JS: &str = include_str!("../assets/vendor/livekit-client.esm.js");
const RNNOISE_WORKLET_JS: &str = include_str!("../assets/vendor/rnnoise-worklet.js");
const RNNOISE_WASM: &[u8] = include_bytes!("../assets/vendor/rnnoise.wasm");
const RNNOISE_SIMD_WASM: &[u8] = include_bytes!("../assets/vendor/rnnoise-simd.wasm");
const TOKEN_BODY_LIMIT: usize = 4 * 1024;

/// The room's own URL. `/r/standup` is the address of a room: it is what a
/// person pastes to a colleague, what the browser reloads into, and what an
/// identity provider will one day be told to come back to. It carries the room
/// and nothing else -- no token, no ticket, no name -- so it is safe to paste
/// anywhere a link goes.
pub const ROOM_PATH_PREFIX: &str = "/r/";

pub const PUBLISH_SOURCES: [&str; 4] =
    ["microphone", "camera", "screen_share", "screen_share_audio"];

pub const SDK_VERSION: &str = "2.22.2";
pub const SDK_PATH: &str = "/vendor/livekit-client-2.22.2.esm.js";

/// `@sapphi-red/web-noise-suppressor`, the AudioWorklet wrapper this binary
/// serves, around `@shiguredo/rnnoise-wasm`'s emscripten build of
/// `xiph/rnnoise`. Every route below carries the version, so a browser holding
/// a cached copy of an older build can never be handed it again after an
/// upgrade. All three projects are named in `assets/vendor/NOTICE.txt`.
pub const RNNOISE_VERSION: &str = "0.4.0";
pub const RNNOISE_WASM_PATH: &str = "/vendor/rnnoise-0.4.0.wasm";
pub const RNNOISE_SIMD_WASM_PATH: &str = "/vendor/rnnoise-simd-0.4.0.wasm";
pub const RNNOISE_WORKLET_PATH: &str = "/vendor/rnnoise-worklet-0.4.0.js";
pub const DENOISE_GATE_PATH: &str = "/denoise-gate.js";
pub const DENOISE_PROBE_PATH: &str = "/denoise-probe.js";

/// The level meter's worklet. It is served like any other module because the
/// page also imports it on the main thread for the processor's name, and
/// `addModule` fetches it from the same place.
pub const LEVEL_METER_PATH: &str = "/level-meter.js";

/// Apache-2.0 and MIT both require the notice to travel with what they cover.
/// It is compiled into this binary like the assets it covers, so a container
/// carrying the binary carries it, and the page links to it.
pub const NOTICE_PATH: &str = "/NOTICE.txt";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

/// Anything that says who is signed in. A `no-cache` response may still be
/// written to disk and revalidated; `no-store` is the one that says do not keep
/// a copy at all, which is what a display name and a join token both want.
const NO_STORE: &str = "no-store";

pub fn router(config: Config) -> Router {
    let csp = content_security_policy(&config.ws_url);
    let front = Front::new(config);
    // The two sign-in routes exist only when there is a sign-in. With the
    // switch off they are not registered at all, so `/auth/callback` is a 404
    // rather than an endpoint answering questions about a feature this
    // deployment does not have.
    let sign_in = match front.requires_sign_in() {
        true => Router::new()
            .route(auth::CALLBACK_PATH, get(auth::callback))
            .route(auth::LOGOUT_PATH, get(auth::logout)),
        false => Router::new(),
    };

    Router::new()
        .route("/", get(index))
        // Every room name serves the same page, including one that cannot be a
        // room. The page reads the name out of its own URL and asks
        // `/api/config` about it; a 404 here would answer a mistyped link with
        // a dead end instead of a lobby with the reason in it and the name
        // still there to be fixed.
        .route(&format!("{ROOM_PATH_PREFIX}{{room}}"), get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/media.js", get(media))
        .route("/bench.js", get(bench))
        .route("/readout.js", get(readout))
        .route("/stage.js", get(stage))
        .route("/room.js", get(room))
        .route("/chat.js", get(chat))
        .route("/pass.js", get(pass))
        .route("/prefs.js", get(prefs))
        .route("/voice.js", get(voice))
        .route(LEVEL_METER_PATH, get(level_meter))
        .route("/denoise.js", get(denoise))
        .route(DENOISE_GATE_PATH, get(denoise_gate))
        .route(DENOISE_PROBE_PATH, get(denoise_probe))
        .route(NOTICE_PATH, get(notice))
        .route(SDK_PATH, get(sdk))
        .route(RNNOISE_WORKLET_PATH, get(rnnoise_worklet))
        .route(RNNOISE_WASM_PATH, get(rnnoise_wasm))
        .route(RNNOISE_SIMD_WASM_PATH, get(rnnoise_simd_wasm))
        .route("/healthz", get(healthz))
        .route("/api/config", get(public_config))
        .route(
            "/api/token",
            post(token).layer(DefaultBodyLimit::max(TOKEN_BODY_LIMIT)),
        )
        .merge(sign_in)
        .with_state(Arc::new(front))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
}

fn signalling_origins(ws_url: &str) -> String {
    match ws_url.split_once("://") {
        Some(("ws", host)) => format!("ws://{host} http://{host}"),
        Some(("wss", host)) => format!("wss://{host} https://{host}"),
        _ => ws_url.to_owned(),
    }
}

fn content_security_policy(ws_url: &str) -> HeaderValue {
    let origins = signalling_origins(ws_url);
    // `wasm-unsafe-eval` is what lets the page compile the denoiser it is
    // served. Without it Chrome refuses `WebAssembly.instantiate` outright and
    // the feature is dead on arrival. It permits WebAssembly compilation and
    // nothing else -- it is not `unsafe-eval`, and the sources a script may
    // still come from are unchanged: this origin, and no other.
    let policy = format!(
        "default-src 'none'; \
         script-src 'self' 'wasm-unsafe-eval'; \
         style-src 'self'; \
         img-src 'self' data:; \
         media-src 'self' blob:; \
         connect-src 'self' {origins}; \
         base-uri 'none'; \
         form-action 'none'; \
         frame-ancestors 'none'"
    );

    HeaderValue::from_str(&policy)
        .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'"))
}

/// The page, or the way to the provider.
///
/// With the switch off this is what it always was. With it on, a load with no
/// session is answered by sending the browser to sign in, and the room this
/// load was addressed to is remembered on this side of the wire so that the
/// callback can put the person back exactly where they were going.
///
/// Only the page is behind this. `/app.js`, `/app.css` and the vendored client
/// stay open, because they carry nothing about anybody and a login page that
/// cannot load a stylesheet is a login page nobody can read; and `/api/config`
/// stays open because it is what the page asks before it can show anything at
/// all. The two things worth protecting are who is signed in, which only a
/// session says, and a join token, which only `/api/token` mints.
async fn index(
    State(front): State<Shared>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if front.requires_sign_in() && auth::signed_in(&front, &headers).is_none() {
        return auth::start_sign_in(&front, &method, &headers, &uri).await;
    }

    asset("text/html; charset=utf-8", NO_CACHE, INDEX_HTML).into_response()
}

async fn stylesheet() -> impl IntoResponse {
    asset("text/css; charset=utf-8", NO_CACHE, APP_CSS)
}

async fn script() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, APP_JS)
}

async fn media() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, MEDIA_JS)
}

async fn bench() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, BENCH_JS)
}

async fn readout() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, READOUT_JS)
}

async fn stage() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, STAGE_JS)
}

async fn room() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, ROOM_JS)
}

async fn chat() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, CHAT_JS)
}

async fn pass() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, PASS_JS)
}

async fn prefs() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, PREFS_JS)
}

async fn voice() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, VOICE_JS)
}

async fn level_meter() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, LEVEL_METER_JS)
}

async fn denoise() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, DENOISE_JS)
}

async fn denoise_gate() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, DENOISE_GATE_JS)
}

async fn denoise_probe() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, DENOISE_PROBE_JS)
}

async fn notice() -> impl IntoResponse {
    asset("text/plain; charset=utf-8", NO_CACHE, NOTICE_TXT)
}

async fn sdk() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", IMMUTABLE, SDK_JS)
}

async fn rnnoise_worklet() -> impl IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        IMMUTABLE,
        RNNOISE_WORKLET_JS,
    )
}

async fn rnnoise_wasm() -> impl IntoResponse {
    binary("application/wasm", IMMUTABLE, RNNOISE_WASM)
}

async fn rnnoise_simd_wasm() -> impl IntoResponse {
    binary("application/wasm", IMMUTABLE, RNNOISE_SIMD_WASM)
}

async fn healthz() -> &'static str {
    "ok"
}

fn asset(
    content_type: &'static str,
    cache_control: &'static str,
    body: &'static str,
) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache_control),
        ],
        body,
    )
}

fn binary(
    content_type: &'static str,
    cache_control: &'static str,
    body: &'static [u8],
) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache_control),
        ],
        body,
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicConfig {
    room: String,
    ws_url: String,
    max_name_length: usize,
    max_room_length: usize,
    room_namespace_separator: char,
    room_error: Option<String>,
    /// Whether this deployment signs people in. It is what the lobby reads to
    /// decide whether to ask for a name at all: with sign-in on, the name is
    /// not the browser's to give.
    sign_in: bool,
    /// Who the *server* says this browser is, or null. It is the display name
    /// and nothing else -- not the subject, not the roles, not the token --
    /// because the page has one use for it, which is to show the person which
    /// account they are about to join as.
    signed_in_as: Option<String>,
}

/// The room the page found in its own URL, handed back for judgement.
///
/// It is optional because `/` names no room at all. It is here rather than in
/// a validation endpoint of its own because the page already fetches this one
/// before it enables the form, so the answer costs no extra round trip and
/// arrives in time to be shown in the lobby instead of after a join.
#[derive(Debug, Deserialize)]
struct ConfigQuery {
    #[serde(default)]
    room: Option<String>,
}

/// What the lobby needs before anybody presses anything: the room to offer,
/// the socket the browser will open, the two lengths the fields bind
/// themselves to, and -- when the URL named a room -- whether that name can be
/// a room at all.
///
/// `roomError` is the reason in the same words the token endpoint would use,
/// because it comes from the same `room_for`. The page owns no second notion
/// of a valid room name: it asks, and prints the answer.
async fn public_config(
    State(front): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<ConfigQuery>,
) -> impl IntoResponse {
    let config = &front.config;
    let room_error = query
        .room
        .as_deref()
        .and_then(|requested| room_for(&config.room, requested).err())
        .map(|error| error.to_string());

    let body = Json(PublicConfig {
        room: config.room.clone(),
        ws_url: config.ws_url.clone(),
        max_name_length: crate::name::MAX_NAME_CHARS,
        max_room_length: crate::name::MAX_ROOM_CHARS,
        room_namespace_separator: crate::name::ROOM_NAMESPACE_SEPARATOR,
        room_error,
        sign_in: front.requires_sign_in(),
        signed_in_as: auth::signed_in(&front, &headers).map(|session| session.display_name),
    });

    // `no-store`, because this one now says who is at the keyboard. A shared
    // cache holding it would hand the previous visitor's name to the next.
    ([(header::CACHE_CONTROL, NO_STORE)], body)
}

#[derive(Debug, Deserialize)]
struct TokenRequest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    room: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    url: String,
    token: String,
    room: String,
    identity: String,
    name: String,
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("the request body must be json with a `name` field")]
    Body,
    #[error("this sign-in has ended. Reload the page to sign in again.")]
    NotSignedIn,
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Room(#[from] RoomError),
    #[error("the token could not be issued")]
    Issue(#[from] AuthError),
    #[error("the server clock is before the unix epoch")]
    Clock,
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Body | Self::Name(_) | Self::Room(_) => StatusCode::BAD_REQUEST,
            Self::NotSignedIn => StatusCode::UNAUTHORIZED,
            Self::Issue(_) | Self::Clock => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %quoted(&self), "token request failed");
        }

        let body = ApiErrorBody {
            error: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

/// Where this demo puts the identity its own sign-in decided.
///
/// mediacore has no field for it and should not: an account is a thing a
/// platform built on this server has and the server itself does not. So it
/// travels as an ordinary participant attribute, which is exactly what an
/// embedding platform is expected to do with its own identifiers, and it ends
/// up on the attendance row of the meeting record with nothing having read it
/// on the way.
const SUBJECT_ATTRIBUTE: &str = "subject";

/// Mints the join token, from whoever this browser has turned out to be.
///
/// With the switch off this is what it always was: an unauthenticated endpoint
/// that takes a name on trust, because there are no accounts and the link to a
/// room is the whole of the invitation.
///
/// With it on the body stops being able to say who anybody is. The name and the
/// identity both come out of the session -- **the `name` field is read and
/// discarded** -- because a browser that can still name itself here is a
/// browser that can join as somebody else, and that is the entire point of
/// having signed anybody in. The room is the one thing the body is still
/// believed about, and it is namespaced exactly as before.
async fn token(
    State(front): State<Shared>,
    headers: HeaderMap,
    body: Result<Json<TokenRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let config = &front.config;
    let session = match front.requires_sign_in() {
        true => Some(auth::signed_in(&front, &headers).ok_or(ApiError::NotSignedIn)?),
        false => None,
    };

    let Json(request) = body.map_err(|_| ApiError::Body)?;

    let (name, identity) = match &session {
        Some(session) => whoever_signed_in(session),
        None => {
            let name = clean_name(&request.name)?;
            let identity = identity_for(&name);

            (name, identity)
        }
    };
    let room = match request.room.as_deref() {
        Some(requested) => room_for(&config.room, requested)?,
        None => config.room.clone(),
    };

    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::Clock)?
        + config.token_ttl;

    let claims = Claims {
        exp: exp.as_secs(),
        iss: config.api_key.clone(),
        sub: identity.clone(),
        name: name.clone(),
        // What this demo has that mediacore does not: an opinion about who
        // somebody is. The provider's subject is this person wherever they go,
        // as opposed to the identity, which is only what one room calls them,
        // and it goes into the attributes because that is where a platform
        // built on mediacore puts its own identifiers -- the core keeps them on
        // the meeting record and never reads them. A guest who typed a name
        // into the box has no such subject and sends none.
        attributes: session
            .as_ref()
            .map(|session| HashMap::from([(SUBJECT_ATTRIBUTE.to_owned(), session.subject.clone())]))
            .unwrap_or_default(),
        video: VideoGrant {
            room_join: Some(true),
            room: room.clone(),
            can_publish: Some(true),
            can_subscribe: Some(true),
            can_publish_data: Some(true),
            // What lets somebody tell the room they have stopped listening.
            // The page writes one attribute and reads everybody else's; the
            // server carries them without reading any.
            can_update_own_metadata: Some(true),
            can_publish_sources: PUBLISH_SOURCES.iter().map(|s| (*s).to_owned()).collect(),
            ..VideoGrant::default()
        },
        ..Claims::default()
    };

    let token = mediacore_auth::issue(&config.api_secret, &claims)?;
    // The identity is the provider's `sub` with the switch on, and `clean_subject`
    // has already refused a control character in it. Quoted anyway: it is the one
    // field on this line that another system chose, and a log line's shape should
    // not rest on a check made in a different file. `room` is this crate's own
    // composition over a charset of letters, digits, `-`, `_` and `.`.
    tracing::info!(identity = %quoted(&identity), %room, "issued a token");

    let body = Json(TokenResponse {
        url: config.ws_url.clone(),
        token,
        room,
        identity,
        name,
    });

    // A join token is a bearer credential with six hours in it. Nothing between
    // here and the browser is supposed to keep a copy -- and while no cache
    // stores a `POST` response unasked, the header is what says so rather than
    // leaving it to a rule somebody has to know.
    Ok(([(header::CACHE_CONTROL, NO_STORE)], body))
}

/// The name and the identity of somebody signed in.
///
/// The identity is the provider's `sub` and nothing appended to it. That is a
/// decision with a visible consequence: a LiveKit identity is unique within a
/// room, so one person joining the same room twice replaces their own first
/// participant rather than sitting beside it. That is what "one person, one
/// seat" means, and it is the right answer for a call -- the alternative is
/// somebody's abandoned tab in the roster with a live microphone in it.
///
/// The anonymous path keeps its per-visit suffix, because there `Alice` is a
/// string two people can both type.
fn whoever_signed_in(session: &Session) -> (String, String) {
    (session.display_name.clone(), session.subject.clone())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
