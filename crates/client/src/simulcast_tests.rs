
use super::*;

#[test]
fn the_default_ladder_climbs_from_a_quarter_to_a_full_frame() {
    let layers = default_layers();

    assert_eq!(layers.len(), 3);
    assert_eq!(
        layers.iter().map(|l| l.rid.as_str()).collect::<Vec<_>>(),
        vec!["q", "h", "f"]
    );
    assert_eq!(
        layers.iter().map(|l| l.quality).collect::<Vec<_>>(),
        vec![
            mediacore::VideoQuality::Low,
            mediacore::VideoQuality::Medium,
            mediacore::VideoQuality::High
        ]
    );
    assert_eq!(layers[0].height, 180);
    assert_eq!(layers[2].width, 1_280);
}

#[test]
fn every_default_layer_carries_its_own_tag_and_size() {
    let layers = default_layers();

    assert_eq!(layers[0].tag(), Some(b'q'));
    assert_eq!(layers[1].tag(), Some(b'h'));
    assert_eq!(layers[2].tag(), Some(b'f'));

    assert_eq!(layers[0].profile.width, 320);
    assert_eq!(layers[0].profile.height, 180);
    assert!(layers[0].profile.payload_size < layers[1].profile.payload_size);
}

#[test]
fn a_layer_becomes_the_video_layer_the_server_reads() {
    let layer = LayerSpec::half().to_video_layer();

    assert_eq!(layer.quality, mediacore::VideoQuality::Medium as i32);
    assert_eq!(layer.width, 640);
    assert_eq!(layer.height, 360);
    assert_eq!(layer.bitrate, 500_000);
    assert_eq!(layer.rid, "h");
}

#[test]
fn a_payload_never_grows_past_a_single_packet() {
    assert_eq!(payload_size_for(1_700_000), MAX_PAYLOAD);
    assert_eq!(payload_size_for(1_000), MIN_PAYLOAD);
    assert_eq!(payload_size_for(150_000), 625);
}

#[test]
fn a_rid_str0m_would_silently_mangle_is_rejected() {
    assert!(matches!(
        LayerSpec::new("", mediacore::VideoQuality::Low, 320, 180, 1).validate(),
        Err(ClientError::InvalidRid(_))
    ));
    assert!(matches!(
        LayerSpec::new("far-too-long", mediacore::VideoQuality::Low, 320, 180, 1).validate(),
        Err(ClientError::InvalidRid(_))
    ));
    assert!(matches!(
        LayerSpec::new("a b", mediacore::VideoQuality::Low, 320, 180, 1).validate(),
        Err(ClientError::InvalidRid(_))
    ));
    assert!(
        LayerSpec::new("hi720", mediacore::VideoQuality::High, 1, 1, 1)
            .validate()
            .is_ok()
    );
}

#[test]
fn the_same_rid_twice_on_one_track_is_refused() {
    let layers = vec![LayerSpec::quarter(), LayerSpec::quarter()];

    assert!(matches!(
        validate_layers(&layers),
        Err(ClientError::DuplicateRid(rid)) if rid == "q"
    ));
    assert!(validate_layers(&default_layers()).is_ok());
}
