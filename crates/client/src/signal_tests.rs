
use super::*;

#[test]
fn a_websocket_url_is_built_from_an_http_base() {
    let url = signal_url("http://127.0.0.1:7880", "abc.def", 17, "mediacore", "0.1.0").unwrap();

    assert_eq!(
        url,
        "ws://127.0.0.1:7880/rtc?access_token=abc.def&protocol=17&sdk=mediacore&version=0.1.0&auto_subscribe=1"
    );
}

#[test]
fn tls_schemes_map_to_wss() {
    let url = signal_url("https://sfu.example/", "t", 17, "mediacore", "0.1.0").unwrap();
    assert!(url.starts_with("wss://sfu.example/rtc?"));
}

#[test]
fn an_explicit_rtc_path_is_not_repeated() {
    let url = signal_url("ws://host:1/rtc", "t", 17, "mediacore", "0.1.0").unwrap();
    assert!(url.starts_with("ws://host:1/rtc?"));
}

#[test]
fn query_values_are_percent_encoded() {
    let url = signal_url("ws://host:1", "t", 17, "media core/1", "0.1.0").unwrap();
    assert!(url.contains("sdk=media%20core%2F1"), "{url}");
}

#[test]
fn a_url_without_a_scheme_is_rejected() {
    let error = signal_url("127.0.0.1:7880", "t", 17, "mediacore", "0.1.0").unwrap_err();
    assert!(matches!(error, ClientError::InvalidUrl(_)));
}
