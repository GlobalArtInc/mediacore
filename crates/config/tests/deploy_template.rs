//! The stack in `deploy/` ships a config with variables in it:
//! `mediacore.yaml.template` is what the server reads, and it fills
//! `${MEDIACORE_...}` in from its own environment as it reads it. Nothing else
//! checks that the result is a config the server accepts, and the failure mode
//! is a crash loop on a host, so it is checked here -- against the very
//! expander the server runs, rather than a copy of its rules.

use std::collections::{BTreeSet, HashMap};

use mediacore_config::{Config, variables};

const TEMPLATE: &str = include_str!("../../../deploy/mediacore.yaml.template");
const START_SFU: &str = include_str!("../../../deploy/start-sfu.sh");

/// A stack with nothing filled in beyond the four values compose insists on:
/// meetings in a directory, no database, no recording.
///
/// The empty strings are not padding. `start-sfu.sh` sets those names to empty
/// deliberately, and empty is a value the server honours -- "no database, no
/// bucket" -- where a name it cannot find at all is a refusal. This list is
/// therefore the ledger of what the stack guarantees, and the last test here
/// holds the template to it.
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

/// Exactly what the server does on the host, minus the file read: the
/// environment is a map here because a test cannot safely set one for the
/// process it shares with every other test.
fn render(values: &[(&str, &str)]) -> anyhow::Result<String> {
    let environment: HashMap<&str, &str> = values.iter().copied().collect();
    let body = variables::expand_with(TEMPLATE, |name| {
        environment.get(name).map(|value| (*value).to_owned())
    })?;

    assert!(
        !body.contains("${"),
        "the rendered config still has a placeholder in it: {body}"
    );

    Ok(body)
}

fn rendered(values: &[(&str, &str)]) -> String {
    render(values).expect("the shipped template asks for something this stack does not set")
}

/// The names `start-sfu.sh` puts in the environment of the server it `exec`s.
///
/// `export` is the mechanism that decides this now. The script hands the server
/// its own process, so a name the script merely defaults with `: "${X:=...}"`
/// and never exports is a name the server cannot read -- and six of the TURN
/// variables are exactly that shape, because they are the script's own working
/// values for building the `ice_servers` block. "The script mentions the name
/// somewhere" is therefore not the question: a comment satisfies it.
fn exported(script: &str) -> BTreeSet<String> {
    // A line continued with a trailing backslash is one statement, and the
    // script's longest export runs over three of them.
    let joined = script.replace("\\\n", " ");
    let mut names = BTreeSet::new();

    for line in joined.lines() {
        let Some(words) = line.trim_start().strip_prefix("export ") else {
            continue;
        };

        for word in words.split_whitespace() {
            // `export NAME=value` is the other shape a shell accepts, and the
            // name is the part in front of the `=` either way.
            let name = word.split('=').next().unwrap_or(word);
            if !name.is_empty() {
                names.insert(name.to_owned());
            }
        }
    }

    names
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
    let config = Config::parse(&rendered(&bare())).expect("the rendered config was refused");

    assert!(!config.sessions.in_postgres());
    assert!(!config.recording.enabled);
}

