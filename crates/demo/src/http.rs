use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mediacore_auth::{AuthError, Claims, VideoGrant};
use serde::{Deserialize, Serialize};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;
use crate::name::{NameError, RoomError, clean_name, identity_for, namespaced_room};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
const BENCH_JS: &str = include_str!("../assets/bench.js");
const READOUT_JS: &str = include_str!("../assets/readout.js");
const SDK_JS: &str = include_str!("../assets/vendor/livekit-client.esm.js");
const TOKEN_BODY_LIMIT: usize = 4 * 1024;

pub const SDK_VERSION: &str = "2.22.2";
pub const SDK_PATH: &str = "/vendor/livekit-client-2.22.2.esm.js";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

pub fn router(config: Config) -> Router {
    let csp = content_security_policy(&config.ws_url);

    Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/bench.js", get(bench))
        .route("/readout.js", get(readout))
        .route(SDK_PATH, get(sdk))
        .route("/healthz", get(healthz))
        .route("/api/config", get(public_config))
        .route(
            "/api/token",
            post(token).layer(DefaultBodyLimit::max(TOKEN_BODY_LIMIT)),
        )
        .with_state(Arc::new(config))
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
    let policy = format!(
        "default-src 'none'; \
         script-src 'self'; \
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

async fn index() -> impl IntoResponse {
    asset("text/html; charset=utf-8", NO_CACHE, INDEX_HTML)
}

async fn stylesheet() -> impl IntoResponse {
    asset("text/css; charset=utf-8", NO_CACHE, APP_CSS)
}

async fn script() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, APP_JS)
}

async fn bench() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, BENCH_JS)
}

async fn readout() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, READOUT_JS)
}

async fn sdk() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", IMMUTABLE, SDK_JS)
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicConfig {
    room: String,
    ws_url: String,
    max_name_length: usize,
    room_namespace_separator: char,
}

async fn public_config(State(config): State<Arc<Config>>) -> Json<PublicConfig> {
    Json(PublicConfig {
        room: config.room.clone(),
        ws_url: config.ws_url.clone(),
        max_name_length: crate::name::MAX_NAME_CHARS,
        room_namespace_separator: crate::name::ROOM_NAMESPACE_SEPARATOR,
    })
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
            tracing::error!(error = %self, "token request failed");
        }

        let body = ApiErrorBody {
            error: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

async fn token(
    State(config): State<Arc<Config>>,
    body: Result<Json<TokenRequest>, JsonRejection>,
) -> Result<Json<TokenResponse>, ApiError> {
    let Json(request) = body.map_err(|_| ApiError::Body)?;

    let name = clean_name(&request.name)?;
    let room = match request.room.as_deref() {
        Some(requested) => namespaced_room(&config.room, requested)?,
        None => config.room.clone(),
    };
    let identity = identity_for(&name);

    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::Clock)?
        + config.token_ttl;

    let claims = Claims {
        exp: exp.as_secs(),
        iss: config.api_key.clone(),
        sub: identity.clone(),
        name: name.clone(),
        video: VideoGrant {
            room_join: Some(true),
            room: room.clone(),
            can_publish: Some(true),
            can_subscribe: Some(true),
            can_publish_data: Some(false),
            can_publish_sources: vec!["microphone".to_owned(), "camera".to_owned()],
            ..VideoGrant::default()
        },
        ..Claims::default()
    };

    let token = mediacore_auth::issue(&config.api_secret, &claims)?;
    tracing::info!(%identity, %room, "issued a demo token");

    Ok(Json(TokenResponse {
        url: config.ws_url.clone(),
        token,
        room,
        identity,
        name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sdk_route_carries_the_vendored_version() {
        assert!(
            SDK_PATH.contains(SDK_VERSION),
            "{SDK_PATH} does not name version {SDK_VERSION}, so a browser would reuse a cached copy of another build"
        );
        assert!(
            APP_JS.contains(SDK_PATH),
            "app.js does not import the sdk from {SDK_PATH}"
        );
    }

    #[test]
    fn every_module_the_page_imports_is_compiled_in() {
        let served = [
            ("/app.js", APP_JS),
            ("/bench.js", BENCH_JS),
            ("/readout.js", READOUT_JS),
        ];

        for (path, module) in served {
            for import in imports(module) {
                assert!(
                    served.iter().any(|(candidate, _)| *candidate == import),
                    "{path} imports {import}, which this server does not serve"
                );
            }
        }

        assert!(
            imports(BENCH_JS).is_empty(),
            "bench.js is meant to be the leaf of the module graph"
        );
    }

    fn imports(module: &str) -> Vec<String> {
        module
            .match_indices("from \"")
            .filter_map(|(index, marker)| {
                let rest = &module[index + marker.len()..];
                rest.split_once('"').map(|(path, _)| path.to_owned())
            })
            .filter(|path| path.ends_with(".js") && !path.contains("/vendor/"))
            .collect()
    }

    #[test]
    fn the_policy_names_the_signalling_origin_and_forbids_the_rest() {
        let policy = content_security_policy("wss://media.example.test");
        let policy = policy.to_str().unwrap();

        assert!(policy.starts_with("default-src 'none';"));
        assert!(policy.contains("script-src 'self';"));
        assert!(
            policy.contains(
                "connect-src 'self' wss://media.example.test https://media.example.test;"
            )
        );
        assert!(policy.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn the_policy_allows_the_http_origin_the_sdk_validates_against() {
        let insecure = content_security_policy("ws://127.0.0.1:7880");
        let insecure = insecure.to_str().unwrap();

        assert!(
            insecure.contains("ws://127.0.0.1:7880 http://127.0.0.1:7880"),
            "livekit-client fetches /rtc/validate over http before it opens the socket, and a \
             policy that names only the socket blocks the join: {insecure}"
        );

        let secure = content_security_policy("wss://media.example.test");
        let secure = secure.to_str().unwrap();

        assert!(secure.contains("wss://media.example.test https://media.example.test"));
        assert!(
            !secure.contains("http://media.example.test"),
            "a secure deployment must not be told to allow plain http: {secure}"
        );
    }
}
