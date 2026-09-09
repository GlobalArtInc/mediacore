
use super::*;

use mediacore_config::Config;
use mediacore_transport::SpawnedPeer;

use crate::state::AppState;

const TRACK: &str = "TR_cam";
const PUBLISHER: &str = "PA_alice";
const SUBSCRIBER: &str = "PA_bob";

async fn app_state() -> AppState {
    let mut config =
        Config::parse("keys:\n  devkey: devsecretdevsecretdevsecretdevsecret\n").unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    // A directory of this server's own for session records: a test must
    // never write into the tree, and two servers sharing a store would
    // read each other's meetings back.
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();

    AppState::start(config)
        .await
        .expect("the media engine starts")
}

fn ladder() -> LayerMap {
    LayerMap::build(&[], &[Rid::from("q"), Rid::from("h"), Rid::from("f")])
}

async fn simulcast_source() -> (AppState, SpawnedPeer, Arc<TrackSource>) {
    let state = app_state().await;
    let peer = state
        .media
        .spawn_publisher()
        .expect("a publisher transport");

    let source = state.router.register_source(
        TRACK,
        TrackSource::new(
            peer.handle.clone(),
            Mid::from("0"),
            TrackKind::Video,
            "video/VP8",
            PUBLISHER,
            ladder(),
            SourceConfig::default(),
        ),
    );

    (state, peer, source)
}

async fn subscriber_sink(state: &AppState) -> (SpawnedPeer, Arc<TrackSink>) {
    let peer = state
        .media
        .spawn_subscriber()
        .expect("a subscriber transport");
    let sink = Arc::new(TrackSink::new(
        TRACK,
        SUBSCRIBER,
        peer.handle.clone(),
        Mid::from("1"),
        Pt::from(96),
        SinkConfig {
            forwarder: forwarder_config(TrackKind::Video, 90_000),
            selector: SelectorConfig::default(),
            start: stream_start(SUBSCRIBER, TRACK, 1),
        },
    ));

    (peer, sink)
}

fn enabled_qualities(message: &signal_response::Message) -> Vec<bool> {
    match message {
        signal_response::Message::SubscribedQualityUpdate(update) => update
            .subscribed_codecs
            .first()
            .map(|codec| {
                codec
                    .qualities
                    .iter()
                    .map(|quality| quality.enabled)
                    .collect()
            })
            .unwrap_or_default(),
        _ => panic!("the publisher was not told about its qualities"),
    }
}

async fn settle(source: &TrackSource) -> signal_response::Message {
    let mut sent = None;

    let delivered = source
        .reconcile(TRACK, |message, _| {
            sent = Some(message);
            true
        })
        .await;

    assert!(delivered, "the announcement was refused");

    sent.expect("the publisher is always told what the server committed to")
}

#[tokio::test]
async fn a_layer_the_publisher_resumes_is_trusted_by_its_first_packet_again() {
    let (_state, _peer, source) = simulcast_source().await;
    let top = SpatialLayer::MAX;
    let now = Instant::now();

    source.observe(top, 7, 900, now);
    assert!(source.active_layers().contains(top));

    source.tick(now + Duration::from_secs(10));
    assert!(
        !source.active_layers().contains(top),
        "the top layer must look gone before dynacast decides anything"
    );

    source.set_demand(SUBSCRIBER, Some(SpatialLayer::BASE));
    let update = source
        .pending_quality_update(TRACK, now)
        .expect("the top layers are dropped");
    assert_eq!(update.enabled, LayerSet::of(SpatialLayer::BASE));
    settle(&source).await;

    source.set_demand(SUBSCRIBER, Some(top));
    let update = source
        .pending_quality_update(TRACK, now)
        .expect("the top layer is asked for again");
    assert_eq!(update.enabled, LayerSet::all());
    settle(&source).await;

    assert!(
        source.observe(top, 7, 900, now + Duration::from_secs(11)),
        "the first packet of a resumed layer left the subscriber on the small picture"
    );
    assert!(source.active_layers().contains(top));
}

