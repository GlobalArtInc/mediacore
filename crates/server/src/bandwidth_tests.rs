use super::*;

use std::time::Duration;

use mediacore_transport::SendQueueEntry;

const TRACK: &str = "TR_cam";

fn config() -> BweConfig {
    BweConfig {
        enabled: true,
        ..BweConfig::default()
    }
}

fn controller(config: &BweConfig) -> (Arc<SubscriberBandwidth>, BandwidthController) {
    let shared = Arc::new(SubscriberBandwidth::default());
    let controller = BandwidthController::new(config, Arc::clone(&shared));

    (shared, controller)
}

fn report(at: Instant, peer_bytes_tx: u64, bytes_tx: u64) -> PeerReport {
    backed_up(at, peer_bytes_tx, bytes_tx, Duration::ZERO)
}

fn backed_up(at: Instant, peer_bytes_tx: u64, bytes_tx: u64, queue_age: Duration) -> PeerReport {
    let mid = mediacore_transport::str0m::media::Mid::from("0");
    let mut queue = SendQueueEntry::empty(mid, None);

    if !queue_age.is_zero() {
        queue.packets = 1;
        queue.bytes = 1_200;
        queue.oldest_unsent = Some(at - queue_age);
    }

    PeerReport {
        bwe_tx: None,
        egress_loss: Some(0.0),
        ingress_loss: None,
        rtt: Some(Duration::from_millis(10)),
        bytes_tx,
        bytes_rx: 0,
        peer_bytes_tx,
        peer_bytes_rx: 0,
        queues: vec![queue],
        at,
    }
}

fn camera(measured: [u64; LAYER_COUNT]) -> SubscribedTrack {
    SubscribedTrack {
        track_sid: TRACK.to_owned(),
        kind: TrackKind::Video,
        source: mediacore::TrackSource::Camera,
        multi_layer: true,
        carrying: true,
        available: LayerSet::all(),
        ceiling: Some(SpatialLayer::MAX),
        measured: measured.map(|kbps| Some(Bitrate::kbps(kbps))),
        declared: [None; LAYER_COUNT],
    }
}

#[test]
fn a_pinned_estimate_is_what_the_interpreter_believes() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let start = Instant::now();
    let tracks = [camera([150, 500, 1_500])];

    shared.pin_estimate(Some(Bitrate::kbps(2_000)));
    controller.tick(&tracks, start);
    controller.on_report(&report(start, 0, 0), 0, start);

    for step in 1..=4 {
        let at = start + Duration::from_secs(step);
        controller.on_report(&report(at, 250_000 * step, 200_000 * step), 300 * step, at);
        controller.tick(&tracks, at);
    }

    assert_eq!(controller.state(), CongestionState::Clear);
    assert_eq!(controller.budget(), Bitrate::kbps(2_000));
    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Unlimited),
        "a link with room for everything caps nothing"
    );

    shared.pin_estimate(Some(Bitrate::kbps(200)));

    for step in 5..=9 {
        let at = start + Duration::from_secs(step);
        controller.on_report(&report(at, 250_000 * step, 200_000 * step), 300 * step, at);
        controller.tick(&tracks, at);
    }

    assert_eq!(
        controller.state(),
        CongestionState::Clear,
        "an estimate that fell with nothing queued behind it is a smaller budget, not a congested link"
    );
    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Capped(SpatialLayer::BASE)),
        "a collapsed estimate left the subscriber on a layer it cannot afford"
    );
}

#[test]
fn a_joining_subscriber_is_handed_the_configured_initial_bitrate() {
    let config = config();
    let (_shared, mut controller) = controller(&config);
    let now = Instant::now();

    assert_eq!(
        controller.budget(),
        Bitrate::kbps(config.initial_kbps.into())
    );

    controller.tick(&[camera([150, 500, 1_500])], now);

    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Capped(SpatialLayer::new(1))),
        "the first second of a subscription is spent inside the configured start, not outside every budget"
    );
}

