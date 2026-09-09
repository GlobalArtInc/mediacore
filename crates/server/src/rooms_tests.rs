use super::*;
use mediacore_auth::VideoGrant;

fn claims(video: VideoGrant) -> Claims {
    Claims {
        video,
        ..Claims::default()
    }
}

#[test]
fn an_admin_token_is_an_admin_of_one_room_only() {
    let admin = claims(VideoGrant {
        room_admin: Some(true),
        room: "standup".to_owned(),
        ..VideoGrant::default()
    });

    assert!(ensure_admin(&admin, "standup").is_ok());
    assert_eq!(
        ensure_admin(&admin, "payroll").unwrap_err().code,
        twirp::Code::PermissionDenied,
        "an admin token that works on any room is a master key for the whole server"
    );
}

#[test]
fn a_join_token_administers_nothing() {
    let joiner = claims(VideoGrant {
        room_join: Some(true),
        room: "standup".to_owned(),
        ..VideoGrant::default()
    });

    assert!(
        ensure_admin(&joiner, "standup").is_err(),
        "roomJoin is the grant everyone in the meeting holds; it must not delete the meeting"
    );
    assert!(ensure_create(&joiner).is_err());
    assert!(ensure_list(&joiner).is_err());
}

#[test]
fn the_grants_do_not_stand_in_for_each_other() {
    let creator = claims(VideoGrant {
        room_create: Some(true),
        ..VideoGrant::default()
    });
    let lister = claims(VideoGrant {
        room_list: Some(true),
        ..VideoGrant::default()
    });

    assert!(ensure_create(&creator).is_ok());
    assert!(ensure_list(&creator).is_err());
    assert!(ensure_list(&lister).is_ok());
    assert!(ensure_create(&lister).is_err());
}

#[test]
fn mcns_room_record_carries_a_session_policy() {
    let policy = SessionPolicy::read(
        r#"{"sessionPolicy":{"minHumans":2,"endGraceMs":90000},"recordingEnabled":true}"#,
    );

    assert_eq!(policy.min_humans, Some(2));
    assert_eq!(
        policy.end_grace,
        Some(90),
        "the grace arrives in milliseconds and the room counts seconds"
    );
}

#[test]
fn metadata_that_says_nothing_about_a_session_leaves_the_policy_alone() {
    assert_eq!(SessionPolicy::read(""), SessionPolicy::default());
    assert_eq!(
        SessionPolicy::read("a note from the backoffice"),
        SessionPolicy::default(),
        "metadata is an opaque blob to everyone but its owner, and most of it is not JSON"
    );
    assert_eq!(
        SessionPolicy::read(r#"{"title":"Standup"}"#),
        SessionPolicy::default()
    );
    assert_eq!(
        SessionPolicy::read(r#"{"sessionPolicy":{"minHumans":2}}"#),
        SessionPolicy {
            min_humans: Some(2),
            end_grace: None,
            record: None,
        },
        "half a policy leaves the other half to the server configuration"
    );
}

/// The rule the recorder next door got wrong: a room where everybody sits
/// in one place and joins from one account has one human in it, and it
/// still has to be recorded.
#[test]
fn a_room_can_ask_to_be_recorded_with_one_person_in_it() {
    let policy = SessionPolicy::read(r#"{"sessionPolicy":{"minHumans":1,"record":true}}"#);

    assert_eq!(
        policy,
        SessionPolicy {
            min_humans: Some(1),
            end_grace: None,
            record: Some(true),
        },
        "one is a floor this server takes as written; a recorder with a minimum of its own \
             that could not go below two is how a meeting room's meetings went missing"
    );
}

#[test]
fn a_room_that_says_nothing_about_recording_leaves_it_to_the_server() {
    assert_eq!(
        SessionPolicy::read(r#"{"sessionPolicy":{"minHumans":2}}"#).record,
        None,
        "a room that never mentioned recording must not be read as having turned it off"
    );
    assert_eq!(
        SessionPolicy::read(r#"{"sessionPolicy":{"record":false}}"#).record,
        Some(false),
        "and one that turned it off has to be heard, on a server that records by default"
    );
}

#[test]
fn a_grace_that_does_not_divide_into_seconds_rounds_up() {
    assert_eq!(
        SessionPolicy::read(r#"{"sessionPolicy":{"endGraceMs":1500}}"#).end_grace,
        Some(2),
        "rounding down would end a session earlier than the caller asked for"
    );
    assert_eq!(
        SessionPolicy::read(r#"{"sessionPolicy":{"endGraceMs":1}}"#).end_grace,
        Some(1),
        "a grace shorter than a second is still a grace, not none at all"
    );
}
