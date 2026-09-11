use super::*;

/// The pair `deploy/docker-compose.local.yml` uses, and long enough to sign
/// with: a fixture the server would refuse is a fixture that tests the wrong
/// refusal.
const KEYS: &str = "keys:\n  devkey: devsecretdevsecretdevsecretdevsecret\n";

/// A database, named the way a deployment names one.
const POSTGRES: &str = "sessions:\n  postgres:\n    url: postgres://mediacore:pw@db/mediacore\n";

/// An object store with everything it needs, and `recording.enabled` left off:
/// a bucket is what makes recording possible, and whether every room records by
/// default is a different question.
const BUCKET: &str = "recording:\n  s3:\n    endpoint: http://minio:9000\n    bucket: meetings\n    \
                      access_key_id: key\n    secret_access_key: secret\n";

/// A config the server accepts, with whatever the test is about added to it.
fn config(body: &str) -> Config {
    Config::parse(&format!("{KEYS}{body}"))
        .expect("a fixture has to be a config the server itself accepts")
}

/// The single refusal one expectation produced.
fn refusal_for(config: &Config, expected: Expected) -> String {
    let mut said = answers(config, expected);

    assert_eq!(said.len(), 1, "one flag has to ask exactly one question");
    said.remove(0)
        .expect_err("this config was meant not to meet the expectation")
}

/// What `advertise` says about the addresses a config states, written the way a
/// config writes them.
fn advertising(addresses: &[&str]) -> Answer {
    let entries: String = addresses
        .iter()
        .map(|address| format!("    - \"{address}\"\n"))
        .collect();

    advertise(&config(&format!("rtc:\n  advertise_addresses:\n{entries}")).rtc)
}

/// The failure this flag exists for: nothing is wrong with the config, and
/// every meeting record has quietly moved onto a directory.
#[test]
fn a_directory_where_a_database_was_asked_for_is_refused_by_name() {
    let refusal = refusal_for(
        &config(""),
        Expected {
            postgres: true,
            ..Expected::default()
        },
    );

    assert!(
        refusal.contains("sessions.postgres.url") && refusal.contains("MEDIACORE_POSTGRES_URL"),
        "the refusal has to name the setting and the variable that fills it: {refusal}"
    );
    assert!(
        refusal.contains("sessions"),
        "and the directory the records would go to instead: {refusal}"
    );
}

#[test]
fn a_database_that_is_named_satisfies_the_flag() {
    let said = answers(
        &config(POSTGRES),
        Expected {
            postgres: true,
            ..Expected::default()
        },
    );

    let line = said[0].as_ref().expect("a named database is agreement");
    assert!(
        !line.contains("pw@"),
        "a url carries a password and must never be printed: {line}"
    );
}

/// Every missing piece at once. A Secret is very often three keys of four, and
/// being told about them one restart at a time is one outage per key.
#[test]
fn recording_with_no_object_store_names_every_setting_that_is_empty() {
    let refusal = refusal_for(
        &config(POSTGRES),
        Expected {
            recording: true,
            ..Expected::default()
        },
    );

    for setting in [
        "recording.s3.endpoint",
        "recording.s3.bucket",
        "recording.s3.access_key_id",
        "recording.s3.secret_access_key",
    ] {
        assert!(
            refusal.contains(setting),
            "{setting} is empty and the refusal does not say so: {refusal}"
        );
    }
}

/// `recording.enabled` is the default for rooms, not a master switch: a
/// deployment that records only the rooms which ask for it has an object store
/// and has it switched off, and that is not a mistake.
#[test]
fn a_bucket_and_credentials_are_enough_even_when_no_room_records_by_default() {
    let said = answers(
        &config(&format!("{POSTGRES}{BUCKET}")),
        Expected {
            recording: true,
            ..Expected::default()
        },
    );

    let line = said[0]
        .as_ref()
        .expect("an object store that is there is agreement");
    assert!(
        line.contains("meetings") && !line.contains("secret"),
        "the bucket is in the ConfigMap and the keys beside it are not: {line}"
    );
}

/// The value a local trial sets on purpose, which is why the server accepts it
/// and only the deployment can call it wrong.
#[test]
fn loopback_is_refused_when_the_deployment_says_the_address_has_to_be_routable() {
    let refusal =
        advertising(&["127.0.0.1"]).expect_err("nothing off this host can reach loopback");

    assert!(
        refusal.contains("127.0.0.1") && refusal.contains("MEDIACORE_ADVERTISE_IP"),
        "the refusal has to quote the address and name what sets it: {refusal}"
    );
}

