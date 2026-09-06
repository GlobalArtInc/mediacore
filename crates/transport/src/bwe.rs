use std::time::{Duration, Instant};

use str0m::bwe::Bitrate;
use str0m::media::{Mid, Rid};
use str0m::stats::PeerStats;

pub const DEFAULT_INITIAL_BITRATE: Bitrate = Bitrate::kbps(600);
pub const MIN_DESIRED_BITRATE: Bitrate = Bitrate::kbps(300);
pub const MAX_DESIRED_BITRATE: Bitrate = Bitrate::mbps(10);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BweSettings {
    pub initial: Bitrate,
    pub desired: Bitrate,
}

impl Default for BweSettings {
    fn default() -> Self {
        Self::new(DEFAULT_INITIAL_BITRATE)
    }
}

impl BweSettings {
    pub fn new(initial: Bitrate) -> Self {
        Self {
            initial,
            desired: clamp_desired(initial),
        }
    }

    pub fn with_desired(mut self, desired: Bitrate) -> Self {
        self.desired = clamp_desired(desired);
        self
    }
}

pub fn clamp_desired(desired: Bitrate) -> Bitrate {
    if desired < MIN_DESIRED_BITRATE {
        return MIN_DESIRED_BITRATE;
    }

    if desired > MAX_DESIRED_BITRATE {
        return MAX_DESIRED_BITRATE;
    }

    desired
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendQueueEntry {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub bytes: usize,
    pub packets: usize,
    pub oldest_unsent: Option<Instant>,
    pub sampled_at: Option<Instant>,
}

impl SendQueueEntry {
    pub fn empty(mid: Mid, rid: Option<Rid>) -> Self {
        Self {
            mid,
            rid,
            bytes: 0,
            packets: 0,
            oldest_unsent: None,
            sampled_at: None,
        }
    }

    pub fn age(&self, now: Instant) -> Duration {
        self.oldest_unsent
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct PeerReport {
    pub bwe_tx: Option<Bitrate>,
    pub egress_loss: Option<f32>,
    pub ingress_loss: Option<f32>,
    pub rtt: Option<Duration>,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub peer_bytes_tx: u64,
    pub peer_bytes_rx: u64,
    pub queues: Vec<SendQueueEntry>,
    pub at: Instant,
}

impl PeerReport {
    pub fn from_stats(stats: PeerStats, queues: Vec<SendQueueEntry>) -> Self {
        Self {
            bwe_tx: stats.bwe_tx,
            egress_loss: stats.egress_loss_fraction,
            ingress_loss: stats.ingress_loss_fraction,
            rtt: stats.rtt,
            bytes_tx: stats.bytes_tx,
            bytes_rx: stats.bytes_rx,
            peer_bytes_tx: stats.peer_bytes_tx,
            peer_bytes_rx: stats.peer_bytes_rx,
            queues,
            at: stats.timestamp,
        }
    }

    pub fn queue_bytes(&self) -> usize {
        self.queues.iter().map(|queue| queue.bytes).sum()
    }

    pub fn queue_packets(&self) -> usize {
        self.queues.iter().map(|queue| queue.packets).sum()
    }

    pub fn oldest_unsent(&self) -> Option<Instant> {
        self.queues
            .iter()
            .filter_map(|queue| queue.oldest_unsent)
            .min()
    }

    pub fn queue_age(&self, now: Instant) -> Duration {
        self.oldest_unsent()
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or_default()
    }

    pub fn overhead_bytes(&self) -> Option<u64> {
        self.peer_bytes_tx.checked_sub(self.bytes_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mid: &str, bytes: usize, packets: usize, oldest: Option<Instant>) -> SendQueueEntry {
        SendQueueEntry {
            mid: Mid::from(mid),
            rid: None,
            bytes,
            packets,
            oldest_unsent: oldest,
            sampled_at: oldest,
        }
    }

    fn report(queues: Vec<SendQueueEntry>, at: Instant) -> PeerReport {
        PeerReport {
            bwe_tx: None,
            egress_loss: None,
            ingress_loss: None,
            rtt: None,
            bytes_tx: 0,
            bytes_rx: 0,
            peer_bytes_tx: 0,
            peer_bytes_rx: 0,
            queues,
            at,
        }
    }

    #[test]
    fn the_desired_bitrate_is_clamped_into_the_range_str0m_can_probe_for() {
        assert_eq!(clamp_desired(Bitrate::ZERO), MIN_DESIRED_BITRATE);
        assert_eq!(clamp_desired(Bitrate::gbps(1)), MAX_DESIRED_BITRATE);
        assert_eq!(clamp_desired(Bitrate::mbps(2)), Bitrate::mbps(2));

        assert_eq!(
            BweSettings::new(Bitrate::kbps(100)).desired,
            MIN_DESIRED_BITRATE,
            "a desired bitrate of zero makes str0m refuse to ever probe, so it has a floor"
        );
        assert_eq!(
            BweSettings::default().initial,
            DEFAULT_INITIAL_BITRATE,
            "the initial estimate is what the pacer starts at, it cannot be left unset"
        );
    }

    #[test]
    fn the_queue_age_is_the_oldest_packet_still_waiting_anywhere() {
        let now = Instant::now();
        let old = now - Duration::from_millis(300);
        let fresh = now - Duration::from_millis(20);

        let report = report(
            vec![
                entry("video", 4_000, 4, Some(fresh)),
                entry("screen", 12_000, 9, Some(old)),
                entry("audio", 0, 0, None),
            ],
            now,
        );

        assert_eq!(report.queue_bytes(), 16_000);
        assert_eq!(report.queue_packets(), 13);
        assert_eq!(report.oldest_unsent(), Some(old));
        assert_eq!(report.queue_age(now), Duration::from_millis(300));
    }

    #[test]
    fn an_empty_send_queue_has_no_age_at_all() {
        let now = Instant::now();
        let report = report(vec![entry("video", 0, 0, None)], now);

        assert_eq!(report.queue_age(now), Duration::ZERO);
        assert_eq!(report.oldest_unsent(), None);
        assert_eq!(
            SendQueueEntry::empty(Mid::from("video"), None).age(now),
            Duration::ZERO
        );
    }

    #[test]
    fn the_overhead_is_the_gap_between_total_bytes_and_payload_bytes() {
        let now = Instant::now();
        let mut report = report(Vec::new(), now);
        report.peer_bytes_tx = 1_200_000;
        report.bytes_tx = 1_000_000;

        assert_eq!(report.overhead_bytes(), Some(200_000));

        report.bytes_tx = 1_300_000;
        assert_eq!(
            report.overhead_bytes(),
            None,
            "payload bytes above total bytes is nonsense and must not wrap"
        );
    }
}
