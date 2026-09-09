
use super::*;

#[test]
fn the_first_value_keeps_room_below_it() {
    let mut seq = SeqUnwrapper::new();

    let first = seq.extend(7);

    assert_eq!(first, 7 + 65536);
    assert_eq!(first & 0xffff, 7);
}

#[test]
fn counting_up_stays_contiguous() {
    let mut seq = SeqUnwrapper::new();
    let first = seq.extend(100);

    for step in 1..=10u64 {
        assert_eq!(seq.extend(100 + step), first + step);
    }
}

#[test]
fn a_forward_wrap_carries_into_the_high_bits() {
    let mut seq = SeqUnwrapper::new();
    let last = seq.extend(65534);

    assert_eq!(seq.extend(65535), last + 1);
    assert_eq!(seq.extend(0), last + 2);
    assert_eq!(seq.extend(1), last + 3);
}

#[test]
fn a_backward_wrap_borrows_from_the_high_bits() {
    let mut seq = SeqUnwrapper::new();
    seq.extend(65534);
    seq.extend(65535);
    let zero = seq.extend(0);

    assert_eq!(seq.extend(65535), zero - 1);
    assert_eq!(seq.extend(65533), zero - 3);
}

#[test]
fn a_jump_beyond_half_the_space_reads_as_a_step_back() {
    let mut seq = SeqUnwrapper::new();
    let base = seq.extend(1000);

    assert_eq!(seq.extend(1000 + 32767), base + 32767);
    assert_eq!(seq.extend(1000), base);
}

#[test]
fn timestamps_wrap_at_thirty_two_bits() {
    let mut time = TimeUnwrapper::new();
    let last = time.extend(u32::MAX as u64);

    assert_eq!(time.extend(0), last + 1);
    assert_eq!(time.extend(2999), last + 3000);
}