#[test]
fn the_wildcard_is_refused_because_it_is_not_an_address_at_all() {
    for wildcard in ["0.0.0.0", "::"] {
        let refusal = advertising(&[wildcard])
            .expect_err("the wildcard is what a socket listens on, not where anything sends");

        assert!(
            refusal.contains(wildcard),
            "the refusal has to quote the address: {refusal}"
        );
    }
}

/// A node whose DHCP lease never arrived reports one of these as
/// `status.hostIP`, and the chart fills it in without a word. It fails exactly
/// the way loopback does, and for everybody who is not on that wire.
#[test]
fn a_link_local_address_is_refused_like_the_failed_lease_it_usually_is() {
    for address in ["169.254.3.4", "fe80::1", "febf::1"] {
        let refusal = advertising(&[address])
            .expect_err("a link-local address is reachable only on its own wire");

        assert!(
            refusal.contains("link-local"),
            "the refusal has to say what kind of address it is: {refusal}"
        );
    }
}

/// The same address written the other way. Judged as it stands it is neither
/// loopback nor the wildcard, so this is the one spelling that could slip
/// through and be exactly as silent.
#[test]
fn an_ipv4_mapped_address_is_judged_as_the_address_it_holds() {
    for (address, what) in [
        ("::ffff:127.0.0.1", "loopback"),
        ("::ffff:169.254.3.4", "link-local"),
    ] {
        let said = advertising(&[address]);
        assert!(
            said.is_err(),
            "{address} is {what} written the other way: {said:?}"
        );
    }

    let said = advertising(&["::ffff:10.251.10.7"]);
    assert!(
        said.is_ok(),
        "a mapped private address is an ordinary one: {said:?}"
    );
}

/// Not refused, and this is the case that matters most: every node this is
/// built for is on a private address behind a relay or a port forward, and a
/// check that refused one would refuse production.
#[test]
fn private_addresses_are_what_production_runs_and_are_not_refused() {
    for address in ["10.251.10.7", "172.16.4.4", "192.168.1.20", "fd00::7"] {
        let said = advertising(&[address]);
        assert!(
            said.is_ok(),
            "{address} is an ordinary node address and was refused: {said:?}"
        );
    }
}

#[test]
fn a_public_address_is_what_the_flag_is_looking_for() {
    let line = advertising(&["203.0.113.7"]).expect("a routable address is agreement");

    assert!(
        line.contains("203.0.113.7"),
        "the line says what will be advertised: {line}"
    );
}

/// An entry may carry a port, and the port is not what is being judged.
#[test]
fn an_entry_with_a_port_is_judged_by_its_address() {
    let said = advertising(&["127.0.0.1:7882"]);
    assert!(
        said.is_err(),
        "a port does not make loopback reachable: {said:?}"
    );

    let said = advertising(&["10.251.10.7:7882"]);
    assert!(
        said.is_ok(),
        "a port is not a reason to refuse an address: {said:?}"
    );
}

/// A list that is wrong is usually wrong more than once, and being told about
/// the entries one restart at a time is one outage per entry -- the rule this
/// module opens with, which the refusal has to keep as well as the flags do.
#[test]
fn every_address_that_cannot_work_is_named_and_not_just_the_first() {
    let refusal = advertising(&["127.0.0.1", "10.251.10.7", "169.254.3.4"])
        .expect_err("two of these three are addresses nothing off this host can send to");

    assert!(
        refusal.contains("127.0.0.1") && refusal.contains("169.254.3.4"),
        "both of the entries that cannot work have to be in the message: {refusal}"
    );
    assert!(
        !refusal.contains("10.251.10.7"),
        "and the one that is fine must not be, or the message names the wrong thing to fix: \
         {refusal}"
    );
}

/// With nothing stated the server picks an address for itself, and inside a pod
/// that is the pod's own address on the cluster network.
#[test]
fn an_address_nobody_stated_is_refused_because_the_server_would_guess_one() {
    let refusal = advertise(&config("").rtc).expect_err("a guess is not a routable address");

    for setting in ["rtc.advertise_addresses", "rtc.node_ip", "rtc.bind_address"] {
        assert!(
            refusal.contains(setting),
            "the refusal has to name every place an address can be stated: {refusal}"
        );
    }
}

/// The fallback the media engine takes when `advertise_addresses` is empty, so
/// the check has to read it the same way or it would refuse a config that works.
#[test]
fn node_ip_stands_in_for_advertise_addresses() {
    let line = advertise(&config("rtc:\n  node_ip: 10.251.10.7\n").rtc)
        .expect("rtc.node_ip is where the media engine looks next");

    assert!(
        line.contains("10.251.10.7"),
        "and it is the address that will be advertised: {line}"
    );
}

