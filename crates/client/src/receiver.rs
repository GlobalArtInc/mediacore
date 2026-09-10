use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::media::layer_tag;
use crate::track::TrackKind;

pub const DEFAULT_PAYLOAD_WINDOW: usize = 128;
pub const DEFAULT_SWITCH_HISTORY: usize = 64;
pub const MAX_LAYERS: usize = 32;
pub const MAX_TRACKED_MISSING: usize = 4_096;

/// The gain of RFC 3550's jitter estimator, section 6.4.1.
const JITTER_GAIN: f64 = 1.0 / 16.0;

/// The clock the sender's timestamps are counted in, per kind.
///
/// One codec per kind is negotiated here -- Opus at 48 kHz and VP8 at 90 kHz --
/// so the kind is enough and the payload type does not have to be resolved back
/// through the answer. A track whose kind is not known yet gets no jitter
/// rather than a guessed clock: the estimator subtracts sender time from
/// arrival time, and a wrong clock rate makes that difference nonsense instead
/// of merely imprecise.
fn clock_rate(kind: Option<TrackKind>) -> Option<u32> {
    match kind {
        Some(TrackKind::Audio) => Some(48_000),
        Some(TrackKind::Video) => Some(90_000),
        None => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivedPacket {
    pub sequence: u64,
    pub timestamp: u32,
    pub payload_type: u8,
    pub ssrc: u32,
    pub marker: bool,
    pub rid: Option<String>,
    pub payload: Vec<u8>,
    /// When this packet came off the socket, if whoever built it knew.
    ///
    /// An `Option` that the receiver never fills in itself, which is what keeps
    /// the derived `PartialEq` honest and the tests that compare whole packets
    /// stable. The two alternatives are both worse: an `Instant` stamped inside
    /// `accept` would make every packet differ from every other one and there
    /// would be nothing left to compare, and a hand-written equality that
    /// skipped the field would let two packets that arrived seconds apart
    /// compare equal -- while every timing in [`Timings`] rests on exactly that
    /// difference. So the field is compared like any other, and a caller that
    /// has no clock to offer says so by leaving it `None` and gets counters
    /// without timings.
    pub arrival: Option<Instant>,
}

impl ReceivedPacket {
    pub fn layer_tag(&self) -> Option<u8> {
        layer_tag(&self.payload)
    }

    pub fn layer(&self) -> LayerId {
        match (self.layer_tag(), self.rid.as_deref()) {
            (Some(tag), _) => LayerId::Tag(tag),
            (None, Some(rid)) => LayerId::Rid(rid.to_owned()),
            (None, None) => LayerId::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LayerId {
    Tag(u8),
    Rid(String),
    #[default]
    Unknown,
}

impl std::fmt::Display for LayerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayerId::Tag(tag) => match char::from(*tag) {
                readable if readable.is_ascii_alphanumeric() => write!(f, "tag:{readable}"),
                _ => write!(f, "tag:{tag:#04x}"),
            },
            LayerId::Rid(rid) => write!(f, "rid:{rid}"),
            LayerId::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerReport {
    pub layer: LayerId,
    pub packets: u64,
    pub payload_bytes: u64,
    pub rids: Vec<String>,
    pub ssrcs: Vec<u32>,
    pub sequence: Option<Span<u64>>,
    pub timestamp: Option<Span<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Switch {
    pub from: LayerId,
    pub to: LayerId,
    pub previous_sequence: u64,
    pub previous_timestamp: u32,
    pub sequence: u64,
    pub timestamp: u32,
    /// When the first packet of `to` arrived.
    ///
    /// A report carries the last [`DEFAULT_SWITCH_HISTORY`] switches of the
    /// track's whole life rather than the ones since anybody last looked, so
    /// whoever measures a stretch of a run has to be able to say which of them
    /// happened inside it. A `gap` on its own cannot answer that -- it is a
    /// length, not a moment -- and every reading built out of a track's own
    /// counters is a difference against an earlier one, which a bounded ring of
    /// past switches has no equivalent of. `None` for a switch whose packets
    /// carried no arrival time, which is also when there is no `gap`.
    pub at: Option<Instant>,
    /// Wall-clock between the last packet of `from` and the first of `to`.
    ///
    /// This is the layer-switch latency, and it is the number the whole of M4
    /// was built to move: everything else about a switch says whether the
    /// stream stayed well-formed across it, and this says how long the picture
    /// was frozen. `None` when either side of the change carried no arrival
    /// time.
    pub gap: Option<Duration>,
}

impl Switch {
    pub fn sequence_step(&self) -> i64 {
        self.sequence as i64 - self.previous_sequence as i64
    }

    pub fn timestamp_step(&self) -> i64 {
        self.timestamp.wrapping_sub(self.previous_timestamp) as i32 as i64
    }

    pub fn is_seamless(&self) -> bool {
        self.sequence_step() == 1 && self.timestamp_step() > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span<T> {
    pub first: T,
    pub last: T,
    pub lowest: T,
    pub highest: T,
}

impl<T: Copy + Ord> Span<T> {
    fn new(value: T) -> Self {
        Self {
            first: value,
            last: value,
            lowest: value,
            highest: value,
        }
    }

    fn extend(&mut self, value: T) {
        self.last = value;
        if value < self.lowest {
            self.lowest = value;
        }
        if value > self.highest {
            self.highest = value;
        }
    }
}

/// What the arrival times say once they are subtracted from each other.
///
/// Every field is optional and every one of them is `None` for the same reason:
/// nothing here can be worked out from packets that arrived without a clock.
/// A report whose timings are empty is not a track that behaved badly, it is a
/// track whose packets were handed over by something that had no arrival time
/// to give -- a decoded capture, a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timings {
    /// From the subscription being established to the first packet on it.
    ///
    /// The clock starts where the receiver was opened, which is where this
    /// client first learnt it would be given the track -- the subscriber offer,
    /// or the media section appearing in it. When the first packet is what
    /// created the receiver in the first place, there was nothing earlier to
    /// measure from and this is near zero rather than absent.
    pub first_packet: Option<Duration>,
    pub first_arrival: Option<Instant>,
    pub last_arrival: Option<Instant>,
    /// First arrival to last: the window everything else was counted over.
    pub span: Option<Duration>,
    /// The inter-arrival jitter of RFC 3550, section 6.4.1.
    pub jitter: Option<Duration>,
}

impl Timings {
    /// Packets per second across `span`, for however many packets that was.
    ///
    /// `None` rather than infinity for a track that produced one packet: a
    /// single arrival spans nothing, and a rate over no time is not a slow rate.
    pub fn rate(&self, packets: u64) -> Option<f64> {
        let seconds = self.span?.as_secs_f64();
        (seconds > 0.0).then(|| packets as f64 / seconds)
    }

    /// How long ago the last packet arrived, as of `now`.
    ///
    /// The receiver does not put this in the report itself: a report is a
    /// snapshot of what arrived and holding a field that ages between being
    /// built and being read would be the one number in it that is wrong by the
    /// time anybody looks. Whoever asks names the moment they are asking about.
    pub fn idle(&self, now: Instant) -> Option<Duration> {
        Some(now.saturating_duration_since(self.last_arrival?))
    }
}

/// The inter-arrival jitter of RFC 3550, section 6.4.1.
///
/// D is the difference between how far apart two packets were sent and how far
/// apart they arrived, and the estimate follows its magnitude with a gain of
/// one sixteenth -- the smoothing the RFC prescribes, and the same number every
/// RTCP receiver report on the wire is built with.
///
/// It is carried in seconds rather than in the sender's own clock ticks. The
/// clock rate has to be known either way, because it is what makes the two
/// halves of D comparable at all, and once it is known a duration is what
/// somebody reading a report wants: `3.2ms` says whether a call is smooth and
/// `288` does not say which clock it was counted in.
#[derive(Debug, Default)]
struct Jitter {
    previous: Option<(Instant, u32)>,
    estimate: f64,
    samples: u64,
}

impl Jitter {
    fn accept(&mut self, arrival: Instant, timestamp: u32, clock_rate: Option<u32>) {
        let previous = self.previous.replace((arrival, timestamp));

        let (Some((was_at, was_stamp)), Some(clock_rate)) = (previous, clock_rate) else {
            return;
        };
        if clock_rate == 0 {
            return;
        }

        // Wrapping, then read as a signed step, exactly as `timestamp_step`
        // does: the sender's clock is 32 bits and rolls over inside an ordinary
        // call, and a packet that came late has a step that is genuinely
        // backwards.
        let sent = f64::from(timestamp.wrapping_sub(was_stamp) as i32) / f64::from(clock_rate);
        let transit = elapsed_secs(was_at, arrival) - sent;

        if !transit.is_finite() {
            return;
        }

        self.estimate += (transit.abs() - self.estimate) * JITTER_GAIN;
        self.samples += 1;
    }

    fn estimate(&self) -> Option<Duration> {
        (self.samples > 0).then(|| Duration::from_secs_f64(self.estimate.max(0.0)))
    }
}

/// Seconds from `from` to `to`, negative when they are the wrong way round.
///
/// `Instant::duration_since` saturates to zero there, which for the jitter
/// estimator would turn a packet handed over out of order into one that took no
/// time at all rather than one that arrived early -- and a reorder read as a
/// perfect transit is the one thing the estimator exists to notice.
fn elapsed_secs(from: Instant, to: Instant) -> f64 {
    if to >= from {
        to.duration_since(from).as_secs_f64()
    } else {
        -from.duration_since(to).as_secs_f64()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackReport {
    pub mid: String,
    pub track_sid: String,
    pub kind: Option<TrackKind>,
    pub packets: u64,
    pub payload_bytes: u64,
    pub duplicates: u64,
    pub reordered: u64,
    pub gaps: u64,
    pub missing: u64,
    pub payload_types: Vec<u8>,
    pub ssrcs: Vec<u32>,
    pub rids: Vec<String>,
    pub sequence: Option<Span<u64>>,
    pub timestamp: Option<Span<u32>>,
    pub layers: Vec<LayerReport>,
    pub switches: Vec<Switch>,
    pub switch_count: u64,
    pub timings: Timings,
    pub window: Vec<ReceivedPacket>,
}

impl TrackReport {
    pub fn layer(&self, layer: &LayerId) -> Option<&LayerReport> {
        self.layers.iter().find(|report| report.layer == *layer)
    }

    pub fn layer_ids(&self) -> Vec<LayerId> {
        self.layers
            .iter()
            .map(|report| report.layer.clone())
            .collect()
    }

    pub fn tags(&self) -> Vec<u8> {
        self.layers
            .iter()
            .filter_map(|report| match report.layer {
                LayerId::Tag(tag) => Some(tag),
                _ => None,
            })
            .collect()
    }

    pub fn current_layer(&self) -> Option<LayerId> {
        self.window.last().map(ReceivedPacket::layer)
    }

    pub fn last_switch(&self) -> Option<&Switch> {
        self.switches.last()
    }

    pub fn unique(&self) -> u64 {
        self.packets - self.duplicates
    }

    pub fn expected(&self) -> u64 {
        match self.sequence {
            Some(span) => span.highest - span.lowest + 1,
            None => 0,
        }
    }

    pub fn is_clean(&self) -> bool {
        self.duplicates == 0 && self.reordered == 0 && self.gaps == 0 && self.missing == 0
    }

    pub fn payloads(&self) -> impl Iterator<Item = &[u8]> {
        self.window.iter().map(|packet| packet.payload.as_slice())
    }
}

fn remember<T: Ord>(seen: &mut BTreeSet<T>, value: T) {
    if seen.len() < MAX_LAYERS || seen.contains(&value) {
        seen.insert(value);
    }
}

/// The layer the last packet was on, and where in the streams it sat.
///
/// Kept whole rather than as a tuple because a switch is built out of all four
/// of these at once, and a fourth anonymous field in a tuple is a field nobody
/// can read at the call site.
#[derive(Debug, Clone)]
struct Current {
    layer: LayerId,
    sequence: u64,
    timestamp: u32,
    arrival: Option<Instant>,
}

#[derive(Debug, Default)]
struct LayerState {
    packets: u64,
    payload_bytes: u64,
    rids: BTreeSet<String>,
    ssrcs: BTreeSet<u32>,
    sequence: Option<Span<u64>>,
    timestamp: Option<Span<u32>>,
}

#[derive(Debug)]
pub(crate) struct TrackReceiver {
    mid: String,
    track_sid: String,
    kind: Option<TrackKind>,
    packets: u64,
    payload_bytes: u64,
    duplicates: u64,
    reordered: u64,
    gaps: u64,
    payload_types: BTreeSet<u8>,
    ssrcs: BTreeSet<u32>,
    rids: BTreeSet<String>,
    sequence: Option<Span<u64>>,
    timestamp: Option<Span<u32>>,
    missing: BTreeSet<u64>,
    layers: Vec<(LayerId, LayerState)>,
    current: Option<Current>,
    switches: VecDeque<Switch>,
    switch_count: u64,
    opened: Instant,
    first_arrival: Option<Instant>,
    last_arrival: Option<Instant>,
    jitter: Jitter,
    window: VecDeque<ReceivedPacket>,
    capacity: usize,
}

impl TrackReceiver {
    /// `opened` is when the subscription to this track was established.
    ///
    /// Handed in rather than read off the clock in here, so that a test can be
    /// the clock -- the same reason the room sweep is handed the time -- and so
    /// that a caller replaying a capture can say when the track really opened
    /// rather than when it got round to replaying it.
    pub(crate) fn new(mid: impl Into<String>, capacity: usize, opened: Instant) -> Self {
        Self {
            mid: mid.into(),
            track_sid: String::new(),
            kind: None,
            packets: 0,
            payload_bytes: 0,
            duplicates: 0,
            reordered: 0,
            gaps: 0,
            payload_types: BTreeSet::new(),
            ssrcs: BTreeSet::new(),
            rids: BTreeSet::new(),
            sequence: None,
            timestamp: None,
            missing: BTreeSet::new(),
            layers: Vec::new(),
            current: None,
            switches: VecDeque::new(),
            switch_count: 0,
            opened,
            first_arrival: None,
            last_arrival: None,
            jitter: Jitter::default(),
            window: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn describe(&mut self, track_sid: Option<String>, kind: Option<TrackKind>) {
        if let Some(track_sid) = track_sid.filter(|sid| !sid.is_empty()) {
            self.track_sid = track_sid;
        }
        if kind.is_some() {
            self.kind = kind;
        }
    }

    pub(crate) fn accept(&mut self, packet: ReceivedPacket) {
        self.packets += 1;
        self.payload_bytes += packet.payload.len() as u64;
        self.payload_types.insert(packet.payload_type);
        remember(&mut self.ssrcs, packet.ssrc);
        if let Some(rid) = &packet.rid {
            remember(&mut self.rids, rid.clone());
        }

        if let Some(arrival) = packet.arrival {
            self.first_arrival.get_or_insert(arrival);
            // Assigned rather than kept as a maximum: these come off one socket
            // in the order they were read from it, and the last one read is the
            // last one that arrived even when its sequence number is behind.
            self.last_arrival = Some(arrival);
            self.jitter
                .accept(arrival, packet.timestamp, clock_rate(self.kind));
        }

        self.observe_layer(&packet);

        match self.sequence.as_mut() {
            None => self.sequence = Some(Span::new(packet.sequence)),
            Some(span) => {
                let highest = span.highest;

                if packet.sequence > highest {
                    if packet.sequence > highest + 1 {
                        self.gaps += 1;
                        let room = MAX_TRACKED_MISSING.saturating_sub(self.missing.len());
                        for lost in ((highest + 1)..packet.sequence).take(room) {
                            self.missing.insert(lost);
                        }
                    }
                } else if self.missing.remove(&packet.sequence) {
                    self.reordered += 1;
                } else {
                    self.duplicates += 1;
                }

                span.extend(packet.sequence);
            }
        }

        match self.timestamp.as_mut() {
            None => self.timestamp = Some(Span::new(packet.timestamp)),
            Some(span) => span.extend(packet.timestamp),
        }

        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(packet);
    }

    fn observe_layer(&mut self, packet: &ReceivedPacket) {
        let layer = packet.layer();

        let known = self.layers.iter().position(|(seen, _)| *seen == layer);
        let known = match known {
            Some(index) => Some(index),
            None if self.layers.len() < MAX_LAYERS => {
                self.layers.push((layer.clone(), LayerState::default()));
                Some(self.layers.len() - 1)
            }
            None => None,
        };

        if let Some((_, state)) = known.and_then(|index| self.layers.get_mut(index)) {
            state.packets += 1;
            state.payload_bytes += packet.payload.len() as u64;
            remember(&mut state.ssrcs, packet.ssrc);
            if let Some(rid) = &packet.rid {
                remember(&mut state.rids, rid.clone());
            }
            match state.sequence.as_mut() {
                None => state.sequence = Some(Span::new(packet.sequence)),
                Some(span) => span.extend(packet.sequence),
            }
            match state.timestamp.as_mut() {
                None => state.timestamp = Some(Span::new(packet.timestamp)),
                Some(span) => span.extend(packet.timestamp),
            }
        }

        match self.current.as_ref() {
            Some(current) if current.layer != layer => {
                let switch = Switch {
                    from: current.layer.clone(),
                    to: layer.clone(),
                    previous_sequence: current.sequence,
                    previous_timestamp: current.timestamp,
                    sequence: packet.sequence,
                    timestamp: packet.timestamp,
                    at: packet.arrival,
                    gap: match (current.arrival, packet.arrival) {
                        (Some(before), Some(after)) => {
                            Some(after.saturating_duration_since(before))
                        }
                        _ => None,
                    },
                };

                self.switch_count += 1;
                if self.switches.len() == DEFAULT_SWITCH_HISTORY {
                    self.switches.pop_front();
                }
                self.switches.push_back(switch);
            }
            _ => {}
        }

        self.current = Some(Current {
            layer,
            sequence: packet.sequence,
            timestamp: packet.timestamp,
            arrival: packet.arrival,
        });
    }

    fn timings(&self) -> Timings {
        Timings {
            first_packet: self
                .first_arrival
                .map(|at| at.saturating_duration_since(self.opened)),
            first_arrival: self.first_arrival,
            last_arrival: self.last_arrival,
            span: self
                .first_arrival
                .zip(self.last_arrival)
                .map(|(first, last)| last.saturating_duration_since(first)),
            jitter: self.jitter.estimate(),
        }
    }

    pub(crate) fn report(&self) -> TrackReport {
        TrackReport {
            mid: self.mid.clone(),
            track_sid: self.track_sid.clone(),
            kind: self.kind,
            packets: self.packets,
            payload_bytes: self.payload_bytes,
            duplicates: self.duplicates,
            reordered: self.reordered,
            gaps: self.gaps,
            missing: self.missing.len() as u64,
            payload_types: self.payload_types.iter().copied().collect(),
            ssrcs: self.ssrcs.iter().copied().collect(),
            rids: self.rids.iter().cloned().collect(),
            sequence: self.sequence,
            timestamp: self.timestamp,
            layers: self
                .layers
                .iter()
                .map(|(layer, state)| LayerReport {
                    layer: layer.clone(),
                    packets: state.packets,
                    payload_bytes: state.payload_bytes,
                    rids: state.rids.iter().cloned().collect(),
                    ssrcs: state.ssrcs.iter().copied().collect(),
                    sequence: state.sequence,
                    timestamp: state.timestamp,
                })
                .collect(),
            switches: self.switches.iter().cloned().collect(),
            switch_count: self.switch_count,
            timings: self.timings(),
            window: self.window.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
#[path = "receiver_tests.rs"]
mod tests;
