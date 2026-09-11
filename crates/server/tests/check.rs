//! `mediacore check` as a Kubernetes init container runs it: a file, an
//! environment, an exit code and a message.
//!
//! The subcommand's product is those last two and nothing else -- a pod that
//! would not start is read with `kubectl logs`, which is the exit code and
//! whatever was written on the way out -- and neither of them exists until a
//! process has really run. So these go through the binary rather than calling
//! into it: what is under test here is that a refusal leaves the pod stopped
//! and the reason on stderr, and that the file is read exactly as the server
//! reads it, variables and all.

use std::process::{Command, Output};

/// The binary cargo built for this test run.
const BINARY: &str = env!("CARGO_BIN_EXE_mediacore");

/// Long enough to sign with, so that a fixture never fails on the wrong thing.
const KEYS: &str = "keys:\n  devkey: devsecretdevsecretdevsecretdevsecret\n";

/// A database and an object store, named the way a deployment names them.
const STORES: &str = "sessions:\n  postgres:\n    url: postgres://mediacore:pw@db/mediacore\n\
                      recording:\n  s3:\n    endpoint: http://minio:9000\n    bucket: meetings\n    \
                      access_key_id: key\n    secret_access_key: secret\n";

/// A node's own address: private, which this deliberately does not refuse.
const ROUTABLE: &str = "rtc:\n  advertise_addresses:\n    - 10.251.10.7\n";

