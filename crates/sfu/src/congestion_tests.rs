
use super::*;

const SPEND: Bitrate = Bitrate::kbps(1_000);
const SECOND: Duration = Duration::from_secs(1);
const INITIAL: Bitrate = Bitrate::kbps(600);

fn monitor() -> CongestionMonitor {
    CongestionMonitor::default()
}

fn relief(at: Instant) -> Sample {
    Sample {
        at,
        raw: Some(Bitrate::kbps(2_000)),
        loss: Some(0.0),
        rtt: Some(Duration::from_millis(40)),
        sent: Bitrate::kbps(900),
        packets: 1_000,
        queue_age: Duration::ZERO,
        queue_bytes: 0,
    }
}

fn queued(at: Instant) -> Sample {
    Sample {
        queue_age: Duration::from_millis(200),
        ..relief(at)
    }
}

fn blind(at: Instant, queue_age: Duration) -> Sample {
    Sample {
        raw: None,
        loss: None,
        queue_age,
        ..relief(at)
    }
}

fn feed(
    monitor: &mut CongestionMonitor,
    sample: fn(Instant) -> Sample,
    count: u32,
    from: Instant,
) -> Instant {
    let mut at = from;

    for _ in 0..count {
        at += SECOND;
        monitor.on_sample(sample(at), SPEND);
    }

    at
}

#[test]
fn the_first_budget_is_the_configured_initial_bitrate() {
    let monitor = monitor();

    assert_eq!(monitor.config().initial, INITIAL);
    assert_eq!(
        monitor.budget(),
        INITIAL,
        "a subscriber that has not been measured yet is handed the configured start, not everything"
    );
}

#[test]
fn a_first_estimate_above_the_initial_budget_raises_it_at_once() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(2_400), now);

    let verdict = monitor.on_sample(relief(now), SPEND);

    assert_eq!(verdict.budget, Bitrate::kbps(2_400));
    assert!(verdict.reallocate);
}

#[test]
fn a_first_estimate_below_the_initial_budget_waits_until_we_spend_that_much() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(300), now);

    let idle = monitor.on_sample(relief(now), Bitrate::kbps(100));

    assert_eq!(
        idle.budget, INITIAL,
        "an estimate below the start is not a reduction while we spend less than it"
    );

    let busy = monitor.on_sample(relief(now + SECOND), SPEND);

    assert_eq!(
        busy.budget,
        Bitrate::kbps(300),
        "once we are spending real bits, the estimate that disagrees with the start wins"
    );
}

#[test]
fn one_pressure_sample_moves_clear_to_hold() {
    let now = Instant::now();
    let mut monitor = monitor();

    assert_eq!(
        monitor.on_sample(relief(now), SPEND).state,
        CongestionState::Clear
    );

    let verdict = monitor.on_sample(queued(now + SECOND), SPEND);

    assert_eq!(verdict.state, CongestionState::Hold);
    assert!(verdict.changed);
    assert!(
        !verdict.reallocate,
        "hold exists to stop re-allocation, not to start one"
    );
}

#[test]
fn two_more_move_hold_to_congested() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let at = feed(&mut monitor, queued, 2, now);
    assert_eq!(monitor.state(), CongestionState::Hold);

    let verdict = monitor.on_sample(queued(at + SECOND), SPEND);

    assert_eq!(verdict.state, CongestionState::Congested);
    assert!(verdict.changed);
}

#[test]
fn three_relief_samples_return_hold_to_clear() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);
    let at = feed(&mut monitor, queued, 1, now);
    assert_eq!(monitor.state(), CongestionState::Hold);

    let at = feed(&mut monitor, relief, 2, at);
    assert_eq!(monitor.state(), CongestionState::Hold);

    let verdict = monitor.on_sample(relief(at + SECOND), SPEND);

    assert_eq!(verdict.state, CongestionState::Clear);
    assert!(verdict.changed);
    assert!(verdict.reallocate, "leaving congestion lifts every cap");
    assert_eq!(
        monitor.committed(),
        Bitrate::kbps(2_000),
        "a clear link's budget is the estimate, not an absence of one"
    );
}