#[test]
fn the_stack_with_recording_switched_on_is_a_config_the_server_accepts() {
    let config = Config::parse(&rendered(&recording())).expect("the rendered config was refused");

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
    let config = Config::parse(&rendered(&relaying())).expect("the rendered config was refused");

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

/// The whole point of moving the substitution into the server: a name nothing
/// set is a refusal that says which one, and not a config whose api secret is an
/// empty string. It is the mistake a deployment actually makes -- a chart that
/// only sets `MEDIACORE_POSTGRES_URL` when somebody configured a database, a
/// `.env` written from memory -- and under `envsubst` it started a server that
/// looked healthy and rejected every token.
#[test]
fn a_variable_the_deployment_forgot_is_refused_by_name() {
    let short: Vec<(&str, &str)> = bare()
        .into_iter()
        .filter(|(name, _)| *name != "MEDIACORE_API_SECRET")
        .collect();

    let refusal = render(&short)
        .expect_err("a stack with no api secret in its environment rendered a config anyway")
        .to_string();

    assert!(
        refusal.contains("MEDIACORE_API_SECRET"),
        "the refusal has to name the variable somebody has to set: {refusal}"
    );
}

/// Two things drift and one used to be caught here. The old check was that every
/// placeholder in the template appeared in the `envsubst` allowlist in
/// `start-sfu.sh`, because a name missing from that list was left in the
/// rendered file verbatim. There is no list any more, and the mechanism that
/// took its place is `export`: the script hands the server its own process, and
/// the server reads the names out of the environment it inherits. So what can
/// drift now is a template that names a variable the script does not export,
/// which is a refusal at startup -- a better failure than a nonsense value, and
/// still not one to discover on a host with people waiting in a room.
#[test]
fn every_variable_the_template_names_is_one_the_stack_exports() {
    assert!(
        !START_SFU
            .lines()
            .any(|line| line.trim_start().starts_with("envsubst")),
        "start-sfu.sh renders the config again. The server does it now, this was the only caller \
         of envsubst left in the image, and a rendered copy is the api secret back on a filesystem"
    );
    assert!(
        START_SFU.contains("--config \"$TEMPLATE\""),
        "start-sfu.sh has to hand the server the file that still has the variables in it: a \
         rendered copy would put the api secret back on a filesystem"
    );

    let ledger: BTreeSet<String> = bare()
        .into_iter()
        .map(|(name, _)| name.to_owned())
        .collect();
    let exported = exported(START_SFU);

    for name in placeholders(TEMPLATE) {
        assert!(
            name.starts_with(variables::PREFIX),
            "the template asks for `{name}`, which is not a name the server fills in: it would \
             be left in the config as text. Names begin with {}",
            variables::PREFIX
        );
        assert!(
            ledger.contains(&name),
            "{name} is in the template and not in the list this test holds the stack to; if the \
             stack really does set it, add it here"
        );
        assert!(
            exported.contains(&name),
            "{name} is in the template and start-sfu.sh does not export it, so it is not in the \
             environment of the server the script execs and the server will refuse to start by \
             name. Export it there, or take it out of the template"
        );
    }

    // The ledger is this test's statement about what the stack guarantees, so a
    // name in it that nothing exports is a guarantee the script does not keep --
    // worth catching when the ledger grows rather than when the template
    // eventually asks for it.
    for name in &ledger {
        assert!(
            exported.contains(name),
            "{name} is in the list this test holds the stack to and start-sfu.sh does not export \
             it"
        );
    }
}

/// Why the check above is `export` and not "the script mentions it". These six
/// are the script's own: it defaults them with `: "${X:=...}"`, builds the
/// `ice_servers` block out of them and exports none of them, so the server never
/// sees one. A guard that asked only whether the name appears in the script
/// would pass a template that asked for `${MEDIACORE_TURN_HOST}` -- and give a
/// server that refuses to start, on a host, for a name the test had blessed.
#[test]
fn a_name_the_script_only_mentions_is_not_a_name_the_server_can_read() {
    let exported = exported(START_SFU);

    for name in [
        "MEDIACORE_TURN_HOST",
        "MEDIACORE_TURN_PORT",
        "MEDIACORE_TURN_TLS_PORT",
        "MEDIACORE_TURN_TTL_SECONDS",
        "MEDIACORE_TURN_TLS_CERT",
        "MEDIACORE_TURN_TLS_KEY",
    ] {
        assert!(
            START_SFU.contains(name),
            "{name} has left start-sfu.sh, so this test is guarding nothing"
        );
        assert!(
            !exported.contains(name),
            "{name} is exported now, which the server can read -- if that is deliberate, this \
             test is the wrong shape and the template may name it"
        );
    }
}

/// The script measures the api secret before the server ever sees it, so its
/// floor has to be the server's floor. Two numbers here is the disagreement
/// `Config::validate` was given this check to end, said twice in one deployment:
/// a stack that started on a secret the server then refuses learns about it from
/// a crash loop, and one the script refuses and the server would take is a
/// deployment that cannot start for a reason nothing else in the product holds.
#[test]
fn the_length_the_script_insists_on_is_the_length_the_server_insists_on() {
    assert!(
        START_SFU.contains("${#MEDIACORE_API_SECRET}"),
        "start-sfu.sh no longer measures MEDIACORE_API_SECRET, so this test guards nothing"
    );
    assert!(
        START_SFU.contains(&format!("-lt {}", mediacore_config::MIN_SECRET_BYTES)),
        "start-sfu.sh refuses a secret at a length that is not the {} bytes the server refuses \
         below, so one of the two will turn away a secret the other signs with",
        mediacore_config::MIN_SECRET_BYTES
    );
}