#[test]
fn a_backed_up_send_queue_is_congestion_the_estimate_never_reported() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let start = Instant::now();
    let tracks = [camera([150, 500, 1_500])];

    shared.pin_estimate(Some(Bitrate::kbps(2_000)));
    controller.on_report(&report(start, 0, 0), 0, start);
    controller.tick(&tracks, start);

    for step in 1..=4 {
        let at = start + Duration::from_secs(step);
        controller.on_report(
            &backed_up(at, 50_000 * step, 40_000 * step, Duration::from_millis(400)),
            300 * step,
            at,
        );
        controller.tick(&tracks, at);
    }

    assert_eq!(
        controller.state(),
        CongestionState::Congested,
        "packets waiting behind the pacer are the one signal nothing else can fake"
    );
    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Capped(SpatialLayer::BASE))
    );
}

#[test]
fn an_estimate_that_arrives_while_one_is_pinned_is_ignored() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let now = Instant::now();

    shared.pin_estimate(Some(Bitrate::kbps(700)));
    controller.on_report(&report(now, 0, 0), 0, now);
    controller.on_estimate(Bitrate::mbps(8), now);
    controller.on_report(
        &report(now + Duration::from_secs(1), 100_000, 80_000),
        100,
        now,
    );

    assert_eq!(controller.budget(), Bitrate::kbps(700));

    shared.pin_estimate(None);
    controller.on_estimate(Bitrate::mbps(8), now);
    controller.on_report(
        &report(now + Duration::from_secs(2), 200_000, 160_000),
        200,
        now,
    );

    assert_eq!(controller.budget(), Bitrate::mbps(8));
}

#[test]
fn the_overhead_is_calibrated_from_what_actually_left_the_socket() {
    let config = config();
    let (_shared, mut controller) = controller(&config);
    let start = Instant::now();

    assert_eq!(controller.overhead_bytes(), config.overhead_bytes);

    controller.on_report(&report(start, 0, 0), 0, start);

    for step in 1..=20u64 {
        let at = start + Duration::from_secs(step);
        let packets = 1_000 * step;
        controller.on_report(&report(at, 1_060_000 * step, 1_000_000 * step), packets, at);
    }

    assert_eq!(
        controller.overhead_bytes(),
        60,
        "the gap between wire bytes and payload bytes is the overhead per packet"
    );
}

#[test]
fn a_window_too_small_to_measure_leaves_the_overhead_alone() {
    let config = config();
    let (_shared, mut controller) = controller(&config);
    let start = Instant::now();

    controller.on_report(&report(start, 0, 0), 0, start);
    controller.on_report(
        &report(start + Duration::from_secs(1), 100_000, 1_000),
        MIN_CALIBRATION_PACKETS - 1,
        start,
    );

    assert_eq!(controller.overhead_bytes(), config.overhead_bytes);
}

#[test]
fn the_wish_is_what_the_subscriber_asked_for_not_what_it_was_granted() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let now = Instant::now();
    let tracks = [camera([150, 500, 1_500])];

    shared.pin_estimate(Some(Bitrate::kbps(200)));
    controller.on_report(&report(now, 0, 0), 0, now);
    controller.tick(&tracks, now);

    let desired = controller.desired_update();
    assert_eq!(
        desired,
        Some(Bitrate::kbps(1_500)),
        "the prober was told what we are spending instead of what we want"
    );
    assert_eq!(
        controller.desired_update(),
        desired,
        "a wish that was never delivered is still owed to the prober"
    );

    controller.desired_delivered(desired.expect("a wish was computed"));
    assert_eq!(
        controller.desired_update(),
        None,
        "a wish that has not moved is not pushed again"
    );
}

#[test]
fn a_wish_below_the_floor_is_still_a_wish_str0m_can_probe_for() {
    let config = config();
    let (_shared, mut controller) = controller(&config);
    let now = Instant::now();
    let tracks = [camera([10, 20, 30])];

    controller.tick(&tracks, now);

    assert_eq!(controller.desired_update(), Some(MIN_DESIRED));
}

