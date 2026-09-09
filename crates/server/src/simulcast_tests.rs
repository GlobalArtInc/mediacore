use super::*;

fn declared(entries: &[(mediacore::VideoQuality, &str, u32, u32)]) -> Vec<mediacore::VideoLayer> {
    entries
        .iter()
        .map(|(quality, rid, width, height)| mediacore::VideoLayer {
            quality: *quality as i32,
            rid: (*rid).to_owned(),
            width: *width,
            height: *height,
            ..Default::default()
        })
        .collect()
}

fn rids(names: &[&str]) -> Vec<Rid> {
    names.iter().map(|name| Rid::from(*name)).collect()
}

fn ladder() -> Vec<mediacore::VideoLayer> {
    declared(&[
        (mediacore::VideoQuality::Low, "q", 320, 180),
        (mediacore::VideoQuality::Medium, "h", 640, 360),
        (mediacore::VideoQuality::High, "f", 1280, 720),
    ])
}

fn nameless_ladder() -> Vec<mediacore::VideoLayer> {
    declared(&[
        (mediacore::VideoQuality::Low, "", 320, 180),
        (mediacore::VideoQuality::Medium, "", 640, 360),
        (mediacore::VideoQuality::High, "", 1280, 720),
    ])
}

#[test]
fn a_declared_ladder_maps_every_rid_to_its_own_layer() {
    let map = LayerMap::build(&ladder(), &rids(&["q", "h", "f"]));

    assert_eq!(map.len(), 3);
    assert!(map.is_simulcast());
    assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
    assert_eq!(
        map.layer_of(Some(Rid::from("h"))),
        Some(SpatialLayer::new(1))
    );
    assert_eq!(map.layer_of(Some(Rid::from("f"))), Some(SpatialLayer::MAX));
    assert_eq!(map.rid_of(SpatialLayer::MAX), Some(Rid::from("f")));
    assert_eq!(map.all(), LayerSet::all());
    assert!(map.surplus().is_empty());
}

#[test]
fn the_offer_order_does_not_decide_which_layer_is_the_low_one() {
    let map = LayerMap::build(&ladder(), &rids(&["f", "q", "h"]));

    assert_eq!(
        map.layer_of(Some(Rid::from("q"))),
        Some(SpatialLayer::BASE),
        "the quality the publisher declared decides, not the order it offered in"
    );
    assert_eq!(map.layer_of(Some(Rid::from("f"))), Some(SpatialLayer::MAX));
}

#[test]
fn a_publisher_that_names_no_rid_still_gives_every_layer_its_size() {
    let map = LayerMap::build(&nameless_ladder(), &rids(&["q", "h", "f"]));
    let ladder = map.ladder();

    assert_eq!(
        ladder.size(SpatialLayer::BASE),
        Some((320, 180)),
        "a real browser client never fills VideoLayer.rid, so every layer lost its size"
    );
    assert_eq!(ladder.size(SpatialLayer::new(1)), Some((640, 360)));
    assert_eq!(ladder.size(SpatialLayer::MAX), Some((1280, 720)));
    assert_eq!(ladder.ceiling_for(320, 180), Some(SpatialLayer::BASE));
    assert_eq!(ladder.ceiling_for(640, 360), Some(SpatialLayer::new(1)));
    assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Low);
}

#[test]
fn a_nameless_ladder_offered_out_of_order_still_lands_on_the_right_layers() {
    let map = LayerMap::build(&nameless_ladder(), &rids(&["f", "h", "q"]));

    assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
    assert_eq!(map.ladder().size(SpatialLayer::MAX), Some((1280, 720)));
}

#[test]
fn a_declaration_that_does_not_line_up_with_the_rids_is_not_guessed_at() {
    let map = LayerMap::build(&nameless_ladder(), &rids(&["h", "f"]));
    let ladder = map.ladder();

    assert_eq!(map.len(), 2);
    assert_eq!(
        ladder.size(SpatialLayer::BASE),
        None,
        "three nameless layers say nothing about which two encodings these rids are"
    );
    assert_eq!(ladder.size(SpatialLayer::new(1)), None);
    assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Low);
    assert_eq!(map.quality_of(SpatialLayer::new(1)), VideoQuality::Medium);
}

