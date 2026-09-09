#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unwrapper<const BITS: u32> {
    last: Option<u64>,
}

impl<const BITS: u32> Unwrapper<BITS> {
    const SPAN: u64 = 1u64 << BITS;
    const MASK: u64 = Self::SPAN - 1;
    const HALF: u64 = 1u64 << (BITS - 1);

    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn last(&self) -> Option<u64> {
        self.last
    }

    pub fn extend(&mut self, value: u64) -> u64 {
        let value = value & Self::MASK;

        let extended = match self.last {
            None => value | Self::SPAN,
            Some(last) => {
                let forward = value.wrapping_sub(last & Self::MASK) & Self::MASK;

                if forward < Self::HALF {
                    last.saturating_add(forward)
                } else {
                    last.checked_sub(Self::SPAN - forward).unwrap_or(value)
                }
            }
        };

        self.last = Some(extended);
        extended
    }
}

pub type SeqUnwrapper = Unwrapper<16>;
pub type TimeUnwrapper = Unwrapper<32>;

#[cfg(test)]
#[path = "seq_tests.rs"]
mod tests;