#[test]
fn congested_never_passes_through_hold_on_the_way_out() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);
    let mut at = feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.state(), CongestionState::Congested);

    let mut seen = Vec::new();
    for _ in 0..5 {
        at += SECOND;
        seen.push(monitor.on_sample(relief(at), SPEND).state);
    }

    assert_eq!(
        seen,
        vec![
            CongestionState::Congested,
            CongestionState::Congested,
            CongestionState::Congested,
            CongestionState::Congested,
            CongestionState::Clear,
        ],
        "there is no congested to hold path, and the claim stands for five seconds"
    );
}

#[test]
fn a_congestion_claim_nothing_renews_lapses_even_with_no_relief() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);
    let mut at = feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.state(), CongestionState::Congested);

    let mut seen = Vec::new();
    for _ in 0..5 {
        at += SECOND;
        seen.push(
            monitor
                .on_sample(blind(at, Duration::from_millis(60)), SPEND)
                .state,
        );
    }

    assert_eq!(
        *seen.last().expect("five samples were fed"),
        CongestionState::Clear,
        "a queue that is neither deep nor drained must not pin the machine in congested for ever"
    );
    assert_eq!(
        monitor.signals().combined(),
        Signal::Indeterminate,
        "nothing in that run was relief"
    );
}

#[test]
fn a_single_late_report_does_not_flap_the_machine() {
    let now = Instant::now();
    let mut monitor = monitor();
    let mut at = feed(&mut monitor, relief, 5, now);

    for _ in 0..6 {
        at += SECOND;
        monitor.on_sample(queued(at), SPEND);
        assert_eq!(monitor.state(), CongestionState::Hold);

        at = feed(&mut monitor, relief, 3, at);
        assert_eq!(
            monitor.state(),
            CongestionState::Clear,
            "one late report followed by calm must not reach congested"
        );
    }
}

#[test]
fn a_budget_above_ninety_five_percent_of_current_spend_is_not_committed() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(1_200), now);
    monitor.on_sample(relief(now), SPEND);
    assert_eq!(monitor.committed(), Bitrate::kbps(1_200));

    let brimming = |at: Instant| Sample {
        sent: Bitrate::kbps(1_120),
        ..queued(at)
    };
    feed(&mut monitor, brimming, 3, now);

    assert_eq!(monitor.state(), CongestionState::Congested);
    assert_eq!(
        monitor.committed(),
        Bitrate::kbps(1_200),
        "a cut to 952 kbps against a spend of 1000 kbps is not a reduction and must be declined"
    );
}

#[test]
fn a_budget_that_is_a_real_reduction_is_committed() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = now;
    let mut verdict = None;
    for _ in 0..3 {
        at += SECOND;
        verdict = Some(monitor.on_sample(queued(at), SPEND));
    }
    let verdict = verdict.expect("three samples were fed");

    assert_eq!(verdict.state, CongestionState::Congested);
    assert_eq!(
        verdict.budget,
        Bitrate::kbps(765),
        "the cut is sized from what is actually leaving"
    );
    assert!(verdict.reallocate);
}

#[test]
fn a_committed_budget_never_falls_below_the_floor() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let trickle = |at: Instant| Sample {
        sent: Bitrate::kbps(100),
        ..queued(at)
    };
    feed(&mut monitor, trickle, 3, now);

    assert_eq!(monitor.committed(), Bitrate::kbps(150));
    assert_eq!(monitor.committed(), monitor.config().min_budget);
}

#[test]
fn a_cut_with_nothing_leaving_the_socket_is_sized_from_the_estimate_alone() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);
    assert_eq!(monitor.committed(), Bitrate::kbps(2_000));

    let stalled = |at: Instant| Sample {
        raw: Some(Bitrate::kbps(300)),
        sent: Bitrate::ZERO,
        ..queued(at)
    };
    feed(&mut monitor, stalled, 3, now);

    assert_eq!(monitor.state(), CongestionState::Congested);
    assert_eq!(
        monitor.committed(),
        Bitrate::kbps(300),
        "with nothing measurably leaving, the estimate is the only size the cut can have"
    );
}

