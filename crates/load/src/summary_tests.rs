use super::*;

use mediacore_client::{Switch, Timings};

fn millis_each(values: &[u64]) -> Vec<Duration> {
    values.iter().copied().map(Duration::from_millis).collect()
}

fn track(packets: u64, missing: u64, timings: Timings) -> TrackReport {
    TrackReport {
        packets,
        payload_bytes: packets * 80,
        missing,
        timings,
        ..TrackReport::default()
    }
}

fn bot(received: Vec<TrackReport>) -> Snapshot {
    Snapshot {
        identity: "bot-0".to_owned(),
        room: "load-0".to_owned(),
        published: vec![TrackStats {
            packets: 100,
            payload_bytes: 8_000,
            sending: true,
        }],
        received,
        disconnect: None,
    }
}

fn frame(since: Instant, baseline: Counters) -> Frame {
    Frame {
        label: "rung 4".to_owned(),
        bots: 4,
        rooms: 1,
        joined: 4,
        join_failures: 0,
        since,
        arrivals_since: since,
        baseline,
        spend: None,
    }
}

/// A rung whose bots were joined over the `ramp` before its window opened.
fn frame_after_ramp(since: Instant, ramp: Duration) -> Frame {
    Frame {
        arrivals_since: since - ramp,
        ..frame(since, Counters::default())
    }
}

fn switch_at(at: Instant, gap: Option<Duration>) -> Switch {
    Switch {
        at: Some(at),
        gap,
        ..Switch::default()
    }
}

fn limits() -> Limits {
    Limits {
        max_loss: 2.0,
        max_cpu: 80.0,
        max_idle: Duration::from_secs(2),
    }
}

#[test]
fn a_percentile_is_a_value_that_was_really_measured() {
    let mut values = millis_each(&[10, 1, 4, 2, 9, 3, 8, 5, 7, 6]);
    let stat = Stat::of(&mut values).expect("a statistic");

    assert_eq!(stat.count, 10);
    assert_eq!(stat.min, Duration::from_millis(1));
    assert_eq!(stat.p50, Duration::from_millis(5));
    assert_eq!(stat.p95, Duration::from_millis(10));
    assert_eq!(stat.max, Duration::from_millis(10));
}

#[test]
fn one_measurement_is_every_percentile_of_itself() {
    let mut values = millis_each(&[42]);
    let stat = Stat::of(&mut values).expect("a statistic");

    assert_eq!(stat.count, 1);
    assert_eq!(stat.p50, Duration::from_millis(42));
    assert_eq!(stat.p95, Duration::from_millis(42));
}

#[test]
fn nothing_measured_is_no_statistic_rather_than_a_zero() {
    assert_eq!(Stat::of(&mut []), None);
}

#[test]
fn a_rung_counts_what_happened_during_it_and_not_before() {
    let earlier = Counters {
        received_packets: 1_000,
        received_bytes: 80_000,
        missing: 10,
        published_packets: 500,
        ..Counters::default()
    };
    let now = Counters {
        received_tracks: 6,
        received_packets: 3_000,
        received_bytes: 240_000,
        missing: 12,
        published_tracks: 2,
        published_packets: 1_500,
        ..Counters::default()
    };

    let rung = now.since(&earlier);

    assert_eq!(rung.received_packets, 2_000);
    assert_eq!(rung.received_bytes, 160_000);
    assert_eq!(rung.missing, 2);
    assert_eq!(rung.published_packets, 1_000);
    // Not a difference: six tracks are being received now, which is the thing
    // the rung is about.
    assert_eq!(rung.received_tracks, 6);
    assert_eq!(rung.published_tracks, 2);
}

#[test]
fn loss_is_measured_against_what_should_have_arrived() {
    let counters = Counters {
        received_packets: 990,
        missing: 10,
        ..Counters::default()
    };

    assert!((counters.loss_percent() - 1.0).abs() < 0.0001);
    assert_eq!(Counters::default().loss_percent(), 0.0);
}

