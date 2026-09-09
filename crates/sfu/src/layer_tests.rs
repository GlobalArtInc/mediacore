
use super::*;

#[test]
fn a_gated_latch_waits_for_a_keyframe_before_taking_a_layer() {
    let mut latch = LayerLatch::new(true);

    assert_eq!(
        latch.select(SpatialLayer::BASE, false),
        LayerDecision::AwaitKeyframe
    );
    assert!(!latch.is_latched());

    assert_eq!(
        latch.select(SpatialLayer::BASE, true),
        LayerDecision::Switch
    );
    assert_eq!(
        latch.select(SpatialLayer::BASE, false),
        LayerDecision::Forward
    );
}

#[test]
fn an_ungated_latch_takes_the_first_packet() {
    let mut latch = LayerLatch::new(false);

    assert_eq!(
        latch.select(SpatialLayer::BASE, false),
        LayerDecision::Switch
    );
    assert_eq!(latch.current(), Some(SpatialLayer::BASE));
}

#[test]
fn a_single_layer_never_switches_again() {
    let mut latch = LayerLatch::new(true);
    latch.select(SpatialLayer::BASE, true);

    for _ in 0..10 {
        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::Forward
        );
    }

    assert!(!latch.request(SpatialLayer::BASE));
    assert!(!latch.is_switching());
}

#[test]
fn a_requested_layer_is_taken_on_its_next_keyframe() {
    let mut latch = LayerLatch::new(true);
    latch.select(SpatialLayer::BASE, true);

    assert!(latch.request(SpatialLayer::new(1)));
    assert!(latch.is_switching());
    assert_eq!(
        latch.select(SpatialLayer::BASE, false),
        LayerDecision::Forward
    );
    assert_eq!(
        latch.select(SpatialLayer::new(1), false),
        LayerDecision::AwaitKeyframe
    );
    assert_eq!(
        latch.select(SpatialLayer::new(1), true),
        LayerDecision::Switch
    );
    assert_eq!(
        latch.select(SpatialLayer::BASE, false),
        LayerDecision::WrongLayer
    );
    assert!(!latch.is_switching());
}

#[test]
fn a_reset_latch_needs_a_keyframe_again() {
    let mut latch = LayerLatch::new(true);
    latch.select(SpatialLayer::BASE, true);
    latch.reset();

    assert_eq!(
        latch.select(SpatialLayer::BASE, false),
        LayerDecision::AwaitKeyframe
    );
}

#[test]
fn layer_indexes_are_clamped() {
    assert_eq!(SpatialLayer::new(9).index(), SpatialLayer::MAX_INDEX);
    assert_eq!(SpatialLayer::default(), SpatialLayer::BASE);
    assert_eq!(SpatialLayer::MAX.index(), SpatialLayer::MAX_INDEX);
}

#[test]
fn a_layer_set_keeps_its_members_in_order() {
    let mut set = LayerSet::EMPTY;

    assert!(set.is_empty());
    assert!(set.insert(SpatialLayer::new(2)));
    assert!(!set.insert(SpatialLayer::new(2)));
    set.insert(SpatialLayer::BASE);

    assert_eq!(set.len(), 2);
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        vec![SpatialLayer::BASE, SpatialLayer::new(2)]
    );
    assert_eq!(set.lowest(), Some(SpatialLayer::BASE));
    assert_eq!(set.highest(), Some(SpatialLayer::new(2)));
    assert_eq!(set.to_string(), "[0,2]");

    assert!(set.remove(SpatialLayer::new(2)));
    assert!(!set.remove(SpatialLayer::new(2)));
    assert_eq!(set.highest(), Some(SpatialLayer::BASE));
}

#[test]
fn a_ceiling_picks_the_best_layer_at_or_below_it() {
    let set: LayerSet = [SpatialLayer::BASE, SpatialLayer::new(2)]
        .into_iter()
        .collect();

    assert_eq!(
        set.highest_at_most(SpatialLayer::new(1)),
        Some(SpatialLayer::BASE)
    );
    assert_eq!(
        set.highest_at_most(SpatialLayer::new(2)),
        Some(SpatialLayer::new(2))
    );

    let high: LayerSet = [SpatialLayer::new(2)].into_iter().collect();
    assert_eq!(high.highest_at_most(SpatialLayer::BASE), None);
}

#[test]
fn upto_covers_every_layer_below_the_ceiling() {
    assert_eq!(LayerSet::upto(SpatialLayer::BASE).len(), 1);
    assert_eq!(LayerSet::upto(SpatialLayer::new(1)).len(), 2);
    assert_eq!(LayerSet::upto(SpatialLayer::MAX), LayerSet::all());
    assert_eq!(LayerSet::all().len(), LAYER_COUNT);
}
