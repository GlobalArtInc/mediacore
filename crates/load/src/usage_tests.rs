use super::*;

/// A real line, from a process that had spent a little of both clocks.
const STAT: &str = "1461 (mediacore-load) S 1391 1461 1391 34816 1461 4194304 5312 0 0 0 \
                    137 42 0 0 20 0 9 0 8817320 1284276224 12905 18446744073709551615 \
                    4194304 12457861 140725726083376 0 0 0 0 4096 17987 0 0 0 17 3 0 0 0 0 0";

const STATM: &str = "313544 12905 6392 2018 0 27446 0\n";

#[test]
fn cpu_time_is_the_two_clocks_added_together() {
    let cpu = cpu_time(STAT).expect("a cpu time");
    assert_eq!(cpu, Duration::from_millis(1_790));
}

#[test]
fn a_name_full_of_parentheses_and_spaces_does_not_move_the_fields() {
    let awkward = STAT.replace("(mediacore-load)", "(load (old) 1) S 0 0 0 0)");
    assert_eq!(cpu_time(&awkward), cpu_time(STAT));
}

#[test]
fn a_truncated_stat_line_is_no_reading_rather_than_a_wrong_one() {
    assert_eq!(cpu_time("1461 (mediacore-load) S 1391"), None);
    assert_eq!(cpu_time("nothing like a stat line"), None);
}

#[test]
fn resident_memory_is_the_second_field_in_pages() {
    assert_eq!(resident_bytes(STATM), Some(12_905 * 4_096));
    assert_eq!(resident_bytes(""), None);
}

#[test]
fn the_spend_between_two_readings_is_a_share_of_the_whole_machine() {
    let cores = std::thread::available_parallelism()
        .map(|cores| cores.get() as f64)
        .unwrap_or(1.0);

    let earlier = Usage {
        cpu: Duration::from_secs(1),
        rss_bytes: 1_000,
        at: Instant::now(),
    };
    let later = Usage {
        cpu: Duration::from_secs(3),
        rss_bytes: 2_000,
        at: earlier.at + Duration::from_secs(4),
    };

    let spend = later.since(&earlier);
    assert_eq!(spend.cpu, Duration::from_secs(2));
    assert_eq!(spend.wall, Duration::from_secs(4));
    assert_eq!(spend.rss_bytes, 2_000);

    let expected = 2.0 / 4.0 / cores * 100.0;
    assert!(
        (spend.cpu_percent - expected).abs() < 0.001,
        "the spend was {spend:?}"
    );
}

#[test]
fn two_readings_taken_in_the_same_instant_do_not_divide_by_zero() {
    let usage = Usage {
        cpu: Duration::from_secs(1),
        rss_bytes: 0,
        at: Instant::now(),
    };

    assert_eq!(usage.since(&usage).cpu_percent, 0.0);
}