#[test]
fn a_track_that_leaves_stops_being_allocated_for() {
    let config = config();
    let (_shared, mut controller) = controller(&config);
    let now = Instant::now();

    controller.tick(&[camera([50, 100, 150])], now);
    assert_eq!(controller.shared.snapshot().video_tracks, 1);

    let released = controller.tick(&[], now);
    assert_eq!(controller.shared.snapshot().video_tracks, 0);
    assert_eq!(controller.allowance(TRACK), None);
    assert!(
        released.is_empty(),
        "a track that was never capped has nothing to give back"
    );
}

#[test]
fn a_capped_track_that_leaves_gives_its_cap_back() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let start = Instant::now();
    let tracks = [camera([150, 500, 1_500])];

    shared.pin_estimate(Some(Bitrate::kbps(2_000)));
    controller.on_report(&report(start, 0, 0), 0, start);
    controller.tick(&tracks, start);

    for step in 1..=2 {
        let at = start + Duration::from_secs(step);
        controller.on_report(&report(at, 250_000 * step, 200_000 * step), 300 * step, at);
        controller.tick(&tracks, at);
    }

    shared.pin_estimate(Some(Bitrate::kbps(200)));

    for step in 3..=5 {
        let at = start + Duration::from_secs(step);
        controller.on_report(&report(at, 250_000 * step, 200_000 * step), 300 * step, at);
        controller.tick(&tracks, at);
    }

    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Capped(SpatialLayer::BASE))
    );

    let released = controller.tick(&[], start + Duration::from_secs(6));

    assert_eq!(
        released,
        vec![Grant {
            key: TRACK.to_owned(),
            allowance: Allowance::Unlimited,
        }],
        "a forwarder was left capped by an allocator that had forgotten it"
    );
}

#[test]
fn audio_is_reserved_off_the_top_and_never_capped() {
    let config = config();
    let (_shared, mut controller) = controller(&config);
    let now = Instant::now();
    let audio = SubscribedTrack {
        track_sid: "TR_mic".to_owned(),
        kind: TrackKind::Audio,
        source: mediacore::TrackSource::Microphone,
        multi_layer: false,
        carrying: true,
        available: LayerSet::all(),
        ceiling: Some(SpatialLayer::BASE),
        measured: [Some(Bitrate::kbps(20)), None, None],
        declared: [None; LAYER_COUNT],
    };

    controller.tick(&[audio], now);

    let snapshot = controller.shared.snapshot();
    assert_eq!(snapshot.audio_tracks, 1);
    assert_eq!(snapshot.video_tracks, 0);
    assert_eq!(
        snapshot.spend,
        Bitrate::kbps(config.audio_reserve_kbps.into()),
        "a quiet audio track still books the room it needs to speak"
    );
    assert_eq!(controller.allowance("TR_mic"), None);
}

#[test]
fn a_single_layer_screenshare_is_exempt_and_a_simulcast_one_outranks_a_camera() {
    let single = enrol(&SubscribedTrack {
        source: mediacore::TrackSource::ScreenShare,
        multi_layer: false,
        ..camera([150, 500, 1_500])
    });
    let multi = enrol(&SubscribedTrack {
        source: mediacore::TrackSource::ScreenShare,
        multi_layer: true,
        ..camera([150, 500, 1_500])
    });
    let camera = enrol(&camera([150, 500, 1_500]));

    assert!(single.exempt);
    assert!(!multi.exempt);
    assert_eq!(multi.priority, SCREENSHARE_PRIORITY);
    assert_eq!(camera.priority, CAMERA_PRIORITY);
    assert!(multi.priority > camera.priority);
}

struct Link {
    at: Instant,
    wire: u64,
    payload: u64,
    packets: u64,
}

impl Link {
    fn new(at: Instant) -> Self {
        Self {
            at,
            wire: 0,
            payload: 0,
            packets: 0,
        }
    }
}

