
use super::*;

#[test]
fn fills_defaults_around_supplied_values() {
    let config = Config::parse("port: 8080\nkeys:\n  key1: secret1\n").unwrap();

    assert_eq!(config.port, 8080);
    assert_eq!(config.keys.get("key1").unwrap(), "secret1");
    assert_eq!(config.room.empty_timeout, 300);
    assert!(config.room.auto_create);
    assert!(config.node_id.starts_with("ND_"));
}

/// A store of files cannot be an upload queue, and a server that has to
/// find that out at the first recording has already held a week of
/// meetings.
#[test]
fn recording_without_a_database_is_refused_at_startup() {
    let refused = Config::parse(
        "keys:\n  k: s\nrecording:\n  enabled: true\n  s3:\n    endpoint: \
             http://minio:9000\n    bucket: meetings\n    access_key_id: k\n    \
             secret_access_key: s\n",
    )
    .expect_err("a recorder with nowhere to queue its uploads must not start");

    assert!(
        refused.to_string().contains("sessions.postgres.url"),
        "the refusal has to name the setting that would fix it: {refused}"
    );
}

/// The object store's deadline is its own number, and it is related to the
/// size ceiling it has to carry.
///
/// It used to be `sessions.postgres.timeout_ms` -- how long one SQL
/// statement may hold a connection -- doing duty as a total deadline for a
/// whole `PUT`. Ten seconds is a fine statement timeout and asks an
/// ordinary two-hour participant's thirty megabytes to move at 24 Mbit/s;
/// past that the upload fails every attempt and the recording is given up
/// on with the audio still on the node's disk and nothing announced.
#[test]
fn an_upload_has_its_own_deadline_and_it_has_to_fit_the_segment_it_carries() {
    let config = Config::parse(
        "keys:\n  k: s\nsessions:\n  postgres:\n    url: postgres://x/y\n    timeout_ms: \
             10000\n    lease_ms: 30000\nrecording:\n  enabled: true\n  s3:\n    endpoint: \
             http://minio:9000\n    bucket: meetings\n    access_key_id: k\n    \
             secret_access_key: s\n",
    )
    .expect("a recording deployment with the defaults");

    assert_eq!(
        config.recording.s3_timeout(),
        Duration::from_millis(u64::from(DEFAULT_RECORDING_S3_TIMEOUT_MS)),
        "an upload's deadline has to come from recording.s3.timeout_ms"
    );
    assert_ne!(
        config.recording.s3_timeout(),
        Duration::from_millis(u64::from(config.sessions.postgres.timeout_ms)),
        "the upload deadline is still the database's statement timeout, which is a number \
             about how long a query may hold a connection and has nothing to say about how long a \
             quarter-gigabyte object takes to reach a bucket"
    );

    let refused = Config::parse(
        "keys:\n  k: s\nsessions:\n  postgres:\n    url: postgres://x/y\nrecording:\n  \
             enabled: true\n  max_segment_mb: 1024\n  s3:\n    endpoint: http://minio:9000\n    \
             bucket: meetings\n    access_key_id: k\n    secret_access_key: s\n    timeout_ms: \
             10000\n",
    )
    .expect_err(
        "a segment ceiling the upload deadline cannot carry at even a megabyte a second has \
             to be refused at startup, not discovered weeks later by somebody asking for a \
             recording",
    );

    assert!(
        refused.to_string().contains("recording.s3.timeout_ms"),
        "the refusal has to name the setting that would fix it: {refused}"
    );
}

#[test]
fn recording_without_a_bucket_is_refused_at_startup() {
    let refused = Config::parse(
        "keys:\n  k: s\nsessions:\n  postgres:\n    url: postgres://x/y\nrecording:\n  \
             enabled: true\n",
    )
    .expect_err("a recorder with nowhere to upload to must not start");

    assert!(
        refused.to_string().contains("recording.s3.endpoint"),
        "{refused}"
    );
}

