use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::routing::get;
use tower_http::cors::{Any, CorsLayer};

use crate::signal::{self, SignalQuery};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/rtc", get(signal::handler))
        .route("/rtc/validate", get(validate))
        .layer(cors())
        .with_state(state)
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
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