fn drive(
    controller: &mut BandwidthController,
    tracks: &[SubscribedTrack],
    link: &mut Link,
    sending: Bitrate,
    seconds: u32,
) {
    drive_queued(controller, tracks, link, sending, Duration::ZERO, seconds);
}

fn drive_queued(
    controller: &mut BandwidthController,
    tracks: &[SubscribedTrack],
    link: &mut Link,
    sending: Bitrate,
    queue_age: Duration,
    seconds: u32,
) {
    let bytes = sending.as_bps() / 8;

    for _ in 0..seconds {
        link.at += Duration::from_secs(1);
        link.wire += bytes;
        link.payload += bytes * 9 / 10;
        link.packets += 1_000;

        controller.on_report(
            &backed_up(link.at, link.wire, link.payload, queue_age),
            link.packets,
            link.at,
        );
        controller.tick(tracks, link.at);
    }
}

fn priced(track_sid: &str, measured: [u64; LAYER_COUNT]) -> SubscribedTrack {
    SubscribedTrack {
        track_sid: track_sid.to_owned(),
        ..camera(measured)
    }
}

#[test]
fn a_link_that_dips_and_comes_back_returns_every_track_to_the_layer_it_asked_for() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let mut link = Link::new(Instant::now());
    let tracks = [
        priced("TR_big", [100, 800, 3_000]),
        priced("TR_one", [50, 150, 300]),
        priced("TR_two", [50, 150, 300]),
    ];

    controller.on_report(&report(link.at, 0, 0), 0, link.at);
    controller.tick(&tracks, link.at);

    shared.pin_estimate(Some(Bitrate::kbps(4_000)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(3_600), 3);

    assert_eq!(controller.state(), CongestionState::Clear);
    assert_eq!(controller.budget(), Bitrate::kbps(4_000));
    for track in &tracks {
        assert_eq!(
            controller.allowance(&track.track_sid),
            Some(Allowance::Unlimited),
            "a link with room for everything caps nothing"
        );
    }

    shared.pin_estimate(Some(Bitrate::kbps(300)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(3_600), 1);

    assert_eq!(
        controller.state(),
        CongestionState::Clear,
        "an estimate that fell with an empty send queue is a budget the allocator can honour, not a congested link"
    );
    assert_eq!(controller.budget(), Bitrate::kbps(300));
    assert_eq!(
        controller.allowance("TR_big"),
        Some(Allowance::Capped(SpatialLayer::BASE)),
        "the cut lands on the tick it is decided"
    );

    shared.pin_estimate(Some(Bitrate::kbps(3_700)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(300), 1);

    assert_eq!(
        [
            controller.allowance("TR_big"),
            controller.allowance("TR_one"),
            controller.allowance("TR_two"),
        ]
        .iter()
        .filter(|allowance| **allowance == Some(Allowance::Unlimited))
        .count(),
        1,
        "recovered headroom is handed to one track per tick, not to all of them at once"
    );

    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(300), 3);

    for track in &tracks {
        assert_eq!(
            controller.allowance(&track.track_sid),
            Some(Allowance::Unlimited),
            "{} never came back after the dip",
            track.track_sid
        );
    }
}

#[test]
fn a_link_worth_exactly_what_the_subscriber_asked_for_reaches_the_top_rung() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let mut link = Link::new(Instant::now());
    let tracks = [camera([150, 500, 1_500])];

    controller.on_report(&report(link.at, 0, 0), 0, link.at);
    controller.tick(&tracks, link.at);
    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Capped(SpatialLayer::new(1))),
        "the subscriber starts inside the configured initial bitrate and has to climb"
    );

    shared.pin_estimate(Some(Bitrate::kbps(1_500)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(1_500), 4);

    assert_eq!(controller.state(), CongestionState::Clear);
    assert_eq!(controller.budget(), controller.shared.snapshot().desired);
    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Unlimited),
        "the wish handed to the prober is the ladder itself, so an estimate that reaches the ladder must be enough to buy it"
    );
}