#[tokio::test]
async fn a_subscriber_of_an_idle_publisher_can_still_ask_for_a_keyframe() {
    let (state, _peer, source) = simulcast_source().await;
    let now = Instant::now();

    source.observe(SpatialLayer::BASE, 5, 400, now);
    source.tick(now + Duration::from_secs(10));

    assert!(
        source.active_layers().is_empty(),
        "the publisher must look idle for this to prove anything"
    );

    assert!(
        state.router.request_keyframe_for(TRACK, SUBSCRIBER, false),
        "the only thing that wakes an idle publisher was thrown away"
    );
    assert!(
        !state.router.request_keyframe_for(TRACK, SUBSCRIBER, false),
        "the rate limiter still holds between requests"
    );
    assert!(
        state.router.request_keyframe_for(TRACK, SUBSCRIBER, true),
        "a forced request goes out anyway"
    );
}

#[tokio::test]
async fn a_muted_publisher_is_not_asked_for_a_keyframe() {
    let (state, _peer, source) = simulcast_source().await;
    let now = Instant::now();

    source.observe(SpatialLayer::BASE, 5, 400, now);

    let (_subscriber, sink) = subscriber_sink(&state).await;
    state.router.add_sink(sink);

    assert!(
        state.router.request_keyframe_for(TRACK, SUBSCRIBER, true),
        "a publisher that is sending answers a forced request"
    );

    state.router.set_publisher_muted(TRACK, true);

    assert!(
        !state.router.request_keyframe_for(TRACK, SUBSCRIBER, true),
        "a muted publisher was poked for a keyframe it cannot answer"
    );

    state.router.set_publisher_muted(TRACK, false);

    assert!(
        state.router.request_keyframe_for(TRACK, SUBSCRIBER, true),
        "the publisher stayed unpokeable after it came back"
    );
}

#[tokio::test]
async fn reconciling_twice_leaves_the_publisher_on_the_committed_state() {
    let (_state, _peer, source) = simulcast_source().await;
    let now = Instant::now();

    source.set_demand(SUBSCRIBER, Some(SpatialLayer::BASE));
    let first = source
        .pending_quality_update(TRACK, now)
        .expect("the narrow decision");
    assert_eq!(first.enabled, LayerSet::of(SpatialLayer::BASE));

    source.set_demand("PA_carol", Some(SpatialLayer::MAX));
    let second = source
        .pending_quality_update(TRACK, now)
        .expect("the wide decision");
    assert_eq!(second.enabled, LayerSet::all());

    let (narrow, wide) = tokio::join!(settle(&source), settle(&source));

    assert_eq!(
        enabled_qualities(&narrow),
        vec![true, true, true],
        "the announcement of the narrow decision reached the publisher after the server had \
             already moved on, and it carried the state nobody wants any more"
    );
    assert_eq!(enabled_qualities(&wide), vec![true, true, true]);
    assert_eq!(
        source.announced_layers(),
        source.committed_layers(),
        "the publisher was left on a state the server had already moved off"
    );
    assert_eq!(source.announced_layers(), Some(LayerSet::all()));
}

#[tokio::test]
async fn a_decision_the_publisher_never_heard_is_announced_again() {
    let (_state, _peer, source) = simulcast_source().await;
    let now = Instant::now();

    source.set_demand(SUBSCRIBER, Some(SpatialLayer::BASE));
    let decision = source
        .pending_quality_update(TRACK, now)
        .expect("the first decision");
    assert_eq!(decision.enabled, LayerSet::of(SpatialLayer::BASE));

    assert!(
        !source.reconcile(TRACK, |_, _| false).await,
        "reconcile claimed the publisher was told"
    );
    assert_eq!(
        source.announced_layers(),
        None,
        "an announcement that never went out must not count as applied"
    );

    let retry = source
        .pending_quality_update(TRACK, now)
        .expect("a decision the publisher never heard is announced again");
    assert_eq!(retry.enabled, LayerSet::of(SpatialLayer::BASE));

    settle(&source).await;

    assert_eq!(
        source.announced_layers(),
        Some(LayerSet::of(SpatialLayer::BASE))
    );
    assert!(
        source.pending_quality_update(TRACK, now).is_none(),
        "a decision the publisher already heard is not repeated"
    );
}

