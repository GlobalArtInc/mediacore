use super::*;

fn identity() -> Identity {
    Identity {
        subject: "3f6b1c88-0000-4000-8000-abcdefabcdef".to_owned(),
        display_name: "Роман".to_owned(),
        roles: vec!["staff".to_owned()],
        id_token: "an.id.token".to_owned(),
    }
}

fn sessions() -> Sessions {
    Sessions::new(Duration::from_secs(60))
}

#[test]
fn a_session_is_found_by_the_cookie_it_was_opened_with() {
    let sessions = sessions();
    let cookie = sessions.open(identity());
    let found = sessions.lookup(&cookie).expect("the session just opened");

    assert_eq!(found.subject, identity().subject);
    assert_eq!(found.display_name, "Роман");
    assert_eq!(found.roles, vec!["staff".to_owned()]);
}

/// The cookie is an id and a signature over it and carries nothing else.
/// A browser that could read who it is out of its own cookie is a browser
/// that could edit it.
#[test]
fn the_cookie_says_nothing_about_the_person() {
    let sessions = sessions();
    let cookie = sessions.open(identity());

    for private in ["Роман", "abcdefabcdef", "staff", "an.id.token"] {
        assert!(
            !cookie.contains(private),
            "the session cookie carries `{private}`, which the browser now holds and could \
                 edit: {cookie}"
        );
    }
}

#[test]
fn a_cookie_this_process_did_not_sign_names_nothing() {
    let sessions = sessions();
    let cookie = sessions.open(identity());
    let (id, signature) = cookie.rsplit_once('.').unwrap();

    for forged in [
        format!("{id}.{}", "0".repeat(signature.len())),
        format!("{id}."),
        id.to_owned(),
        format!("{id}.not-hex-at-all"),
        format!("{}.{signature}", random_token()),
        String::new(),
    ] {
        assert!(
            sessions.lookup(&forged).is_none(),
            "`{forged}` was accepted as a session cookie"
        );
    }

    assert!(
        sessions.lookup(&cookie).is_some(),
        "the cookie this process signed stopped working"
    );
}

/// Two processes are two key,s so a cookie carried from one to the other
/// names nothing. That is what a restart does, and it is the reason the
/// README says a restart signs everybody out rather than leaving it to be
/// discovered.
#[test]
fn a_cookie_from_another_process_names_nothing() {
    let cookie = sessions().open(identity());

    assert!(
        sessions().lookup(&cookie).is_none(),
        "a session cookie signed by another process was honoured"
    );
}

#[test]
fn a_session_that_has_run_out_is_gone() {
    let sessions = Sessions::new(Duration::from_millis(0));
    let cookie = sessions.open(identity());

    assert!(
        sessions.lookup(&cookie).is_none(),
        "a session past its lifetime was still answering"
    );
}

#[test]
fn closing_a_session_hands_back_what_it_held_once() {
    let sessions = sessions();
    let cookie = sessions.open(identity());

    let closed = sessions.close(&cookie).expect("the session that was open");
    assert_eq!(
        closed.id_token(),
        "an.id.token",
        "logout cannot tell the provider which sign-in is ending"
    );
    assert!(
        sessions.close(&cookie).is_none() && sessions.lookup(&cookie).is_none(),
        "a closed session came back"
    );
}

/// A login in flight, started the way [`crate::auth::start_sign_in`] does.
fn started(sessions: &Sessions, going_to: &str, browser: &str) -> String {
    let state = random_token();
    assert!(
        sessions.start_login(
            &state,
            Login::new(
                going_to.to_owned(),
                random_token(),
                random_token(),
                fingerprint(browser),
            ),
        ),
        "there was no room for a sign-in in an empty store"
    );

    state
}

/// The callback spends the login. A second callback carrying the same
/// `state` -- a reload, a back button, somebody's copy of the link -- finds
/// nothing, and the authorization code it carries can never be redeemed
/// twice.
#[test]
fn a_login_can_only_be_finished_once() {
    let sessions = sessions();
    let state = random_token();
    sessions.start_login(
        &state,
        Login::new(
            "/r/standup".to_owned(),
            "a-nonce".to_owned(),
            "a-verifier".to_owned(),
            fingerprint("the-cookie"),
        ),
    );

    let login = sessions
        .finish_login(&state, |_| true)
        .expect("the login just started");
    assert_eq!(login.return_to, "/r/standup");
    assert_eq!(login.nonce, "a-nonce");
    assert_eq!(
        sessions.finish_login(&state, |_| true).err(),
        Some(NotFinished::Unknown),
        "the same state finished a login twice, so a code could be redeemed twice"
    );
}

