use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::routing::{any, get};
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

use crate::rooms;
use crate::sessions;
use crate::signal::{self, SignalQuery};
use crate::state::AppState;
use crate::swagger;

pub fn router(state: AppState) -> Router {
    let livekit_alias = state.config.twirp.livekit_alias;

    let mut router = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/rtc", get(signal::handler))
        .route("/rtc/validate", get(validate))
        .route(rooms::SERVICE_PATH, any(rooms::handler))
        // What happened in a room, as opposed to what is happening in it. Not
        // under `/twirp`, because these are not Twirp: LiveKit has no session
        // record, so there is no generated client anywhere that would call
        // them and nothing to be compatible with. See `sessions.rs`.
        .route(sessions::ROOM_SESSIONS_PATH, get(sessions::of_room))
        .route(sessions::ROOM_SESSION_PATH, get(sessions::current))
        .route(sessions::SESSION_PATH, get(sessions::one));

    // The compatibility route, on by default. It is the same handler, so it
    // answers the same method set with the same grants and the same error
    // bodies; adding it here rather than in `rooms` keeps the decision to serve
    // it in one place and out of the dispatcher, which has no business knowing
    // about configuration.
    if livekit_alias {
        router = router.route(rooms::ALIAS_PATH, any(rooms::handler));
    }

    router
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
    let sessions = state.registry.sessions();

    let delivery = state.delivery.as_ref();

    format!(
        "rooms={} participants={participants} publishers={} rtp_packets={} rtp_bytes={} \
         tracks={} subscriptions={} forwarded_packets={} forwarded_bytes={} bwe={} \
         allocated_subscribers={} sessions={} live_sessions={} resident_sessions={} \
         owed_sessions={} unwritten_sessions={} events={} owed_events={} \
         undelivered_events={} unwritten_events={} ice={}",
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
        sessions.count(),
        sessions.live_count(),
        // What the history costs in memory. `sessions` grows for every meeting
        // this server has ever held; this one is meant not to, and an operator
        // watching it climb beside a flat `live_sessions` is watching a leak.
        sessions.resident(),
        // Records recorded and not yet on the disk. It rises while a burst of
        // arrivals outruns the disk and falls again; one that stays up is a
        // disk that cannot keep pace with the meetings held on top of it, and
        // what a crash would cost is however long it stays up.
        sessions.owed(),
        // Not decoration. Every other number here goes up when the server is
        // working; this one goes up when a meeting has been held and will not
        // be there to look up tomorrow, and there is nowhere else an operator
        // would see that. It comes back down if a later write of the same
        // record lands, so a disk that hiccuped once is not reported for ever
        // as a meeting that is going to be lost.
        sessions.unwritten(),
        state.events.emitted(),
        // Events promised to somebody and not yet taken by them. A number that
        // sits above zero is an endpoint or a broker that is behind or away;
        // one that climbs is one that is not coming back.
        delivery.map_or(0, |delivery| delivery.owed()),
        // The alertable one. Something outside this server is missing events
        // and nothing else will ever tell anybody: given up on after every
        // attempt, or never attempted because that destination was too far
        // behind. What they were announcing is still recorded here, which is
        // the only reason this is a number and not a crash.
        delivery.map_or(0, |delivery| delivery.undelivered()),
        // Outbox records that could not be written: events that are being
        // attempted now and will not be retried after a restart.
        delivery.map_or(0, |delivery| delivery.unwritten()),
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
