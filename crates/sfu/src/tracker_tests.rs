use super::*;

const BASE: SpatialLayer = SpatialLayer::BASE;

fn high() -> SpatialLayer {
    SpatialLayer::new(2)
}

fn fast() -> TrackerConfig {
    let layer = LayerTrackerConfig {
        samples_required: 2,
        cycles_required: 3,
    };

    TrackerConfig {
        cycle: Duration::from_millis(100),
        rate_window: Duration::from_millis(200),
        layers: [layer; LAYER_COUNT],
    }
}

fn feed(
    tracker: &mut LayerTracker,
    layer: SpatialLayer,
    packets: u32,
    from: Instant,
    step: Duration,
) -> Instant {
    let mut at = from;
    for _ in 0..packets {
        tracker.observe(layer, 100, at);
        at += step;
    }
    at
}

#[test]
fn the_first_packet_of_a_layer_declares_it_active() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    let changes = tracker.observe(BASE, 120, now);

    assert!(changes.activated.contains(BASE));
    assert!(tracker.is_active(BASE));
    assert_eq!(tracker.active(), LayerSet::of(BASE));
    assert_eq!(tracker.packets(BASE), 1);
    assert_eq!(tracker.bytes(BASE), 120);
}

#[test]
fn padding_does_not_keep_a_layer_alive() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    assert!(tracker.observe(BASE, 0, now).is_empty());
    assert!(!tracker.is_active(BASE));
    assert_eq!(tracker.packets(BASE), 0);
}

#[test]
fn a_layer_that_stops_arriving_is_declared_gone_after_one_cycle() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    let at = feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));
    assert!(tracker.is_active(BASE));

    let changes = tracker.tick(at + Duration::from_millis(200));

    assert!(changes.stopped.contains(BASE));
    assert!(!tracker.is_active(BASE));
    assert!(tracker.active().is_empty());
    assert!(tracker.seen().contains(BASE));
}

#[test]
fn a_thin_layer_is_declared_gone_even_while_a_trickle_arrives() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    tracker.observe(high(), 100, now);
    assert!(tracker.is_active(high()));

    let changes = tracker.observe(high(), 100, now + Duration::from_millis(150));

    assert!(changes.stopped.contains(high()));
    assert!(!tracker.is_active(high()));
}

#[test]
fn a_layer_that_resumes_needs_several_good_cycles_before_it_counts() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));
    tracker.tick(now + Duration::from_millis(300));
    assert!(!tracker.is_active(BASE));

    let mut at = now + Duration::from_millis(300);
    let mut activated = None;

    for cycle in 0..5 {
        for _ in 0..4 {
            let changes = tracker.observe(BASE, 100, at);
            if changes.activated.contains(BASE) && activated.is_none() {
                activated = Some(cycle);
            }
            at += Duration::from_millis(25);
        }
    }

    assert_eq!(
        activated,
        Some(3),
        "the layer must survive three full cycles before it is trusted again"
    );
    assert!(tracker.is_active(BASE));
}

#[test]
fn one_layer_stopping_leaves_the_others_alone() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    let mut at = now;
    for _ in 0..12 {
        tracker.observe(BASE, 100, at);
        tracker.observe(high(), 100, at);
        at += Duration::from_millis(20);
    }

    assert_eq!(tracker.active(), LayerSet::from_iter([BASE, high()]));

    for _ in 0..12 {
        tracker.observe(BASE, 100, at);
        at += Duration::from_millis(20);
    }

    assert_eq!(tracker.active(), LayerSet::of(BASE));
    assert!(!tracker.is_active(high()));
}

#[test]
fn a_long_silence_reports_the_stop_once() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));

    let changes = tracker.tick(now + Duration::from_secs(30));
    assert!(changes.stopped.contains(BASE));

    assert!(tracker.tick(now + Duration::from_secs(60)).is_empty());
}

