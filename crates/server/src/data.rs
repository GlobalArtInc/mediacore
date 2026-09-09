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
#[path = "data_tests.rs"]
mod tests;
