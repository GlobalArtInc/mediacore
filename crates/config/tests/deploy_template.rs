//! The stack in `deploy/` is rendered, not written: `start-sfu.sh` runs
//! `envsubst` over `mediacore.yaml.template` and the server reads the result.
//! Nothing else checks that what comes out is a config the server accepts, and
//! the failure mode is a crash loop on a host, so it is checked here.

use std::collections::BTreeSet;

use mediacore_config::Config;

const TEMPLATE: &str = include_str!("../../../deploy/mediacore.yaml.template");
const START_SFU: &str = include_str!("../../../deploy/start-sfu.sh");

/// A stack with nothing filled in beyond the four values compose insists on:
/// meetings in a directory, no database, no recording.
fn bare() -> Vec<(&'static str, &'static str)> {
    vec![
        ("MEDIACORE_LOG_LEVEL", "info"),
        ("MEDIACORE_API_KEY", "mediacore"),
        (
            "MEDIACORE_API_SECRET",
            "0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
        ("MEDIACORE_RTC_UDP_PORT", "7882"),
        ("MEDIACORE_ADVERTISE_IP", "203.0.113.7"),
        ("MEDIACORE_POSTGRES_URL", ""),
        ("MEDIACORE_RECORDING_ENABLED", "false"),
        ("MEDIACORE_S3_ENDPOINT", ""),
        ("MEDIACORE_S3_REGION", "us-east-1"),
        ("MEDIACORE_S3_BUCKET", ""),
        ("MEDIACORE_S3_ACCESS_KEY_ID", ""),
        ("MEDIACORE_S3_SECRET_ACCESS_KEY", ""),
        ("MEDIACORE_ICE_SERVERS_BLOCK", ""),
    ]
}

/// What `start-sfu.sh` builds when `MEDIACORE_TURN_SECRET` is set, copied from
/// it verbatim -- the indentation included, because that is the whole of what
/// makes these lines a member of `rtc:` rather than a top-level key the server
/// refuses.
const ICE_SERVERS_BLOCK: &str = r#"  ice_servers:
    - urls:
        - "turn:turn.example.com:3478?transport=udp"
        - "turn:turn.example.com:3478?transport=tcp"
        - "turns:turn.example.com:5349?transport=tcp"
      secret: "0123456789abcdef0123456789abcdef"
      ttl_seconds: 86400"#;

/// The same stack with the relay switched on: `MEDIACORE_TURN_SECRET` set, and
/// certificate files named, which is the longest of the three shapes
/// `start-sfu.sh` emits.
fn relaying() -> Vec<(&'static str, &'static str)> {
    let mut values = bare();
    for (name, value) in &mut values {
        if *name == "MEDIACORE_ICE_SERVERS_BLOCK" {
            *value = ICE_SERVERS_BLOCK;
        }
    }
    values
}

/// The same stack with the recording switched on: a database and a bucket.
fn recording() -> Vec<(&'static str, &'static str)> {
    let mut values = bare();
    for (name, value) in &mut values {
        *value = match *name {
            "MEDIACORE_POSTGRES_URL" => "postgres://mediacore@db.example.com:5432/mediacore",
            "MEDIACORE_RECORDING_ENABLED" => "true",
            "MEDIACORE_S3_ENDPOINT" => "https://storage.example.com",
            "MEDIACORE_S3_BUCKET" => "mediacore",
            "MEDIACORE_S3_ACCESS_KEY_ID" => "AKIAEXAMPLE",
            "MEDIACORE_S3_SECRET_ACCESS_KEY" => "secret",
            _ => continue,
        };
    }
    values
}

fn render(values: &[(&str, &str)]) -> String {
    let mut body = TEMPLATE.to_owned();
    for (name, value) in values {
        body = body.replace(&format!("${{{name}}}"), value);
    }
    assert!(
        !body.contains("${"),
        "the rendered config still has a placeholder in it: {body}"
    );
    body
}

fn placeholders(body: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = body;
    while let Some(at) = rest.find("${") {
        rest = &rest[at + 2..];
        let end = rest.find('}').expect("an unterminated placeholder");
        found.insert(rest[..end].to_owned());
        rest = &rest[end + 1..];
    }
    found
}

#[test]
fn the_stack_as_shipped_is_a_config_the_server_accepts() {
    let config = Config::parse(&render(&bare())).expect("the rendered config was refused");

    assert!(!config.sessions.in_postgres());
    assert!(!config.recording.enabled);
}

#[test]
fn the_stack_with_recording_switched_on_is_a_config_the_server_accepts() {
    let config = Config::parse(&render(&recording())).expect("the rendered config was refused");

    assert!(config.sessions.in_postgres());
    assert!(config.recording.enabled);
    assert_eq!(config.recording.s3.bucket, "mediacore");
}

/// The relay is configured in one place a template cannot reach: `start-sfu.sh`
/// builds the whole `ice_servers` block, because a template can fill a value in
/// and cannot leave a block out. So the yaml it writes is checked here, where a
/// wrong indent or a missing quote is a test and not a crash loop on a host with
/// people waiting in a room.
#[test]
fn the_stack_with_the_relay_switched_on_is_a_config_the_server_accepts() {
    let config = Config::parse(&render(&relaying())).expect("the rendered config was refused");

    let [relay] = &config.rtc.ice_servers[..] else {
        panic!(
            "expected exactly one ice server, got {:?}",
            config.rtc.ice_servers
        );
    };
    assert_eq!(relay.urls.len(), 3);
    assert_eq!(relay.secret, "0123456789abcdef0123456789abcdef");
    assert_eq!(relay.ttl_seconds, 86400);
    assert!(relay.username.is_empty());
}

/// `envsubst` is given an explicit list of names, and substitutes only those.
/// A placeholder the template gained and the list did not is left in the
/// rendered file verbatim, which is a config the server refuses -- on the host,
/// not here.
#[test]
fn every_placeholder_in_the_template_is_one_start_sfu_substitutes() {
    let substituted = placeholders(
        START_SFU
            .lines()
            .find(|line| line.contains("envsubst"))
            .expect("start-sfu.sh no longer calls envsubst"),
    );

    for name in placeholders(TEMPLATE) {
        assert!(
            substituted.contains(&name),
            "{name} is in the template and not in the envsubst list in start-sfu.sh"
        );
    }
}