#[test]
fn a_declaration_that_names_its_rids_survives_a_shorter_offer() {
    let map = LayerMap::build(&ladder(), &rids(&["h", "f"]));
    let sizes = map.ladder();

    assert_eq!(sizes.size(SpatialLayer::BASE), Some((640, 360)));
    assert_eq!(sizes.size(SpatialLayer::new(1)), Some((1280, 720)));
    assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Medium);
    assert_eq!(map.quality_of(SpatialLayer::new(1)), VideoQuality::High);
}

#[test]
fn a_track_without_rids_is_described_by_the_top_layer_declared_for_it() {
    let map = LayerMap::build(&nameless_ladder(), &[]);

    assert_eq!(map.len(), 1);
    assert_eq!(
        map.ladder().size(SpatialLayer::BASE),
        Some((1280, 720)),
        "a single stream carries the full picture, not the smallest declared one"
    );
    assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::High);
}

#[test]
fn rids_nobody_declared_fall_back_to_the_well_known_ladder() {
    let map = LayerMap::build(&[], &rids(&["f", "q"]));

    assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
    assert_eq!(
        map.layer_of(Some(Rid::from("f"))),
        Some(SpatialLayer::new(1)),
        "two rids occupy the two lowest layers whatever they are called"
    );

    let unknown = LayerMap::build(&[], &rids(&["left", "right"]));
    assert_eq!(
        unknown.layer_of(Some(Rid::from("left"))),
        Some(SpatialLayer::BASE)
    );
    assert_eq!(
        unknown.layer_of(Some(Rid::from("right"))),
        Some(SpatialLayer::new(1))
    );
}

#[test]
fn a_ladder_deeper_than_this_server_carries_keeps_its_three_lowest_rids() {
    let map = LayerMap::build(&[], &rids(&["q", "h", "f", "x"]));

    assert_eq!(map.len(), 3, "a fourth rid was folded onto the top layer");
    assert_eq!(map.layer_of(Some(Rid::from("f"))), Some(SpatialLayer::MAX));
    assert_eq!(map.rid_of(SpatialLayer::MAX), Some(Rid::from("f")));
    assert_eq!(
        map.layer_of(Some(Rid::from("x"))),
        None,
        "a rid the ladder does not carry must not be counted as one that it does"
    );
    assert_eq!(map.surplus(), &rids(&["x"])[..]);
    assert_eq!(map.rids().len(), 3);
}

#[test]
fn a_track_without_rids_is_one_layer_and_answers_for_every_packet() {
    let map = LayerMap::build(
        &declared(&[(mediacore::VideoQuality::High, "", 1280, 720)]),
        &[],
    );

    assert_eq!(map.len(), 1);
    assert!(!map.is_simulcast());
    assert_eq!(map.layer_of(None), Some(SpatialLayer::BASE));
    assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
    assert_eq!(map.rid_of(SpatialLayer::BASE), None);
    assert!(map.rids().is_empty());
}

#[test]
fn the_reported_layers_carry_the_ssrc_of_the_stream_that_arrived() {
    let map = LayerMap::build(&ladder(), &rids(&["q", "h", "f"]));
    let mut ssrcs = [None; LAYER_COUNT];
    ssrcs[0] = Some(11);
    ssrcs[1] = Some(22);

    let present = LayerSet::from_iter([SpatialLayer::BASE, SpatialLayer::new(1)]);
    let layers = map.video_layers(present, &ssrcs);

    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].rid, "q");
    assert_eq!(layers[0].ssrc, 11);
    assert_eq!(layers[0].width, 320);
    assert_eq!(layers[0].quality, mediacore::VideoQuality::Low as i32);
    assert_eq!(layers[1].ssrc, 22);
    assert_eq!(layers[1].spatial_layer, 1);
}

