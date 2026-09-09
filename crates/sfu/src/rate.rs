use std::time::{Duration, Instant};

use crate::bitrate::Bitrate;

pub const DEFAULT_OVERHEAD_BYTES: u32 = 40;

const MIN_WINDOW: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bucket {
    bytes: u64,
    packets: u64,
}

impl Bucket {
    const EMPTY: Self = Self {
        bytes: 0,
        packets: 0,
    };

    fn add(&mut self, payload_len: usize) {
        self.bytes = self.bytes.saturating_add(payload_len as u64);
        self.packets = self.packets.saturating_add(1);
    }

    fn is_empty(&self) -> bool {
        self.packets == 0
    }

    fn bits(&self, overhead_bytes: u32) -> u64 {
        self.bytes
            .saturating_add(self.packets.saturating_mul(u64::from(overhead_bytes)))
            .saturating_mul(8)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateWindow {
    window: Duration,
    current: Bucket,
    previous: Bucket,
    started: Option<Instant>,
    complete: bool,
}

impl RateWindow {
    pub fn new(window: Duration) -> Self {
        Self {
            window: window.max(MIN_WINDOW),
            current: Bucket::EMPTY,
            previous: Bucket::EMPTY,
            started: None,
            complete: false,
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    pub fn observe(&mut self, payload_len: usize, now: Instant) {
        self.roll(now);
        self.current.add(payload_len);
    }

    pub fn reset(&mut self) {
        self.current = Bucket::EMPTY;
        self.previous = Bucket::EMPTY;
        self.started = None;
        self.complete = false;
    }

    pub fn packets(&self) -> u64 {
        self.previous.packets.saturating_add(self.current.packets)
    }

    pub fn bits_per_second(&self, overhead_bytes: u32, now: Instant) -> Option<Bitrate> {
        let started = self.started?;
        let elapsed = now.saturating_duration_since(started);
        let window = self.window;

        let (previous, current, offset) = if elapsed < window {
            if !self.complete {
                return None;
            }
            (self.previous, self.current, elapsed)
        } else if elapsed < window.saturating_mul(2) {
            (self.current, Bucket::EMPTY, elapsed - window)
        } else {
            return None;
        };

        if previous.is_empty() && current.is_empty() {
            return None;
        }

        let span = window.as_secs_f64();
        let weight = 1.0 - offset.as_secs_f64() / span;
        let bits =
            previous.bits(overhead_bytes) as f64 * weight + current.bits(overhead_bytes) as f64;

        Some(Bitrate::from_bits_per_second(bits / span))
    }

    fn roll(&mut self, now: Instant) {
        let Some(started) = self.started else {
            self.started = Some(now);
            return;
        };

        let elapsed = now.saturating_duration_since(started);
        if elapsed < self.window {
            return;
        }

        let window = self.window.as_nanos();
        let full = elapsed.as_nanos() / window;

        self.previous = if full == 1 {
            self.current
        } else {
            Bucket::EMPTY
        };
        self.current = Bucket::EMPTY;
        self.complete = true;

        let remainder = u64::try_from(elapsed.as_nanos() - full * window).unwrap_or(0);
        self.started = now
            .checked_sub(Duration::from_nanos(remainder))
            .or(Some(now));
    }
}

impl Default for RateWindow {
    fn default() -> Self {
        Self::new(Duration::from_secs(1))
    }
}

#[cfg(test)]
#[path = "rate_tests.rs"]
mod tests;