#[test]
fn a_track_that_was_already_running_is_left_out_of_the_rungs_first_packet() {
    let rung_started = Instant::now();
    let old = Timings {
        first_packet: Some(Duration::from_millis(500)),
        first_arrival: Some(rung_started - Duration::from_secs(30)),
        last_arrival: Some(rung_started + Duration::from_secs(9)),
        span: Some(Duration::from_secs(39)),
        jitter: Some(Duration::from_micros(400)),
    };
    let fresh = Timings {
        first_packet: Some(Duration::from_millis(120)),
        first_arrival: Some(rung_started + Duration::from_millis(120)),
        last_arrival: Some(rung_started + Duration::from_secs(9)),
        span: Some(Duration::from_millis(8_880)),
        jitter: Some(Duration::from_micros(800)),
    };

    let snapshots = vec![bot(vec![track(1_000, 0, old), track(800, 0, fresh)])];
    let now = rung_started + Duration::from_secs(10);
    let sample = Sample::build(frame(rung_started, Counters::default()), &snapshots, now);

    let first_packet = sample.first_packet.expect("a first packet statistic");
    assert_eq!(first_packet.count, 1);
    assert_eq!(first_packet.max, Duration::from_millis(120));

    // Jitter is a moving estimate rather than a total, so both tracks say what
    // they are doing now and both are counted.
    let jitter = sample.jitter.expect("a jitter statistic");
    assert_eq!(jitter.count, 2);

    let idle = sample.idle.expect("an idle statistic");
    assert_eq!(idle.max, Duration::from_secs(1));
    assert_eq!(sample.elapsed, Duration::from_secs(10));
}

#[test]
fn the_gap_across_every_layer_switch_is_gathered() {
    let started = Instant::now();
    let mut report = track(500, 0, Timings::default());
    report.switch_count = 3;
    report.switches = vec![
        switch_at(started, Some(Duration::from_millis(40))),
        switch_at(
            started + Duration::from_millis(500),
            Some(Duration::from_millis(180)),
        ),
        switch_at(started + Duration::from_millis(700), None),
    ];

    let sample = Sample::build(
        frame(started, Counters::default()),
        &[bot(vec![report])],
        started + Duration::from_secs(1),
    );

    let gaps = sample.switch_gap.expect("a switch gap statistic");
    assert_eq!(gaps.count, 2);
    assert_eq!(gaps.max, Duration::from_millis(180));
    assert_eq!(sample.counters.switches, 3);
}

#[test]
fn a_rung_is_not_told_the_gaps_of_the_rungs_below_it() {
    let started = Instant::now();
    let mut report = track(500, 0, Timings::default());
    // A track that has been up since the second rung: the ring still holds the
    // slow switch it made back then, and the rung being reported made one.
    report.switch_count = 2;
    report.switches = vec![
        switch_at(
            started - Duration::from_secs(20),
            Some(Duration::from_secs(1)),
        ),
        switch_at(
            started + Duration::from_millis(300),
            Some(Duration::from_millis(25)),
        ),
    ];

    let sample = Sample::build(
        frame(started, Counters::default()),
        &[bot(vec![report])],
        started + Duration::from_secs(8),
    );

    let gaps = sample.switch_gap.expect("a switch gap statistic");
    assert_eq!(gaps.count, 1);
    assert_eq!(
        gaps.max,
        Duration::from_millis(25),
        "an earlier rung's worst switch was reported as this one's"
    );
}

#[test]
fn a_rungs_first_packets_are_the_ones_its_own_ramp_asked_for() {
    let ramp = Duration::from_secs(2);
    let started = Instant::now();
    // Subscribed during the ramp, which is where a rung's new tracks are always
    // subscribed, and heard from before the rung's window even opened.
    let ramped = Timings {
        first_packet: Some(Duration::from_millis(90)),
        first_arrival: Some(started - ramp + Duration::from_millis(90)),
        last_arrival: Some(started + Duration::from_secs(8)),
        span: Some(Duration::from_secs(10)),
        jitter: None,
    };
    // Running since the rung below: not this rung's to report.
    let older = Timings {
        first_packet: Some(Duration::from_secs(3)),
        first_arrival: Some(started - Duration::from_secs(30)),
        last_arrival: Some(started + Duration::from_secs(8)),
        span: Some(Duration::from_secs(38)),
        jitter: None,
    };

    let sample = Sample::build(
        frame_after_ramp(started, ramp),
        &[bot(vec![track(900, 0, ramped), track(900, 0, older)])],
        started + Duration::from_secs(8),
    );

    let first_packet = sample.first_packet.expect("a first packet statistic");
    assert_eq!(first_packet.count, 1);
    assert_eq!(first_packet.max, Duration::from_millis(90));
}

#[test]
fn a_sample_inside_every_limit_says_nothing_gave() {
    let started = Instant::now();
    let sample = Sample::build(
        frame(started, Counters::default()),
        &[bot(vec![track(
            1_000,
            0,
            Timings {
                last_arrival: Some(started + Duration::from_secs(9)),
                ..Timings::default()
            },
        )])],
        started + Duration::from_secs(9),
    );

    assert_eq!(sample.gave(&limits()), None);
}