#[test]
fn a_state_this_process_never_issued_finishes_nothing() {
    let sessions = sessions();
    started(&sessions, "/", "the-cookie");

    assert_eq!(
        sessions.finish_login(&random_token(), |_| true).err(),
        Some(NotFinished::Unknown)
    );
    assert_eq!(
        sessions.finish_login("", |_| true).err(),
        Some(NotFinished::Unknown)
    );
}

/// The `state` is not a secret: it is in the address bar while the
/// provider's page is open, and in the provider's access log afterwards. A
/// callback that fails the browser check has redeemed nothing, so it must
/// leave the sign-in exactly where it found it -- otherwise anybody who
/// read a `state` anywhere can end that sign-in by fetching one URL.
#[test]
fn a_callback_that_cannot_prove_the_browser_leaves_the_sign_in_standing() {
    let sessions = sessions();
    let state = started(&sessions, "/r/standup", "the-cookie");

    assert_eq!(
        sessions.finish_login(&state, |_| false).err(),
        Some(NotFinished::WrongBrowser)
    );
    assert_eq!(
        sessions
            .finish_login(&state, |login| login.browser == fingerprint("the-cookie"))
            .map(|login| login.return_to),
        Ok("/r/standup".to_owned()),
        "somebody who knew nothing but the state ended a sign-in they could not finish"
    );
}

/// Two tabs, one browser. They keep their own `state`, their own nonce and
/// their own room, and they share the one login cookie -- which is what the
/// cookie is for: it says *which browser*, and there is one browser here.
#[test]
fn two_sign_ins_at_once_do_not_stand_on_each_other() {
    let sessions = sessions();
    let first = started(&sessions, "/r/standup", "one-browser");
    let second = started(&sessions, "/r/retro", "one-browser");

    assert_ne!(first, second);
    assert_eq!(
        sessions
            .finish_login(&second, |_| true)
            .map(|login| login.return_to),
        Ok("/r/retro".to_owned())
    );
    assert!(
        sessions.another_login_in(&fingerprint("one-browser")),
        "the second tab's callback would have cleared the login cookie the first tab still \
             has to prove itself with"
    );
    assert_eq!(
        sessions
            .finish_login(&first, |_| true)
            .map(|login| login.return_to),
        Ok("/r/standup".to_owned()),
        "a second tab signing in threw away the first tab's room"
    );
    assert!(
        !sessions.another_login_in(&fingerprint("one-browser")),
        "the login cookie would be left standing with nothing left to prove"
    );
}

/// The flood, at the store: four thousand sign-ins nobody will ever finish,
/// started by somebody with no account and no cookie, while one real person
/// is at the provider typing a password. Dropping the oldest to make room
/// dropped exactly that person, because the oldest entry is the one that
/// has been waiting longest -- so a newcomer is refused instead.
#[test]
fn a_flood_of_sign_ins_does_not_throw_out_the_one_somebody_is_in_the_middle_of() {
    let sessions = sessions();
    let victim = started(&sessions, "/r/standup", "the-victims-browser");

    let mut refused = 0;
    for _ in 0..(MAX_PENDING_LOGINS + 16) {
        let flood = Login::new(
            "/".to_owned(),
            random_token(),
            random_token(),
            fingerprint(&random_token()),
        );
        if !sessions.start_login(&random_token(), flood) {
            refused += 1;
        }
    }

    assert!(
        refused >= 16,
        "{refused} of the flood were refused, so the ceiling let the map grow past it"
    );

    let pending = sessions.store.lock().unwrap().logins.len();
    assert!(
        pending <= MAX_PENDING_LOGINS,
        "{pending} sign-ins are in flight, which is more than this process said it would keep"
    );

    assert!(
        sessions
            .finish_login(&victim, |login| login.browser
                == fingerprint("the-victims-browser"))
            .is_ok(),
        "a flood of anonymous page loads threw out the sign-in somebody was in the middle of, \
             so nobody can sign in while it lasts"
    );
}

#[test]
fn two_tokens_are_never_the_same() {
    let token = random_token();

    assert_eq!(token.len(), TOKEN_BYTES * 2);
    assert_ne!(token, random_token());
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "a token has characters that would have to be escaped in a url or a cookie: {token}"
    );
    assert!(
        (43..=128).contains(&token.len()),
        "a token is not a length RFC 7636 allows a PKCE verifier: {}",
        token.len()
    );
}

