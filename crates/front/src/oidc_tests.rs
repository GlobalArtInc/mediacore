
use super::*;

#[test]
fn the_authorize_url_carries_everything_the_flow_needs() {
    let url = authorize_url(
        "https://sso.example.test/authorize",
        &Authorize {
            client_id: "a-client",
            redirect_uri: "https://rooms.example.test/auth/callback",
            scopes: DEFAULT_SCOPES,
            state: "a-state",
            nonce: "a-nonce",
            challenge: "a-challenge",
        },
    )
    .unwrap();

    let parsed = Url::parse(&url).unwrap();
    let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

    assert_eq!(
        parsed.origin(),
        Url::parse("https://sso.example.test").unwrap().origin()
    );
    assert_eq!(parsed.path(), "/authorize");
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["response_mode"], "query");
    assert_eq!(query["client_id"], "a-client");
    assert_eq!(
        query["redirect_uri"],
        "https://rooms.example.test/auth/callback"
    );
    assert_eq!(query["scope"], DEFAULT_SCOPES);
    assert_eq!(query["state"], "a-state");
    assert_eq!(query["nonce"], "a-nonce");
    assert_eq!(query["code_challenge"], "a-challenge");
    assert_eq!(query["code_challenge_method"], "S256");
}

/// Anything but `code` is a flow where a token comes back through the
/// browser, which is the flow this deliberately does not implement.
#[test]
fn nothing_but_the_code_flow_is_ever_asked_for() {
    assert_eq!(RESPONSE_TYPE, "code");
    assert_eq!(RESPONSE_MODE, "query");
    assert_eq!(CODE_CHALLENGE_METHOD, "S256");
    assert!(
        !DEFAULT_SCOPES.contains("offline_access"),
        "a refresh token is asked for that nothing here ever spends"
    );
    assert!(DEFAULT_SCOPES.starts_with("openid"));
}

/// A `&` or a `?` in any of these has to come out as one parameter. The
/// redirect URI is the one that really carries a query in the wild.
#[test]
fn a_parameter_with_punctuation_in_it_stays_one_parameter() {
    let url = authorize_url(
        "https://sso.example.test/authorize?tenant=one",
        &Authorize {
            client_id: "a&b",
            redirect_uri: "https://rooms.example.test/auth/callback?next=/r/a b",
            scopes: "openid profile",
            state: "a-state",
            nonce: "a-nonce",
            challenge: "a-challenge",
        },
    )
    .unwrap();

    let parsed = Url::parse(&url).unwrap();
    let query: Vec<_> = parsed.query_pairs().into_owned().collect();

    assert!(query.contains(&("client_id".to_owned(), "a&b".to_owned())));
    assert!(query.contains(&(
        "redirect_uri".to_owned(),
        "https://rooms.example.test/auth/callback?next=/r/a b".to_owned()
    )));
    assert!(
        query.contains(&("tenant".to_owned(), "one".to_owned())),
        "the endpoint's own query was thrown away: {url}"
    );
    assert_eq!(
        query.iter().filter(|(name, _)| name == "scope").count(),
        1,
        "a parameter was split in two: {url}"
    );
}

#[test]
fn an_authorization_endpoint_that_is_not_a_url_is_refused() {
    assert!(matches!(
        authorize_url(
            "not a url",
            &Authorize {
                client_id: "a-client",
                redirect_uri: "https://rooms.example.test/auth/callback",
                scopes: DEFAULT_SCOPES,
                state: "a-state",
                nonce: "a-nonce",
                challenge: "a-challenge",
            }
        ),
        Err(ProviderError::NotAUrl { .. })
    ));
}

/// RFC 7636 appendix B, verbatim. It is here because a challenge that is
/// nearly right fails at the provider with an error that says nothing.
#[test]
fn the_pkce_challenge_is_the_one_in_the_rfc() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn the_challenge_is_not_the_verifier() {
    let verifier = "a-verifier-nobody-else-holds-abcdefghijklmnop";
    let challenge = pkce_challenge(verifier);

    assert!(
        !challenge.contains(verifier),
        "the redirect would carry the verifier, which is the one thing pkce keeps back"
    );
    assert!(
        !challenge.contains('=') && !challenge.contains('+') && !challenge.contains('/'),
        "the challenge is not base64url, so it would need escaping in the redirect: {challenge}"
    );
    assert_ne!(challenge, pkce_challenge("a-different-verifier"));
}

#[test]
fn a_path_on_this_server_is_the_only_thing_that_comes_back_as_one() {
    for path in ["/", "/r/standup", "/r/standup?video=1", "/r/a.b-c_d"] {
        assert_eq!(
            local_path(path).as_deref(),
            Some(path),
            "`{path}` was refused"
        );
    }

    for elsewhere in [
        "//evil.example",
        "//evil.example/r/standup",
        "/\\evil.example",
        "https://evil.example/r/standup",
        "http://evil.example",
        "r/standup",
        "",
        "javascript:alert(1)",
        "/r/standup\r\nSet-Cookie: a=b",
    ] {
        assert_eq!(
            local_path(elsewhere),
            None,
            "`{elsewhere}` was accepted as a path on this server, so a redirect built from it \
                 leaves this origin"
        );
    }
}

