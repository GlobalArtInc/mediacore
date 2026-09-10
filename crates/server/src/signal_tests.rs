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

fn signal_query(access_token: &str) -> SignalQuery {
    SignalQuery {
        access_token: access_token.to_owned(),
        protocol: None,
        sdk: None,
        version: None,
        reconnect: None,
        sid: None,
        auto_subscribe: None,
    }
}

/// A browser cannot set a header on a websocket handshake, so the pass has to
/// be readable out of the address. Every server SDK used to send it there too
/// and the newer ones no longer do -- `server-sdk-go` 2.18 sends
/// `Authorization: Bearer` on both the handshake and the validate request and
/// nothing in the query -- so a server that reads only one of the two answers
/// 400 to an entire family of clients.
#[test]
fn a_pass_is_read_from_the_address_or_from_the_authorization_header() {
    let empty = HeaderMap::new();
    assert_eq!(
        token_from(&signal_query("from-the-address"), &empty),
        Some("from-the-address"),
        "a browser has nowhere else to put it"
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer from-the-header".parse().unwrap(),
    );
    assert_eq!(
        token_from(&signal_query(""), &headers),
        Some("from-the-header"),
        "a client that only sets the header is refused by a server that only reads the query"
    );
    assert_eq!(
        token_from(&signal_query("from-the-address"), &headers),
        Some("from-the-header"),
        "a client that sent both meant the one it went out of its way to set"
    );
}

/// Optional in the query is not optional overall: a connection with a pass in
/// neither place has to be refused, or making room for the header would have
/// opened every room to anybody who asked.
#[test]
fn a_connection_carrying_no_pass_at_all_is_refused() {
    assert_eq!(token_from(&signal_query(""), &HeaderMap::new()), None);

    let mut basic = HeaderMap::new();
    basic.insert(
        axum::http::header::AUTHORIZATION,
        "Basic aGVsbG86d29ybGQ=".parse().unwrap(),
    );
    assert_eq!(
        token_from(&signal_query(""), &basic),
        None,
        "a header that is not a bearer token is not a pass"
    );
}
