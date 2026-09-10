use super::*;

/// The mark is what stops a stale view on two nodes becoming a ring.
#[test]
fn a_connection_that_was_already_carried_says_so() {
    assert!(!already_relayed(""));
    assert!(!already_relayed("access_token=abc&protocol=17"));
    assert!(already_relayed("access_token=abc&mediacore_relayed=1"));
    assert!(already_relayed("mediacore_relayed=1"));
    assert!(
        !already_relayed("mediacore_relayed=0"),
        "a mark that says no was read as yes"
    );
    assert!(
        !already_relayed("not_mediacore_relayed=1"),
        "a parameter that merely ends in the mark's name was read as the mark"
    );
}

/// The inner connection carries everything the outer one did. Dropping a
/// parameter here would mean the owner deciding something differently from
/// what the client asked for -- `auto_subscribe` is the one that would hurt.
#[test]
fn the_inner_connection_carries_the_whole_query_and_the_mark() {
    let url = inner_url(
        "ws://10.0.0.7:7880",
        "access_token=abc&protocol=17&auto_subscribe=0",
    );

    assert_eq!(
        url,
        "ws://10.0.0.7:7880/rtc?access_token=abc&protocol=17&auto_subscribe=0&mediacore_relayed=1"
    );
    assert!(already_relayed(url.split_once('?').unwrap().1));
}

#[test]
fn a_trailing_slash_on_the_advertised_address_does_not_double() {
    assert_eq!(
        inner_url("ws://10.0.0.7:7880/", "access_token=abc"),
        "ws://10.0.0.7:7880/rtc?access_token=abc&mediacore_relayed=1"
    );
}

/// A client that sent no query at all still has to arrive marked, or the owner
/// would carry it onwards to whoever it thinks holds the room -- which, with
/// no token, is nobody.
#[test]
fn an_empty_query_still_arrives_marked() {
    assert_eq!(
        inner_url("ws://10.0.0.7:7880", ""),
        "ws://10.0.0.7:7880/rtc?mediacore_relayed=1"
    );
}
