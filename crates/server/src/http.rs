use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::routing::{any, get};
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

use crate::rooms;
use crate::signal::{self, SignalQuery};
use crate::state::AppState;
use crate::swagger;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/rtc", get(signal::handler))
        .route("/rtc/validate", get(validate))
        .route(rooms::SERVICE_PATH, any(rooms::handler))
        // Merged, not routed here, so the page carries a content security policy
        // and the signalling routes do not.
        .merge(swagger::router())
        .layer(cors())
        .with_state(state)
}

/// Wide open, because the demo page and any backoffice reach this from a
/// browser on some other origin.
///
/// The request headers are mirrored rather than answered with `*`: the Fetch
/// standard makes `Access-Control-Allow-Headers: *` a wildcard over every
/// header name *except* `Authorization`, which has to be named. A browser
/// calling the room service sends exactly that header, so the wildcard alone
/// would fail its preflight and no call would ever be made.
fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers(Any)
        .allow_methods([
            Method::OPTIONS,
            Method::HEAD,
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
        ])
        .max_age(Duration::from_secs(86_400))
}

async fn index() -> &'static str {
    "mediacore"
}

async fn healthz(State(state): State<AppState>) -> String {
    let rooms = state.registry.rooms();
    let participants: usize = rooms.iter().map(|room| room.participant_count()).sum();
    let media = state.publishers.totals();
    let forwarding = state.router.totals();

    format!(
        "rooms={} participants={participants} publishers={} rtp_packets={} rtp_bytes={} \
         tracks={} subscriptions={} forwarded_packets={} forwarded_bytes={} bwe={} \
         allocated_subscribers={} ice={}",
        rooms.len(),
        state.publishers.count(),
        media.rtp_packets,
        media.rtp_bytes,
        forwarding.tracks,
        forwarding.subscriptions,
        forwarding.packets,
        forwarding.bytes,
        if state.bwe_config().enabled {
            "on"
        } else {
            "off"
        },
        state.bandwidth.count(),
        state.media.local_addr()
    )
}

async fn validate(
    State(state): State<AppState>,
    Query(query): Query<SignalQuery>,
) -> (StatusCode, String) {
    match mediacore_auth::verify_join(&query.access_token, &state.config.keys) {
        Ok(claims) => (StatusCode::OK, claims.identity().to_owned()),
        Err(error) => (StatusCode::UNAUTHORIZED, error.to_string()),
    }
}