#[test]
fn a_cut_with_no_estimate_is_sized_from_what_left_the_socket() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);
    assert_eq!(monitor.committed(), Bitrate::kbps(2_000));

    let unmeasured = |at: Instant| Sample {
        raw: None,
        loss: None,
        sent: Bitrate::kbps(800),
        ..queued(at)
    };
    feed(&mut monitor, unmeasured, 3, now);

    assert_eq!(monitor.state(), CongestionState::Congested);
    assert_eq!(
        monitor.committed(),
        Bitrate::kbps(680),
        "with no estimate at all the cut is a fraction of the measured egress"
    );
}

#[test]
fn loss_on_a_sparse_window_is_not_congestion() {
    let now = Instant::now();
    let mut monitor = monitor();
    let sparse = |at: Instant| Sample {
        loss: Some(0.5),
        packets: 10,
        ..relief(at)
    };

    let mut at = now;
    for _ in 0..8 {
        at += SECOND;
        monitor.on_sample(sparse(at), SPEND);
    }

    assert_eq!(
        monitor.signals().loss,
        Signal::Relief,
        "half of ten packets lost on a quiet link does not even count as loss"
    );
    assert_eq!(monitor.state(), CongestionState::Clear);
}

#[test]
fn weighted_loss_is_rate_weighted_not_a_bare_percentage() {
    let now = Instant::now();
    let mut monitor = monitor();
    let at = |step: u32| now + SECOND * step;

    let sample = |step: u32, loss: f32, packets: u64| Sample {
        loss: Some(loss),
        packets,
        ..relief(at(step))
    };

    monitor.on_sample(sample(0, 0.0, 1_000), SPEND);

    monitor.on_sample(sample(1, 0.2, 5_000), SPEND);
    assert_eq!(monitor.signals().loss, Signal::Pressure);

    monitor.on_sample(sample(2, 0.1, 1_000), SPEND);
    assert_eq!(
        monitor.signals().loss,
        Signal::Pressure,
        "0.30 is over the pressure threshold"
    );

    monitor.on_sample(sample(3, 0.08, 1_000), SPEND);
    assert_eq!(
        monitor.signals().loss,
        Signal::Indeterminate,
        "0.24 sits between the relief and pressure thresholds"
    );

    monitor.on_sample(sample(4, 0.05, 1_000), SPEND);
    assert_eq!(
        monitor.signals().loss,
        Signal::Indeterminate,
        "0.15 is above the relief threshold and is not yet recovery"
    );

    monitor.on_sample(sample(5, 0.02, 1_000), SPEND);
    assert_eq!(
        monitor.signals().loss,
        Signal::Relief,
        "0.06 is below the relief threshold"
    );
}

#[test]
fn loss_at_a_real_packet_rate_is_still_not_congestion_without_a_backlog() {
    let now = Instant::now();
    let mut monitor = monitor();
    let wifi = |at: Instant| Sample {
        loss: Some(0.2),
        packets: 5_000,
        ..relief(at)
    };

    let mut at = now;
    for _ in 0..12 {
        at += SECOND;
        monitor.on_sample(wifi(at), SPEND);
    }

    assert_eq!(monitor.signals().loss, Signal::Pressure);
    assert_eq!(
        monitor.signals().corroborated_loss(),
        Signal::Indeterminate,
        "loss with nothing queued behind it is not a delay signal"
    );
    assert_eq!(
        monitor.signals().combined(),
        Signal::Indeterminate,
        "loss is not allowed to originate a verdict on its own, in either direction"
    );
    assert_eq!(
        monitor.state(),
        CongestionState::Clear,
        "a lossy but uncongested link must not be pinned to congested"
    );
}

