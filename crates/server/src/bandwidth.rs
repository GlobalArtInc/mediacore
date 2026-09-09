use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mediacore_config::BweConfig;
use mediacore_proto::mediacore;
use mediacore_room::TrackKind;
use mediacore_sfu::{
    AllocatorConfig, Allowance, AudioTrack, BandwidthAllocator, Bitrate, CAMERA_PRIORITY,
    Conditions, CongestionConfig, CongestionMonitor, CongestionState, Grant, LAYER_COUNT, LayerSet,
    SCREENSHARE_PRIORITY, Sample, SpatialLayer, VideoTrack,
};
use mediacore_transport::PeerReport;

pub const MIN_OVERHEAD_BYTES: u32 = 20;
pub const MAX_OVERHEAD_BYTES: u32 = 80;
pub const MIN_CALIBRATION_PACKETS: u64 = 30;
const OVERHEAD_SCALE: u32 = 16;
pub const MIN_DESIRED: Bitrate = Bitrate::kbps(300);
pub const DESIRED_STEP_PERCENT: u64 = 10;
pub const UPGRADE_HEADROOM_PERCENT: u64 = 115;
pub const STUCK_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BandwidthSnapshot {
    pub state: CongestionState,
    pub budget: Bitrate,
    pub spend: Bitrate,
    pub desired: Bitrate,
    pub estimate: Option<Bitrate>,
    pub sent: Bitrate,
    pub overhead_bytes: u32,
    pub video_tracks: usize,
    pub audio_tracks: usize,
    pub samples: u64,
}

#[derive(Debug, Default)]
pub struct SubscriberBandwidth {
    pinned: Mutex<Option<Bitrate>>,
    snapshot: Mutex<BandwidthSnapshot>,
}

impl SubscriberBandwidth {
    pub fn pin_estimate(&self, estimate: Option<Bitrate>) {
        *self
            .pinned
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = estimate;
    }