/// A bucket named without `enabled` is a deployment that records the rooms
/// that ask for it, so it is checked as hard as one that records
/// everything.
#[test]
fn a_bucket_named_without_the_default_switch_is_still_a_recording_deployment() {
    let refused = Config::parse(
        "keys:\n  k: s\nrecording:\n  s3:\n    endpoint: http://minio:9000\n    bucket: \
             meetings\n    access_key_id: k\n    secret_access_key: s\n",
    )
    .expect_err("a bucket with no database behind it must not start either");

    assert!(
        refused.to_string().contains("sessions.postgres.url"),
        "{refused}"
    );
}

#[test]
fn a_server_that_records_nothing_needs_none_of_it() {
    let config = Config::parse("keys:\n  k: s\n").expect("a server with no recording");

    assert!(!config.recording.enabled);
    assert!(
        !config.recording.possible(),
        "no bucket is no recording, and a deployment that never asked for it must not be \
             refused for not configuring it"
    );
}

#[test]
fn an_unknown_recording_setting_is_a_refusal_and_not_a_silent_default() {
    let refused = Config::parse("keys:\n  k: s\nrecording:\n  enabld: true\n")
        .expect_err("a typo in a setting name has to be a refusal");

    assert!(refused.to_string().contains("enabld"), "{refused}");
}

#[test]
fn the_session_policy_is_off_until_a_room_asks_for_one() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

    assert_eq!(
        config.room.min_humans, 0,
        "upgrading must not start ending rooms that nobody asked to be ended"
    );
    assert_eq!(config.room.end_grace, DEFAULT_END_GRACE_SECONDS);
    assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Suffix);
    assert!(
        !config.sessions.in_postgres(),
        "the sample names a database, so copying it points somebody's meetings at a server \
             they did not choose -- and the single process the sample describes does not need one"
    );
}

#[test]
fn the_livekit_twirp_alias_is_on_until_a_deployment_retires_it() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

    assert!(
        config.twirp.livekit_alias,
        "a deployment that upgrades without touching its config keeps answering \
             /twirp/livekit.RoomService/ -- every LiveKit server SDK builds that path in \
             generated code and cannot be pointed at another one"
    );

    let retired =
        Config::parse("keys:\n  key1: secret1\ntwirp:\n  livekit_alias: false\n").unwrap();
    assert!(!retired.twirp.livekit_alias);
}

#[test]
fn the_sweep_has_a_tick_of_its_own() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
    assert_eq!(
        config.room.sweep_interval(),
        Duration::from_secs(1),
        "the session policy and both room timeouts are counted in whole seconds, and nothing \
             else on the server enforces them"
    );

    let quick = Config::parse("keys:\n  key1: secret1\nroom:\n  sweep_interval_ms: 50\n").unwrap();
    assert_eq!(quick.room.sweep_interval(), Duration::from_millis(50));
}

#[test]
fn the_session_policy_and_the_duplicate_rule_are_configurable() {
    let config = Config::parse(
            "keys:\n  key1: secret1\nroom:\n  min_humans: 2\n  end_grace: 90\n  duplicate_identity: displace\n",
        )
        .unwrap();

    assert_eq!(config.room.min_humans, 2);
    assert_eq!(config.room.end_grace, 90);
    assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Displace);
    assert!(
        config.room.auto_create,
        "the rest of the room block kept its defaults"
    );

    assert!(
        Config::parse("keys:\n  key1: secret1\nroom:\n  duplicate_identity: rename\n").is_err(),
        "a rule the server does not have must not be read as one it does"
    );
}

#[test]
fn meetings_are_recorded_somewhere_whether_or_not_a_config_says_so() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

    assert_eq!(
        config.sessions.directory,
        PathBuf::from("sessions"),
        "a deployment that upgrades without touching its config still has to keep a record of \
             its meetings; there is no configuration that turns recording off"
    );

    let chosen =
        Config::parse("keys:\n  key1: secret1\nsessions:\n  directory: /srv/meetings\n").unwrap();
    assert_eq!(chosen.sessions.directory, PathBuf::from("/srv/meetings"));

    assert!(
        Config::parse("keys:\n  key1: secret1\nsessions:\n  directory: \"\"\n").is_err(),
        "an empty directory is a server that starts and quietly records nothing"
    );
}