#[test]
fn a_stored_fingerprint_is_not_the_secret() {
    let secret = random_token();
    let stored = fingerprint(&secret);

    assert!(
        !stored.contains(&secret),
        "the login store keeps the browser's own secret, so reading it is enough to finish \
             somebody else's sign-in"
    );
    assert_eq!(stored.len(), 64);
    assert_ne!(stored, fingerprint(&random_token()));
}

#[test]
fn secrets_are_compared_whole() {
    assert!(same_secret("a-nonce", "a-nonce"));
    assert!(!same_secret("a-nonce", "a-nonce "));
    assert!(!same_secret("a-nonce", "b-nonce"));
    assert!(!same_secret("", "a-nonce"));
    assert!(same_secret("", ""));
}

/// Every attribute on the session cookie is doing something. Losing one
/// quietly is how a cookie becomes readable by a script, or sent on a
/// cross-site request, or accepted from a sibling host.
#[test]
fn the_session_cookie_is_locked_down() {
    let cookie = session_cookie("value", 3600);

    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=3600",
    ] {
        assert!(
            cookie.contains(attribute),
            "the session cookie lost `{attribute}`: {cookie}"
        );
    }
    assert!(
        cookie.starts_with("__Host-"),
        "the session cookie lost its `__Host-` prefix, so a sibling host can set one: {cookie}"
    );
    assert!(
        !cookie.contains("Domain="),
        "a `Domain` widens the cookie to every host under it: {cookie}"
    );

    assert!(spent_session_cookie().contains("Max-Age=0"));
    assert!(login_cookie("a-secret").contains("HttpOnly; Secure; SameSite=Lax"));
    assert!(spent_login_cookie().contains("Max-Age=0"));
}

/// One name, whatever the sign-in. The old one carried the `state`, so a
/// browser that loaded the page a hundred times held a hundred cookies for
/// ten minutes each -- and anything that could make a browser fetch this
/// origin could plant them by the hundred.
#[test]
fn a_browser_can_only_ever_hold_one_login_cookie() {
    let one = login_cookie("a-secret");
    let another = login_cookie("a-different-secret");

    let name = |cookie: &str| cookie.split('=').next().unwrap().to_owned();
    assert_eq!(
        name(&one),
        name(&another),
        "two sign-ins left two cookies, so a browser collects one per page load: {one}"
    );
    assert_eq!(name(&one), LOGIN_COOKIE);
    assert_eq!(name(&spent_login_cookie()), LOGIN_COOKIE);
    assert!(
        one.starts_with("__Host-"),
        "the login cookie lost its `__Host-` prefix, so a sibling host can set one: {one}"
    );
}

#[test]
fn nothing_the_provider_said_reaches_a_log_through_a_debug() {
    let sessions = sessions();
    let cookie = sessions.open(identity());
    let session = sessions.lookup(&cookie).unwrap();

    let rendered = format!("{session:?}");
    assert!(
        !rendered.contains("an.id.token"),
        "the id token was printed, and it is a bearer credential at the provider: {rendered}"
    );
    assert!(rendered.contains("Роман"), "{rendered}");

    let login = Login::new(
        "/r/standup".to_owned(),
        "the-nonce".to_owned(),
        "the-verifier".to_owned(),
        fingerprint("the-cookie"),
    );
    let rendered = format!("{login:?}");
    for secret in ["the-nonce", "the-verifier"] {
        assert!(
            !rendered.contains(secret),
            "`{secret}` was printed, and it is what makes this sign-in this sign-in: \
                 {rendered}"
        );
    }

    let rendered = format!("{sessions:?}");
    assert!(
        !rendered.contains(&cookie) && !rendered.contains("Роман"),
        "the session store printed what it holds: {rendered}"
    );
}

#[test]
fn hex_survives_a_round_trip() {
    for bytes in [
        vec![],
        vec![0],
        vec![0xff, 0x00, 0x10],
        (0u8..=255).collect(),
    ] {
        assert_eq!(decode_hex(&encode_hex(&bytes)).as_deref(), Some(&bytes[..]));
    }

    assert_eq!(decode_hex("abc"), None, "an odd number of nibbles decoded");
    assert_eq!(decode_hex("zz"), None, "a non-hex pair decoded");
}
