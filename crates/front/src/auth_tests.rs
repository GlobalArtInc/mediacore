
use axum::http::HeaderValue;

use super::*;

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn a_cookie_is_found_however_the_browser_packed_them() {
    let one = headers(&[("cookie", "a=1; __Host-mediacore_session=abc; b=2")]);
    assert_eq!(
        cookies(&one, "__Host-mediacore_session").collect::<Vec<_>>(),
        vec!["abc"]
    );

    let split = headers(&[
        ("cookie", "a=1"),
        ("cookie", "__Host-mediacore_session=abc"),
    ]);
    assert_eq!(
        cookies(&split, "__Host-mediacore_session").collect::<Vec<_>>(),
        vec!["abc"],
        "a browser that sends one Cookie header per cookie was not read"
    );
}

/// Two cookies of one name is what a host that changed configuration
/// leaves behind. Reading only the first would let the dead one shadow the
/// live one, and the person would be signed out with a valid session in
/// their browser.
#[test]
fn every_cookie_of_a_name_is_offered_and_not_just_the_first() {
    let both = headers(&[(
        "cookie",
        "__Host-mediacore_session=stale; __Host-mediacore_session=live",
    )]);

    assert_eq!(
        cookies(&both, "__Host-mediacore_session").collect::<Vec<_>>(),
        vec!["stale", "live"]
    );
}

#[test]
fn a_cookie_that_is_not_there_is_not_an_empty_one() {
    let none = headers(&[("cookie", "__Host-mediacore_session=; a=1")]);
    assert_eq!(
        cookies(&none, "__Host-mediacore_session").count(),
        0,
        "an empty cookie was offered as a session id"
    );
    assert_eq!(
        cookies(&HeaderMap::new(), "__Host-mediacore_session").count(),
        0
    );
}

#[test]
fn a_page_this_server_writes_cannot_carry_markup_into_itself() {
    assert_eq!(
        escaped(r#"<script>alert("x")</script>"#),
        "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"
    );
    assert_eq!(escaped("a & b"), "a &amp; b");
    assert_eq!(escaped("Роман"), "Роман");
}

#[test]
fn the_registered_paths_are_the_ones_this_answers_at() {
    assert_eq!(CALLBACK_PATH, "/auth/callback");
    assert_eq!(LOGOUT_PATH, "/auth/logout");
}
