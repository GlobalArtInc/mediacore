
use std::collections::HashMap;

use super::*;

fn env(pairs: &[(&str, &str)]) -> impl Environment {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();

    move |name: &'static str| Ok(map.get(name).cloned())
}

fn complete() -> Vec<(&'static str, &'static str)> {
    vec![
        (API_KEY, "devkey"),
        (API_SECRET, "devsecretdevsecretdevsecretdevsecret"),
        (WS_URL, "wss://media.example.test/"),
    ]
}

#[test]
fn reads_a_complete_environment() {
    let config = Config::from_source(&env(&complete())).unwrap();

    assert_eq!(config.api_key, "devkey");
    assert_eq!(config.ws_url, "wss://media.example.test");
    assert_eq!(config.room, DEFAULT_ROOM);
    assert_eq!(config.port, DEFAULT_PORT);
}

#[test]
fn takes_the_room_and_port_when_they_are_given() {
    let mut pairs = complete();
    pairs.push((ROOM, "standup"));
    pairs.push((PORT, "9000"));

    let config = Config::from_source(&env(&pairs)).unwrap();
    assert_eq!(config.room, "standup");
    assert_eq!(config.port, 9000);
}

#[test]
fn a_missing_variable_is_named() {
    for missing in [API_KEY, API_SECRET, WS_URL] {
        let pairs: Vec<_> = complete()
            .into_iter()
            .filter(|(name, _)| *name != missing)
            .collect();

        assert_eq!(
            Config::from_source(&env(&pairs)).unwrap_err(),
            ConfigError::Missing(missing),
            "{missing} was allowed to be absent"
        );
    }
}

#[test]
fn an_empty_variable_counts_as_missing() {
    let mut pairs = complete();
    pairs.retain(|(name, _)| *name != API_SECRET);
    pairs.push((API_SECRET, "   "));

    assert_eq!(
        Config::from_source(&env(&pairs)).unwrap_err(),
        ConfigError::Missing(API_SECRET)
    );
}

#[test]
fn a_short_secret_is_refused() {
    let mut pairs = complete();
    pairs.retain(|(name, _)| *name != API_SECRET);
    pairs.push((API_SECRET, "too-short"));

    assert_eq!(
        Config::from_source(&env(&pairs)).unwrap_err(),
        ConfigError::ShortSecret { name: API_SECRET }
    );
}

#[test]
fn the_debug_rendering_keeps_the_secret_out_of_the_logs() {
    let config = Config::from_source(&env(&complete())).unwrap();
    let rendered = format!("{config:?}");

    assert!(
        !rendered.contains(&config.api_secret),
        "the api secret was printed: {rendered}"
    );
    assert!(rendered.contains(REDACTED));
    assert!(rendered.contains("devkey"));
}

#[test]
fn a_url_that_is_not_a_bare_host_is_refused() {
    for bad in [
        "wss://https://media.example.test",
        "wss://media.example.test/rtc",
        "wss://user@media.example.test",
        "wss://media.example.test?a=b",
        "wss://media example.test",
        "wss://media.example.test;script-src *",
        "wss://media.example.test'",
        "wss://",
    ] {
        let mut pairs = complete();
        pairs.retain(|(name, _)| *name != WS_URL);
        pairs.push((WS_URL, bad));

        assert!(
            matches!(
                Config::from_source(&env(&pairs)),
                Err(ConfigError::NotAWebSocketUrl { .. })
            ),
            "`{bad}` was accepted as a websocket url"
        );
    }
}

#[test]
fn a_bracketed_ipv6_host_is_accepted() {
    let mut pairs = complete();
    pairs.retain(|(name, _)| *name != WS_URL);
    pairs.push((WS_URL, "wss://[2001:db8::1]:7880/"));

    let config = Config::from_source(&env(&pairs)).unwrap();
    assert_eq!(config.ws_url, "wss://[2001:db8::1]:7880");
}

#[test]
fn an_http_url_is_refused() {
    let mut pairs = complete();
    pairs.retain(|(name, _)| *name != WS_URL);
    pairs.push((WS_URL, "https://media.example.test"));

    assert!(matches!(
        Config::from_source(&env(&pairs)),
        Err(ConfigError::NotAWebSocketUrl { .. })
    ));
}

#[test]
fn a_nonsense_port_is_refused() {
    let mut pairs = complete();
    pairs.push((PORT, "https"));

    assert!(matches!(
        Config::from_source(&env(&pairs)),
        Err(ConfigError::NotAPort { .. })
    ));
}

#[test]
fn a_nonsense_room_is_refused() {
    let mut pairs = complete();
    pairs.push((ROOM, "not a room"));

    assert!(matches!(
        Config::from_source(&env(&pairs)),
        Err(ConfigError::BadRoom { .. })
    ));
}

fn signing_in() -> Vec<(&'static str, &'static str)> {
    let mut pairs = complete();
    pairs.push((OIDC_ISSUER, "https://sso.mcn.test"));
    pairs.push((OIDC_CLIENT_ID, "a-client"));
    pairs.push((OIDC_CLIENT_SECRET, "a-client-secret"));
    pairs.push((PUBLIC_URL, "https://rooms.example.test"));
    pairs
}

/// A switch, not a mandate: an environment that says nothing about an
/// identity provider gets the page it always had.
#[test]
fn signing_in_is_off_until_an_issuer_says_otherwise() {
    assert!(
        Config::from_source(&env(&complete()))
            .unwrap()
            .oidc
            .is_none()
    );
    assert!(
        Config::from_source(&env(&signing_in()))
            .unwrap()
            .oidc
            .is_some()
    );
}