#[test]
fn the_size_a_subscriber_asks_for_picks_the_layer_that_covers_it() {
    let ladder = LayerMap::build(&ladder(), &rids(&["q", "h", "f"])).ladder();

    assert_eq!(ladder.ceiling_for(320, 180), Some(SpatialLayer::BASE));
    assert_eq!(ladder.ceiling_for(640, 360), Some(SpatialLayer::new(1)));
    assert_eq!(ladder.ceiling_for(1920, 1080), Some(SpatialLayer::MAX));
}

#[test]
fn a_size_request_wins_over_the_quality_field() {
    let settings = mediacore::UpdateTrackSettings {
        quality: mediacore::VideoQuality::High as i32,
        width: 640,
        height: 360,
        ..Default::default()
    };

    assert_eq!(
        preference_of(&settings),
        LayerPreference::Size {
            width: 640,
            height: 360
        }
    );

    let settings = mediacore::UpdateTrackSettings {
        quality: mediacore::VideoQuality::Low as i32,
        ..Default::default()
    };
    assert_eq!(
        preference_of(&settings),
        LayerPreference::Quality(VideoQuality::Low)
    );
}

#[test]
fn the_publisher_is_told_about_every_quality_below_the_one_it_must_send() {
    let map = LayerMap::build(&ladder(), &rids(&["q", "h", "f"]));
    let qualities = map.subscribed_qualities(Some(SpatialLayer::new(1)));

    assert_eq!(qualities.len(), 3);
    assert!(qualities[0].enabled);
    assert!(qualities[1].enabled);
    assert!(!qualities[2].enabled);

    let off = map.subscribed_qualities(None);
    assert_eq!(off.len(), 3);
    assert!(off.iter().all(|quality| !quality.enabled));
}

#[test]
fn the_layer_a_quality_asks_for_is_the_layer_that_reports_that_quality() {
    let map = LayerMap::build(
        &declared(&[
            (mediacore::VideoQuality::Medium, "", 640, 360),
            (mediacore::VideoQuality::High, "", 1280, 720),
        ]),
        &rids(&["q", "h"]),
    );
    let ladder = map.ladder();

    for (asked, layer) in [
        (mediacore::VideoQuality::Low, SpatialLayer::BASE),
        (mediacore::VideoQuality::Medium, SpatialLayer::BASE),
        (mediacore::VideoQuality::High, SpatialLayer::new(1)),
    ] {
        let settings = mediacore::UpdateTrackSettings {
            quality: asked as i32,
            ..Default::default()
        };
        let LayerPreference::Quality(quality) = preference_of(&settings) else {
            panic!("a quality request must not turn into a size request");
        };

        assert_eq!(
            ladder.layer_of(quality),
            Some(layer),
            "a subscriber asking for {quality} landed on a layer the publisher never \
                 declared as {quality}"
        );
    }

    assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Medium);
    assert_eq!(map.quality_of(SpatialLayer::new(1)), VideoQuality::High);
}

#[test]
fn the_publisher_keeps_the_encoding_its_subscribers_are_actually_on() {
    let map = LayerMap::build(
        &declared(&[
            (mediacore::VideoQuality::Medium, "", 640, 360),
            (mediacore::VideoQuality::High, "", 1280, 720),
        ]),
        &rids(&["q", "h"]),
    );

    assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Medium);

    let qualities = map.subscribed_qualities(Some(SpatialLayer::BASE));

    assert!(qualities[0].enabled);
    assert!(
        qualities[1].enabled,
        "the publisher was told to stop the encoding its subscribers are receiving"
    );
    assert!(!qualities[2].enabled);

    let qualities = map.subscribed_qualities(Some(SpatialLayer::new(1)));
    assert!(qualities[1].enabled);
    assert!(qualities[2].enabled);
}