#[test]
fn heavy_loss_keeps_a_drained_queue_from_reading_as_recovery() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = now + SECOND;
    monitor.on_sample(queued(at), SPEND);
    assert_eq!(monitor.state(), CongestionState::Hold);

    let lossy = |at: Instant| Sample {
        loss: Some(0.2),
        packets: 5_000,
        ..relief(at)
    };

    for _ in 0..4 {
        at += SECOND;
        let verdict = monitor.on_sample(lossy(at), SPEND);

        assert_eq!(verdict.signals.combined(), Signal::Indeterminate);
        assert_eq!(
            verdict.state,
            CongestionState::Hold,
            "a fifth of the packets dropped is not the evidence that a link recovered, however empty the pacer is"
        );
    }

    at += SECOND;
    assert_eq!(
        monitor.on_sample(lossy(at), SPEND).state,
        CongestionState::Clear,
        "a hold nothing escalated still lapses on its own"
    );
}

#[test]
fn a_collapsed_estimate_keeps_a_drained_queue_from_reading_as_recovery() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = now + SECOND;
    monitor.on_sample(queued(at), SPEND);
    assert_eq!(monitor.state(), CongestionState::Hold);

    let collapsed = |at: Instant| Sample {
        raw: Some(Bitrate::kbps(500)),
        ..relief(at)
    };

    for _ in 0..4 {
        at += SECOND;
        let verdict = monitor.on_sample(collapsed(at), SPEND);

        assert_eq!(verdict.signals.estimate, Signal::Pressure);
        assert_eq!(
            verdict.signals.corroborated_estimate(),
            Signal::Indeterminate
        );
        assert_eq!(
            verdict.state,
            CongestionState::Hold,
            "an estimate at half of what we offer is not the evidence that a link recovered"
        );
    }

    at += SECOND;
    assert_eq!(
        monitor.on_sample(collapsed(at), SPEND).state,
        CongestionState::Clear
    );
}

#[test]
fn a_queue_short_of_the_pressure_threshold_corroborates_nothing() {
    let now = Instant::now();
    let mut monitor = monitor();
    let stirring = |at: Instant| Sample {
        loss: Some(0.2),
        packets: 5_000,
        queue_age: Duration::from_millis(100),
        ..relief(at)
    };

    let mut at = now;
    for _ in 0..6 {
        at += SECOND;
        let verdict = monitor.on_sample(stirring(at), SPEND);

        assert_eq!(verdict.signals.queue, Signal::Indeterminate);
        assert_eq!(
            verdict.signals.corroborated_loss(),
            Signal::Indeterminate,
            "what corroborates loss is a queue past the pressure threshold, not merely a queue that is not empty"
        );
        assert_eq!(verdict.state, CongestionState::Clear);
    }
}

#[test]
fn loss_beside_a_backlog_is_read_as_the_corroboration_it_is() {
    let now = Instant::now();
    let mut monitor = monitor();
    let both = |at: Instant| Sample {
        loss: Some(0.2),
        packets: 5_000,
        ..queued(at)
    };

    monitor.on_sample(both(now), SPEND);
    monitor.on_sample(both(now + SECOND), SPEND);

    assert_eq!(monitor.signals().corroborated_loss(), Signal::Pressure);
    assert_eq!(monitor.state(), CongestionState::Hold);
}

#[test]
fn a_queue_that_ages_past_the_threshold_is_congestion_without_any_estimate() {
    let now = Instant::now();
    let mut monitor = monitor();
    let mut at = now;

    for _ in 0..3 {
        at += SECOND;
        monitor.on_sample(blind(at, Duration::from_millis(200)), SPEND);
    }

    assert_eq!(monitor.state(), CongestionState::Congested);
    assert_eq!(monitor.signals().estimate, Signal::Indeterminate);
    assert_eq!(monitor.signals().queue, Signal::Pressure);
}