    pub fn pinned(&self) -> Option<Bitrate> {
        *self
            .pinned
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn snapshot(&self) -> BandwidthSnapshot {
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn publish(&self, snapshot: BandwidthSnapshot) {
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = snapshot;
    }
}

#[derive(Debug, Default)]
pub struct Subscribers {
    inner: Mutex<HashMap<String, Arc<SubscriberBandwidth>>>,
}

impl Subscribers {
    pub fn of(&self, subscriber_sid: &str) -> Arc<SubscriberBandwidth> {
        Arc::clone(self.entries().entry(subscriber_sid.to_owned()).or_default())
    }

    pub fn get(&self, subscriber_sid: &str) -> Option<Arc<SubscriberBandwidth>> {
        self.entries().get(subscriber_sid).cloned()
    }

    pub fn remove(&self, subscriber_sid: &str) {
        self.entries().remove(subscriber_sid);
    }

    pub fn count(&self) -> usize {
        self.entries().len()
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<String, Arc<SubscriberBandwidth>>> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Debug, Clone)]
pub struct SubscribedTrack {
    pub track_sid: String,
    pub kind: TrackKind,
    pub source: mediacore::TrackSource,
    pub multi_layer: bool,
    pub carrying: bool,
    pub available: LayerSet,
    pub ceiling: Option<SpatialLayer>,
    pub measured: [Option<Bitrate>; LAYER_COUNT],
    pub declared: [Option<Bitrate>; LAYER_COUNT],
}

#[derive(Debug, Clone, Copy)]
struct Mark {
    at: Instant,
    peer_bytes_tx: u64,
    bytes_tx: u64,
    packets: u64,
}

pub struct BandwidthController {
    monitor: CongestionMonitor,
    allocator: BandwidthAllocator<String>,
    shared: Arc<SubscriberBandwidth>,
    enrolled: BTreeSet<String>,
    max_desired: Bitrate,
    overhead: u32,
    mark: Option<Mark>,
    desired: Option<Bitrate>,
    pinned: Option<Bitrate>,
    sent: Bitrate,
    samples: u64,
    stuck: Option<Stuck>,
}

#[derive(Debug, Clone, Copy)]
struct Stuck {
    since: Instant,
    best: Bitrate,
    warned: bool,
}

impl BandwidthController {
    pub fn new(config: &BweConfig, shared: Arc<SubscriberBandwidth>) -> Self {
        Self {
            monitor: CongestionMonitor::new(congestion_config(config)),
            allocator: BandwidthAllocator::new(allocator_config(config)),
            shared,
            enrolled: BTreeSet::new(),
            max_desired: Bitrate::kbps(config.max_desired_kbps.into()),
            overhead: config.overhead_bytes * OVERHEAD_SCALE,
            mark: None,
            desired: None,
            pinned: None,
            sent: Bitrate::ZERO,
            samples: 0,
            stuck: None,
        }
    }

    pub fn state(&self) -> CongestionState {
        self.monitor.state()
    }

    pub fn budget(&self) -> Bitrate {
        self.monitor.budget()
    }

    pub fn overhead_bytes(&self) -> u32 {
        (self.overhead + OVERHEAD_SCALE / 2) / OVERHEAD_SCALE
    }

    pub fn allowance(&self, track_sid: &str) -> Option<Allowance> {
        self.allocator.grant(&track_sid.to_owned())
    }

    pub fn on_estimate(&mut self, bitrate: Bitrate, at: Instant) {
        if self.shared.pinned().is_some() {
            return;
        }

        self.monitor.on_estimate(bitrate, at);
    }

    pub fn on_report(&mut self, report: &PeerReport, packets: u64, now: Instant) {
        self.follow_pin(report.at);

        let mark = Mark {
            at: report.at,
            peer_bytes_tx: report.peer_bytes_tx,
            bytes_tx: report.bytes_tx,
            packets,
        };
        let Some(previous) = self.mark.replace(mark) else {
            return;
        };

        let elapsed = report.at.saturating_duration_since(previous.at);
        if elapsed.is_zero() {
            return;
        }

        let wire = report.peer_bytes_tx.saturating_sub(previous.peer_bytes_tx);
        let payload = report.bytes_tx.saturating_sub(previous.bytes_tx);
        let forwarded = packets.saturating_sub(previous.packets);

        self.calibrate(wire, payload, forwarded);
        self.sent = Bitrate::from_bits_per_second((wire as f64) * 8.0 / elapsed.as_secs_f64());

        let sample = Sample {
            at: report.at,
            raw: self.pinned.or_else(|| report.bwe_tx.map(Bitrate::from)),
            loss: report.egress_loss,
            rtt: report.rtt,
            sent: self.sent,
            packets: forwarded,
            queue_age: report.queue_age(now),
            queue_bytes: report.queue_bytes(),
        };

        self.samples += 1;
        let spend = self.allocator.spend();
        let verdict = self.monitor.on_sample(sample, spend);

        if verdict.changed {
            tracing::debug!(
                state = %verdict.state,
                budget = %verdict.budget,
                spend = %spend,
                sent = %sample.sent,
                queue_age = ?sample.queue_age,
                loss = ?sample.loss,
                rtt = ?sample.rtt,
                "a subscriber link changed congestion state"
            );
        } else {
            tracing::trace!(
                state = %verdict.state,
                budget = %verdict.budget,
                spend = %spend,
                sent = %sample.sent,
                overhead = self.overhead_bytes(),
                "subscriber bandwidth sample"
            );
        }

        self.warn_if_stuck(sample.at);
        self.publish();
    }

    pub fn tick(&mut self, tracks: &[SubscribedTrack], now: Instant) -> Vec<Grant<String>> {
        let mut grants = self.sync(tracks);

        let conditions = Conditions {
            now,
            budget: self.monitor.budget(),
            state: self.monitor.state(),
            state_since: self.monitor.state_since(),
        };
        grants.extend(self.allocator.allocate(conditions));

        for grant in &grants {
            tracing::debug!(
                track = %grant.key,
                allowance = %grant.allowance,
                budget = %conditions.budget,
                state = %conditions.state,
                "a subscriber track was granted a new allowance"
            );
        }

        self.publish();

        grants
    }

    pub fn desired_update(&self) -> Option<Bitrate> {
        let wish = self.wish();

        let moved = match self.desired {
            None => true,
            Some(previous) => {
                let step = previous.scaled(DESIRED_STEP_PERCENT, 100);
                wish > previous.saturating_add(step) || wish < previous.saturating_sub(step)
            }
        };

        moved.then_some(wish)
    }

    pub fn desired_delivered(&mut self, desired: Bitrate) {
        self.desired = Some(desired);
    }

    fn wish(&self) -> Bitrate {
        let wish = self.allocator.desired();

        if wish < MIN_DESIRED {
            return MIN_DESIRED;
        }

        wish.min(self.max_desired)
    }

    fn follow_pin(&mut self, at: Instant) {
        let pinned = self.shared.pinned();

        if pinned != self.pinned {
            tracing::debug!(
                pinned = ?pinned.map(Bitrate::as_kbps),
                "a subscriber estimate was pinned by hand"
            );
            self.pinned = pinned;
        }

        if let Some(pinned) = pinned {
            self.monitor.on_estimate(pinned, at);
        }
    }

    fn warn_if_stuck(&mut self, at: Instant) {
        let held_back = !self.monitor.state().is_clear() || self.allocator.is_held_back();

        if !held_back {
            self.stuck = None;
            return;
        }

        let best = self.monitor.estimate().unwrap_or(Bitrate::ZERO);
        let mut stuck = self.stuck.unwrap_or(Stuck {
            since: at,
            best,
            warned: false,
        });

        if best > stuck.best {
            stuck = Stuck {
                since: at,
                best,
                warned: false,
            };
            self.stuck = Some(stuck);
            return;
        }

        if stuck.warned || at.saturating_duration_since(stuck.since) < STUCK_AFTER {
            self.stuck = Some(stuck);
            return;
        }

        stuck.warned = true;
        self.stuck = Some(stuck);

        tracing::warn!(
            state = %self.monitor.state(),
            for_secs = at.saturating_duration_since(stuck.since).as_secs(),
            budget = %self.monitor.budget(),
            estimate = ?self.monitor.estimate().map(Bitrate::as_kbps),
            spend = %self.allocator.spend(),
            "a subscriber has been held below what it asked for for a long time with no upward estimate movement"
        );
    }

    pub fn is_stuck(&self) -> bool {
        self.stuck.is_some_and(|stuck| stuck.warned)
    }

    fn calibrate(&mut self, wire: u64, payload: u64, packets: u64) {
        if packets < MIN_CALIBRATION_PACKETS {
            return;
        }

        let observed = (wire.saturating_sub(payload) / packets) as u32;
        let observed = observed.clamp(MIN_OVERHEAD_BYTES, MAX_OVERHEAD_BYTES);

        self.overhead = (self.overhead * 3 + observed * OVERHEAD_SCALE) / 4;
    }

    fn sync(&mut self, tracks: &[SubscribedTrack]) -> Vec<Grant<String>> {
        let mut present = BTreeSet::new();

        for track in tracks {
            present.insert(track.track_sid.clone());

            match track.kind {
                TrackKind::Audio => self
                    .allocator
                    .upsert_audio(track.track_sid.clone(), audio_track(track)),
                TrackKind::Video => self
                    .allocator
                    .upsert_video(track.track_sid.clone(), enrol(track)),
            }
        }

        let gone: Vec<String> = self.enrolled.difference(&present).cloned().collect();
        let mut released = Vec::new();

        for key in gone {
            let granted = self.allocator.grant(&key);
            self.allocator.remove(&key);

            if granted.is_some_and(|granted| granted != Allowance::Unlimited) {
                released.push(Grant {
                    key,
                    allowance: Allowance::Unlimited,
                });
            }
        }

        self.enrolled = present;

        released
    }

    fn publish(&self) {
        self.shared.publish(BandwidthSnapshot {
            state: self.monitor.state(),
            budget: self.monitor.budget(),
            spend: self.allocator.spend(),
            desired: self.wish(),
            estimate: self.monitor.estimate(),
            sent: self.sent,
            overhead_bytes: self.overhead_bytes(),
            video_tracks: self.allocator.video_tracks(),
            audio_tracks: self.allocator.audio_tracks(),
            samples: self.samples,
        });
    }
}

fn audio_track(track: &SubscribedTrack) -> AudioTrack {
    if track.carrying {
        AudioTrack::carrying(track.measured[0])
    } else {
        AudioTrack::silent()
    }
}

fn enrol(track: &SubscribedTrack) -> VideoTrack {
    let enrolled = VideoTrack::new(track.available, track.ceiling)
        .with_measured(track.measured)
        .with_declared(track.declared);

    match track.source {
        mediacore::TrackSource::ScreenShare | mediacore::TrackSource::ScreenShareAudio => {
            if track.multi_layer {
                enrolled.with_priority(SCREENSHARE_PRIORITY)
            } else {
                enrolled.exempt()
            }
        }
        _ => enrolled.with_priority(CAMERA_PRIORITY),
    }
}

fn allocator_config(config: &BweConfig) -> AllocatorConfig {
    AllocatorConfig {
        allow_pause: config.allow_pause,
        audio_floor: Bitrate::kbps(config.audio_reserve_kbps.into()),
        recovery_hold: config.recovery_hold(),
        recovery_step: config.recovery_step(),
        upgrade_headroom_percent: UPGRADE_HEADROOM_PERCENT,
        max_overrun_percent: config.max_overrun_percent.into(),
        ..AllocatorConfig::default()
    }
}

fn congestion_config(config: &BweConfig) -> CongestionConfig {
    CongestionConfig {
        initial: Bitrate::kbps(config.initial_kbps.into()),
        min_budget: Bitrate::kbps(config.min_budget_kbps.into()),
        queue_pressure: config.queue_pressure(),
        ..CongestionConfig::default()
    }
}

#[cfg(test)]
#[path = "bandwidth_tests.rs"]
mod tests;