#[test]
fn nobody_is_told_anything_until_a_destination_is_configured() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

    assert!(
        !config.events.has_destinations(),
        "an upgrade started posting webhooks nobody asked for, at urls nobody configured"
    );
    assert_eq!(config.events.queue, DEFAULT_EVENT_QUEUE);
    assert_eq!(config.events.attempts, DEFAULT_EVENT_ATTEMPTS);
    assert_eq!(config.events.directory, PathBuf::from("events"));
    assert_eq!(config.events.kafka.topic, DEFAULT_KAFKA_TOPIC);
    assert!(config.events.kafka.brokers.is_empty());
}

#[test]
fn the_webhook_block_is_configurable() {
    let config = Config::parse(
        "keys:\n  key1: secret1\nevents:\n  directory: /var/lib/mediacore/events\n  attempts: 3\n  \
             webhooks:\n    api_key: key1\n    urls:\n      - https://example.test/hook\n",
    )
    .unwrap();

    assert!(config.events.has_destinations());
    assert_eq!(
        config.events.webhooks.urls,
        vec!["https://example.test/hook"]
    );
    assert_eq!(config.events.attempts, 3);
    assert_eq!(
        config.events.directory,
        PathBuf::from("/var/lib/mediacore/events")
    );
    assert_eq!(
        config.events.timeout(),
        Duration::from_millis(DEFAULT_EVENT_TIMEOUT_MS.into()),
        "the rest of the events block kept its defaults"
    );
}

/// A webhook is signed with the secret belonging to the key named here,
/// and a receiver looks that key up to know which secret to check. Naming
/// a key this server does not have produces a token nobody can verify --
/// on every event, silently, until somebody reads the receiver's 401s.
#[test]
fn a_webhook_signed_by_a_key_this_server_does_not_have_is_refused_at_startup() {
    assert!(
        Config::parse(
            "keys:\n  key1: secret1\nevents:\n  webhooks:\n    api_key: someone-elses-key\n    \
                 urls:\n      - https://example.test/hook\n"
        )
        .is_err(),
        "every webhook this server sent would be signed with an empty secret and rejected"
    );
    assert!(
        Config::parse(
            "keys:\n  key1: secret1\nevents:\n  webhooks:\n    urls:\n \
                 - https://example.test/hook\n"
        )
        .is_err(),
        "endpoints with no signing key named at all were accepted"
    );
    assert!(
        Config::parse(
            "keys:\n  key1: secret1\nevents:\n  webhooks:\n    api_key: key1\n    urls:\n      \
                 - example.test/hook\n"
        )
        .is_err(),
        "a url with no scheme is a url nothing will ever be posted to"
    );
}

/// Finding I. A destination is named by its url, and the outbox strikes a
/// record off once every name that owed it has settled -- so two entries
/// with one name are one name, the first worker to finish deletes the
/// record while the second has not been tried, and a crash then loses that
/// event with nothing to say so.
#[test]
fn the_same_endpoint_cannot_be_named_twice() {
    let duplicated = Config::parse(
        "keys:\n  key1: secret1\nevents:\n  directory: /tmp/mediacore-events\n  webhooks:\n    \
             api_key: key1\n    urls:\n      - https://example.test/hook\n      - \
             https://example.test/hook\n",
    );

    assert!(
        duplicated.is_err(),
        "one url named twice is one destination that owes twice and settles once: the first \
             worker to finish deletes the outbox record while the second endpoint has not been \
             tried, and a crash there loses the event with no counter and no line"
    );
    assert!(
        Config::parse(
            "keys:\n  key1: secret1\nevents:\n  directory: /tmp/mediacore-events\n  webhooks:\n    \
                 api_key: key1\n    urls:\n      - https://example.test/hook\n      - \
                 https://example.test/other\n"
        )
        .is_ok(),
        "two different endpoints are two destinations and always were"
    );
}

