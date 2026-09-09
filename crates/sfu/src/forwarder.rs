use std::time::{Duration, Instant};

use str0m::media::MediaKind;
use str0m::rtp::SeqNo;

use crate::history::{DeltaHistory, SeqWindow, Slot};
use crate::ladder::LayerLadder;
use crate::layer::{LayerDecision, LayerLatch, LayerSet, SpatialLayer};
use crate::packet::{
    DropReason, ForwardedPacket, Forwarding, InboundPacket, subscriber_extensions,
};
use crate::selector::{Allowance, LayerPreference, LayerSelector, SelectorConfig};
use crate::seq::{SeqUnwrapper, TimeUnwrapper};

const DEFAULT_KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_KEYFRAME_ATTEMPTS: u32 = 20;
const MAX_RESUME_ADVANCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwarderConfig {
    pub kind: MediaKind,
    pub clock_rate: u32,
    pub keyframe_interval: Duration,
    pub keyframe_attempts: u32,
}

impl ForwarderConfig {
    pub fn audio() -> Self {
        Self {
            kind: MediaKind::Audio,
            clock_rate: 48_000,
            keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
            keyframe_attempts: DEFAULT_KEYFRAME_ATTEMPTS,
        }
    }

    pub fn video() -> Self {
        Self {
            kind: MediaKind::Video,
            clock_rate: 90_000,
            keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
            keyframe_attempts: DEFAULT_KEYFRAME_ATTEMPTS,
        }
    }