#[test]
fn the_queue_thresholds_are_where_they_are_written() {
    let now = Instant::now();
    let mut monitor = monitor();
    let at = |millis: u64| now + Duration::from_millis(millis);

    let read = |monitor: &mut CongestionMonitor, at: Instant, age: u64| {
        monitor.on_sample(blind(at, Duration::from_millis(age)), SPEND);
        monitor.signals().queue
    };

    assert_eq!(read(&mut monitor, at(1_000), 29), Signal::Relief);
    assert_eq!(read(&mut monitor, at(2_000), 30), Signal::Indeterminate);
    assert_eq!(read(&mut monitor, at(3_000), 150), Signal::Indeterminate);
    assert_eq!(read(&mut monitor, at(4_000), 151), Signal::Pressure);
}

#[test]
fn a_shallow_queue_alone_never_moves_the_machine() {
    let now = Instant::now();
    let mut monitor = monitor();
    let mut at = now;

    for _ in 0..10 {
        at += SECOND;
        monitor.on_sample(blind(at, Duration::from_millis(60)), SPEND);
    }

    assert_eq!(monitor.state(), CongestionState::Clear);
    assert_eq!(monitor.signals().queue, Signal::Indeterminate);
}

#[test]
fn a_cut_that_did_not_help_is_cut_again_after_the_recut_delay() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.committed(), Bitrate::kbps(765));

    let still_bad = |at: Instant| Sample {
        sent: Bitrate::kbps(580),
        ..queued(at)
    };
    let spend = Bitrate::kbps(620);

    at += SECOND;
    let verdict = monitor.on_sample(still_bad(at), spend);
    assert!(
        !verdict.reallocate,
        "a second of pressure is not yet a failed cut"
    );

    at += SECOND;
    let verdict = monitor.on_sample(still_bad(at), spend);

    assert!(
        verdict.reallocate,
        "the cut did not help and was not repeated"
    );
    assert_eq!(verdict.budget, Bitrate::kbps(493));
}

#[test]
fn a_recut_is_measured_from_the_budget_when_more_than_that_is_still_leaving() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let overspending = |at: Instant| Sample {
        sent: Bitrate::kbps(800),
        ..queued(at)
    };

    let mut at = feed(&mut monitor, overspending, 3, now);
    assert_eq!(monitor.committed(), Bitrate::kbps(680));

    at += SECOND;
    monitor.on_sample(overspending(at), SPEND);

    at += SECOND;
    let verdict = monitor.on_sample(overspending(at), SPEND);

    assert!(verdict.reallocate);
    assert_eq!(
        verdict.budget,
        Bitrate::kbps(578),
        "the allocator has not caught up with the last cut, so the next one is measured from the budget in force rather than from the overspend it was meant to end"
    );
}

#[test]
fn a_cut_whose_symptom_is_loss_rather_than_a_queue_is_still_cut_again() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.state(), CongestionState::Congested);
    assert_eq!(monitor.committed(), Bitrate::kbps(765));

    let dropping = |at: Instant| Sample {
        loss: Some(0.2),
        packets: 5_000,
        sent: Bitrate::kbps(580),
        queue_age: Duration::from_millis(5),
        ..relief(at)
    };
    let spend = Bitrate::kbps(620);

    at += SECOND;
    assert!(
        !monitor.on_sample(dropping(at), spend).reallocate,
        "a second of anything is still inside the recut delay"
    );

    at += SECOND;
    let verdict = monitor.on_sample(dropping(at), spend);

    assert_eq!(verdict.signals.queue, Signal::Relief);
    assert_eq!(verdict.state, CongestionState::Congested);
    assert!(
        verdict.reallocate,
        "the bottleneck is dropping what the pacer hands it instead of queueing it, and the cut we made did not stop that"
    );
    assert_eq!(verdict.budget, Bitrate::kbps(493));
}

#[test]
fn an_estimate_that_fell_because_we_cut_is_not_evidence_that_the_cut_failed() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.committed(), Bitrate::kbps(765));

    let after = |at: Instant| Sample {
        raw: Some(Bitrate::kbps(400)),
        sent: Bitrate::kbps(700),
        queue_age: Duration::from_millis(5),
        ..relief(at)
    };
    let spend = Bitrate::kbps(750);

    for _ in 0..4 {
        at += SECOND;
        let verdict = monitor.on_sample(after(at), spend);

        assert_eq!(verdict.signals.estimate, Signal::Pressure);
        assert!(
            !verdict.reallocate,
            "the estimate is bounded by what we chose to send, so it cannot be the evidence that sending less did not work"
        );
        assert_eq!(verdict.budget, Bitrate::kbps(765));
    }
}