#[test]
fn rejects_a_delivery_policy_that_delivers_nothing() {
    assert!(
        Config::parse("keys:\n  key1: secret1\nevents:\n  attempts: 0\n").is_err(),
        "a destination configured to be offered an event zero times is a destination that is \
             configured and never written to"
    );
    assert!(Config::parse("keys:\n  key1: secret1\nevents:\n  queue: 0\n").is_err());
    assert!(Config::parse("keys:\n  key1: secret1\nevents:\n  timeout_ms: 0\n").is_err());
    assert!(
        Config::parse(
            "keys:\n  key1: secret1\nevents:\n  backoff_ms: 5000\n  max_backoff_ms: 1000\n"
        )
        .is_err()
    );
    assert!(
        Config::parse("keys:\n  key1: secret1\nevents:\n  nope: 1\n").is_err(),
        "a delivery setting this server does not have must not be read as one it does"
    );
}

#[test]
fn a_broker_can_be_named_with_settings_of_its_own() {
    let config = Config::parse(
        "keys:\n  key1: secret1\nevents:\n  kafka:\n    brokers: kafka1:9093,kafka2:9093\n    \
             topic: mediacore.events\n    properties:\n      security.protocol: ssl\n",
    )
    .unwrap();

    assert!(config.events.has_destinations());
    assert_eq!(config.events.kafka.topic, "mediacore.events");
    assert_eq!(
        config
            .events
            .kafka
            .properties
            .get("security.protocol")
            .map(String::as_str),
        Some("ssl"),
        "a librdkafka setting was dropped, so a broker that needs tls is unreachable and the \
             config that says otherwise parses"
    );

    assert!(
        Config::parse(
            "keys:\n  key1: secret1\nevents:\n  kafka:\n    brokers: kafka1:9093\n    topic: \"\"\n"
        )
        .is_err(),
        "a broker with no topic is a producer with nowhere to produce to"
    );
}

#[test]
fn rejects_config_without_keys() {
    assert!(Config::parse("port: 8080\n").is_err());
}

#[test]
fn rejects_unknown_fields() {
    assert!(Config::parse("keys:\n  key1: secret1\nnope: 1\n").is_err());
}

#[test]
fn the_ice_socket_defaults_to_a_single_udp_port() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

    assert_eq!(config.rtc.udp_port, 7882);
    assert_eq!(config.rtc.bind_address, "0.0.0.0");
    assert!(config.rtc.use_socket_address);
    assert_eq!(config.rtc.candidate_ports(), vec![7882]);
}

#[test]
fn a_port_range_is_used_when_no_single_port_is_set() {
    let config = Config::parse(
            "keys:\n  key1: secret1\nrtc:\n  udp_port: 0\n  port_range_start: 50000\n  port_range_end: 50002\n",
        )
        .unwrap();

    assert_eq!(config.rtc.candidate_ports(), vec![50000, 50001, 50002]);
}

#[test]
fn an_empty_port_range_leaves_the_port_to_the_operating_system() {
    let config = Config::parse(
        "keys:\n  key1: secret1\nrtc:\n  udp_port: 0\n  port_range_start: 0\n  port_range_end: 0\n",
    )
    .unwrap();

    assert_eq!(config.rtc.candidate_ports(), vec![0]);
}

#[test]
fn advertised_addresses_take_the_bound_port_unless_they_carry_one() {
    let config = Config::parse(
            "keys:\n  key1: secret1\nrtc:\n  advertise_addresses:\n    - 203.0.113.7\n    - 203.0.113.8:1234\n",
        )
        .unwrap();

    let advertised = config.rtc.advertise(7882);
    assert_eq!(advertised[0].to_string(), "203.0.113.7:7882");
    assert_eq!(advertised[1].to_string(), "203.0.113.8:1234");
}

#[test]
fn the_ice_disconnected_grace_period_is_configurable() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
    assert_eq!(
        config.rtc.ice_disconnected_grace(),
        Duration::from_secs(DEFAULT_ICE_DISCONNECTED_TIMEOUT_SECONDS.into())
    );

    let config =
        Config::parse("keys:\n  key1: secret1\nrtc:\n  ice_disconnected_timeout: 3\n").unwrap();
    assert_eq!(config.rtc.ice_disconnected_grace(), Duration::from_secs(3));
}

