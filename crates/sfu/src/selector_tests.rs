use super::*;

const LOW: SpatialLayer = SpatialLayer::BASE;

fn mid() -> SpatialLayer {
    SpatialLayer::new(1)
}

fn high() -> SpatialLayer {
    SpatialLayer::new(2)
}

fn all() -> LayerSet {
    LayerSet::all()
}

fn selector() -> LayerSelector {
    LayerSelector::new(SelectorConfig {
        upgrade_hold: Duration::from_secs(2),
    })
}

#[test]
fn the_first_decision_takes_the_wanted_layer_without_a_ramp() {
    let now = Instant::now();
    let mut selector = selector();

    assert_eq!(selector.update(all(), now), Some(high()));
    assert_eq!(selector.target(), Some(high()));
    assert_eq!(selector.update(all(), now), None);
}

#[test]
fn a_quality_request_is_honoured_immediately_in_both_directions() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);

    assert_eq!(
        selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now),
        Some(LOW)
    );
    assert_eq!(
        selector.set_preference(LayerPreference::Quality(VideoQuality::High), now),
        Some(high())
    );
    assert_eq!(
        selector.set_preference(LayerPreference::Quality(VideoQuality::High), now),
        None
    );
}

#[test]
fn a_layer_that_goes_away_is_left_immediately() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);

    let left = selector.update(LayerSet::from_iter([LOW, mid()]), now);

    assert_eq!(left, Some(mid()));
    assert_eq!(selector.target(), Some(mid()));
}

#[test]
fn the_first_climb_up_the_ladder_is_not_held_back() {
    let mut now = Instant::now();
    let mut selector = selector();

    assert_eq!(selector.update(LayerSet::of(LOW), now), Some(LOW));

    now += Duration::from_millis(20);
    assert_eq!(
        selector.update(LayerSet::from_iter([LOW, mid()]), now),
        Some(mid()),
        "a subscriber that has never been higher waited out the hold on its way up"
    );

    now += Duration::from_millis(20);
    assert_eq!(selector.update(all(), now), Some(high()));
    assert_eq!(selector.peak(), Some(high()));
    assert_eq!(selector.pending(), None);
}

#[test]
fn a_layer_that_comes_back_is_taken_only_after_the_hold() {
    let mut now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);
    selector.update(LayerSet::from_iter([LOW, mid()]), now);

    now += Duration::from_millis(100);
    assert_eq!(selector.update(all(), now), None);
    assert_eq!(selector.pending(), Some(high()));

    now += Duration::from_millis(1_000);
    assert_eq!(selector.update(all(), now), None);

    now += Duration::from_millis(1_000);
    assert_eq!(selector.update(all(), now), Some(high()));
    assert_eq!(selector.pending(), None);
}

#[test]
fn a_blinking_layer_never_pulls_the_subscriber_back_up() {
    let mut now = Instant::now();
    let mut selector = selector();
    let mut commits = Vec::new();

    commits.extend(selector.update(all(), now));
    commits.extend(selector.update(LayerSet::from_iter([LOW, mid()]), now));

    for _ in 0..10 {
        now += Duration::from_millis(700);
        commits.extend(selector.update(all(), now));
        now += Duration::from_millis(700);
        commits.extend(selector.update(LayerSet::from_iter([LOW, mid()]), now));
    }

    assert_eq!(commits, vec![high(), mid()]);
    assert_eq!(selector.target(), Some(mid()));
}

#[test]
fn a_pending_upgrade_matures_on_a_tick_alone() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);
    selector.update(LayerSet::from_iter([LOW]), now);

    assert_eq!(selector.update(all(), now), None);
    assert_eq!(selector.tick(now + Duration::from_millis(500)), None);
    assert_eq!(selector.tick(now + Duration::from_secs(3)), Some(high()));
}

#[test]
fn nothing_available_keeps_the_current_target() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);

    assert_eq!(selector.update(LayerSet::EMPTY, now), None);
    assert_eq!(selector.target(), Some(high()));
    assert_eq!(selector.desired(), None);
}

#[test]
fn a_ceiling_with_nothing_below_it_overshoots_to_the_nearest_layer() {
    let now = Instant::now();
    let mut selector = selector();
    selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now);

    assert_eq!(
        selector.update(LayerSet::from_iter([mid(), high()]), now),
        Some(mid())
    );
}

#[test]
fn quality_off_asks_for_no_layer_at_all() {
    let now = Instant::now();
    let mut selector = selector();

    selector.set_preference(LayerPreference::Quality(VideoQuality::Off), now);

    assert_eq!(selector.ceiling(), None);
    assert_eq!(selector.desired(), None);
    assert_eq!(selector.update(all(), now), None);
    assert_eq!(selector.target(), None);
}

