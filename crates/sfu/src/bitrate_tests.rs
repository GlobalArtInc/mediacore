
use super::*;

#[test]
fn a_bitrate_scales_without_overflowing() {
    assert_eq!(Bitrate::kbps(1_000).scaled(85, 100), Bitrate::kbps(850));
    assert_eq!(Bitrate::MAX.scaled(2, 1), Bitrate::MAX);
    assert_eq!(Bitrate::kbps(500).scaled(1, 0), Bitrate::ZERO);
}

#[test]
fn a_bitrate_saturates_at_both_ends() {
    assert_eq!(
        Bitrate::kbps(1).saturating_sub(Bitrate::kbps(9)),
        Bitrate::ZERO
    );
    assert_eq!(Bitrate::MAX.saturating_add(Bitrate::kbps(1)), Bitrate::MAX);
}

#[test]
fn an_invalid_str0m_bitrate_reads_as_zero() {
    assert_eq!(Bitrate::from(str0m::bwe::Bitrate::INFINITY), Bitrate::ZERO);
    assert_eq!(
        Bitrate::from(str0m::bwe::Bitrate::bps(600_000)),
        Bitrate::kbps(600)
    );
    assert_eq!(
        str0m::bwe::Bitrate::from(Bitrate::kbps(600)).as_u64(),
        600_000
    );
}

#[test]
fn a_bitrate_prints_at_a_useful_scale() {
    assert_eq!(Bitrate::bps(400).to_string(), "400bps");
    assert_eq!(Bitrate::kbps(1_500).to_string(), "1500kbps");
    assert_eq!(Bitrate::MAX.to_string(), "unlimited");
}
