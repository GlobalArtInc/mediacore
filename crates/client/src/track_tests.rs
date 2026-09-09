use super::*;
use prost::Message as _;

#[test]
fn an_audio_spec_asks_for_a_microphone_track() {
    let spec = TrackSpec::audio("mic").with_cid("TR_fixed");
    let request = spec.to_request();

    assert_eq!(request.cid, "TR_fixed");
    assert_eq!(request.name, "mic");
    assert_eq!(request.r#type, mediacore::TrackType::Audio as i32);
    assert_eq!(request.source, mediacore::TrackSource::Microphone as i32);
    assert!(request.layers.is_empty());
    assert_eq!(request.simulcast_codecs[0].codec, "opus");
}

#[test]
fn a_video_spec_declares_one_layer_with_its_dimensions() {
    let request = TrackSpec::video("cam").to_request();

    assert_eq!(request.r#type, mediacore::TrackType::Video as i32);
    assert_eq!(request.source, mediacore::TrackSource::Camera as i32);
    assert_eq!(request.width, 320);
    assert_eq!(request.height, 240);
    assert_eq!(request.layers.len(), 1);
    assert_eq!(
        request.layers[0].quality,
        mediacore::VideoQuality::High as i32
    );
    assert_eq!(request.simulcast_codecs[0].codec, "vp8");
}

#[test]
fn a_simulcast_spec_declares_one_layer_per_encoding() {
    let spec = TrackSpec::simulcast_video("cam");
    let request = spec.to_request();

    assert_eq!(spec.rids(), vec!["q", "h", "f"]);
    assert_eq!(request.layers.len(), 3);
    assert_eq!(request.layers[0].rid, "q");
    assert_eq!(
        request.layers[0].quality,
        mediacore::VideoQuality::Low as i32
    );
    assert_eq!(request.layers[1].rid, "h");
    assert_eq!(
        request.layers[1].quality,
        mediacore::VideoQuality::Medium as i32
    );
    assert_eq!(request.layers[2].rid, "f");
    assert_eq!(
        request.layers[2].quality,
        mediacore::VideoQuality::High as i32
    );
    assert_eq!(request.layers[2].bitrate, 1_700_000);
}

#[test]
fn a_simulcast_spec_reports_the_top_layer_as_the_track_size() {
    let request = TrackSpec::simulcast_video("cam").to_request();

    assert_eq!(request.width, 1_280);
    assert_eq!(request.height, 720);
}

#[test]
fn each_layer_of_a_spec_can_be_looked_up_by_rid() {
    let spec = TrackSpec::simulcast_video("cam");

    assert_eq!(spec.layer("h").map(|layer| layer.height), Some(360));
    assert_eq!(spec.layer("nope"), None);
    assert!(spec.is_simulcast());
    assert!(!TrackSpec::video("cam").is_simulcast());
}

#[test]
fn cids_are_unique_per_spec() {
    assert_ne!(TrackSpec::audio("a").cid, TrackSpec::audio("a").cid);
}

#[test]
fn an_add_track_request_survives_a_protobuf_round_trip() {
    let request = TrackSpec::video("cam").with_cid("TR_fixed").to_request();
    let frame = mediacore::SignalRequest {
        message: Some(mediacore::signal_request::Message::AddTrack(
            request.clone(),
        )),
    };

    let bytes = frame.encode_to_vec();
    let decoded = mediacore::SignalRequest::decode(bytes.as_slice()).unwrap();

    match decoded.message.unwrap() {
        mediacore::signal_request::Message::AddTrack(decoded) => {
            assert_eq!(decoded.cid, request.cid);
            assert_eq!(decoded.r#type, request.r#type);
            assert_eq!(decoded.layers, request.layers);
        }
        other => panic!("expected an add_track request, got {other:?}"),
    }
}
