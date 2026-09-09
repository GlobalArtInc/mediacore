use super::*;
use prost::Message as _;

fn encoded(message: signal_request::Message) -> signal_request::Message {
    let frame = mediacore::SignalRequest {
        message: Some(message),
    };
    let bytes = frame.encode_to_vec();
    mediacore::SignalRequest::decode(bytes.as_slice())
        .expect("a decodable request")
        .message
        .expect("a message")
}

#[test]
fn subscribing_asks_for_the_named_tracks() {
    let request = SubscriptionRequest::subscribe(["TR_a", "TR_b"]);
    let proto = request.to_proto();

    assert!(proto.subscribe);
    assert_eq!(proto.track_sids, vec!["TR_a", "TR_b"]);
    assert!(proto.participant_tracks.is_empty());
}

#[test]
fn unsubscribing_clears_the_subscribe_flag() {
    let proto = SubscriptionRequest::unsubscribe(["TR_a"]).to_proto();

    assert!(!proto.subscribe);
    assert_eq!(proto.track_sids, vec!["TR_a"]);
}

#[test]
fn participant_tracks_are_mirrored_into_the_flat_sid_list() {
    let request = SubscriptionRequest::subscribe(Vec::<String>::new())
        .from_participant("PA_bob", ["TR_a", "TR_b"])
        .from_participant("PA_carol", ["TR_c"]);
    let proto = request.to_proto();

    assert_eq!(proto.track_sids, vec!["TR_a", "TR_b", "TR_c"]);
    assert_eq!(proto.participant_tracks.len(), 2);
    assert_eq!(proto.participant_tracks[0].participant_sid, "PA_bob");
    assert_eq!(proto.participant_tracks[0].track_sids, vec!["TR_a", "TR_b"]);
    assert_eq!(proto.participant_tracks[1].track_sids, vec!["TR_c"]);
}

#[test]
fn a_sid_named_twice_is_listed_once() {
    let proto = SubscriptionRequest::subscribe(["TR_a"])
        .from_participant("PA_bob", ["TR_a"])
        .to_proto();

    assert_eq!(proto.track_sids, vec!["TR_a"]);
    assert_eq!(proto.participant_tracks[0].track_sids, vec!["TR_a"]);
}

#[test]
fn a_subscription_survives_a_protobuf_round_trip() {
    let message = SubscriptionRequest::subscribe(["TR_a"])
        .from_participant("PA_bob", ["TR_b"])
        .to_message();

    match encoded(message) {
        signal_request::Message::Subscription(update) => {
            assert!(update.subscribe);
            assert_eq!(update.track_sids, vec!["TR_a", "TR_b"]);
            assert_eq!(update.participant_tracks[0].participant_sid, "PA_bob");
        }
        other => panic!("expected a subscription request, got {other:?}"),
    }
}

#[test]
fn track_settings_ask_for_the_top_quality_unless_told_otherwise() {
    let settings = TrackSettings::for_tracks(["TR_a"]);
    assert_eq!(
        settings.to_proto().quality,
        mediacore::VideoQuality::High as i32
    );

    let low = settings.with_quality(mediacore::VideoQuality::Low);
    assert_eq!(low.to_proto().quality, mediacore::VideoQuality::Low as i32);
}

#[test]
fn asking_for_a_quality_and_asking_for_a_size_are_both_carried() {
    let message = TrackSettings::for_tracks(["TR_a"])
        .with_quality(mediacore::VideoQuality::Medium)
        .with_size(640, 360)
        .to_message();

    match encoded(message) {
        signal_request::Message::TrackSetting(settings) => {
            assert_eq!(settings.quality, mediacore::VideoQuality::Medium as i32);
            assert_eq!(settings.width, 640);
            assert_eq!(settings.height, 360);
        }
        other => panic!("expected track settings, got {other:?}"),
    }
}

#[test]
fn track_settings_carry_the_requested_size() {
    let message = TrackSettings::for_tracks(["TR_a"])
        .with_size(320, 240)
        .with_fps(15)
        .with_priority(1)
        .disabled(true)
        .to_message();

    match encoded(message) {
        signal_request::Message::TrackSetting(settings) => {
            assert_eq!(settings.track_sids, vec!["TR_a"]);
            assert!(settings.disabled);
            assert_eq!(settings.width, 320);
            assert_eq!(settings.height, 240);
            assert_eq!(settings.fps, 15);
            assert_eq!(settings.priority, 1);
        }
        other => panic!("expected track settings, got {other:?}"),
    }
}