#[tokio::test]
async fn pausing_a_layer_that_is_still_arriving_does_not_declare_it_gone() {
    let (_state, _peer, source) = simulcast_source().await;
    let top = SpatialLayer::MAX;
    let now = Instant::now();

    source.observe(SpatialLayer::BASE, 5, 400, now);
    source.observe(top, 7, 900, now);
    assert!(source.active_layers().contains(top));

    source.set_demand(SUBSCRIBER, Some(top));
    source
        .pending_quality_update(TRACK, now)
        .expect("every layer is asked for");
    settle(&source).await;
    assert!(source.active_layers().contains(top));

    source.set_demand(SUBSCRIBER, Some(SpatialLayer::BASE));
    assert!(
        source.pending_quality_update(TRACK, now).is_none(),
        "the layer is held open for the pause delay first"
    );

    let update = source
        .pending_quality_update(TRACK, now + Duration::from_secs(6))
        .expect("the top layers are dropped");
    assert_eq!(update.enabled, LayerSet::of(SpatialLayer::BASE));
    settle(&source).await;

    assert!(
        source.active_layers().contains(top),
        "a layer that is still arriving was declared gone by the decision to pause it"
    );
}

#[test]
fn two_subscribers_of_one_track_get_different_stream_starts() {
    let first = stream_start("PA_alice", "TR_cam", 1);
    let second = stream_start("PA_bob", "TR_cam", 1);

    assert_ne!(first.seq, second.seq);
    assert_ne!(first.timestamp, second.timestamp);
}

#[test]
fn a_stream_start_never_lands_in_the_publishers_low_sequence_space() {
    for nonce in 0..64u64 {
        let start = stream_start("PA_alice", "TR_cam", nonce);

        assert!(start.seq >= 1 << 15, "sequence {} is too low", start.seq);
        assert!(start.seq < 1 << 16);
        assert!(start.timestamp >= 1 << 31);
    }
}

#[test]
fn the_forwarder_clock_rate_follows_the_negotiated_codec() {
    assert_eq!(forwarder_config(TrackKind::Audio, 0).clock_rate, 48_000);
    assert_eq!(forwarder_config(TrackKind::Video, 0).clock_rate, 90_000);
    assert_eq!(
        forwarder_config(TrackKind::Audio, 16_000).clock_rate,
        16_000
    );
}

#[test]
fn an_empty_router_reports_nothing() {
    let router = MediaRouter::default();

    assert_eq!(router.totals(), RouterTotals::default());
    assert_eq!(router.subscriber_count("TR_missing"), 0);
    assert!(router.source("TR_missing").is_none());
    assert!(!router.request_keyframe_for("TR_missing", "PA_bob", true));
    assert_eq!(router.remove_sink("TR_missing", "PA_bob"), 0);
    assert!(router.live_layers("TR_missing").is_none());
    assert!(
        router
            .pending_quality_update("TR_missing", Instant::now())
            .is_none()
    );
    assert!(router.tick_track("TR_missing", Instant::now()).is_empty());
}

#[test]
fn a_keyframe_is_never_asked_for_on_a_layer_that_is_not_arriving() {
    let low = SpatialLayer::BASE;
    let high = SpatialLayer::MAX;

    assert_eq!(arriving(LayerSet::EMPTY, low), None);
    assert_eq!(arriving(LayerSet::all(), high), Some(high));
    assert_eq!(
        arriving(LayerSet::of(low), high),
        Some(low),
        "a subscriber waiting for a layer nobody sends is answered by the one that is arriving"
    );
    assert_eq!(
        arriving(LayerSet::of(high), low),
        Some(high),
        "the same overshoot the selector makes"
    );
}

