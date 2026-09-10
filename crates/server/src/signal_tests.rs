use super::*;

#[test]
fn a_token_naming_only_sources_we_do_not_know_still_restricts() {
    let grant = VideoGrant {
        can_publish_sources: vec!["hologram".to_owned()],
        ..VideoGrant::default()
    };

    let sources = publish_sources(&grant);

    assert!(
        !sources.is_empty(),
        "an empty list means unrestricted, so dropping every unknown source turns a token that \
             restricts publishing into one that permits everything"
    );
    assert!(
        !sources.contains(&(mediacore::TrackSource::Camera as i32))
            && !sources.contains(&(mediacore::TrackSource::Microphone as i32))
            && !sources.contains(&(mediacore::TrackSource::ScreenShare as i32)),
        "an unknown source must match nothing publishable"
    );
}

#[test]
fn a_token_that_names_no_sources_stays_unrestricted() {
    assert!(publish_sources(&VideoGrant::default()).is_empty());
}

fn claims(video: VideoGrant, kind: &str) -> Claims {
    Claims {
        video,
        kind: kind.to_owned(),
        ..Claims::default()
    }
}

#[test]
fn a_recorder_and_an_agent_are_not_counted_as_people() {
    let recorder = VideoGrant {
        recorder: Some(true),
        ..VideoGrant::default()
    };
    let agent = VideoGrant {
        agent: Some(true),
        ..VideoGrant::default()
    };

    assert_eq!(
        kind_from(&claims(recorder, "")),
        ParticipantKind::Egress,
        "a recorder counted as a person keeps a room open after the last person left it"
    );
    assert_eq!(kind_from(&claims(agent, "")), ParticipantKind::Agent);
    assert_eq!(
        kind_from(&claims(VideoGrant::default(), "egress")),
        ParticipantKind::Egress,
        "a token that says what it is in the kind claim is telling us the same thing"
    );
    assert!(
        kind_from(&claims(VideoGrant::default(), "")).is_human(),
        "an ordinary token is somebody who came to talk"
    );
    assert_eq!(
        kind_from(&claims(VideoGrant::default(), "sip")),
        ParticipantKind::Sip,
        "`livekit-sip` reads this back off the roster to tell a second call from a person, \
         and two calls that both look like people wait for each other's audio until the \
         ringing timeout gives up on both"
    );
    assert!(
        kind_from(&claims(VideoGrant::default(), "sip")).is_human(),
        "a caller on a phone is still a person in the room"
    );
}

#[test]
fn a_session_only_claims_a_resume_when_the_client_says_it_is_one() {
    let query = |reconnect: Option<&str>, sid: Option<&str>| SignalQuery {
        access_token: String::new(),
        protocol: None,
        sdk: None,
        version: None,
        reconnect: reconnect.map(str::to_owned),
        sid: sid.map(str::to_owned),
        auto_subscribe: None,
    };

    assert_eq!(
        query(Some("1"), Some("PA_one")).resumes(),
        Some("PA_one".to_owned())
    );
    assert_eq!(
        query(None, Some("PA_one")).resumes(),
        None,
        "a fresh connection naming a session sid would evict whoever holds it"
    );
    assert_eq!(query(Some("1"), None).resumes(), None);
    assert_eq!(query(Some("1"), Some("")).resumes(), None);
}
