use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;

use crate::signal::{self, SignalQuery};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/rtc", get(signal::handler))
        .route("/rtc/validate", get(validate))
        .with_state(state)
}

async fn index() -> &'static str {
    "mediacore"
}

async fn healthz(State(state): State<AppState>) -> String {
    let rooms = state.registry.rooms();
    let participants: usize = rooms.iter().map(|room| room.participant_count()).sum();
    format!("rooms={} participants={participants}", rooms.len())
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
