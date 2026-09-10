use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::Serialize;
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
        .route("/readyz", get(readyz))
        .route(METRICS_PATH, get(metrics))
        .route(CLUSTER_PATH, get(cluster))
        .route("/rtc", get(signal::handler))
        .route("/rtc/validate", get(validate))
        .route(rooms::SERVICE_PATH, any(rooms::handler))
        // What happened in a room, as opposed to what is happening in it. Not
        // under `/twirp`, because these are not Twirp: LiveKit has no session
        // record, so there is no generated client anywhere that would call
        // them and nothing to be compatible with. See `sessions.rs`.
        .route(sessions::ROOM_SESSIONS_PATH, get(sessions::of_room))
        .route(sessions::ROOM_SESSION_PATH, get(sessions::current))
        .route(sessions::ROOMS_SESSIONS_PATH, get(sessions::now))
        .route(sessions::SESSION_PATH, get(sessions::one))
        .route(sessions::SESSION_RECORDINGS_PATH, get(sessions::recordings));

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

/// Who is running this, and what each of them is holding.
pub const CLUSTER_PATH: &str = "/api/cluster";

/// The cluster as this node last saw it.
///
/// Read out of the snapshot the beat keeps, never out of the database, so
/// asking costs nothing and a database that is briefly away costs the answer
/// its freshness rather than its existence. `readAtMs` says when it was true,
/// which is the field a reader needs to know how much to trust the rest.
///
/// Behind the same grant that reads a meeting: it says which rooms are running
/// where, and a token that may not read a room's history may not learn that
/// the room is on node three either.
async fn cluster(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = sessions::reading_cluster(&state, &headers) {
        return error.into_response();
    }

    let view = state.cluster.view();

    Json(ClusterView {
        node: state.cluster.id().to_owned(),
        joined: state.cluster.joined(),
        read_at_ms: view.read_at_ms,
        nodes: view
            .nodes
            .iter()
            .map(|node| NodeView {
                id: node.id.clone(),
                signal_url: node.signal_url.clone(),
                advertise: node.advertise.clone(),
                version: node.version.clone(),
                region: node.region.clone(),
                state: node.state.as_str(),
                alive: node.alive(view.read_at_ms, view.lease_ms()),
                started_at_ms: node.started_at_ms,
                kept_alive_at_ms: node.kept_alive_at_ms,
                rooms: node.rooms.clone(),
            })
            .collect(),
    })
    .into_response()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClusterView {
    /// This node, so a caller reading one of several answers knows which it
    /// reached.
    node: String,
    /// Whether this node registers at all. `false` is a deployment running one
    /// replica, and then `nodes` is empty -- which is a different thing from a
    /// cluster whose nodes have all stopped.
    joined: bool,
    read_at_ms: i64,
    nodes: Vec<NodeView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeView {
    id: String,
    signal_url: String,
    advertise: String,
    version: String,
    region: String,
    state: &'static str,
    /// Whether it was still beating when this was read. Computed here rather
    /// than left to the caller, which would need the lease to work it out.
    alive: bool,
    started_at_ms: i64,
    kept_alive_at_ms: i64,
    rooms: Vec<String>,
}

/// Whether this node should be sent new arrivals.
///
/// Separate from `/healthz` on purpose, and not a field on it. `/healthz` says
/// whether the process is working and is parsed by a compose healthcheck and by
/// three test files, so its shape is not free to change; this answers a
/// different question -- *should traffic come here* -- and answers it with a
/// status code, because that is the only thing a load balancer reads.
///
/// A draining node is still healthy: it is finishing the meetings it holds and
/// its `/healthz` stays 200 until it exits. What it must not get is somebody
/// new, which is what the 503 here is for.
async fn readyz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    match state.draining() {
        true => (StatusCode::SERVICE_UNAVAILABLE, "draining\n"),
        false => (StatusCode::OK, "ready\n"),
    }
}

/// What is happening on this node, in one line.
///
/// Every number on it is gathered by [`crate::metrics::snapshot`] and rendered
/// by [`crate::metrics::Snapshot::health_line`], which is also what `/metrics`
/// renders from -- the two answers are one reading, so they cannot disagree
/// about a number and cannot drift apart when one of them gains a field. The
/// shape of the line is not this handler's to change: see the module.
async fn healthz(State(state): State<AppState>) -> String {
    crate::metrics::snapshot(&state).health_line()
}

/// What a scrape reads.
pub const METRICS_PATH: &str = "/metrics";

/// The same reading as `/healthz`, for a collector rather than a person.
///
/// **Open, exactly as `/healthz` is, and deliberately.** A scrape carries what
/// it was configured with and nothing else: a `ServiceMonitor` pointed at a
/// port has no meeting token to present, and an endpoint that demanded one
/// would be an endpoint nobody ever managed to scrape -- which is how a server
/// ends up with no monitoring at all. What makes that safe is what is *not* in
/// the answer: no room, participant or track appears anywhere in it, by design
/// and not by accident (see [`crate::metrics`]), so the whole document is
/// counts of things plus this node's own id, version and region. It is the
/// health line in another dialect, and the health line has always been open.
///
/// What it is not is public. This is the signalling port; the proxy in front of
/// a deployment publishes `/rtc`, `/twirp` and `/swagger` and hands everything
/// else to the front end, so `/metrics` is reachable from the cluster network
/// and from the host, which is where a collector runs. A deployment that put
/// this port on the internet would be publishing `/healthz` and the signalling
/// socket with it, and would have a bigger problem than this route.
async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, crate::metrics::CONTENT_TYPE)],
        crate::metrics::snapshot(&state).exposition(),
    )
        .into_response()
}

async fn validate(
    State(state): State<AppState>,
    Query(query): Query<SignalQuery>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    let Some(token) = signal::token_from(&query, &headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            "no pass on the request: put it in `access_token` or in an `Authorization: Bearer` \
             header"
                .to_owned(),
        );
    };

    match mediacore_auth::verify_join(token, &state.config.keys) {
        Ok(claims) => (StatusCode::OK, claims.identity().to_owned()),
        Err(error) => (StatusCode::UNAUTHORIZED, error.to_string()),
    }
}
