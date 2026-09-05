use std::time::{Duration, Instant};

use str0m::media::MediaKind;
use str0m::rtp::SeqNo;

use crate::history::{DeltaHistory, SeqWindow, Slot};
use crate::layer::{LayerDecision, LayerSelector, SpatialLayer};
use crate::packet::{
    DropReason, ForwardedPacket, Forwarding, InboundPacket, subscriber_extensions,
};
use crate::seq::{SeqUnwrapper, TimeUnwrapper};

const DEFAULT_KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
const MAX_RESUME_ADVANCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwarderConfig {
    pub kind: MediaKind,
    pub clock_rate: u32,
    pub keyframe_interval: Duration,
}

impl ForwarderConfig {
    pub fn audio() -> Self {
        Self {
            kind: MediaKind::Audio,
            clock_rate: 48_000,
            keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
        }
    }

    pub fn video() -> Self {
        Self {
            kind: MediaKind::Video,
            clock_rate: 90_000,
            keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
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
    layers: LayerSelector,
    source: Option<SourceState>,
    out: OutgoingState,
    publisher_muted: bool,
    subscriber_paused: bool,
    keyframe_needed: bool,
    keyframe_asked: Option<Instant>,
    stats: ForwarderStats,
}

impl TrackForwarder {
    pub fn new(config: ForwarderConfig, start: StreamStart) -> Self {
        Self {
            layers: LayerSelector::new(config.gated()),
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
            keyframe_needed: config.gated(),
            keyframe_asked: None,
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
        self.layers.current()
    }

    pub fn target_layer(&self) -> SpatialLayer {
        self.layers.target()
    }

    pub fn request_layer(&mut self, layer: SpatialLayer) -> bool {
        if !self.layers.request(layer) {
            return false;
        }

        self.keyframe_needed = self.config.gated();
        true
    }

    pub fn needs_keyframe(&self) -> bool {
        self.keyframe_needed
    }

    pub fn poll_keyframe_request(&mut self, now: Instant) -> bool {
        if !self.keyframe_needed {
            return false;
        }

        if let Some(asked) = self.keyframe_asked
            && now.saturating_duration_since(asked) < self.config.keyframe_interval
        {
            return false;
        }

        self.keyframe_asked = Some(now);
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

        self.subscriber_paused = paused;
        if !paused {
            self.resync();
        }
    }

    pub fn resync(&mut self) {
        self.layers.reset();
        self.source = None;
        self.keyframe_needed = self.config.gated();
        self.keyframe_asked = None;
    }

    pub fn forward(&mut self, packet: &InboundPacket) -> Forwarding {
        if self.publisher_muted {
            self.stats.muted += 1;
            return Forwarding::Drop(DropReason::PublisherMuted);
        }

        if self.subscriber_paused {
            self.stats.paused += 1;
            return Forwarding::Drop(DropReason::SubscriberPaused);
        }

        let mut switching = match self.layers.select(packet.layer, packet.keyframe) {
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
                self.layers.reset();
                self.keyframe_needed = true;
                self.stats.gated += 1;
                return Forwarding::Drop(DropReason::AwaitingKeyframe);
            }

            switching = true;
        }

        if switching {
            let source = self.begin_source(packet);
            self.source = Some(source);
            self.stats.source_starts += 1;
            if self.out.started {
                self.stats.layer_switches += 1;
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
            self.keyframe_needed = false;
            self.keyframe_asked = None;
            self.stats.keyframes += 1;
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
mod tests {
    use super::*;
    use crate::packet::DropReason;

    const PUBLISHER: u32 = 0x1111_1111;
    const RESTARTED: u32 = 0x2222_2222;
    const TICK: Duration = Duration::from_millis(20);
    const STEP: u32 = 1800;

    fn video(seed: u64) -> TrackForwarder {
        TrackForwarder::new(ForwarderConfig::video(), StreamStart::from_seed(seed))
    }

    fn audio(seed: u64) -> TrackForwarder {
        TrackForwarder::new(ForwarderConfig::audio(), StreamStart::from_seed(seed))
    }

    fn base() -> Instant {
        Instant::now()
    }

    fn packet(ssrc: u32, seq: u16, timestamp: u32, arrival: Instant) -> InboundPacket {
        InboundPacket::new(ssrc, seq, timestamp, vec![0xab, 0xcd, 0xef], arrival)
    }

    fn keyframe(ssrc: u32, seq: u16, timestamp: u32, arrival: Instant) -> InboundPacket {
        packet(ssrc, seq, timestamp, arrival).with_keyframe(true)
    }

    fn sent(result: &Forwarding) -> &ForwardedPacket {
        result.sent().expect("packet is forwarded")
    }

    #[test]
    fn a_video_subscriber_waits_for_a_keyframe_before_seeing_anything() {
        let now = base();
        let mut forwarder = video(1);

        assert!(forwarder.needs_keyframe());
        for step in 0..3u16 {
            let result = forwarder.forward(&packet(
                PUBLISHER,
                100 + step,
                9000 + u32::from(step) * STEP,
                now + TICK * u32::from(step),
            ));
            assert_eq!(result.dropped(), Some(DropReason::AwaitingKeyframe));
        }

        assert!(forwarder.poll_keyframe_request(now));
        assert!(!forwarder.poll_keyframe_request(now + Duration::from_millis(100)));
        assert!(forwarder.poll_keyframe_request(now + Duration::from_millis(600)));

        let result = forwarder.forward(&keyframe(PUBLISHER, 103, 9000 + 3 * STEP, now + TICK * 3));
        let first = sent(&result);

        assert_eq!(*first.seq, StreamStart::from_seed(1).seq);
        assert_eq!(first.ext_timestamp, StreamStart::from_seed(1).timestamp);
        assert!(first.switched);
        assert!(first.nackable);
        assert!(!forwarder.needs_keyframe());
        assert!(!forwarder.poll_keyframe_request(now + Duration::from_secs(5)));
        assert_eq!(forwarder.stats().gated, 3);
    }

    #[test]
    fn an_audio_subscriber_starts_on_the_first_packet() {
        let now = base();
        let mut forwarder = audio(1);

        assert!(!forwarder.needs_keyframe());
        let result = forwarder.forward(&packet(PUBLISHER, 7, 480, now));
        let first = sent(&result);

        assert_eq!(*first.seq, StreamStart::from_seed(1).seq);
        assert!(!first.nackable);
    }

    #[test]
    fn the_outgoing_stream_survives_a_publisher_restart() {
        let now = base();
        let mut forwarder = video(1);

        let mut last_seq = 0;
        let mut last_time = 0;
        for step in 0..4u16 {
            let result = forwarder.forward(
                &packet(
                    PUBLISHER,
                    500 + step,
                    70_000 + u32::from(step) * STEP,
                    now + TICK * u32::from(step),
                )
                .with_keyframe(step == 0),
            );
            let out = sent(&result);
            last_seq = *out.seq;
            last_time = out.ext_timestamp;
        }

        let held = forwarder.forward(&packet(RESTARTED, 9, 12, now + TICK * 5));
        assert_eq!(held.dropped(), Some(DropReason::AwaitingKeyframe));
        assert!(forwarder.needs_keyframe());

        let result = forwarder.forward(&keyframe(RESTARTED, 10, 21, now + TICK * 6));
        let first = sent(&result);

        assert_eq!(*first.seq, last_seq + 1);
        assert!(first.switched);
        assert!(
            first.ext_timestamp > last_time,
            "restart must not move the subscriber clock backwards"
        );
        assert_eq!(
            first.ext_timestamp - last_time,
            u64::from(TICK.as_millis() as u32 * 3 * 90),
            "the restart gap is paid for in the subscriber timestamp space"
        );

        let follow = forwarder.forward(&packet(RESTARTED, 11, 21 + STEP, now + TICK * 7));
        let follow = sent(&follow);
        assert_eq!(*follow.seq, last_seq + 2);
        assert_eq!(follow.ext_timestamp, first.ext_timestamp + u64::from(STEP));
        assert_eq!(forwarder.stats().source_starts, 2);
        assert_eq!(forwarder.stats().layer_switches, 1);
    }

    #[test]
    fn a_source_switch_never_reuses_an_outgoing_sequence_number() {
        let now = base();
        let mut forwarder = video(21);
        let mut emitted: Vec<u64> = Vec::new();
        let mut record = |result: &Forwarding| {
            if let Some(packet) = result.sent() {
                emitted.push(*packet.seq);
            }
        };

        for step in 0..6u16 {
            let result = forwarder.forward(
                &packet(
                    PUBLISHER,
                    600 + step,
                    30_000 + u32::from(step) * STEP,
                    now + TICK * u32::from(step),
                )
                .with_keyframe(step == 0),
            );
            record(&result);
        }

        let late = forwarder.forward(&packet(PUBLISHER, 603, 30_000 + 3 * STEP, now + TICK * 6));
        assert_eq!(late.dropped(), Some(DropReason::Duplicate));

        let restart = forwarder.forward(&keyframe(RESTARTED, 1_000, 77, now + TICK * 7));
        record(&restart);
        assert!(sent(&restart).switched);

        for (index, seq) in [998u16, 999, 1_001, 1_002, 997].into_iter().enumerate() {
            let step = index as u32 + 8;
            let result = forwarder.forward(&packet(
                RESTARTED,
                seq,
                77 + u32::from(seq.wrapping_sub(1_000)) * STEP,
                now + TICK * step,
            ));
            record(&result);
        }

        let mut seen: Vec<u64> = emitted.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            emitted.len(),
            "the outgoing stream reused a sequence number across the source switch: {emitted:?}"
        );

        let switch = emitted[6];
        assert_eq!(
            switch,
            emitted[5] + 1,
            "the new source must continue where the old one stopped"
        );
        assert!(
            emitted[6..].iter().all(|seq| *seq >= switch),
            "a packet from before the switch point was mapped below it: {emitted:?}"
        );
        assert_eq!(
            forwarder.stats().too_old,
            3,
            "packets the new source sent before the switch point must be refused"
        );
    }

    #[test]
    fn sixteen_bit_wraparound_is_invisible_to_the_subscriber() {
        let now = base();
        let mut forwarder = video(3);

        let start = forwarder.forward(&keyframe(PUBLISHER, 65_533, 1000, now));
        let first = *sent(&start).seq;

        for (index, seq) in [65_534u16, 65_535, 0, 1, 2].into_iter().enumerate() {
            let step = index as u32 + 1;
            let result = forwarder.forward(&packet(
                PUBLISHER,
                seq,
                1000 + step * STEP,
                now + TICK * step,
            ));
            assert_eq!(*sent(&result).seq, first + u64::from(step));
        }
    }

    #[test]
    fn a_late_packet_from_before_the_wrap_still_maps_below_it() {
        let now = base();
        let mut forwarder = video(3);

        forwarder.forward(&keyframe(PUBLISHER, 65_534, 20_000, now));
        forwarder.forward(&packet(PUBLISHER, 65_535, 20_000 + STEP, now + TICK));
        let after_wrap =
            forwarder.forward(&packet(PUBLISHER, 1, 20_000 + 3 * STEP, now + TICK * 2));
        let after_wrap = *sent(&after_wrap).seq;

        let late = forwarder.forward(&packet(PUBLISHER, 0, 20_000 + 2 * STEP, now + TICK * 3));
        assert_eq!(*sent(&late).seq, after_wrap - 1);

        let older = forwarder.forward(&packet(PUBLISHER, 65_533, 20_000 - STEP, now + TICK * 4));
        assert_eq!(*sent(&older).seq, after_wrap - 4);
    }

    #[test]
    fn out_of_order_input_keeps_its_place_and_duplicates_are_dropped() {
        let now = base();
        let mut forwarder = video(4);

        let start = forwarder.forward(&keyframe(PUBLISHER, 200, 5000, now));
        let first = *sent(&start).seq;

        let third = forwarder.forward(&packet(PUBLISHER, 202, 5000 + 2 * STEP, now + TICK * 2));
        assert_eq!(*sent(&third).seq, first + 2);

        let second = forwarder.forward(&packet(PUBLISHER, 201, 5000 + STEP, now + TICK * 3));
        let second = sent(&second);
        assert_eq!(*second.seq, first + 1);
        assert_eq!(
            second.ext_timestamp,
            sent(&start).ext_timestamp + u64::from(STEP)
        );

        let again = forwarder.forward(&packet(PUBLISHER, 201, 5000 + STEP, now + TICK * 4));
        assert_eq!(again.dropped(), Some(DropReason::Duplicate));

        let repeat_head =
            forwarder.forward(&packet(PUBLISHER, 202, 5000 + 2 * STEP, now + TICK * 5));
        assert_eq!(repeat_head.dropped(), Some(DropReason::Duplicate));

        let ancient = forwarder.forward(&packet(
            PUBLISHER,
            200u16.wrapping_sub(2000),
            1,
            now + TICK * 6,
        ));
        assert_eq!(ancient.dropped(), Some(DropReason::TooOld));

        let stats = forwarder.stats();
        assert_eq!(stats.duplicates, 2);
        assert_eq!(stats.out_of_order, 1);
        assert_eq!(stats.too_old, 1);
    }

    #[test]
    fn a_gap_in_the_publisher_stream_stays_a_gap() {
        let now = base();
        let mut forwarder = video(5);

        let start = forwarder.forward(&keyframe(PUBLISHER, 100, 4000, now));
        let first = *sent(&start).seq;

        let after_gap = forwarder.forward(&packet(PUBLISHER, 105, 4000 + 5 * STEP, now + TICK * 5));
        assert_eq!(*sent(&after_gap).seq, first + 5);

        let next = forwarder.forward(&packet(PUBLISHER, 106, 4000 + 6 * STEP, now + TICK * 6));
        assert_eq!(*sent(&next).seq, first + 6);
        assert_eq!(forwarder.stats().gaps, 1);
        assert_eq!(forwarder.stats().forwarded, 3);
    }

    #[test]
    fn a_padding_only_packet_is_dropped_without_leaving_a_hole() {
        let now = base();
        let mut forwarder = video(6);

        let start = forwarder.forward(&keyframe(PUBLISHER, 300, 8000, now));
        let first = *sent(&start).seq;

        let padding = forwarder.forward(&InboundPacket::new(
            PUBLISHER,
            301,
            8000,
            Vec::new(),
            now + TICK,
        ));
        assert_eq!(padding.dropped(), Some(DropReason::PaddingOnly));

        let next = forwarder.forward(&packet(PUBLISHER, 302, 8000 + STEP, now + TICK * 2));
        assert_eq!(*sent(&next).seq, first + 1);

        let late = forwarder.forward(&packet(PUBLISHER, 301, 8000, now + TICK * 3));
        assert_eq!(late.dropped(), Some(DropReason::Duplicate));

        let following = forwarder.forward(&packet(PUBLISHER, 303, 8000 + 2 * STEP, now + TICK * 4));
        assert_eq!(*sent(&following).seq, first + 2);
        assert_eq!(forwarder.stats().padding, 1);
    }

    #[test]
    fn two_subscribers_of_one_track_own_their_sequence_spaces() {
        let now = base();
        let mut first = video(11);
        let mut second = video(12);

        let opening = keyframe(PUBLISHER, 900, 60_000, now);
        let head = *sent(&first.forward(&opening)).seq;
        let other_head = *sent(&second.forward(&opening)).seq;
        assert_ne!(head, other_head);

        second.set_subscriber_paused(true);

        for step in 1..=4u16 {
            let feed = packet(
                PUBLISHER,
                900 + step,
                60_000 + u32::from(step) * STEP,
                now + TICK * u32::from(step),
            );
            assert_eq!(*sent(&first.forward(&feed)).seq, head + u64::from(step));
            assert_eq!(
                second.forward(&feed).dropped(),
                Some(DropReason::SubscriberPaused)
            );
        }

        second.set_subscriber_paused(false);
        assert!(second.needs_keyframe());

        let resumed = keyframe(PUBLISHER, 905, 60_000 + 5 * STEP, now + TICK * 5);
        assert_eq!(*sent(&first.forward(&resumed)).seq, head + 5);

        let restart = second.forward(&resumed);
        assert_eq!(*sent(&restart).seq, other_head + 1);
        assert_eq!(first.stats().forwarded, 6);
        assert_eq!(second.stats().forwarded, 2);
    }

    #[test]
    fn a_muted_track_resumes_on_a_fresh_keyframe() {
        let now = base();
        let mut forwarder = video(7);

        let start = forwarder.forward(&keyframe(PUBLISHER, 10, 1000, now));
        let head = *sent(&start).seq;
        let head_time = sent(&start).ext_timestamp;

        forwarder.set_publisher_muted(true);
        let muted = forwarder.forward(&packet(PUBLISHER, 11, 1000 + STEP, now + TICK));
        assert_eq!(muted.dropped(), Some(DropReason::PublisherMuted));

        forwarder.set_publisher_muted(false);
        assert!(forwarder.needs_keyframe());

        let ignored = forwarder.forward(&packet(PUBLISHER, 40, 1000 + 30 * STEP, now + TICK * 30));
        assert_eq!(ignored.dropped(), Some(DropReason::AwaitingKeyframe));

        let resumed = forwarder.forward(&keyframe(
            PUBLISHER,
            41,
            1000 + 31 * STEP,
            now + Duration::from_secs(2),
        ));
        let resumed = sent(&resumed);

        assert_eq!(*resumed.seq, head + 1);
        assert_eq!(resumed.ext_timestamp, head_time + 2 * 90_000);
    }

    #[test]
    fn the_marker_bit_and_frame_boundary_follow_the_source() {
        let now = base();
        let mut forwarder = video(8);

        forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
        assert!(!forwarder.on_frame_boundary());

        let end = forwarder.forward(&packet(PUBLISHER, 2, 1000, now + TICK).with_marker(true));
        assert!(sent(&end).marker);
        assert!(forwarder.on_frame_boundary());

        let next = forwarder.forward(&packet(PUBLISHER, 3, 1000 + STEP, now + TICK * 2));
        assert!(!sent(&next).marker);
        assert!(!forwarder.on_frame_boundary());
    }

    #[test]
    fn a_single_layer_track_never_reports_a_layer_switch_after_the_first_packet() {
        let now = base();
        let mut forwarder = video(9);

        let start = forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
        assert!(sent(&start).switched);
        assert_eq!(forwarder.current_layer(), Some(SpatialLayer::BASE));

        for step in 1..5u16 {
            let result = forwarder.forward(
                &packet(
                    PUBLISHER,
                    1 + step,
                    1000 + u32::from(step) * STEP,
                    now + TICK * u32::from(step),
                )
                .with_keyframe(step == 2),
            );
            assert!(!sent(&result).switched);
        }

        assert!(!forwarder.request_layer(SpatialLayer::BASE));
        assert_eq!(forwarder.stats().layer_switches, 0);
    }

    #[test]
    fn packets_on_a_layer_that_was_not_selected_are_dropped() {
        let now = base();
        let mut forwarder = video(10);

        forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));

        let other = forwarder.forward(
            &packet(PUBLISHER, 2, 1000 + STEP, now + TICK).with_layer(SpatialLayer::new(1)),
        );

        assert_eq!(other.dropped(), Some(DropReason::WrongLayer));
        assert_eq!(forwarder.stats().wrong_layer, 1);
    }
}