/// And the step after that one. `use_socket_address` is on by default, so a
/// config that binds the media socket to one interface has stated its address
/// there -- `resolve_advertise` hands out that exact address -- and a check
/// that stopped at `node_ip` would refuse a deployment that works.
#[test]
fn a_media_socket_bound_to_one_interface_states_its_address() {
    let line = advertise(&config("rtc:\n  bind_address: 10.251.10.7\n").rtc)
        .expect("this is the address the media engine will advertise");

    assert!(
        line.contains("10.251.10.7"),
        "and it is the address that will be advertised: {line}"
    );

    let refusal = advertise(&config("rtc:\n  bind_address: 127.0.0.1\n").rtc)
        .expect_err("a socket bound to loopback advertises loopback");
    assert!(
        refusal.contains("127.0.0.1"),
        "judged like any other stated address: {refusal}"
    );
}

/// The two ways that step is not a statement, and the media engine takes
/// neither: the wildcard is what it binds when nobody chose an interface, and
/// `use_socket_address: false` is a deployment saying "not this one".
#[test]
fn a_bound_address_the_media_engine_would_not_advertise_states_nothing() {
    for body in [
        "rtc:\n  bind_address: 0.0.0.0\n",
        "rtc:\n  bind_address: 10.251.10.7\n  use_socket_address: false\n",
    ] {
        let refusal = advertise(&config(body).rtc)
            .expect_err("the engine would reach past this and guess an address");

        assert!(
            refusal.contains("rtc.bind_address"),
            "and the refusal has to say why that step was not one: {refusal}"
        );
    }
}

/// The one refusal here no flag asks for. `Config::validate` counts the bind
/// addresses and never reads one, so this is the last thing between a config it
/// accepts and a process that exits on `invalid socket address syntax` with
/// nothing named.
#[test]
fn a_bind_address_the_server_cannot_listen_on_is_refused_although_the_config_is_valid() {
    let refusal = listen(&config("bind_addresses:\n  - eth0\n"))
        .expect_err("an interface name is not an address to listen on");

    assert!(
        refusal.contains("eth0:") && refusal.contains("bind_addresses"),
        "the refusal has to quote what the server would build and name the setting: {refusal}"
    );
}

/// Built with the same `format!` the server builds it with, brackets and all:
/// a check that knew better than `main` would pass a config `main` dies on.
#[test]
fn an_ipv6_bind_address_is_judged_the_way_the_server_joins_it_to_the_port() {
    assert!(
        listen(&config("bind_addresses:\n  - \"::\"\n")).is_err(),
        "`:::7880` is what the server builds out of this, and it is not an address"
    );
    assert!(
        listen(&config("bind_addresses:\n  - \"[::]\"\n")).is_ok(),
        "the bracketed spelling is the one that survives being joined to a port"
    );
}

/// A chart states one intent at a time -- Postgres without recording, a routable
/// address without either -- so a flag must answer for itself and for nothing
/// else.
#[test]
fn the_flags_are_independent_of_one_another() {
    // Fails all three: no database, no bucket, no address stated.
    let config = config("");

    assert!(
        answers(&config, Expected::default()).is_empty(),
        "a deployment that asked for nothing has nothing to refuse"
    );

    for (expected, setting) in [
        (
            Expected {
                postgres: true,
                ..Expected::default()
            },
            "sessions.postgres.url",
        ),
        (
            Expected {
                recording: true,
                ..Expected::default()
            },
            "recording.s3.bucket",
        ),
        (
            Expected {
                routable_advertise: true,
                ..Expected::default()
            },
            "rtc.advertise_addresses",
        ),
    ] {
        let refusal = refusal_for(&config, expected);
        assert!(
            refusal.contains(setting),
            "the flag that was passed has to be the one that answered: {refusal}"
        );
    }

    let all = answers(
        &config,
        Expected {
            postgres: true,
            recording: true,
            routable_advertise: true,
        },
    );
    assert_eq!(
        all.iter().filter(|answer| answer.is_err()).count(),
        3,
        "three flags on a config that meets none of them is three refusals, not the first"
    );

    // And the other direction: a config that meets every one of them.
    let complete = config_meeting_everything();
    assert!(
        answers(
            &complete,
            Expected {
                postgres: true,
                recording: true,
                routable_advertise: true,
            }
        )
        .iter()
        .all(Result::is_ok),
        "a deployment that did what it said has nothing refused"
    );
}

fn config_meeting_everything() -> Config {
    config(&format!(
        "{POSTGRES}{BUCKET}rtc:\n  advertise_addresses:\n    - 10.251.10.7\n"
    ))
}