/// Half-configured is the state worth refusing at startup. An issuer with
/// no client id starts a process that serves the page, redirects nobody
/// and fails at the first press of Join, with nothing in the log to say
/// which variable was missing.
#[test]
fn an_issuer_with_nothing_beside_it_is_refused_by_name() {
    for missing in [OIDC_CLIENT_ID, OIDC_CLIENT_SECRET, PUBLIC_URL] {
        let pairs: Vec<_> = signing_in()
            .into_iter()
            .filter(|(name, _)| *name != missing)
            .collect();

        assert_eq!(
            Config::from_source(&env(&pairs)).unwrap_err(),
            ConfigError::NeedsSignIn { name: missing },
            "{missing} was allowed to be absent with an issuer set"
        );
    }
}

/// The provider has this written down. Deriving it from one variable is
/// what stops it drifting from the registered value, and the registered
/// value is what a provider refuses a request over.
#[test]
fn the_redirect_uri_is_derived_from_the_one_public_url() {
    for public in [
        "https://rooms.example.test",
        "https://rooms.example.test/",
        "https://10.251.10.201:5400",
    ] {
        let mut pairs = signing_in();
        pairs.retain(|(name, _)| *name != PUBLIC_URL);
        pairs.push((PUBLIC_URL, public));

        let oidc = Config::from_source(&env(&pairs)).unwrap().oidc.unwrap();
        let expected = public.trim_end_matches('/');

        assert_eq!(oidc.redirect_uri(), format!("{expected}/auth/callback"));
        assert_eq!(oidc.post_logout_uri(), format!("{expected}/"));
    }
}

/// A public url with a path in it describes a deployment this binary
/// cannot serve -- the page addresses `/app.js` and `/r/<room>` from the
/// root -- and would build a redirect uri no provider has ever seen.
#[test]
fn a_public_url_that_is_not_an_origin_is_refused() {
    for bad in [
        ("https://rooms.example.test/meet", true),
        ("rooms.example.test", false),
        ("ftp://rooms.example.test", false),
        ("https://rooms.example.test?a=b", false),
        ("https://rooms.example.test#a", false),
    ] {
        let mut pairs = signing_in();
        pairs.retain(|(name, _)| *name != PUBLIC_URL);
        pairs.push((PUBLIC_URL, bad.0));

        let refused = Config::from_source(&env(&pairs)).unwrap_err();
        let expected_shape = match bad.1 {
            true => matches!(refused, ConfigError::NotAnOrigin { .. }),
            false => matches!(refused, ConfigError::NotAWebUrl { .. }),
        };

        assert!(
            expected_shape,
            "`{}` was accepted, and answered {refused}",
            bad.0
        );
    }
}

/// An issuer may carry a path -- plenty of providers put a realm there --
/// so it is checked for being a url and nothing more.
#[test]
fn an_issuer_may_name_a_realm_but_has_to_be_a_url() {
    let mut pairs = signing_in();
    pairs.retain(|(name, _)| *name != OIDC_ISSUER);
    pairs.push((OIDC_ISSUER, "https://sso.mcn.test/realms/mediacore/"));

    let oidc = Config::from_source(&env(&pairs)).unwrap().oidc.unwrap();
    assert_eq!(
        oidc.issuer, "https://sso.mcn.test/realms/mediacore",
        "a trailing slash on the issuer would be compared against a token's `iss` and lose"
    );

    pairs.retain(|(name, _)| *name != OIDC_ISSUER);
    pairs.push((OIDC_ISSUER, "sso.mcn.test"));
    assert!(matches!(
        Config::from_source(&env(&pairs)),
        Err(ConfigError::NotAWebUrl { .. })
    ));
}

#[test]
fn a_session_lifetime_that_is_not_one_is_refused() {
    let mut pairs = signing_in();
    assert_eq!(
        Config::from_source(&env(&pairs))
            .unwrap()
            .oidc
            .unwrap()
            .session_ttl,
        DEFAULT_SESSION_TTL
    );

    pairs.push((SESSION_TTL, "3600"));
    assert_eq!(
        Config::from_source(&env(&pairs))
            .unwrap()
            .oidc
            .unwrap()
            .session_ttl,
        Duration::from_secs(3600)
    );

    for bad in ["0", "10", "an hour", "-1", "99999999"] {
        let mut pairs = signing_in();
        pairs.push((SESSION_TTL, bad));

        assert!(
            matches!(
                Config::from_source(&env(&pairs)),
                Err(ConfigError::NotASessionLifetime { .. })
            ),
            "`{bad}` was accepted as how long a session lasts"
        );
    }
}

/// The provider chose this secret, not us, so its length is not ours to
/// argue with -- unlike `MEDIACORE_API_SECRET`, which this process makes.
#[test]
fn a_short_client_secret_is_the_providers_business() {
    let mut pairs = signing_in();
    pairs.retain(|(name, _)| *name != OIDC_CLIENT_SECRET);
    pairs.push((OIDC_CLIENT_SECRET, "short"));

    assert_eq!(
        Config::from_source(&env(&pairs))
            .unwrap()
            .oidc
            .unwrap()
            .client_secret,
        "short"
    );
}

#[test]
fn the_debug_rendering_keeps_the_client_secret_out_of_the_logs() {
    let config = Config::from_source(&env(&signing_in())).unwrap();
    let rendered = format!("{config:?}");

    assert!(
        !rendered.contains("a-client-secret"),
        "the client secret was printed: {rendered}"
    );
    assert_eq!(
        rendered.matches(REDACTED).count(),
        2,
        "one of the two secrets is being printed: {rendered}"
    );
    assert!(
        rendered.contains("a-client") && rendered.contains("https://sso.mcn.test"),
        "the debug rendering says nothing useful about the provider: {rendered}"
    );
}