#[test]
fn a_return_url_that_is_not_ours_becomes_the_front_page() {
    let public = "https://rooms.example.test";

    assert_eq!(
        safe_return_url("/r/standup", public),
        "https://rooms.example.test/r/standup"
    );
    assert_eq!(
        safe_return_url("https://rooms.example.test/r/standup", public),
        "https://rooms.example.test/r/standup"
    );

    for elsewhere in [
        "https://evil.example/",
        "http://rooms.example.test/r/standup",
        "https://rooms.example.test.evil.example/",
        "https://rooms.example.test:8443/",
        "//evil.example",
        "javascript:alert(1)",
        "",
    ] {
        assert_eq!(
            safe_return_url(elsewhere, public),
            "https://rooms.example.test/",
            "`{elsewhere}` was honoured as somewhere to send a person after signing out"
        );
    }
}

#[test]
fn a_trailing_slash_on_the_public_url_changes_nothing() {
    for public in [
        "https://rooms.example.test",
        "https://rooms.example.test/",
        " https://rooms.example.test/ ",
    ] {
        assert_eq!(
            safe_return_url("/r/standup", public),
            "https://rooms.example.test/r/standup"
        );
    }
}

#[test]
fn the_name_shown_is_the_first_of_the_three_the_provider_gave() {
    let full = Person {
        subject: "a-sub".to_owned(),
        name: Some("Роман Морозов".to_owned()),
        preferred_username: Some("roman".to_owned()),
        email: Some("roman@example.test".to_owned()),
        roles: vec![],
    };
    assert_eq!(full.display_name(), "Роман Морозов");

    let no_name = Person {
        name: None,
        ..full.clone()
    };
    assert_eq!(no_name.display_name(), "roman");

    let only_email = Person {
        name: None,
        preferred_username: None,
        ..full.clone()
    };
    assert_eq!(only_email.display_name(), "roman@example.test");

    let nothing = Person {
        subject: "a-sub".to_owned(),
        ..Person::default()
    };
    assert_eq!(
        nothing.display_name(),
        "a-sub",
        "a roster entry with no text at all is worse than an ugly one"
    );
}

/// A directory is not a source of short printable strings, and whatever it
/// returns goes into a token and onto everybody's screen.
#[test]
fn a_name_the_directory_returned_is_cleaned_like_a_typed_one() {
    let long = Person {
        subject: "a-sub".to_owned(),
        name: Some("Ро".repeat(60)),
        ..Person::default()
    };
    assert_eq!(long.display_name().chars().count(), MAX_NAME_CHARS);

    let ragged = Person {
        subject: "a-sub".to_owned(),
        name: Some("  Роман  ".to_owned()),
        ..Person::default()
    };
    assert_eq!(ragged.display_name(), "Роман");

    let control = Person {
        subject: "a-sub".to_owned(),
        name: Some("Ро\u{7}ман".to_owned()),
        preferred_username: Some("roman".to_owned()),
        ..Person::default()
    };
    assert_eq!(
        control.display_name(),
        "roman",
        "a name with a control character in it was shown rather than passed over"
    );

    let blank = Person {
        subject: "a-sub".to_owned(),
        name: Some("   ".to_owned()),
        email: Some("roman@example.test".to_owned()),
        ..Person::default()
    };
    assert_eq!(blank.display_name(), "roman@example.test");
}

#[test]
fn a_subject_that_could_not_be_an_identity_is_refused() {
    assert_eq!(clean_subject(" a-sub ").unwrap(), "a-sub");
    assert_eq!(
        clean_subject("3f6b1c88-0000-4000-8000-abcdefabcdef").unwrap(),
        "3f6b1c88-0000-4000-8000-abcdefabcdef"
    );

    for bad in ["", "   ", "a\u{0}sub", "a\nsub"] {
        assert!(
            matches!(clean_subject(bad), Err(IdTokenError::UnusableSubject)),
            "`{bad}` was accepted as somebody's identity"
        );
    }
    assert!(clean_subject(&"a".repeat(MAX_SUBJECT_CHARS + 1)).is_err());
}

#[test]
fn a_claim_is_a_string_or_a_list_of_them_and_nothing_else() {
    assert_eq!(strings_in(&serde_json::json!("one")), vec!["one"]);
    assert_eq!(
        strings_in(&serde_json::json!(["one", "two"])),
        vec!["one", "two"]
    );
    assert_eq!(
        strings_in(&serde_json::json!(["one", 2, null, { "three": true }])),
        vec!["one"],
        "a role that is not a string was carried into the session as one"
    );
    assert!(strings_in(&serde_json::json!(null)).is_empty());
    assert!(strings_in(&serde_json::json!(7)).is_empty());
}

#[test]
fn the_id_token_the_provider_signs_with_is_the_one_we_check_against() {
    assert_eq!(ID_TOKEN_ALGORITHM, Algorithm::RS256);
}