#[test]
fn the_negotiation_timeout_is_configurable() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
    assert_eq!(
        config.rtc.negotiation_grace(),
        Duration::from_secs(DEFAULT_NEGOTIATION_TIMEOUT_SECONDS.into())
    );

    let config = Config::parse("keys:\n  key1: secret1\nrtc:\n  negotiation_timeout: 2\n").unwrap();
    assert_eq!(config.rtc.negotiation_grace(), Duration::from_secs(2));
}

#[test]
fn the_simulcast_timings_are_configurable() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
    assert_eq!(
        config.video.dynacast_pause_delay(),
        Duration::from_millis(DEFAULT_DYNACAST_PAUSE_DELAY_MS.into())
    );
    assert_eq!(
        config.video.layer_upgrade_hold(),
        Duration::from_millis(DEFAULT_LAYER_UPGRADE_HOLD_MS.into())
    );

    let config = Config::parse(
            "keys:\n  key1: secret1\nvideo:\n  dynacast_pause_delay_ms: 250\n  layer_upgrade_hold_ms: 100\n",
        )
        .unwrap();
    assert_eq!(
        config.video.dynacast_pause_delay(),
        Duration::from_millis(250)
    );
    assert_eq!(
        config.video.layer_upgrade_hold(),
        Duration::from_millis(100)
    );
}

/// The url is the switch. There is nothing else to set and nothing that
/// can disagree with it.
#[test]
fn naming_a_database_is_what_chooses_the_database() {
    let files = Config::parse("keys:\n  devkey: devsecret\n").expect("a config with no store");
    assert!(!files.sessions.in_postgres());

    let database = Config::parse(
        "keys:\n  devkey: devsecret\nsessions:\n  postgres:\n    url: \
             postgres://mediacore@db/mediacore\n",
    )
    .expect("a config that names a database");
    assert!(database.sessions.in_postgres());
    assert_eq!(
        database.sessions.postgres.connections, DEFAULT_SESSION_CONNECTIONS,
        "naming a database made a deployment spell out everything else about it as well"
    );
}

/// A lease shorter than three statement timeouts can run out while the node
/// holding it is waiting on the very statement that would have renewed it,
/// and its meetings would then be closed under people who are still
/// talking. Refused at startup, where it is a number somebody chose, rather
/// than at three in the morning, where it is an outage.
#[test]
fn a_lease_that_cannot_outlive_a_statement_is_refused() {
    let refusal = Config::parse(
        "keys:\n  devkey: devsecret\nsessions:\n  postgres:\n    url: \
             postgres://mediacore@db/mediacore\n    timeout_ms: 10000\n    lease_ms: 15000\n",
    )
    .expect_err("a lease shorter than three statement timeouts was accepted");

    assert!(
        refusal.to_string().contains("lease_ms"),
        "the refusal has to name the setting somebody has to change, and it says: {refusal}"
    );
}

#[test]
fn a_directory_is_still_required_when_no_database_is_named() {
    let refusal = Config::parse("keys:\n  devkey: devsecret\nsessions:\n  directory: \"\"\n")
        .expect_err("a server with nowhere at all to put a meeting record started");

    assert!(refusal.to_string().contains("sessions.directory"));
}

#[test]
fn the_shipped_sample_is_a_config_this_server_parses() {
    let config = Config::parse(include_str!("../../../config-sample.yaml"))
        .expect("config-sample.yaml has drifted away from the config this server reads");

    assert_eq!(config.port, 7880);
    assert_eq!(
        config.room.min_humans, 0,
        "the sample must not ship a policy that ends rooms nobody asked it to end"
    );
    assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Suffix);
    assert!(
        !config.sessions.in_postgres(),
        "the sample names a database, so copying it points somebody's meetings at a server \
             they did not choose -- and the single process the sample describes does not need one"
    );
    assert!(
        !config.events.has_destinations(),
        "the sample ships a destination, so copying it starts posting somebody's meetings to \
             an address they did not choose"
    );
    assert!(
        !config.video.bwe.enabled,
        "the sample must not turn bandwidth estimation on before the bench has passed"
    );
    assert_eq!(config.video.bwe.initial_kbps, DEFAULT_BWE_INITIAL_KBPS);
    assert_eq!(
        config.video.bwe.allocation_interval_ms,
        DEFAULT_BWE_ALLOCATION_INTERVAL_MS
    );
}

