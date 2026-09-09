use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mediacore_config::DataConfig;
use mediacore_proto::mediacore::{self, data_packet};
use mediacore_room::Room;
use mediacore_transport::{DataChannel, OutgoingData, PeerHandle};
use prost::Message as _;

#[derive(Debug, Default)]
pub struct DataRouter {
    peers: Mutex<HashMap<String, DataPeer>>,
    packets: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Debug)]
struct DataPeer {
    peer: PeerHandle,
    reliable: bool,
    lossy: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataTotals {
    pub peers: usize,
    pub packets: u64,
    pub bytes: u64,
    pub dropped: u64,
}

impl DataRouter {
    pub fn opened(&self, participant_sid: &str, peer: &PeerHandle, channel: DataChannel) {
        let mut peers = self.lock();
        let entry = peers
            .entry(participant_sid.to_owned())
            .or_insert_with(|| DataPeer {
                peer: peer.clone(),
                reliable: false,
                lossy: false,
            });

        entry.peer = peer.clone();

        match channel {
            DataChannel::Reliable => entry.reliable = true,
            DataChannel::Lossy => entry.lossy = true,
        }
    }

    pub fn closed(&self, participant_sid: &str, channel: DataChannel) {
        let mut peers = self.lock();
        let Some(entry) = peers.get_mut(participant_sid) else {
            return;
        };

        match channel {
            DataChannel::Reliable => entry.reliable = false,
            DataChannel::Lossy => entry.lossy = false,
        }

        if !entry.reliable && !entry.lossy {
            peers.remove(participant_sid);
        }
    }

    pub fn remove(&self, participant_sid: &str) {
        self.lock().remove(participant_sid);
    }

    pub fn send(&self, participant_sid: &str, channel: DataChannel, payload: &[u8]) -> bool {
        let outgoing = {
            let peers = self.lock();
            let Some(entry) = peers.get(participant_sid) else {
                return false;
            };

            let channel = match channel {
                DataChannel::Lossy if !entry.lossy => DataChannel::Reliable,
                DataChannel::Reliable if !entry.reliable => DataChannel::Lossy,
                channel => channel,
            };

            let open = match channel {
                DataChannel::Reliable => entry.reliable,
                DataChannel::Lossy => entry.lossy,
            };

            if !open {
                return false;
            }

            (
                entry.peer.clone(),
                OutgoingData {
                    channel,
                    binary: true,
                    payload: payload.into(),
                },
            )
        };

        let (peer, packet) = outgoing;
        let bytes = packet.payload.len() as u64;

        match peer.write_data(packet) {
            Ok(()) => {
                self.packets.fetch_add(1, Ordering::Relaxed);
                self.bytes.fetch_add(bytes, Ordering::Relaxed);
                true
            }
            Err(error) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(participant = %participant_sid, %error, "a data packet was not written");
                false
            }
        }
    }

    pub fn totals(&self) -> DataTotals {
        DataTotals {
            peers: self.lock().len(),
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, DataPeer>> {
        self.peers.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[allow(deprecated)]
pub fn distribute(
    router: &DataRouter,
    room: &Room,
    packet: &mediacore::DataPacket,
    sender_sid: Option<&str>,
) -> usize {
    let channel = match packet.kind() {
        data_packet::Kind::Lossy => DataChannel::Lossy,
        data_packet::Kind::Reliable => DataChannel::Reliable,
    };

    let payload = packet.encode_to_vec();

    room.data_recipients(sender_sid, &packet.destination_identities)
        .into_iter()
        .filter(|participant| router.send(&participant.sid, channel, &payload))
        .count()
}

#[derive(Debug)]
pub struct DataLimiter {
    tokens: f32,
    rate: f32,
    burst: f32,
    last: Option<Instant>,
}

impl DataLimiter {
    pub fn new(config: &DataConfig) -> Self {
        let burst = config.burst.max(config.rate) as f32;

        Self {
            tokens: burst,
            rate: config.rate as f32,
            burst,
            last: None,
        }
    }

    pub fn admit(&mut self, now: Instant) -> bool {
        let elapsed = match self.last {
            Some(last) => now.saturating_duration_since(last),
            None => Duration::ZERO,
        };

        self.last = Some(now);
        self.tokens = (self.tokens + elapsed.as_secs_f32() * self.rate).min(self.burst);

        if self.tokens < 1.0 {
            return false;
        }

        self.tokens -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(rate: u32, burst: u32) -> DataLimiter {
        DataLimiter::new(&DataConfig {
            enabled: true,
            max_packet_bytes: 64_000,
            rate,
            burst,
        })
    }

    #[test]
    fn a_burst_is_admitted_and_the_next_packet_is_not() {
        let mut limiter = limiter(10, 20);
        let now = Instant::now();

        for _ in 0..20 {
            assert!(limiter.admit(now));
        }

        assert!(!limiter.admit(now));
    }

    #[test]
    fn the_bucket_refills_at_the_rate_it_was_given() {
        let mut limiter = limiter(10, 20);
        let now = Instant::now();

        for _ in 0..20 {
            assert!(limiter.admit(now));
        }

        assert!(!limiter.admit(now));
        assert!(limiter.admit(now + Duration::from_millis(100)));
        assert!(!limiter.admit(now + Duration::from_millis(100)));
    }

    #[test]
    fn a_burst_under_the_rate_is_lifted_to_it() {
        let mut limiter = limiter(10, 1);
        let now = Instant::now();

        for _ in 0..10 {
            assert!(limiter.admit(now));
        }

        assert!(!limiter.admit(now));
    }

    #[test]
    fn a_router_with_nobody_in_it_delivers_nothing() {
        let router = DataRouter::default();

        assert!(!router.send("PA_nobody", DataChannel::Reliable, b"anything"));
        assert_eq!(
            router.totals(),
            DataTotals {
                peers: 0,
                packets: 0,
                bytes: 0,
                dropped: 0,
            }
        );
    }
}