#[test]
fn a_recut_waits_five_round_trips_on_a_slow_link() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = feed(&mut monitor, queued, 3, now);
    let slow = |at: Instant| Sample {
        sent: Bitrate::kbps(580),
        rtt: Some(Duration::from_millis(1_000)),
        ..queued(at)
    };

    for _ in 0..4 {
        at += SECOND;
        assert!(
            !monitor.on_sample(slow(at), Bitrate::kbps(620)).reallocate,
            "four seconds is inside five round trips of a second"
        );
    }

    at += SECOND;
    assert!(monitor.on_sample(slow(at), Bitrate::kbps(620)).reallocate);
}

#[test]
fn a_cut_that_worked_is_not_the_evidence_for_the_next_one() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.state(), CongestionState::Congested);
    assert_eq!(monitor.committed(), Bitrate::kbps(765));

    let after = |at: Instant| Sample {
        raw: Some(Bitrate::kbps(700)),
        sent: Bitrate::kbps(700),
        queue_age: Duration::from_millis(5),
        ..relief(at)
    };
    let spend = Bitrate::kbps(750);

    for _ in 0..4 {
        at += SECOND;
        let verdict = monitor.on_sample(after(at), spend);

        assert!(
            !verdict.reallocate,
            "the estimate fell because we sent less, and that is not a failed cut"
        );
        assert_eq!(
            verdict.budget,
            Bitrate::kbps(765),
            "a cut that landed must not size the next one"
        );
    }

    at += SECOND;
    assert_eq!(
        monitor.on_sample(after(at), spend).state,
        CongestionState::Clear
    );
}

#[test]
fn the_estimate_is_read_against_what_we_offer_not_against_its_own_past() {
    let now = Instant::now();
    let mut monitor = monitor();
    let mut at = now;

    let loaded = |at: Instant, raw: u64| Sample {
        raw: Some(Bitrate::kbps(raw)),
        ..relief(at)
    };

    at += SECOND;
    monitor.on_sample(loaded(at, 3_000), Bitrate::kbps(3_000));
    assert_eq!(monitor.signals().estimate, Signal::Relief);

    at += SECOND;
    monitor.on_sample(loaded(at, 700), Bitrate::kbps(700));

    assert_eq!(
        monitor.signals().estimate,
        Signal::Relief,
        "a quarter of the earlier estimate is still relief when we offer a quarter as much"
    );
    assert_eq!(monitor.state(), CongestionState::Clear);
}

#[test]
fn the_offer_thresholds_are_where_they_are_written() {
    let now = Instant::now();
    let mut monitor = monitor();

    let read = |monitor: &mut CongestionMonitor, step: u32, raw: u64| {
        let sample = Sample {
            raw: Some(Bitrate::kbps(raw)),
            ..relief(now + SECOND * step)
        };
        monitor.on_sample(sample, SPEND);
        monitor.signals().estimate
    };

    assert_eq!(read(&mut monitor, 1, 849), Signal::Pressure);
    assert_eq!(read(&mut monitor, 2, 850), Signal::Indeterminate);
    assert_eq!(read(&mut monitor, 3, 949), Signal::Indeterminate);
    assert_eq!(read(&mut monitor, 4, 950), Signal::Relief);
}