    fn gated(&self) -> bool {
        self.kind == MediaKind::Video
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStart {
    pub seq: u64,
    pub timestamp: u64,
}

impl StreamStart {
    pub fn from_seed(seed: u64) -> Self {
        let first = mix(seed);
        let second = mix(first);

        Self {
            seq: (first & 0x3fff) + (1 << 15),
            timestamp: (second & 0x3fff_ffff) + (1 << 31),
        }
    }
}

fn mix(value: u64) -> u64 {
    let mut value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForwarderStats {
    pub forwarded: u64,
    pub bytes: u64,
    pub keyframes: u64,
    pub gaps: u64,
    pub out_of_order: u64,
    pub duplicates: u64,
    pub padding: u64,
    pub too_old: u64,
    pub gated: u64,
    pub wrong_layer: u64,
    pub muted: u64,
    pub paused: u64,
    pub source_starts: u64,
    pub layer_switches: u64,
    pub truncated_frames: u64,
}

#[derive(Debug)]
struct SourceState {
    ssrc: u32,
    seq: SeqUnwrapper,
    time: TimeUnwrapper,
    highest: u64,
    window: SeqWindow,
    seq_deltas: DeltaHistory,
    time_delta: i64,
}

#[derive(Debug)]
enum Translation {
    Send {
        seq: u64,
        timestamp: u64,
        gap: bool,
        out_of_order: bool,
    },
    Drop(DropReason),
}

impl SourceState {
    fn translate(&mut self, packet: &InboundPacket) -> Translation {
        let seq = self.seq.extend(u64::from(packet.seq));
        let timestamp = self.time.extend(u64::from(packet.timestamp));

        if seq > self.highest {
            let gap = seq - self.highest > 1;

            if !gap && packet.is_padding() {
                self.highest = seq;
                self.window.observe(seq);
                self.seq_deltas.shift(seq + 1, -1);
                return Translation::Drop(DropReason::PaddingOnly);
            }

            self.highest = seq;
            self.window.observe(seq);

            return Translation::Send {
                seq: shift(seq, self.seq_deltas.current()),
                timestamp: shift(timestamp, self.time_delta),
                gap,
                out_of_order: false,
            };
        }

        match self.window.observe(seq) {
            Slot::Seen => Translation::Drop(DropReason::Duplicate),
            Slot::TooOld => Translation::Drop(DropReason::TooOld),
            Slot::New => match self.seq_deltas.at(seq) {
                Some(delta) => Translation::Send {
                    seq: shift(seq, delta),
                    timestamp: shift(timestamp, self.time_delta),
                    gap: false,
                    out_of_order: true,
                },
                None => Translation::Drop(DropReason::TooOld),
            },
        }
    }
}

fn shift(value: u64, delta: i64) -> u64 {
    value.wrapping_add_signed(delta)
}

#[derive(Debug, Clone, Copy)]
struct OutgoingState {
    started: bool,
    seq: u64,
    timestamp: u64,
    marker: bool,
    arrival: Option<Instant>,
}

#[derive(Debug)]
pub struct TrackForwarder {
    config: ForwarderConfig,
    start: StreamStart,
    latch: LayerLatch,
    selector: LayerSelector,
    source: Option<SourceState>,
    out: OutgoingState,
    publisher_muted: bool,
    subscriber_paused: bool,
    bandwidth_paused: bool,
    keyframe_needed: bool,
    keyframe_asked: Option<Instant>,
    keyframe_attempts: u32,
    stats: ForwarderStats,
}

impl TrackForwarder {
    pub fn new(config: ForwarderConfig, start: StreamStart) -> Self {
        Self::with_selector(config, start, SelectorConfig::default())
    }

    pub fn with_selector(
        config: ForwarderConfig,
        start: StreamStart,
        selector: SelectorConfig,
    ) -> Self {
        Self {
            latch: LayerLatch::new(config.gated()),
            selector: LayerSelector::new(selector),
            config,
            start,
            source: None,
            out: OutgoingState {
                started: false,
                seq: 0,
                timestamp: 0,
                marker: true,
                arrival: None,
            },
            publisher_muted: false,
            subscriber_paused: false,
            bandwidth_paused: false,
            keyframe_needed: config.gated(),
            keyframe_asked: None,
            keyframe_attempts: 0,
            stats: ForwarderStats::default(),
        }
    }

    pub fn config(&self) -> &ForwarderConfig {
        &self.config
    }

    pub fn stats(&self) -> ForwarderStats {
        self.stats
    }

    pub fn current_layer(&self) -> Option<SpatialLayer> {
        self.latch.current()
    }

    pub fn target_layer(&self) -> SpatialLayer {
        self.latch.target()
    }

    pub fn is_switching(&self) -> bool {
        self.latch.is_switching()
    }

    pub fn request_layer(&mut self, layer: SpatialLayer) -> bool {
        if !self.latch.request(layer) {
            return false;
        }

        tracing::debug!(
            target = %layer,
            current = ?self.latch.current(),
            "subscriber asked for another layer"
        );

        self.keyframe_needed = self.config.gated() && self.latch.current() != Some(layer);
        self.keyframe_asked = None;
        self.keyframe_attempts = 0;

        true
    }

    pub fn selector(&self) -> &LayerSelector {
        &self.selector
    }

    pub fn preference(&self) -> LayerPreference {
        self.selector.preference()
    }

    pub fn set_preference(&mut self, preference: LayerPreference, now: Instant) -> bool {
        let target = self.selector.set_preference(preference, now);
        self.take_target(target)
    }

    pub fn set_ladder(&mut self, ladder: LayerLadder, now: Instant) -> bool {
        let target = self.selector.set_ladder(ladder, now);
        self.take_target(target)
    }

    pub fn allowance(&self) -> Allowance {
        self.selector.allowance()
    }

    pub fn set_allowance(&mut self, allowance: Allowance, now: Instant) -> bool {
        let was = self.is_paused();
        let target = self.selector.set_allowance(allowance, now);

        self.bandwidth_paused = allowance.is_paused();
        self.apply_pause(was);

        self.take_target(target)
    }

    pub fn observe_layers(&mut self, available: LayerSet, now: Instant) -> bool {
        let target = self.selector.update(available, now);
        self.take_target(target)
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        let target = self.selector.tick(now);
        self.take_target(target)
    }

    pub fn wanted_layer(&self) -> Option<SpatialLayer> {
        if self.subscriber_paused {
            return None;
        }

        self.selector.preferred_ceiling()
    }

    fn take_target(&mut self, target: Option<SpatialLayer>) -> bool {
        match target {
            Some(layer) => self.request_layer(layer),
            None => false,
        }
    }

    pub fn needs_keyframe(&self) -> bool {
        self.keyframe_needed
    }

    pub fn keyframe_layer(&self) -> Option<SpatialLayer> {
        self.wants_keyframe().then(|| self.latch.target())
    }

    pub fn keyframe_attempts(&self) -> u32 {
        self.keyframe_attempts
    }

    fn wants_keyframe(&self) -> bool {
        self.keyframe_needed
            && !self.publisher_muted
            && !self.is_paused()
            && self.keyframe_attempts < self.config.keyframe_attempts
    }

    pub fn poll_keyframe_request(&mut self, now: Instant) -> bool {
        if !self.wants_keyframe() {
            return false;
        }

        if let Some(asked) = self.keyframe_asked
            && now.saturating_duration_since(asked) < self.config.keyframe_interval
        {
            return false;
        }

        self.keyframe_asked = Some(now);
        self.keyframe_attempts += 1;
        true
    }

    pub fn on_frame_boundary(&self) -> bool {
        self.out.marker
    }

    pub fn last_sent(&self) -> Option<(SeqNo, u32)> {
        self.out
            .started
            .then(|| (SeqNo::from(self.out.seq), self.out.timestamp as u32))
    }

    pub fn is_publisher_muted(&self) -> bool {
        self.publisher_muted
    }

    pub fn is_subscriber_paused(&self) -> bool {
        self.subscriber_paused
    }

    pub fn is_bandwidth_paused(&self) -> bool {
        self.bandwidth_paused
    }

    pub fn is_paused(&self) -> bool {
        self.subscriber_paused || self.bandwidth_paused
    }

    pub fn set_publisher_muted(&mut self, muted: bool) {
        if self.publisher_muted == muted {
            return;
        }

        self.publisher_muted = muted;
        if !muted {
            self.resync();
        }
    }

    pub fn set_subscriber_paused(&mut self, paused: bool) {
        if self.subscriber_paused == paused {
            return;
        }

        let was = self.is_paused();
        self.subscriber_paused = paused;
        self.apply_pause(was);
    }

    fn apply_pause(&mut self, was: bool) {
        if was && !self.is_paused() {
            self.resync();
        }
    }

    pub fn resync(&mut self) {
        self.latch.reset();
        self.source = None;
        self.keyframe_needed = self.config.gated();
        self.keyframe_asked = None;
        self.keyframe_attempts = 0;
    }

    pub fn forward(&mut self, packet: &InboundPacket) -> Forwarding {
        if self.publisher_muted {
            self.stats.muted += 1;
            return Forwarding::Drop(DropReason::PublisherMuted);
        }

        if self.is_paused() {
            self.stats.paused += 1;
            return Forwarding::Drop(DropReason::SubscriberPaused);
        }

        if self.keyframe_needed {
            self.keyframe_attempts = 0;
        }

        let mut switching = match self.latch.select(packet.layer, packet.keyframe) {
            LayerDecision::Forward => false,
            LayerDecision::Switch => true,
            LayerDecision::AwaitKeyframe => {
                self.keyframe_needed = true;
                self.stats.gated += 1;
                return Forwarding::Drop(DropReason::AwaitingKeyframe);
            }
            LayerDecision::WrongLayer => {
                self.stats.wrong_layer += 1;
                return Forwarding::Drop(DropReason::WrongLayer);
            }
        };

        let restarted = match &self.source {
            Some(source) => source.ssrc != packet.ssrc,
            None => true,
        };

        if restarted && !switching {
            if self.config.gated() && !packet.keyframe {
                tracing::debug!(
                    ssrc = packet.ssrc,
                    "publisher restarted the stream, holding until a keyframe"
                );
                self.latch.reset();
                self.keyframe_needed = true;
                self.keyframe_attempts = 0;
                self.stats.gated += 1;
                return Forwarding::Drop(DropReason::AwaitingKeyframe);
            }

            switching = true;
        }

        if switching {
            let source = self.begin_source(packet);
            let previous = self.source.replace(source);
            self.stats.source_starts += 1;

            if self.out.started {
                self.stats.layer_switches += 1;

                if !self.out.marker {
                    self.stats.truncated_frames += 1;
                }

                tracing::debug!(
                    layer = %packet.layer,
                    from = previous.map(|source| source.ssrc),
                    to = packet.ssrc,
                    "switching the subscriber onto another source"
                );
            }
        }

        let translation = match self.source.as_mut() {
            Some(source) => source.translate(packet),
            None => Translation::Drop(DropReason::AwaitingKeyframe),
        };

        let (seq, timestamp, gap, out_of_order) = match translation {
            Translation::Send {
                seq,
                timestamp,
                gap,
                out_of_order,
            } => (seq, timestamp, gap, out_of_order),
            Translation::Drop(reason) => {
                match reason {
                    DropReason::Duplicate => self.stats.duplicates += 1,
                    DropReason::PaddingOnly => self.stats.padding += 1,
                    DropReason::TooOld => self.stats.too_old += 1,
                    _ => {}
                }
                return Forwarding::Drop(reason);
            }
        };

        if gap {
            self.stats.gaps += 1;
        }
        if out_of_order {
            self.stats.out_of_order += 1;
        }

        if !out_of_order {
            self.out.seq = seq;
            self.out.timestamp = timestamp;
            self.out.marker = packet.marker;
        }
        self.out.started = true;
        self.out.arrival = Some(packet.arrival);

        if packet.keyframe {
            self.stats.keyframes += 1;

            if !self.latch.is_switching() {
                self.keyframe_needed = false;
                self.keyframe_asked = None;
                self.keyframe_attempts = 0;
            }
        }

        self.stats.forwarded += 1;
        self.stats.bytes += packet.payload.len() as u64;

        Forwarding::Send(Box::new(ForwardedPacket {
            seq: SeqNo::from(seq),
            timestamp: timestamp as u32,
            ext_timestamp: timestamp,
            marker: packet.marker,
            keyframe: packet.keyframe,
            nackable: self.config.kind == MediaKind::Video,
            switched: switching,
            payload: packet.payload.clone(),
            ext: subscriber_extensions(&packet.ext),
        }))
    }

    fn begin_source(&self, packet: &InboundPacket) -> SourceState {
        let mut seq = SeqUnwrapper::new();
        let mut time = TimeUnwrapper::new();
        let first_seq = seq.extend(u64::from(packet.seq));
        let first_time = time.extend(u64::from(packet.timestamp));

        let (target_seq, target_time) = if self.out.started {
            (
                self.out.seq.saturating_add(1),
                self.out
                    .timestamp
                    .saturating_add(self.resume_advance(packet)),
            )
        } else {
            (self.start.seq, self.start.timestamp)
        };

        let seq_delta = (target_seq as i64).wrapping_sub(first_seq as i64);
        let time_delta = (target_time as i64).wrapping_sub(first_time as i64);
        let anchor = if self.out.started { first_seq } else { 0 };

        SourceState {
            ssrc: packet.ssrc,
            seq,
            time,
            highest: first_seq.saturating_sub(1),
            window: SeqWindow::new(),
            seq_deltas: DeltaHistory::anchored(anchor, seq_delta),
            time_delta,
        }
    }

    fn resume_advance(&self, packet: &InboundPacket) -> u64 {
        let Some(previous) = self.out.arrival else {
            return 1;
        };

        let elapsed = packet
            .arrival
            .saturating_duration_since(previous)
            .min(MAX_RESUME_ADVANCE);

        let ticks = elapsed.as_nanos() * u128::from(self.config.clock_rate) / 1_000_000_000;

        u64::try_from(ticks).unwrap_or(u64::from(u32::MAX)).max(1)
    }
}

#[cfg(test)]
#[path = "forwarder_tests.rs"]
mod tests;
