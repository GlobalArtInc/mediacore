use super::*;

fn permission(
    all_participants: bool,
    tracks: Vec<mediacore::TrackPermission>,
) -> mediacore::SubscriptionPermission {
    mediacore::SubscriptionPermission {
        all_participants,
        track_permissions: tracks,
    }
}

fn listener(
    sid: &str,
    identity: &str,
    all_tracks: bool,
    track_sids: &[&str],
) -> mediacore::TrackPermission {
    mediacore::TrackPermission {
        participant_sid: sid.to_owned(),
        all_tracks,
        track_sids: track_sids.iter().map(|sid| (*sid).to_owned()).collect(),
        participant_identity: identity.to_owned(),
    }
}

#[test]
fn a_publisher_who_said_nothing_admits_everybody() {
    let audience = Audience::default();

    assert!(audience.is_open());
    assert!(audience.admits("PA_other", "bob", "TR_camera"));
}

#[test]
fn naming_nobody_admits_nobody() {
    let mut audience = Audience::default();
    audience.set(&permission(false, Vec::new()));

    assert!(!audience.is_open());
    assert!(!audience.admits("PA_other", "bob", "TR_camera"));
}

#[test]
fn an_entry_naming_neither_a_sid_nor_an_identity_admits_nobody() {
    let mut audience = Audience::default();
    audience.set(&permission(false, vec![listener("", "", true, &[])]));

    assert!(!audience.admits("PA_other", "bob", "TR_camera"));
    assert!(!audience.admits("", "", "TR_camera"));
}

#[test]
fn a_listener_is_found_by_either_sid_or_identity() {
    let mut audience = Audience::default();
    audience.set(&permission(
        false,
        vec![
            listener("PA_bob", "", true, &[]),
            listener("", "carol", true, &[]),
        ],
    ));

    assert!(audience.admits("PA_bob", "somebody-else", "TR_camera"));
    assert!(audience.admits("PA_carol", "carol", "TR_camera"));
    assert!(!audience.admits("PA_dave", "dave", "TR_camera"));
}

#[test]
fn naming_tracks_admits_those_and_no_others() {
    let mut audience = Audience::default();
    audience.set(&permission(
        false,
        vec![listener("PA_bob", "bob", false, &["TR_microphone"])],
    ));

    assert!(audience.admits("PA_bob", "bob", "TR_microphone"));
    assert!(!audience.admits("PA_bob", "bob", "TR_camera"));
}

#[test]
fn saying_all_participants_opens_an_audience_that_was_closed() {
    let mut audience = Audience::default();
    audience.set(&permission(
        false,
        vec![listener("PA_bob", "bob", true, &[])],
    ));
    assert!(!audience.admits("PA_dave", "dave", "TR_camera"));

    audience.set(&permission(true, Vec::new()));

    assert!(audience.is_open());
    assert!(audience.admits("PA_dave", "dave", "TR_camera"));
}

#[test]
fn an_identity_is_matched_exactly_and_never_read_as_a_person() {
    let mut audience = Audience::default();
    audience.set(&permission(false, vec![listener("", "alice#2", true, &[])]));

    assert!(audience.admits("PA_second", "alice#2", "TR_camera"));
    assert!(!audience.admits("PA_first", "alice", "TR_camera"));
}