#[test]
fn a_backlog_that_outlives_a_cut_is_cut_again() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let mut link = Link::new(Instant::now());
    let tracks = [priced("TR_big", [100, 800, 3_000])];

    controller.on_report(&report(link.at, 0, 0), 0, link.at);
    shared.pin_estimate(Some(Bitrate::kbps(4_000)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(3_000), 3);
    assert_eq!(controller.allowance("TR_big"), Some(Allowance::Unlimited));

    drive_queued(
        &mut controller,
        &tracks,
        &mut link,
        Bitrate::kbps(3_000),
        Duration::from_millis(400),
        1,
    );
    assert_eq!(
        controller.state(),
        CongestionState::Hold,
        "the first sample of a backlog holds, it does not cut"
    );
    assert_eq!(
        controller.allowance("TR_big"),
        Some(Allowance::Unlimited),
        "a hold changes nothing"
    );

    drive_queued(
        &mut controller,
        &tracks,
        &mut link,
        Bitrate::kbps(3_000),
        Duration::from_millis(400),
        2,
    );

    assert_eq!(controller.state(), CongestionState::Congested);
    assert_eq!(controller.budget(), Bitrate::kbps(2_550));

    drive_queued(
        &mut controller,
        &tracks,
        &mut link,
        Bitrate::kbps(800),
        Duration::from_millis(400),
        2,
    );

    assert_eq!(
        controller.budget(),
        Bitrate::kbps(680),
        "the queue never drained, so the cut is repeated against what is now leaving"
    );
    assert_eq!(
        controller.allowance("TR_big"),
        Some(Allowance::Capped(SpatialLayer::BASE))
    );
}

#[test]
fn a_subscriber_held_below_what_it_asked_for_is_reported_even_on_a_clear_link() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let mut link = Link::new(Instant::now());
    let tracks = [camera([150, 500, 1_500])];

    controller.on_report(&report(link.at, 0, 0), 0, link.at);
    shared.pin_estimate(Some(Bitrate::kbps(400)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(150), 30);

    assert_eq!(controller.state(), CongestionState::Clear);
    assert_eq!(
        controller.allowance(TRACK),
        Some(Allowance::Capped(SpatialLayer::BASE))
    );
    assert!(
        !controller.is_stuck(),
        "a minute has not passed and nothing is wrong yet"
    );

    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(150), 40);

    assert!(
        controller.is_stuck(),
        "a subscriber capped for a minute with an estimate that never grows is the silent failure"
    );
}

