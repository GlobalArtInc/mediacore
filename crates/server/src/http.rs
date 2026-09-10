use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::{Json, Router};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
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

async fn healthz(State(state): State<AppState>) -> String {
    let rooms = state.registry.rooms();
    let participants: usize = rooms.iter().map(|room| room.participant_count()).sum();
    let media = state.publishers.totals();
    let forwarding = state.router.totals();
    let sessions = state.registry.sessions();

    let delivery = state.delivery.as_ref();
    let recording = state.recording.as_ref();

    // Where the meetings go, and whether it answered the last time anybody
    // asked it anything: `files:/var/lib/...`, `postgres:host:5432/db:up`, or
    // the same with `:down`. A server whose database is away carries calls
    // perfectly well and records nothing, and this is the field that says which
    // of those is happening.
    let store = format!(
        "{}:{}",
        sessions.describe(),
        if sessions.healthy() { "up" } else { "down" }
    );

    format!(
        "rooms={} participants={participants} publishers={} rtp_packets={} rtp_bytes={} \
         tracks={} subscriptions={} forwarded_packets={} forwarded_bytes={} bwe={} \
         allocated_subscribers={} session_store={} sessions={} live_sessions={} \
         resident_sessions={} owed_sessions={} unwritten_sessions={} events={} owed_events={} \
         undelivered_events={} unwritten_events={} recording={} unwritten_recordings={} \
         dropped_audio_packets={} abandoned_recordings={} stuck_recording_work={} ice={}",
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
        store,
        // How many meetings the store holds. Against a database this is the
        // whole cluster's, and it is a recent answer rather than a fresh one:
        // it is a `count(*)`, and scraping health must not be a reason to walk
        // the table.
        sessions.count(),
        // Running on this node. Against a database the cluster may be holding
        // more.
        sessions.live_count(),
        // What the history costs in memory. `sessions` grows for every meeting
        // this server has ever held; this one is meant not to, and an operator
        // watching it climb beside a flat `live_sessions` is watching a leak.
        sessions.resident(),
        // Records recorded and not yet in the store. It rises while a burst of
        // arrivals outruns the store and falls again; one that stays up is a
        // store that cannot keep pace with the meetings held on top of it, and
        // what a crash would cost is however long it stays up.
        sessions.owed(),
        // Not decoration. Every other number here goes up when the server is
        // working; this one goes up when a meeting has been held and will not
        // be there to look up tomorrow, and there is nowhere else an operator
        // would see that. It comes back down if a later write of the same
        // record lands, so a store that hiccuped once is not reported for ever
        // as a meeting that is going to be lost. It is what a database outage
        // looks like from outside: calls carry on, this climbs.
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
        // Whether this node can record at all, and whether it still can. `off`
        // is a deployment that means to record and has a configuration problem,
        // which is otherwise discovered a week later when somebody asks for a
        // recording. `stopped` is worse and is the one to alert on: the one
        // thread that writes files is gone, so this node is holding meetings
        // and recording none of them. Nothing restarts it -- what it was
        // writing is on the disk for the next start to finish, and the answer
        // is to restart the process.
        match recording {
            None => "off",
            Some(recording) if recording.recorder.alive() => "on",
            Some(_) => "stopped",
        },
        // Recordings finished on this node and not yet written into the record
        // store. Above zero means the store is away: the audio and everything
        // that describes it are on this node's disk, and what the outage costs
        // is the delay before the object store gets them.
        recording.map_or(0, |recording| recording.owed.load(Ordering::Relaxed)),
        // The alertable one. Audio this node threw away because the queue
        // between the media path and the writer was full, which means the disk
        // could not keep up with the meetings on top of it. It is bounded on
        // purpose -- a wedged disk must cost recording and never the calls --
        // and this is the number that says what the bound cost.
        recording.map_or(0, |recording| recording.dropped()),
        // Recordings the writer gave up on because something in them made it
        // panic. What reaches that thread came off the network from somebody
        // else's encoder, so anything above zero says a publisher sent
        // something this server could not handle -- and that it cost one
        // recording rather than every recording on the node, which is what it
        // used to cost.
        recording.map_or(0, |recording| recording.abandoned()),
        // Recording work in the shared queue that nobody is going to move:
        // given up on, or waiting for a node that has not come back. The
        // commonest cause is an upload whose node is gone for good -- the file
        // is on a disk nobody has, so no other node can do the work -- and
        // without this it is invisible: the row is not `failed`, it is not
        // claimed, and every node walks past it for ever. Cluster-wide, and a
        // recent answer rather than a fresh one.
        sessions.stuck(),
        state.media.local_addr()
    )
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
