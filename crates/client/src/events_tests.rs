
use super::*;

#[test]
fn the_unknown_subscription_code_reads_as_success() {
    assert_eq!(subscription_error(0), None);
}

#[test]
fn a_known_subscription_code_is_reported() {
    assert_eq!(
        subscription_error(mediacore::SubscriptionError::SeTrackNotfound as i32),
        Some(mediacore::SubscriptionError::SeTrackNotfound)
    );
}

#[test]
fn an_unmapped_subscription_code_falls_back_to_unknown() {
    assert_eq!(
        subscription_error(9_999),
        Some(mediacore::SubscriptionError::SeUnknown)
    );
}

#[test]
fn a_remote_track_exposes_its_sid_and_type() {
    let track = RemoteTrack {
        participant_sid: "PA_bob".to_owned(),
        participant_identity: "bob".to_owned(),
        info: mediacore::TrackInfo {
            sid: "TR_a".to_owned(),
            name: "mic".to_owned(),
            r#type: mediacore::TrackType::Audio as i32,
            ..mediacore::TrackInfo::default()
        },
    };

    assert_eq!(track.sid(), "TR_a");
    assert_eq!(track.name(), "mic");
    assert_eq!(track.track_type(), mediacore::TrackType::Audio);
}
