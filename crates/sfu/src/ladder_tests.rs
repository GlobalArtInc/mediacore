use super::*;

const LOW: SpatialLayer = SpatialLayer::BASE;

fn mid() -> SpatialLayer {
    SpatialLayer::new(1)
}

fn high() -> SpatialLayer {
    SpatialLayer::new(2)
}

#[test]
fn an_undeclared_ladder_falls_back_to_the_well_known_one() {
    let ladder = LayerLadder::new();

    assert!(ladder.is_empty());
    assert_eq!(ladder.quality_of(LOW), VideoQuality::Low);
    assert_eq!(ladder.quality_of(mid()), VideoQuality::Medium);
    assert_eq!(ladder.quality_of(high()), VideoQuality::High);
    assert_eq!(ladder.layer_of(VideoQuality::Low), Some(LOW));
    assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(mid()));
    assert_eq!(ladder.layer_of(VideoQuality::High), Some(high()));
    assert_eq!(ladder.layer_of(VideoQuality::Off), None);
}

#[test]
fn every_declared_rung_answers_for_the_quality_it_was_declared_with() {
    let ladders = [
        vec![
            (LOW, VideoQuality::Low),
            (mid(), VideoQuality::Medium),
            (high(), VideoQuality::High),
        ],
        vec![(LOW, VideoQuality::Medium), (mid(), VideoQuality::High)],
        vec![(LOW, VideoQuality::Low), (mid(), VideoQuality::High)],
        vec![(LOW, VideoQuality::Low), (mid(), VideoQuality::Medium)],
        vec![(LOW, VideoQuality::High)],
    ];

    for declared in ladders {
        let ladder = declared
            .iter()
            .fold(LayerLadder::new(), |ladder, (layer, quality)| {
                ladder.with_quality(*layer, *quality)
            });

        for (layer, quality) in declared {
            assert_eq!(
                ladder.quality_of(layer),
                quality,
                "the ladder forgot what layer {layer} was declared as"
            );
            assert_eq!(
                ladder.layer_of(quality),
                Some(layer),
                "layer {layer} was declared {quality}, but asking for {quality} landed elsewhere"
            );
        }
    }
}

#[test]
fn a_short_ladder_answers_from_what_the_publisher_declared() {
    let ladder = LayerLadder::new()
        .with_quality(LOW, VideoQuality::Medium)
        .with_quality(mid(), VideoQuality::High);

    assert_eq!(
        ladder.layer_of(VideoQuality::Medium),
        Some(LOW),
        "the publisher's medium encoding is the bottom rung of this ladder"
    );
    assert_eq!(ladder.layer_of(VideoQuality::High), Some(mid()));
    assert_eq!(
        ladder.layer_of(VideoQuality::Low),
        Some(LOW),
        "a quality below everything published is answered by the smallest rung"
    );
    assert_eq!(ladder.quality_of(LOW), VideoQuality::Medium);
    assert_eq!(ladder.quality_of(mid()), VideoQuality::High);
}

#[test]
fn a_quality_above_the_ladder_is_answered_by_its_top_rung() {
    let ladder = LayerLadder::new()
        .with_quality(LOW, VideoQuality::Low)
        .with_quality(mid(), VideoQuality::Medium);

    assert_eq!(ladder.layer_of(VideoQuality::High), Some(mid()));
    assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(mid()));
    assert_eq!(ladder.layer_of(VideoQuality::Low), Some(LOW));
}

#[test]
fn a_quality_between_two_rungs_rounds_up() {
    let ladder = LayerLadder::new()
        .with_quality(LOW, VideoQuality::Low)
        .with_quality(mid(), VideoQuality::High);

    assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(mid()));
}

#[test]
fn a_single_rung_answers_every_quality() {
    let ladder = LayerLadder::new().with_quality(LOW, VideoQuality::High);

    assert_eq!(ladder.layer_of(VideoQuality::Low), Some(LOW));
    assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(LOW));
    assert_eq!(ladder.layer_of(VideoQuality::High), Some(LOW));
    assert_eq!(ladder.layer_of(VideoQuality::Off), None);
}

#[test]
fn a_video_size_maps_to_the_smallest_rung_that_covers_it() {
    let ladder = LayerLadder::new()
        .with_size(LOW, 320, 180)
        .with_size(mid(), 640, 360)
        .with_size(high(), 1280, 720);

    assert_eq!(ladder.ceiling_for(320, 180), Some(LOW));
    assert_eq!(ladder.ceiling_for(400, 240), Some(mid()));
    assert_eq!(ladder.ceiling_for(640, 360), Some(mid()));
    assert_eq!(ladder.ceiling_for(1920, 1080), Some(high()));
    assert_eq!(ladder.ceiling_for(1280, 720), Some(high()));
    assert_eq!(LayerLadder::new().ceiling_for(320, 180), None);
}

#[test]
fn a_rung_without_a_size_is_not_offered_to_a_size_request() {
    let ladder = LayerLadder::new()
        .with_quality(LOW, VideoQuality::Low)
        .with_size(mid(), 640, 360)
        .with_size(LOW, 0, 0);

    assert!(!ladder.is_empty());
    assert_eq!(ladder.size(LOW), None);
    assert_eq!(ladder.ceiling_for(320, 180), Some(mid()));
}

#[test]
fn a_declared_bitrate_is_carried_through_the_ladder() {
    let ladder = LayerLadder::new()
        .with_bitrate(LOW, 150_000)
        .with_bitrate(mid(), 0);

    assert_eq!(ladder.declared_bitrate(LOW), Some(Bitrate::kbps(150)));
    assert_eq!(
        ladder.declared_bitrate(mid()),
        None,
        "a publisher that declared nothing declared nothing, not zero"
    );
    assert_eq!(ladder.declared_bitrate(high()), None);
    assert_eq!(
        ladder.declared_bitrates(),
        [Some(Bitrate::kbps(150)), None, None]
    );
    assert!(!ladder.is_empty());
}