#[test]
fn a_layer_that_starts_mid_cycle_is_judged_on_a_cycle_of_its_own() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    feed(&mut tracker, BASE, 5, now, Duration::from_millis(20));

    let changes = tracker.observe(high(), 100, now + Duration::from_millis(90));
    assert!(changes.activated.contains(high()));

    let changes = tracker.observe(BASE, 100, now + Duration::from_millis(100));
    assert!(
        !changes.stopped.contains(high()),
        "the layer was judged on a cycle that started before it existed"
    );
    assert!(tracker.is_active(high()));

    let mut at = now + Duration::from_millis(110);
    for _ in 0..4 {
        tracker.observe(BASE, 100, at);
        tracker.observe(high(), 100, at);
        at += Duration::from_millis(20);
    }

    assert!(tracker.tick(now + Duration::from_millis(200)).is_empty());
    assert!(tracker.is_active(high()));
    assert!(tracker.is_active(BASE));
}

#[test]
fn a_layer_put_back_to_fresh_is_trusted_by_its_next_packet() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    feed(&mut tracker, high(), 4, now, Duration::from_millis(20));
    tracker.tick(now + Duration::from_millis(300));

    assert!(!tracker.is_active(high()));
    assert!(tracker.seen().contains(high()));

    assert!(tracker.reset_layer(high()).is_empty());
    assert!(!tracker.seen().contains(high()));

    let changes = tracker.observe(high(), 100, now + Duration::from_millis(320));

    assert!(changes.activated.contains(high()));
    assert!(tracker.is_active(high()));
    assert_eq!(
        tracker.packets(high()),
        5,
        "the layer counters belong to the track, not to one run of it"
    );
}

#[test]
fn resetting_one_layer_leaves_the_others_where_they_are() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    tracker.observe(BASE, 100, now);
    tracker.observe(high(), 100, now);
    assert_eq!(tracker.active(), LayerSet::from_iter([BASE, high()]));

    let changes = tracker.reset_layer(high());

    assert!(changes.stopped.contains(high()));
    assert!(!changes.stopped.contains(BASE));
    assert_eq!(tracker.active(), LayerSet::of(BASE));
}

#[test]
fn pausing_the_tracker_drops_every_layer_and_starts_over() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(fast());

    feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));
    let changes = tracker.set_paused(true);

    assert!(changes.stopped.contains(BASE));
    assert!(tracker.active().is_empty());
    assert!(
        tracker
            .observe(BASE, 100, now + Duration::from_millis(100))
            .is_empty()
    );
    assert!(!tracker.is_active(BASE));

    tracker.set_paused(false);
    let changes = tracker.observe(BASE, 100, now + Duration::from_millis(200));
    assert!(changes.activated.contains(BASE));
}

#[test]
fn a_layer_is_priced_from_what_actually_arrived() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(TrackerConfig::video());

    assert_eq!(tracker.rate(BASE, 40, now), None);

    let at = feed(&mut tracker, BASE, 100, now, Duration::from_millis(10));

    assert_eq!(
        tracker.rate(BASE, 40, at),
        Some(Bitrate::bps((100 + 40) * 8 * 100)),
        "the price of a layer is its payload plus its per-packet wire overhead"
    );
    assert_eq!(
        tracker.rate(SpatialLayer::new(1), 40, at),
        None,
        "a layer nobody published has no measured price"
    );
}

#[test]
fn a_screenshare_is_priced_over_a_longer_window_than_a_camera() {
    assert_eq!(TrackerConfig::video().rate_window, Duration::from_secs(1));
    assert_eq!(
        TrackerConfig::screenshare().rate_window,
        Duration::from_secs(4)
    );
}

#[test]
fn resetting_a_layer_forgets_its_price_but_keeps_its_window() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(TrackerConfig::video());
    let at = feed(&mut tracker, BASE, 100, now, Duration::from_millis(10));

    assert!(tracker.rate(BASE, 40, at).is_some());
    tracker.reset_layer(BASE);
    assert_eq!(tracker.rate(BASE, 40, at), None);

    let at = feed(&mut tracker, BASE, 100, at, Duration::from_millis(10));
    assert!(
        tracker.rate(BASE, 40, at).is_some(),
        "a reset layer must be able to be priced again"
    );
}

#[test]
fn every_rate_is_reported_at_once() {
    let now = Instant::now();
    let mut tracker = LayerTracker::new(TrackerConfig::video());
    let at = feed(&mut tracker, BASE, 100, now, Duration::from_millis(10));

    let rates = tracker.rates(40, at);

    assert_eq!(rates[0], tracker.rate(BASE, 40, at));
    assert_eq!(rates[1], None);
    assert_eq!(rates[2], None);
}
