use super::*;

const LOW: SpatialLayer = SpatialLayer::BASE;

fn ladder() -> LayerLadder {
    LayerLadder::new()
        .with_quality(LOW, VideoQuality::Low)
        .with_quality(mid(), VideoQuality::Medium)
        .with_quality(high(), VideoQuality::High)
}

fn mid() -> SpatialLayer {
    SpatialLayer::new(1)
}

fn high() -> SpatialLayer {
    SpatialLayer::new(2)
}

#[test]
fn nobody_subscribed_means_the_publisher_can_stop() {
    let demand: TrackDemand<&str> = TrackDemand::new();

    assert!(demand.is_empty());
    assert!(demand.wanted().is_empty());
    assert!(demand.enabled().is_empty());
    assert_eq!(demand.quality(&ladder()), VideoQuality::Off);
}

#[test]
fn the_wanted_set_is_the_union_of_what_subscribers_ask_for() {
    let mut demand = TrackDemand::new();

    assert!(demand.set("alice", Some(LOW)));
    assert!(demand.set("bob", Some(high())));
    assert!(!demand.set("carol", Some(high())));

    assert_eq!(demand.wanted(), LayerSet::from_iter([LOW, high()]));
    assert_eq!(demand.enabled(), LayerSet::all());
    assert_eq!(demand.quality(&ladder()), VideoQuality::High);
    assert_eq!(demand.subscribers(), 3);
}

#[test]
fn a_subscriber_that_wants_nothing_does_not_hold_a_layer_open() {
    let mut demand = TrackDemand::new();
    demand.set("alice", Some(mid()));

    assert!(demand.set("alice", None));
    assert!(demand.wanted().is_empty());
    assert_eq!(demand.quality(&ladder()), VideoQuality::Off);
    assert_eq!(demand.subscribers(), 1);
}

#[test]
fn the_last_subscriber_of_a_layer_leaving_closes_it() {
    let mut demand = TrackDemand::new();
    demand.set("alice", Some(LOW));
    demand.set("bob", Some(high()));

    assert!(demand.remove(&"bob"));
    assert_eq!(demand.wanted(), LayerSet::of(LOW));
    assert_eq!(demand.enabled(), LayerSet::of(LOW));
    assert_eq!(demand.quality(&ladder()), VideoQuality::Low);

    assert!(!demand.remove(&"bob"));
    assert!(demand.clear());
    assert!(demand.wanted().is_empty());
}

#[test]
fn the_quality_reported_to_the_publisher_is_the_one_it_declared() {
    let short = LayerLadder::new()
        .with_quality(LOW, VideoQuality::Medium)
        .with_quality(mid(), VideoQuality::High);
    let mut demand = TrackDemand::new();
    demand.set("alice", Some(LOW));

    assert_eq!(
        demand.quality(&short),
        VideoQuality::Medium,
        "the bottom rung of this ladder is the publisher's medium encoding"
    );
    assert_eq!(demand.quality(&ladder()), VideoQuality::Low);
}

#[test]
fn enabling_a_layer_enables_everything_below_it() {
    let mut demand = TrackDemand::new();
    demand.set("alice", Some(high()));

    assert_eq!(demand.wanted(), LayerSet::of(high()));
    assert_eq!(demand.enabled(), LayerSet::all());
}