#[test]
fn bandwidth_estimation_is_off_until_it_is_asked_for() {
    let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

    assert!(!config.video.bwe.enabled);
    assert_eq!(config.video.bwe.initial_kbps, DEFAULT_BWE_INITIAL_KBPS);
    assert_eq!(
        config.video.bwe.min_budget_kbps,
        DEFAULT_BWE_MIN_BUDGET_KBPS
    );
    assert_eq!(
        config.video.bwe.audio_reserve_kbps,
        DEFAULT_BWE_AUDIO_RESERVE_KBPS
    );
    assert!(!config.video.bwe.allow_pause);
    assert_eq!(
        config.video.bwe.allocation_interval(),
        Duration::from_millis(DEFAULT_BWE_ALLOCATION_INTERVAL_MS.into())
    );
    assert_eq!(
        config.video.bwe.recovery_hold(),
        Duration::from_millis(DEFAULT_BWE_RECOVERY_HOLD_MS.into())
    );
    assert_eq!(
        config.video.bwe.recovery_step(),
        Duration::from_millis(DEFAULT_BWE_RECOVERY_STEP_MS.into())
    );
    assert_eq!(
        config.video.bwe.queue_pressure(),
        Duration::from_millis(DEFAULT_BWE_QUEUE_PRESSURE_MS.into())
    );
}

#[test]
fn the_bandwidth_block_is_configurable() {
    let config = Config::parse(
        "keys:\n  key1: secret1\nvideo:\n  bwe:\n    enabled: true\n    initial_kbps: 1200\n    \
             allow_pause: true\n    audio_reserve_kbps: 64\n    allocation_interval_ms: 100\n",
    )
    .unwrap();

    assert!(config.video.bwe.enabled);
    assert_eq!(config.video.bwe.initial_kbps, 1_200);
    assert!(config.video.bwe.allow_pause);
    assert_eq!(config.video.bwe.audio_reserve_kbps, 64);
    assert_eq!(
        config.video.bwe.allocation_interval(),
        Duration::from_millis(100)
    );
    assert_eq!(
        config.video.dynacast_pause_delay(),
        Duration::from_millis(DEFAULT_DYNACAST_PAUSE_DELAY_MS.into()),
        "the rest of the video block kept its defaults"
    );
}

#[test]
fn rejects_a_bandwidth_block_that_cannot_run() {
    assert!(
        Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    allocation_interval_ms: 0\n")
            .is_err(),
        "an allocator that never ticks is a silently dead allocator"
    );
    assert!(
        Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    initial_kbps: 0\n").is_err()
    );
    assert!(
            Config::parse(
                "keys:\n  key1: secret1\nvideo:\n  bwe:\n    initial_kbps: 4000\n    max_desired_kbps: 1000\n"
            )
            .is_err()
        );
    assert!(
        Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    overhead_bytes: 0\n").is_err()
    );
    assert!(Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    nope: 1\n").is_err());
}

#[test]
fn rejects_addresses_that_are_not_addresses() {
    assert!(Config::parse("keys:\n  key1: secret1\nrtc:\n  bind_address: nowhere\n").is_err());
    assert!(
        Config::parse("keys:\n  key1: secret1\nrtc:\n  advertise_addresses:\n    - nowhere\n")
            .is_err()
    );
    assert!(
            Config::parse(
                "keys:\n  key1: secret1\nrtc:\n  udp_port: 0\n  port_range_start: 60000\n  port_range_end: 50000\n"
            )
            .is_err()
        );
}
