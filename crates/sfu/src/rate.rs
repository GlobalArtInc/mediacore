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
mod tests {
    use super::*;

    const PAYLOAD: usize = 1_200;
    const OVERHEAD: u32 = 40;

    fn feed(window: &mut RateWindow, packets: u32, from: Instant, step: Duration) -> Instant {
        let mut at = from;
        for _ in 0..packets {
            window.observe(PAYLOAD, at);
            at += step;
        }
        at
    }

    #[test]
    fn an_incomplete_window_has_no_price_yet() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));

        assert_eq!(window.bits_per_second(OVERHEAD, now), None);
        window.observe(PAYLOAD, now);

        assert_eq!(
            window.bits_per_second(OVERHEAD, now + Duration::from_millis(500)),
            None,
            "half a window of traffic must not be reported as a full window's rate"
        );
    }

    #[test]
    fn a_steady_stream_is_priced_at_what_it_costs_on_the_wire() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));

        let at = feed(&mut window, 100, now, Duration::from_millis(10));
        let rate = window
            .bits_per_second(OVERHEAD, at)
            .expect("a full window has been measured");

        let expected = (PAYLOAD as u64 + u64::from(OVERHEAD)) * 8 * 100;
        let delta = rate.as_bps().abs_diff(expected);

        assert!(
            delta * 20 < expected,
            "{rate} is not within five percent of {expected}bps"
        );
    }

    #[test]
    fn the_overhead_term_is_what_separates_payload_from_wire() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));
        let at = feed(&mut window, 100, now, Duration::from_millis(10));

        let bare = window.bits_per_second(0, at).expect("measured");
        let wire = window.bits_per_second(OVERHEAD, at).expect("measured");

        assert!(wire > bare);
        let ratio = wire.as_bps() as f64 / bare.as_bps() as f64;
        let expected = (PAYLOAD as f64 + f64::from(OVERHEAD)) / PAYLOAD as f64;

        assert!((ratio - expected).abs() < 0.01, "{ratio} is not {expected}");
    }

    #[test]
    fn a_price_follows_a_real_change_within_two_windows() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));

        let at = feed(&mut window, 100, now, Duration::from_millis(10));
        let before = window.bits_per_second(OVERHEAD, at).expect("measured");

        let at = feed(&mut window, 40, at, Duration::from_millis(50));
        let after = window.bits_per_second(OVERHEAD, at).expect("measured");

        assert!(
            after < before.scaled(1, 2),
            "a stream that halved twice over should not still be priced at {before}, got {after}"
        );
    }

    #[test]
    fn a_burst_inside_a_window_does_not_become_the_price() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));

        let at = feed(&mut window, 100, now, Duration::from_millis(10));
        let steady = window.bits_per_second(OVERHEAD, at).expect("measured");

        let burst_at = feed(&mut window, 30, at, Duration::from_millis(1));
        let instantaneous = Bitrate::bps((PAYLOAD as u64 + u64::from(OVERHEAD)) * 8 * 1_000);
        let during = window
            .bits_per_second(OVERHEAD, burst_at)
            .expect("measured");

        assert!(
            during < instantaneous.scaled(1, 4),
            "the price followed the burst's instantaneous rate of {instantaneous}"
        );

        let at = feed(&mut window, 97, burst_at, Duration::from_millis(10));
        let priced = window.bits_per_second(OVERHEAD, at).expect("measured");

        assert!(
            priced < steady.scaled(3, 2),
            "a keyframe burst moved the price from {steady} to {priced}"
        );
        assert!(
            priced > steady,
            "the burst was real traffic and belongs in the price"
        );
    }

    #[test]
    fn a_window_that_has_gone_quiet_has_no_price_at_all() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));
        let at = feed(&mut window, 100, now, Duration::from_millis(10));

        assert!(window.bits_per_second(OVERHEAD, at).is_some());
        assert_eq!(
            window.bits_per_second(OVERHEAD, at + Duration::from_secs(3)),
            None,
            "a layer that stopped must fall back to a declared price, not to zero"
        );
    }

    #[test]
    fn a_long_silence_rolls_both_buckets_away() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(1));
        feed(&mut window, 100, now, Duration::from_millis(10));

        let at = now + Duration::from_secs(30);
        window.observe(PAYLOAD, at);

        assert_eq!(window.packets(), 1);
        assert_eq!(
            window.bits_per_second(OVERHEAD, at + Duration::from_millis(500)),
            Some(Bitrate::bps((PAYLOAD as u64 + u64::from(OVERHEAD)) * 8)),
            "one packet after a silence is priced as one packet in the window"
        );
    }

    #[test]
    fn a_reset_window_starts_over() {
        let now = Instant::now();
        let mut window = RateWindow::new(Duration::from_secs(4));
        let at = feed(&mut window, 400, now, Duration::from_millis(10));

        assert!(window.bits_per_second(OVERHEAD, at).is_some());
        window.reset();

        assert_eq!(window.bits_per_second(OVERHEAD, at), None);
        assert_eq!(window.packets(), 0);
        assert_eq!(window.window(), Duration::from_secs(4));
    }
}
