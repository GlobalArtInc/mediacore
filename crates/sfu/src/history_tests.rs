use super::*;

#[test]
fn a_fresh_sequence_number_is_new_and_then_seen() {
    let mut window = SeqWindow::new();

    assert_eq!(window.observe(10), Slot::New);
    assert_eq!(window.observe(10), Slot::Seen);
    assert_eq!(window.observe(11), Slot::New);
    assert_eq!(window.observe(11), Slot::Seen);
}

#[test]
fn a_late_filler_inside_the_window_is_new_once() {
    let mut window = SeqWindow::new();
    window.observe(100);
    window.observe(104);

    assert_eq!(window.observe(101), Slot::New);
    assert_eq!(window.observe(101), Slot::Seen);
    assert_eq!(window.observe(104), Slot::Seen);
}

#[test]
fn anything_older_than_the_window_is_refused() {
    let mut window = SeqWindow::new();
    window.observe(10_000);

    assert_eq!(window.observe(10_000 - WINDOW), Slot::TooOld);
    assert_eq!(window.observe(10_000 - WINDOW + 1), Slot::New);
}

#[test]
fn a_jump_past_the_window_forgets_everything_behind_it() {
    let mut window = SeqWindow::new();
    window.observe(5);
    window.observe(5 + WINDOW * 2);

    assert_eq!(window.observe(5 + WINDOW * 2 - 1), Slot::New);
}

#[test]
fn a_delta_change_only_applies_from_its_own_position() {
    let mut history = DeltaHistory::anchored(0, -5);
    history.shift(120, -1);

    assert_eq!(history.at(119), Some(-5));
    assert_eq!(history.at(120), Some(-6));
    assert_eq!(history.current(), -6);
    assert_eq!(history.at(0), Some(-5));
}

#[test]
fn nothing_before_the_anchor_can_be_translated() {
    let history = DeltaHistory::anchored(500, 7);

    assert_eq!(history.at(500), Some(7));
    assert_eq!(history.at(501), Some(7));
    assert_eq!(
        history.at(499),
        None,
        "a packet from before the anchor has no place in the outgoing stream"
    );
}

#[test]
fn the_oldest_delta_is_dropped_when_the_history_is_full() {
    let mut history = DeltaHistory::anchored(0, 0);
    for step in 1..=DELTA_ENTRIES as u64 {
        history.shift(step * 10, -1);
    }

    assert_eq!(history.at(0), None);
    assert_eq!(history.current(), -(DELTA_ENTRIES as i64));
}
