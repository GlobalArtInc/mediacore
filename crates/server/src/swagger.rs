//! The page at `/swagger`, and everything it loads.
//!
//! It is served by the **SFU**, not by the front end, and that is the whole
//! design. Try it out is a call to `/twirp/mediacore.RoomService/...`, so the
//! page has to sit on the same origin as the API or every button is a
//! cross-origin request to another port -- one the browser will preflight, the
//! certificate may not cover, and an operator poking at `127.0.0.1:7880` has no
//! proxy in front of at all. Serving it here means the page works through Caddy
//! on the public origin *and* works over an SSH tunnel straight to the SFU,
//! with nothing to configure either way. The front end also deploys on its own
//! schedule from this tree, and an API reference that lags the server it
//! documents is the failure this whole page exists to avoid.
//!
//! What is on it comes from [`crate::openapi`]. This module only serves it.

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::openapi;
use crate::state::AppState;

const PAGE: &str = include_str!("../assets/swagger.html");
const STYLE: &str = include_str!("../assets/swagger.css");
const INIT: &str = include_str!("../assets/swagger-init.js");
const NOTICE: &str = include_str!("../assets/vendor/NOTICE.txt");
const UI_STYLE: &str = include_str!("../assets/vendor/swagger-ui-5.32.15.css");
const UI_SCRIPT: &str = include_str!("../assets/vendor/swagger-ui-bundle-5.32.15.js");

/// swagger-ui, vendored rather than loaded from a CDN.
///
/// The same rule the front end follows for the LiveKit SDK and RNNoise: a
/// deployment with no route to the internet still gets a working page, the
/// content security policy below can forbid third-party script outright, and
/// nobody can change what this browser executes by changing what a CDN
/// answers. Both routes carry the version, so a browser holding a cached copy
/// of the previous build can never be handed it again. `assets/vendor/NOTICE.txt`
/// names it and everything webpack built into it, and is served at
/// `/swagger/NOTICE.txt` because Apache-2.0 asks for that.
pub const UI_VERSION: &str = "5.32.15";

pub const PAGE_PATH: &str = "/swagger";
pub const SPEC_PATH: &str = "/swagger/openapi.json";
pub const NOTICE_PATH: &str = "/swagger/NOTICE.txt";
pub const STYLE_PATH: &str = "/swagger/swagger.css";
pub const INIT_PATH: &str = "/swagger/swagger-init.js";
pub const UI_STYLE_PATH: &str = "/swagger/vendor/swagger-ui-5.32.15.css";
pub const UI_SCRIPT_PATH: &str = "/swagger/vendor/swagger-ui-bundle-5.32.15.js";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(PAGE_PATH, get(page))
        // A trailing slash is what a browser leaves behind after somebody
        // types the path by hand, and answering it with a 404 on a
        // documentation page is a poor first impression.
        .route(
            "/swagger/",
            get(|| async { Redirect::permanent(PAGE_PATH) }),
        )
        .route(SPEC_PATH, get(spec))
        .route(STYLE_PATH, get(style))
        .route(INIT_PATH, get(init))
        .route(NOTICE_PATH, get(notice))
        .route(UI_STYLE_PATH, get(ui_style))
        .route(UI_SCRIPT_PATH, get(ui_script))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            content_security_policy(),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
}

/// Nothing loads from anywhere but here, and nothing here is inline.
///
/// `style-src` is the one concession: swagger-ui is React and writes style
/// attributes onto the elements it renders, which `unsafe-inline` is the only
/// way to allow. Script has no such concession -- the page carries no inline
/// script at all, which is why `swagger-init.js` is a file. `connect-src
/// 'self'` is what Try it out needs and is also the reason the page has to be
/// served from the same origin as `/twirp`.
fn content_security_policy() -> HeaderValue {
    HeaderValue::from_static(
        "default-src 'none'; \
         script-src 'self'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; \
         font-src 'self' data:; \
         connect-src 'self'; \
         base-uri 'none'; \
         form-action 'none'; \
         frame-ancestors 'none'",
    )
}

async fn page() -> impl IntoResponse {
    asset("text/html; charset=utf-8", NO_CACHE, PAGE)
}

async fn spec() -> impl IntoResponse {
    asset("application/json; charset=utf-8", NO_CACHE, openapi::json())
}

async fn style() -> impl IntoResponse {
    asset("text/css; charset=utf-8", NO_CACHE, STYLE)
}

async fn init() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", NO_CACHE, INIT)
}

async fn notice() -> impl IntoResponse {
    asset("text/plain; charset=utf-8", NO_CACHE, NOTICE)
}

async fn ui_style() -> impl IntoResponse {
    asset("text/css; charset=utf-8", IMMUTABLE, UI_STYLE)
}

async fn ui_script() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", IMMUTABLE, UI_SCRIPT)
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