#[test]
fn a_quality_request_lands_on_the_layer_the_publisher_declared_it_as() {
    let now = Instant::now();
    let mut selector = selector();
    let ladder = LayerLadder::new()
        .with_quality(LOW, VideoQuality::Medium)
        .with_quality(mid(), VideoQuality::High);

    selector.set_ladder(ladder, now);
    selector.update(LayerSet::from_iter([LOW, mid()]), now);

    assert_eq!(
        selector.set_preference(LayerPreference::Quality(VideoQuality::Medium), now),
        Some(LOW),
        "the publisher's medium encoding is the bottom rung, not the middle one"
    );
    assert_eq!(
        selector.set_preference(LayerPreference::Quality(VideoQuality::High), now),
        Some(mid())
    );
}

#[test]
fn an_unknown_layer_size_falls_back_to_the_top_layer() {
    let now = Instant::now();
    let mut selector = selector();

    selector.set_preference(
        LayerPreference::Size {
            width: 320,
            height: 180,
        },
        now,
    );

    assert_eq!(selector.ceiling(), Some(SpatialLayer::MAX));

    let ladder = LayerLadder::new()
        .with_size(LOW, 320, 180)
        .with_size(mid(), 640, 360)
        .with_size(high(), 1280, 720);

    assert_eq!(selector.set_ladder(ladder, now), None);
    assert_eq!(selector.ceiling(), Some(LOW));
    assert_eq!(selector.update(all(), now), Some(LOW));
}

#[test]
fn a_smaller_video_size_downgrades_at_once() {
    let now = Instant::now();
    let ladder = LayerLadder::new()
        .with_size(LOW, 320, 180)
        .with_size(mid(), 640, 360)
        .with_size(high(), 1280, 720);
    let mut selector = selector();
    selector.set_ladder(ladder, now);
    selector.update(all(), now);

    assert_eq!(
        selector.set_preference(
            LayerPreference::Size {
                width: 640,
                height: 360
            },
            now
        ),
        Some(mid())
    );
}

#[test]
fn an_allowance_caps_a_preference_but_never_raises_it() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);

    assert_eq!(
        selector.set_allowance(Allowance::Capped(LOW), now),
        Some(LOW)
    );
    assert_eq!(selector.ceiling(), Some(LOW));
    assert_eq!(
        selector.preferred_ceiling(),
        Some(high()),
        "the allowance is a second ceiling, not a new preference"
    );

    selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now);
    assert_eq!(
        selector.set_allowance(Allowance::Capped(high()), now),
        None,
        "an allowance above what the subscriber asked for changes nothing"
    );
    assert_eq!(selector.ceiling(), Some(LOW));
}

#[test]
fn a_lifted_allowance_returns_to_what_the_subscriber_asked_for() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);
    selector.set_allowance(Allowance::Capped(LOW), now);

    assert_eq!(
        selector.set_allowance(Allowance::Unlimited, now),
        Some(high()),
        "a subscriber that gets bandwidth back does not have to ask again"
    );
    assert_eq!(selector.allowance(), Allowance::Unlimited);
}

#[test]
fn a_capped_selector_does_not_overshoot_to_a_layer_above_the_cap() {
    let now = Instant::now();
    let mut selector = selector();
    selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now);
    selector.set_allowance(Allowance::Capped(LOW), now);

    assert_eq!(
        selector.update(LayerSet::from_iter([mid(), high()]), now),
        None
    );
    assert_eq!(selector.desired(), None);
    assert_eq!(selector.target(), None);

    selector.set_allowance(Allowance::Unlimited, now);
    assert_eq!(
        selector.desired(),
        Some(mid()),
        "without a budget the old overshoot is still what a subscriber gets"
    );
}

#[test]
fn a_paused_allowance_asks_for_no_layer_but_keeps_the_target_it_had() {
    let now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);

    assert_eq!(selector.set_allowance(Allowance::Paused, now), None);
    assert_eq!(selector.ceiling(), None);
    assert_eq!(selector.desired(), None);
    assert_eq!(selector.target(), Some(high()));
}

#[test]
fn an_allowance_change_bypasses_the_upgrade_hold() {
    let mut now = Instant::now();
    let mut selector = selector();
    selector.update(all(), now);
    selector.set_allowance(Allowance::Capped(LOW), now);

    now += Duration::from_millis(100);

    assert_eq!(
        selector.set_allowance(Allowance::Capped(mid()), now),
        Some(mid()),
        "an allocator grant is not a blinking publisher and must not wait out the hold"
    );
}
