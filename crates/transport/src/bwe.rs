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
#[path = "bwe_tests.rs"]
mod tests;