#[test]
fn dropping_a_track_takes_its_source_and_every_sink_with_it() {
    let router = MediaRouter::default();

    assert_eq!(router.drop_track("TR_missing"), 0);

    router.mark_subscribed("TR_cam");
    assert_eq!(router.drop_track("TR_cam"), 0);
    assert_eq!(router.totals().tracks, 0);
    assert!(
        router.mark_subscribed("TR_cam"),
        "a dropped track keeps nothing behind, not even its announcement"
    );
}

#[test]
fn a_track_is_announced_to_its_publisher_only_once() {
    let router = MediaRouter::default();

    assert!(router.mark_subscribed("TR_cam"));
    assert!(!router.mark_subscribed("TR_cam"));
    assert!(router.mark_subscribed("TR_mic"));
}

fn dynacast(pause_delay: Duration) -> Dynacast {
    Dynacast {
        committed: None,
        applied: None,
        deadline: None,
        pause_delay,
    }
}

#[test]
fn a_layer_a_subscriber_needs_is_enabled_at_once() {
    let mut dynacast = dynacast(Duration::from_secs(5));
    let now = Instant::now();

    assert_eq!(
        dynacast.poll(LayerSet::upto(SpatialLayer::BASE), now),
        Some(LayerSet::of(SpatialLayer::BASE)),
        "the first subscriber is answered without waiting"
    );
    dynacast.applied = dynacast.committed;

    assert_eq!(
        dynacast.poll(LayerSet::all(), now),
        Some(LayerSet::all()),
        "a subscriber that needs a higher layer never waits for it"
    );
}

#[test]
fn a_layer_nobody_wants_is_held_open_for_the_pause_delay() {
    let mut dynacast = dynacast(Duration::from_millis(200));
    let now = Instant::now();

    dynacast.committed = Some(LayerSet::all());
    dynacast.applied = Some(LayerSet::all());

    assert_eq!(dynacast.poll(LayerSet::EMPTY, now), None);
    assert_eq!(
        dynacast.poll(LayerSet::EMPTY, now + Duration::from_millis(100)),
        None,
        "the publisher is not stopped on the first quiet moment"
    );
    assert_eq!(
        dynacast.poll(LayerSet::EMPTY, now + Duration::from_millis(250)),
        Some(LayerSet::EMPTY)
    );
}

#[test]
fn demand_that_comes_back_before_the_delay_cancels_the_pause() {
    let mut dynacast = dynacast(Duration::from_millis(200));
    let now = Instant::now();

    dynacast.committed = Some(LayerSet::all());
    dynacast.applied = Some(LayerSet::all());

    assert_eq!(dynacast.poll(LayerSet::EMPTY, now), None);
    assert_eq!(
        dynacast.poll(LayerSet::all(), now + Duration::from_millis(50)),
        None,
        "nothing is sent, the publisher was never told to stop"
    );
    assert_eq!(
        dynacast.poll(LayerSet::EMPTY, now + Duration::from_millis(250)),
        None,
        "the clock restarts, it is not still running from the first drop"
    );
}

#[tokio::test]
async fn a_screen_share_is_not_judged_by_camera_thresholds() {
    assert_eq!(
        tracker_config(mediacore::TrackSource::ScreenShare),
        TrackerConfig::screenshare(),
        "a still screen share stops sending by design and would be declared gone"
    );
    assert_eq!(
        tracker_config(mediacore::TrackSource::ScreenShareAudio),
        TrackerConfig::screenshare()
    );
    assert_eq!(
        tracker_config(mediacore::TrackSource::Camera),
        TrackerConfig::video()
    );
    assert_eq!(
        tracker_config(mediacore::TrackSource::Unknown),
        TrackerConfig::video()
    );

    let state = app_state().await;

    assert_eq!(
        state
            .source_config(mediacore::TrackSource::ScreenShare)
            .tracker,
        TrackerConfig::screenshare()
    );
    assert_eq!(
        state.source_config(mediacore::TrackSource::Camera).tracker,
        TrackerConfig::video()
    );
}
