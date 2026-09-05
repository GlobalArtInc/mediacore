use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub struct PublisherStats {
    connected: AtomicBool,
    rtp_packets: AtomicU64,
    rtp_bytes: AtomicU64,
    rtp_without_mid_extension: AtomicU64,
    rtp_unattributed: AtomicU64,
    bound_tracks: AtomicU64,
    tracks: Mutex<HashMap<String, TrackSnapshot>>,
}

impl PublisherStats {
    pub fn mark_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
    }

    pub fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    pub fn count_rtp(&self, track_sid: Option<&str>, bytes: usize, carried_mid_extension: bool) {
        self.rtp_packets.fetch_add(1, Ordering::Relaxed);
        self.rtp_bytes.fetch_add(bytes as u64, Ordering::Relaxed);

        if !carried_mid_extension {
            self.rtp_without_mid_extension
                .fetch_add(1, Ordering::Relaxed);
        }

        let Some(track_sid) = track_sid else {
            self.rtp_unattributed.fetch_add(1, Ordering::Relaxed);
            return;
        };

        let mut tracks = self.tracks();
        let track = tracks.entry(track_sid.to_owned()).or_default();
        track.rtp_packets += 1;
        track.rtp_bytes += bytes as u64;
        if !carried_mid_extension {
            track.rtp_without_mid_extension += 1;
        }
    }

    pub fn count_bound_track(&self) {
        self.bound_tracks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn track(&self, track_sid: &str) -> Option<TrackSnapshot> {
        self.tracks().get(track_sid).copied()
    }

    pub fn attributed_tracks(&self) -> Vec<String> {
        let mut sids: Vec<String> = self.tracks().keys().cloned().collect();
        sids.sort();
        sids
    }

    pub fn snapshot(&self) -> PublisherSnapshot {
        PublisherSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            rtp_packets: self.rtp_packets.load(Ordering::Relaxed),
            rtp_bytes: self.rtp_bytes.load(Ordering::Relaxed),
            rtp_without_mid_extension: self.rtp_without_mid_extension.load(Ordering::Relaxed),
            rtp_unattributed: self.rtp_unattributed.load(Ordering::Relaxed),
            bound_tracks: self.bound_tracks.load(Ordering::Relaxed),
        }
    }

    fn tracks(&self) -> std::sync::MutexGuard<'_, HashMap<String, TrackSnapshot>> {
        self.tracks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublisherSnapshot {
    pub connected: bool,
    pub rtp_packets: u64,
    pub rtp_bytes: u64,
    pub rtp_without_mid_extension: u64,
    pub rtp_unattributed: u64,
    pub bound_tracks: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackSnapshot {
    pub rtp_packets: u64,
    pub rtp_bytes: u64,
    pub rtp_without_mid_extension: u64,
}

#[derive(Debug, Default)]
pub struct Publishers {
    inner: Mutex<HashMap<String, Arc<PublisherStats>>>,
}

impl Publishers {
    pub fn register(&self, participant_sid: &str) -> Arc<PublisherStats> {
        let stats = Arc::new(PublisherStats::default());
        self.lock()
            .insert(participant_sid.to_owned(), Arc::clone(&stats));
        stats
    }

    pub fn remove(&self, participant_sid: &str) {
        self.lock().remove(participant_sid);
    }

    pub fn snapshot(&self, participant_sid: &str) -> Option<PublisherSnapshot> {
        self.lock()
            .get(participant_sid)
            .map(|stats| stats.snapshot())
    }

    pub fn track_snapshot(&self, participant_sid: &str, track_sid: &str) -> Option<TrackSnapshot> {
        self.lock()
            .get(participant_sid)
            .and_then(|stats| stats.track(track_sid))
    }

    pub fn attributed_tracks(&self, participant_sid: &str) -> Vec<String> {
        self.lock()
            .get(participant_sid)
            .map(|stats| stats.attributed_tracks())
            .unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.lock().len()
    }

    pub fn totals(&self) -> PublisherSnapshot {
        self.lock().values().map(|stats| stats.snapshot()).fold(
            PublisherSnapshot::default(),
            |mut total, one| {
                total.connected |= one.connected;
                total.rtp_packets += one.rtp_packets;
                total.rtp_bytes += one.rtp_bytes;
                total.rtp_without_mid_extension += one.rtp_without_mid_extension;
                total.rtp_unattributed += one.rtp_unattributed;
                total.bound_tracks += one.bound_tracks;
                total
            },
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<PublisherStats>>> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_are_kept_per_participant_and_dropped_with_it() {
        let publishers = Publishers::default();
        let alice = publishers.register("PA_alice");
        publishers.register("PA_bob");

        alice.mark_connected();
        alice.count_rtp(Some("TR_A1"), 100, true);
        alice.count_rtp(Some("TR_A1"), 40, false);
        alice.count_bound_track();

        let snapshot = publishers.snapshot("PA_alice").unwrap();
        assert!(snapshot.connected);
        assert_eq!(snapshot.rtp_packets, 2);
        assert_eq!(snapshot.rtp_bytes, 140);
        assert_eq!(snapshot.rtp_without_mid_extension, 1);
        assert_eq!(snapshot.bound_tracks, 1);

        assert_eq!(publishers.totals().rtp_packets, 2);
        assert_eq!(publishers.count(), 2);

        publishers.remove("PA_alice");
        assert_eq!(publishers.snapshot("PA_alice"), None);
        assert_eq!(publishers.totals().rtp_packets, 0);
    }

    #[test]
    fn packets_are_counted_against_the_track_they_belong_to() {
        let publishers = Publishers::default();
        let alice = publishers.register("PA_alice");

        alice.count_rtp(Some("TR_A1"), 100, true);
        alice.count_rtp(Some("TR_V1"), 900, false);
        alice.count_rtp(Some("TR_V1"), 800, false);
        alice.count_rtp(None, 50, true);

        assert_eq!(
            publishers.track_snapshot("PA_alice", "TR_A1"),
            Some(TrackSnapshot {
                rtp_packets: 1,
                rtp_bytes: 100,
                rtp_without_mid_extension: 0,
            })
        );
        assert_eq!(
            publishers.track_snapshot("PA_alice", "TR_V1"),
            Some(TrackSnapshot {
                rtp_packets: 2,
                rtp_bytes: 1700,
                rtp_without_mid_extension: 2,
            })
        );
        assert_eq!(publishers.track_snapshot("PA_alice", "TR_missing"), None);
        assert_eq!(
            publishers.attributed_tracks("PA_alice"),
            vec!["TR_A1".to_owned(), "TR_V1".to_owned()]
        );
        assert_eq!(publishers.snapshot("PA_alice").unwrap().rtp_unattributed, 1);
    }
}