#[test]
fn the_bots_own_cpu_is_answered_before_anything_it_would_explain() {
    let started = Instant::now();
    let mut sample = Sample::build(
        frame(started, Counters::default()),
        &[bot(vec![track(900, 100, Timings::default())])],
        started + Duration::from_secs(9),
    );
    sample.join_failures = 3;
    sample.spend = Some(Spend {
        cpu: Duration::from_secs(30),
        wall: Duration::from_secs(10),
        cpu_percent: 96.0,
        rss_bytes: 1 << 30,
    });

    assert_eq!(
        sample.gave(&limits()),
        Some(Gave::BenchCpu { percent: 96.0 }),
        "a pinned generator explains the loss beside it"
    );

    sample.spend = None;
    assert_eq!(sample.gave(&limits()), Some(Gave::Joins { failures: 3 }));

    sample.join_failures = 0;
    assert_eq!(sample.gave(&limits()), Some(Gave::Loss { percent: 10.0 }));
}

#[test]
fn a_track_that_has_gone_quiet_is_a_ceiling_of_its_own() {
    let started = Instant::now();
    let sample = Sample::build(
        frame(started, Counters::default()),
        &[bot(vec![track(
            1_000,
            0,
            Timings {
                last_arrival: Some(started + Duration::from_secs(1)),
                ..Timings::default()
            },
        )])],
        started + Duration::from_secs(9),
    );

    assert_eq!(
        sample.gave(&limits()),
        Some(Gave::Silence {
            tracks: 1,
            worst: Duration::from_secs(8),
        })
    );
}

#[test]
fn a_bot_the_server_threw_out_is_a_ceiling_of_its_own() {
    let started = Instant::now();
    let mut thrown = bot(vec![track(10, 0, Timings::default())]);
    thrown.disconnect = Some("ROOM_CLOSED".to_owned());

    let sample = Sample::build(
        frame(started, Counters::default()),
        &[thrown],
        started + Duration::from_secs(1),
    );

    assert_eq!(sample.disconnected, 1);
    assert_eq!(sample.gave(&limits()), Some(Gave::Disconnected { bots: 1 }));
}

#[test]
fn every_row_has_one_field_per_column_of_the_header() {
    let started = Instant::now();
    let columns = Sample::csv_header().split(',').count();

    let empty = Sample::build(
        frame(started, Counters::default()),
        &[],
        started + Duration::from_secs(1),
    );
    assert_eq!(empty.csv_row().split(',').count(), columns);

    let mut full = Sample::build(
        frame(started, Counters::default()),
        &[bot(vec![track(
            100,
            1,
            Timings {
                first_packet: Some(Duration::from_millis(80)),
                first_arrival: Some(started + Duration::from_millis(80)),
                last_arrival: Some(started + Duration::from_secs(1)),
                span: Some(Duration::from_millis(920)),
                jitter: Some(Duration::from_micros(250)),
            },
        )])],
        started + Duration::from_secs(1),
    );
    full.spend = Some(Spend {
        cpu: Duration::from_secs(1),
        wall: Duration::from_secs(10),
        cpu_percent: 12.5,
        rss_bytes: 1 << 20,
    });

    let row = full.csv_row();
    assert_eq!(row.split(',').count(), columns, "{row}");
    assert!(row.starts_with("rung 4,4,1,4,0,0,"), "{row}");
    assert!(row.ends_with("12.50,1048576"), "{row}");
}

#[test]
fn a_label_carrying_a_comma_cannot_shift_the_row() {
    let started = Instant::now();
    let mut labelled = frame(started, Counters::default());
    labelled.label = "rung 4, second try".to_owned();

    let sample = Sample::build(labelled, &[], started + Duration::from_secs(1));
    assert!(
        sample.csv_row().starts_with("\"rung 4, second try\","),
        "{}",
        sample.csv_row()
    );
}

#[test]
fn the_kinds_of_track_being_received_are_counted_apart() {
    let audio = TrackReport {
        kind: Some(TrackKind::Audio),
        ..TrackReport::default()
    };
    let video = TrackReport {
        kind: Some(TrackKind::Video),
        ..TrackReport::default()
    };

    assert_eq!(
        kinds(&[bot(vec![audio, video.clone(), video])]),
        (1, 2),
        "one audio track and two video ones"
    );
}
