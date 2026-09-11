use std::collections::HashMap;

use super::*;

/// The environment as a test writes it. A map rather than the real environment,
/// because these tests share one process with each other and with the rest of
/// this crate's.
fn given(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn rendered(body: &str, pairs: &[(&str, &str)]) -> anyhow::Result<String> {
    let environment = given(pairs);
    expand_with(body, |name| environment.get(name).cloned())
}

#[test]
fn a_name_the_environment_has_is_filled_in() {
    let body = rendered(
        "keys:\n  \"${MEDIACORE_API_KEY}\": \"${MEDIACORE_API_SECRET}\"\n",
        &[
            ("MEDIACORE_API_KEY", "mediacore"),
            ("MEDIACORE_API_SECRET", "0123456789abcdef0123456789abcdef"),
        ],
    )
    .expect("a config whose variables are all set was refused");

    assert_eq!(
        body,
        "keys:\n  \"mediacore\": \"0123456789abcdef0123456789abcdef\"\n"
    );
}

/// The failure this whole module exists to stop. `envsubst` filled an unset
/// name in with nothing and said nothing, so a deployment that forgot one line
/// got a server that started, answered, and rejected every token anybody
/// presented -- which reads from the outside like a client bug and is not one.
#[test]
fn a_name_the_environment_does_not_have_is_refused_by_name() {
    let refusal = rendered(
        "keys:\n  \"${MEDIACORE_API_KEY}\": \"${MEDIACORE_API_SECRET}\"\n",
        &[("MEDIACORE_API_KEY", "mediacore")],
    )
    .expect_err("a config whose api secret would have expanded to nothing was accepted");

    assert!(
        refusal.to_string().contains("MEDIACORE_API_SECRET"),
        "the refusal has to name the variable somebody has to set, and it says: {refusal}"
    );
}

/// One restart per missing name is one outage per missing name, and a deployment
/// that forgot the database url has usually forgotten the bucket credentials
/// beside it.
#[test]
fn every_name_the_environment_is_missing_is_named_at_once() {
    let refusal = rendered(
        "url: \"${MEDIACORE_POSTGRES_URL}\"\nkey: \"${MEDIACORE_S3_ACCESS_KEY_ID}\"\nsecret: \
         \"${MEDIACORE_S3_SECRET_ACCESS_KEY}\"\n",
        &[],
    )
    .expect_err("a config with three unset variables was accepted");

    let refusal = refusal.to_string();
    for name in [
        "MEDIACORE_POSTGRES_URL",
        "MEDIACORE_S3_ACCESS_KEY_ID",
        "MEDIACORE_S3_SECRET_ACCESS_KEY",
    ] {
        assert!(
            refusal.contains(name),
            "{name} is missing too and the refusal does not say so: {refusal}"
        );
    }
}

/// Set to nothing is a decision somebody made; unset is the absence of one. The
/// difference is the whole of how `deploy/start-sfu.sh` says "this stack has no
/// database and no bucket" -- it sets those names to empty deliberately -- and
/// collapsing the two would either refuse the single-process stack or accept the
/// deployment that forgot its api secret.
#[test]
fn a_name_set_to_nothing_is_a_value_and_not_a_refusal() {
    let body = rendered(
        "postgres:\n  url: \"${MEDIACORE_POSTGRES_URL}\"\n",
        &[("MEDIACORE_POSTGRES_URL", "")],
    )
    .expect("an empty value is what `no database` looks like, and it was refused");

    assert_eq!(body, "postgres:\n  url: \"\"\n");
}

/// Without an escape there is no way to write a dollar at all, and both a
/// password and a piece of room metadata may contain one.
#[test]
fn two_dollars_are_one_dollar() {
    let body = rendered(
        "postgres:\n  url: \"postgres://mediacore:pa$$word@db/mediacore\"\n",
        &[],
    )
    .expect("a password with a dollar in it was refused");

    assert_eq!(
        body,
        "postgres:\n  url: \"postgres://mediacore:pa$word@db/mediacore\"\n"
    );
}

/// The property `envsubst`'s seven-name list had, kept by the prefix: a
/// `${...}` this server does not own is somebody else's text and is none of its
/// business.
#[test]
fn a_name_that_is_not_ours_is_left_exactly_as_written() {
    let body = rendered(
        "# see ${HOME}/.config for the client\nmetadata: '{\"template\": \"${room.name}\"}'\n",
        &[("HOME", "/root")],
    )
    .expect("a config carrying somebody else's placeholder was refused");

    assert_eq!(
        body,
        "# see ${HOME}/.config for the client\nmetadata: '{\"template\": \"${room.name}\"}'\n",
        "a name outside the MEDIACORE_ prefix must come through untouched even when the \
         environment happens to have it"
    );
}

/// A config with nothing to fill in is the common case, and it must come out of
/// this the same bytes it went in: whatever else changed, a deployment that
/// writes its values into the file is reading exactly what it wrote.
#[test]
fn a_config_with_no_variables_is_unchanged_byte_for_byte() {
    let body = "port: 7880\nkeys:\n  devkey: devsecretdevsecretdevsecretdevsecret\n";

    assert_eq!(
        rendered(body, &[]).expect("a config with no variables in it was refused"),
        body
    );
}

/// `config-sample.yaml` is the file people copy, and it documents this syntax by
/// writing it out, which means it contains the escape. So it is the one real
/// config that exercises `$$`: it has to load on a machine with nothing set at
/// all, and the doubled dollars have to survive as single ones in the text a
/// reader ends up looking at.
#[test]
fn the_shipped_sample_asks_the_environment_for_nothing() {
    const SAMPLE: &str = include_str!("../../../config-sample.yaml");

    let body = rendered(SAMPLE, &[]).expect(
        "config-sample.yaml asks for a variable, so copying it gives somebody a server that will \
         not start until they set one",
    );

    assert!(
        !body.contains("$$"),
        "the sample's escapes did not survive as single dollars"
    );
    assert!(
        body.contains("${MEDIACORE_API_SECRET}"),
        "the sample documents the syntax, and an escaped placeholder has to come out as the text \
         it describes"
    );
}

/// A dollar is a character, and a config may end on one. It is worth its own
/// test because the scan looks at what follows a dollar and there is nothing
/// following this one.
#[test]
fn a_dollar_at_the_end_of_the_file_is_a_dollar() {
    assert_eq!(
        rendered("prefix: meetings/$", &[]).unwrap(),
        "prefix: meetings/$"
    );
    assert_eq!(rendered("$", &[]).unwrap(), "$");
    assert_eq!(rendered("$$", &[]).unwrap(), "$");
}

/// A value is data. A database password that happens to contain what looks like
/// a placeholder must not be able to reach for the api secret, and one that
/// contains two dollars must arrive with two.
#[test]
fn a_value_that_was_filled_in_is_never_read_again() {
    let body = rendered(
        "password: \"${MEDIACORE_POSTGRES_PASSWORD}\"\n",
        &[
            (
                "MEDIACORE_POSTGRES_PASSWORD",
                "${MEDIACORE_API_SECRET}-pa$$word",
            ),
            ("MEDIACORE_API_SECRET", "0123456789abcdef0123456789abcdef"),
        ],
    )
    .expect("a password that looks like a placeholder was refused");

    assert_eq!(body, "password: \"${MEDIACORE_API_SECRET}-pa$$word\"\n");
}

/// Four ways to almost write a placeholder, every one of which would otherwise
/// end as an api secret that is literally the text somebody typed, on a server
/// that starts and rejects every token. The bare form is the one that used to
/// work: `envsubst` filled it in, so passing it through would break a file that
/// was correct yesterday.
#[test]
fn a_near_miss_is_refused_and_the_refusal_says_what_to_write() {
    let secret = [("MEDIACORE_API_SECRET", "0123456789abcdef0123456789abcdef")];

    for (body, expected) in [
        ("secret: \"${mediacore_api_secret}\"\n", "upper case"),
        ("secret: \"${MEDIACORE_API_SECRET \"\n", "$$"),
        ("secret: \"${MEDIACORE_API_SECRET\"\n", "$$"),
        ("secret: \"$MEDIACORE_API_SECRET\"\n", "$$"),
    ] {
        let refusal = rendered(body, &secret)
            .expect_err(&format!(
                "`{body}` left the api secret in the config as text"
            ))
            .to_string();

        assert!(
            refusal.to_lowercase().contains("mediacore_api_secret"),
            "the refusal has to name what was written: {refusal}"
        );
        assert!(
            refusal.contains(expected),
            "the refusal has to say what to write instead, and `{expected}` is not in it: {refusal}"
        );
    }
}

/// A name outside the prefix in a shape that is not a name at all -- the case
/// the near-miss refusals must not catch, because it is somebody else's text.
#[test]
fn a_shape_that_is_not_a_name_and_is_not_ours_is_left_alone() {
    for body in ["a: ${}\n", "b: ${not a name}\n", "c: ${\n", "d: $ 5\n"] {
        assert_eq!(
            rendered(body, &[]).expect("somebody else's text was refused"),
            body
        );
    }
}

/// The `ice_servers:` block `start-sfu.sh` builds is several lines with their
/// own indentation, and it arrives through one variable. A substitution that
/// only worked on scalars would leave the relay out of the config and the
/// deployment would find out from a room where nobody behind a NAT can be heard.
#[test]
fn a_value_may_be_a_whole_block_of_yaml() {
    let block = "  ice_servers:\n    - urls:\n        - \"turn:turn.example.com:3478\"";
    let body = rendered(
        "rtc:\n  udp_port: 7882\n${MEDIACORE_ICE_SERVERS_BLOCK}\n",
        &[("MEDIACORE_ICE_SERVERS_BLOCK", block)],
    )
    .expect("a multi-line value was refused");

    assert_eq!(body, format!("rtc:\n  udp_port: 7882\n{block}\n"));
}