#[test]
fn an_estimate_that_grows_again_clears_the_stuck_report() {
    let config = config();
    let (shared, mut controller) = controller(&config);
    let mut link = Link::new(Instant::now());
    let tracks = [camera([150, 500, 1_500])];

    controller.on_report(&report(link.at, 0, 0), 0, link.at);
    shared.pin_estimate(Some(Bitrate::kbps(400)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(150), 70);
    assert!(controller.is_stuck());

    shared.pin_estimate(Some(Bitrate::kbps(3_000)));
    drive(&mut controller, &tracks, &mut link, Bitrate::kbps(150), 2);

    assert!(!controller.is_stuck());
}

const WIFI_LOSS: f32 = 0.20;
const WIFI_ESTIMATE: Bitrate = Bitrate::kbps(666);
const WIRE_PACKET_BYTES: u64 = 1_100;
const BENCH_SECONDS: u32 = 600;

fn bench_tracks() -> Vec<SubscribedTrack> {
    let mut tracks = Vec::new();

    for publisher in ["alice", "bob", "carol"] {
        tracks.push(SubscribedTrack {
            track_sid: format!("TR_cam_{publisher}"),
            kind: TrackKind::Video,
            source: mediacore::TrackSource::Camera,
            multi_layer: true,
            carrying: true,
            available: LayerSet::all(),
            ceiling: Some(SpatialLayer::MAX),
            measured: [150, 500, 1_700].map(|kbps| Some(Bitrate::kbps(kbps))),
            declared: [None; LAYER_COUNT],
        });
        tracks.push(SubscribedTrack {
            track_sid: format!("TR_mic_{publisher}"),
            kind: TrackKind::Audio,
            source: mediacore::TrackSource::Microphone,
            multi_layer: false,
            carrying: true,
            available: LayerSet::all(),
            ceiling: Some(SpatialLayer::BASE),
            measured: [Some(Bitrate::kbps(30)), None, None],
            declared: [None; LAYER_COUNT],
        });
    }

    tracks
}

#[test]
fn the_bench_wifi_scenario_is_a_smaller_budget_and_not_a_congested_link() {
    let mut config = config();
    config.allocation_interval_ms = 250;
    let (_shared, mut controller) = controller(&config);
    let tracks = bench_tracks();
    let start = Instant::now();

    let mut wire = 0u64;
    let mut payload = 0u64;
    let mut packets = 0u64;
    let mut backlog = 0u64;
    let mut worst_queue = Duration::ZERO;
    let mut changes: HashMap<String, u32> = HashMap::new();
    let mut states: Vec<CongestionState> = Vec::new();
    let mut paused = 0usize;

    controller.on_report(&report(start, 0, 0), 0, start);

    for second in 1..=BENCH_SECONDS {
        let at = start + Duration::from_secs(second.into());
        let offered = controller.shared.snapshot().spend.as_bps();
        let drain = WIFI_ESTIMATE.as_bps();
        let leaving = (offered + backlog).min(drain);
        backlog = offered + backlog - leaving;

        let queue_age = Duration::from_secs_f64(backlog as f64 / drain as f64);
        worst_queue = worst_queue.max(queue_age);

        wire += leaving / 8;
        payload += leaving / 8 * (WIRE_PACKET_BYTES - 40) / WIRE_PACKET_BYTES;
        packets += leaving / 8 / WIRE_PACKET_BYTES;

        let mut report = backed_up(at, wire, payload, queue_age);
        report.bwe_tx = Some(WIFI_ESTIMATE.into());
        report.egress_loss = Some(WIFI_LOSS);
        report.rtt = Some(Duration::from_millis(20));

        controller.on_estimate(WIFI_ESTIMATE, at);
        controller.on_report(&report, packets, at);

        for slot in 0..4 {
            let tick = at + Duration::from_millis(250 * slot);

            for grant in controller.tick(&tracks, tick) {
                if grant.allowance.is_paused() {
                    paused += 1;
                }

                *changes.entry(grant.key).or_default() += 1;
            }
        }

        let state = controller.state();
        if !states.contains(&state) {
            states.push(state);
        }
    }

    assert_eq!(
        states,
        vec![CongestionState::Clear],
        "twenty percent loss on a five megabit link with nothing queued is not congestion"
    );
    assert_eq!(paused, 0, "no tile may report paused on this run");
    assert!(
        worst_queue < config.queue_pressure(),
        "the budget tracked the estimate closely enough that no backlog ever formed, worst was {worst_queue:?}"
    );

    assert_eq!(
        changes.len(),
        3,
        "every video track was allocated for at least once, so the run was not idle"
    );

    for (track, count) in &changes {
        assert!(
            *count <= 1,
            "{track} changed layer {count} times in ten minutes; the bench allows under two per minute"
        );
    }

    assert_eq!(
        controller.budget(),
        WIFI_ESTIMATE,
        "the budget is what the loss controller left of the link, and it is honoured rather than fought"
    );
}

#[test]
fn a_subscriber_registry_hands_out_one_handle_per_subscriber() {
    let subscribers = Subscribers::default();

    let first = subscribers.of("PA_bob");
    first.pin_estimate(Some(Bitrate::kbps(500)));

    assert_eq!(subscribers.of("PA_bob").pinned(), Some(Bitrate::kbps(500)));
    assert_eq!(subscribers.count(), 1);
    assert!(subscribers.get("PA_carol").is_none());

    subscribers.remove("PA_bob");
    assert_eq!(subscribers.count(), 0);
    assert!(subscribers.of("PA_bob").pinned().is_none());
}
