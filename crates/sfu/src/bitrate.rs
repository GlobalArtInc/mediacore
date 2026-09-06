#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Bitrate(u64);

impl Bitrate {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(u64::MAX);

    pub const fn bps(bps: u64) -> Self {
        Self(bps)
    }

    pub const fn kbps(kbps: u64) -> Self {
        Self(kbps.saturating_mul(1_000))
    }

    pub const fn mbps(mbps: u64) -> Self {
        Self(mbps.saturating_mul(1_000_000))
    }

    pub const fn as_bps(self) -> u64 {
        self.0
    }

    pub const fn as_kbps(self) -> u64 {
        self.0 / 1_000
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub const fn is_unlimited(self) -> bool {
        self.0 == u64::MAX
    }

    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub fn scaled(self, numerator: u64, denominator: u64) -> Self {
        if denominator == 0 {
            return Self::ZERO;
        }

        let scaled = u128::from(self.0) * u128::from(numerator) / u128::from(denominator);
        Self(u64::try_from(scaled).unwrap_or(u64::MAX))
    }

    pub fn from_bits_per_second(bits: f64) -> Self {
        if !bits.is_finite() || bits <= 0.0 {
            return Self::ZERO;
        }

        Self(bits.round() as u64)
    }
}

impl From<u64> for Bitrate {
    fn from(value: u64) -> Self {
        Self::bps(value)
    }
}

impl From<Bitrate> for str0m::bwe::Bitrate {
    fn from(value: Bitrate) -> Self {
        str0m::bwe::Bitrate::bps(value.as_bps())
    }
}

impl From<str0m::bwe::Bitrate> for Bitrate {
    fn from(value: str0m::bwe::Bitrate) -> Self {
        match value.as_valid() {
            Some(valid) => Self::bps(valid.as_u64()),
            None => Self::ZERO,
        }
    }
}

impl std::iter::Sum for Bitrate {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Self::saturating_add)
    }
}

impl std::fmt::Display for Bitrate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_unlimited() {
            return write!(f, "unlimited");
        }

        if self.0 < 1_000 {
            return write!(f, "{}bps", self.0);
        }

        write!(f, "{}kbps", self.as_kbps())
    }
}

#[cfg(test)]
mod tests {
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
}