#[test]
fn a_falling_estimate_lowers_the_budget_without_ever_claiming_congestion() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(3_000), now);
    monitor.on_sample(relief(now), Bitrate::kbps(3_000));
    assert_eq!(monitor.committed(), Bitrate::kbps(3_000));

    monitor.on_estimate(Bitrate::kbps(666), now + SECOND);
    let collapsed = |at: Instant| Sample {
        raw: Some(Bitrate::kbps(666)),
        loss: Some(0.2),
        packets: 5_000,
        sent: Bitrate::kbps(650),
        ..relief(at)
    };

    let mut at = now;
    let mut states = Vec::new();
    for _ in 0..10 {
        at += SECOND;
        states.push(monitor.on_sample(collapsed(at), Bitrate::kbps(3_000)).state);
    }

    assert!(
        states.iter().copied().all(CongestionState::is_clear),
        "a link whose estimate collapsed with nothing queued behind it is not congested"
    );
    assert_eq!(
        monitor.committed(),
        Bitrate::kbps(666),
        "the budget follows the estimate down on a clear link, which is what stops a queue forming"
    );
    assert_eq!(monitor.signals().estimate, Signal::Pressure);
}

#[test]
fn a_hold_that_will_not_resolve_itself_lapses_to_clear() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = now;
    let mut states = Vec::new();
    for step in 0..6 {
        at += SECOND;
        let sample = if step % 2 == 0 {
            queued(at)
        } else {
            relief(at)
        };
        states.push(monitor.on_sample(sample, SPEND).state);
    }

    assert!(
        !states.contains(&CongestionState::Congested),
        "two pressure samples that are not consecutive are not the two the front door asks for"
    );
    assert_eq!(
        monitor.state(),
        CongestionState::Clear,
        "a hold nothing escalated within five seconds lapses"
    );
}

#[test]
fn a_hold_nothing_confirmed_returns_to_clear_on_its_own() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);

    let mut at = now + SECOND;
    monitor.on_sample(queued(at), SPEND);
    assert_eq!(monitor.state(), CongestionState::Hold);

    for _ in 0..6 {
        at += SECOND;
        monitor.on_sample(blind(at, Duration::from_millis(60)), SPEND);
    }

    assert_eq!(
        monitor.state(),
        CongestionState::Clear,
        "a hold nothing confirmed is not congestion"
    );
}

#[test]
fn a_clear_link_commits_the_estimate_as_a_budget() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(400), now);

    let verdict = monitor.on_sample(relief(now), SPEND);

    assert_eq!(verdict.state, CongestionState::Clear);
    assert_eq!(
        verdict.budget,
        Bitrate::kbps(400),
        "a clear link still has a budget, and it is the estimate"
    );
    assert!(verdict.reallocate);
}

#[test]
fn a_quiet_link_never_lowers_a_committed_budget() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(3_000), now);
    monitor.on_sample(relief(now), SPEND);
    assert_eq!(monitor.committed(), Bitrate::kbps(3_000));

    monitor.on_estimate(Bitrate::kbps(120), now + SECOND);
    let quiet = |at: Instant| Sample {
        sent: Bitrate::kbps(80),
        ..relief(at)
    };

    let mut at = now;
    for _ in 0..5 {
        at += SECOND;
        monitor.on_sample(quiet(at), Bitrate::kbps(100));
    }

    assert_eq!(monitor.state(), CongestionState::Clear);
    assert_eq!(
        monitor.committed(),
        Bitrate::kbps(3_000),
        "a quiet link reads as a small estimate and must not cost the room a step"
    );
}

#[test]
fn hold_never_asks_for_a_reallocation() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_estimate(Bitrate::kbps(200), now);
    monitor.on_sample(relief(now), SPEND);

    let verdict = monitor.on_sample(queued(now + SECOND), SPEND);

    assert_eq!(verdict.state, CongestionState::Hold);
    assert!(!verdict.reallocate);
    assert_eq!(
        verdict.budget,
        Bitrate::kbps(200),
        "hold commits no new budget, it freezes the one already in force"
    );
}

#[test]
fn a_reset_subscriber_starts_from_the_configured_initial_bitrate_again() {
    let now = Instant::now();
    let mut monitor = monitor();
    monitor.on_sample(relief(now), SPEND);
    feed(&mut monitor, queued, 3, now);
    assert_eq!(monitor.state(), CongestionState::Congested);

    monitor.reset();

    assert_eq!(monitor.state(), CongestionState::Clear);
    assert_eq!(monitor.committed(), INITIAL);
}