fn check(body: &str, flags: &[&str], environment: &[(&str, &str)]) -> Output {
    let directory = tempfile::TempDir::new().expect("a directory to put the config in");
    let path = directory.path().join("mediacore.yaml");
    std::fs::write(&path, format!("{KEYS}{body}")).expect("writing the config");

    let mut command = Command::new(BINARY);
    command.arg("--config").arg(&path).arg("check").args(flags);

    // These tests are about what the environment holds, so whatever the machine
    // running them exports must not reach the child and decide the answer.
    for name in ["MEDIACORE_CONFIG", "MEDIACORE_POSTGRES_URL"] {
        command.env_remove(name);
    }
    for (name, value) in environment {
        command.env(name, value);
    }

    command.output().expect("running the check")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_config_that_is_what_the_deployment_asked_for_exits_zero() {
    let output = check(
        &format!("{STORES}{ROUTABLE}"),
        &[
            "--expect-postgres",
            "--expect-recording",
            "--routable-advertise",
        ],
        &[],
    );

    assert!(
        output.status.success(),
        "the pod has to be allowed to start: {}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("10.251.10.7"),
        "and say what it agreed to: {}",
        stdout(&output)
    );
}

/// The caller is a Helm template, and a chart that emits `--expect-postgres`
/// for its database and again for its recording -- which needs one -- is the
/// obvious way to write it and says nothing contradictory. Clap refuses a
/// repeated flag by default, with `cannot be used multiple times` and no hint
/// as to which condition duplicated it.
///
/// Asked of a config that meets neither of them, deliberately. Against one that
/// meets both, the run exits 0 whether the second `--expect-postgres` stated the
/// flag or reset it to its default, so the assertion would hold on the day an
/// `overrides_with` changed meaning underneath it and a chart's second condition
/// began cancelling its first -- which is a check that installs, runs, agrees
/// with everything and guards nothing. Here the repeated flag is the only thing
/// that puts `sessions.postgres.url` in the message: the refusal beside it names
/// the object store and never the database.
#[test]
fn a_flag_the_chart_emits_under_two_conditions_is_still_one_statement() {
    let output = check(
        "",
        &[
            "--expect-postgres",
            "--expect-recording",
            "--expect-postgres",
        ],
        &[],
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "1 is this subcommand's refusal; 2 would be clap refusing the repeated flag itself: {}",
        stderr(&output)
    );

    let said = stderr(&output);
    assert!(
        said.contains("sessions.postgres.url"),
        "saying it twice has to be saying it once, and this is the refusal only the repeated flag \
         asks for: {said}"
    );
    assert!(
        said.contains("recording.s3.bucket"),
        "and the statement beside it still stands: {said}"
    );
}

/// And the ordering a chart author reaches for first. `--config` after the
/// subcommand reads as the flag belonging to the subcommand that uses it, and
/// it used to be `error: unexpected argument '--config' found` -- a clap usage
/// line in the `args:` of an init container, over an argument order.
#[test]
fn the_config_path_may_be_written_on_either_side_of_the_subcommand() {
    let directory = tempfile::TempDir::new().expect("a directory to put the config in");
    let path = directory.path().join("mediacore.yaml");
    std::fs::write(&path, format!("{KEYS}{STORES}")).expect("writing the config");

    for arguments in [
        vec!["--config", path.to_str().expect("a utf-8 path"), "check"],
        vec!["check", "--config", path.to_str().expect("a utf-8 path")],
    ] {
        let output = Command::new(BINARY)
            .args(&arguments)
            .env_remove("MEDIACORE_CONFIG")
            .output()
            .expect("running the check");

        assert!(
            output.status.success(),
            "`mediacore {}` has to read that file: {}",
            arguments.join(" "),
            stderr(&output)
        );
    }
}

/// The whole of the contract an init container has: a non-zero code stops the
/// pod, and the reason is on the stream `kubectl logs` shows with nothing else
/// mixed into it.
#[test]
fn a_refusal_stops_the_pod_and_leaves_the_reason_on_stderr() {
    let output = check(
        "rtc:\n  advertise_addresses:\n    - 127.0.0.1\n",
        &["--routable-advertise"],
        &[],
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "a refusal is a failure, not a warning"
    );
    assert!(
        stdout(&output).is_empty(),
        "a refusal says nothing on stdout: {}",
        stdout(&output)
    );

    let said = stderr(&output);
    assert!(
        said.contains("127.0.0.1") && said.contains("MEDIACORE_ADVERTISE_IP"),
        "the reason has to quote the address and name what sets it: {said}"
    );
}

/// One name per restart is one outage per name, which is the reason
/// `mediacore_config::variables` collects missing variables rather than raising
/// the first of them.
#[test]
fn every_expectation_that_was_not_met_is_reported_at_once() {
    let output = check(
        "",
        &[
            "--expect-postgres",
            "--expect-recording",
            "--routable-advertise",
        ],
        &[],
    );

    let said = stderr(&output);
    assert!(!output.status.success(), "this config meets none of them");
    for setting in [
        "sessions.postgres.url",
        "recording.s3.bucket",
        "rtc.advertise_addresses",
    ] {
        assert!(
            said.contains(setting),
            "{setting} was not met and is not in the message: {said}"
        );
    }
}

/// A deployment that asked for none of it is a deployment this must not refuse:
/// one process, meetings in a directory, and a media address it is happy to
/// guess.
#[test]
fn a_check_with_no_expectations_accepts_a_config_that_states_nothing() {
    let output = check("", &[], &[]);

    assert!(
        output.status.success(),
        "nothing was asked for and nothing is wrong: {}",
        stderr(&output)
    );
}

/// The file is read the way the server reads it -- `${MEDIACORE_*}` filled in
/// from this process's environment -- rather than parsed a second way. A check
/// that read it differently would pass a config the server then refuses.
#[test]
fn a_variable_the_environment_does_not_have_is_named_before_anything_else() {
    let output = check(
        "sessions:\n  postgres:\n    url: \"${MEDIACORE_POSTGRES_URL}\"\n",
        &["--expect-postgres"],
        &[],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("MEDIACORE_POSTGRES_URL"),
        "the missing variable is the thing to fix, and it has to be named: {}",
        stderr(&output)
    );
}

/// And the same file passes once it is set, which is what says the expectation
/// is judged against the value rather than against the `${...}` in the text.
#[test]
fn the_same_file_passes_once_the_variable_is_set() {
    let output = check(
        "sessions:\n  postgres:\n    url: \"${MEDIACORE_POSTGRES_URL}\"\n",
        &["--expect-postgres"],
        &[(
            "MEDIACORE_POSTGRES_URL",
            "postgres://mediacore:pw@db/mediacore",
        )],
    );

    assert!(
        output.status.success(),
        "the variable is set and the database is named: {}",
        stderr(&output)
    );
    assert!(
        !stdout(&output).contains("pw@"),
        "a url carries a password and this line is read by whoever can read the pod: {}",
        stdout(&output)
    );
}

/// Most of what an init container is for is here even with no flags at all: the
/// server's own validation, run once where it is a message, instead of by the
/// server container where it is a restart loop.
#[test]
fn the_servers_own_refusals_are_made_here_rather_than_in_a_restart_loop() {
    // A name where an address belongs, which is what MEDIACORE_ADVERTISE_IP set
    // to a host name renders as. `Config::validate` refuses it on its own, with
    // no flag asked for and no opinion from this subcommand.
    let output = check(
        "rtc:\n  advertise_addresses:\n    - node.example.com\n",
        &[],
        &[],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("rtc.advertise_addresses"),
        "the server's own refusal has to reach the init container's log: {}",
        stderr(&output)
    );
}
