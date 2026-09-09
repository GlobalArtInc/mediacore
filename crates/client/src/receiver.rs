use std::collections::{BTreeSet, VecDeque};

use crate::media::layer_tag;
use crate::track::TrackKind;

pub const DEFAULT_PAYLOAD_WINDOW: usize = 128;
pub const DEFAULT_SWITCH_HISTORY: usize = 64;
pub const MAX_LAYERS: usize = 32;
pub const MAX_TRACKED_MISSING: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivedPacket {
    pub sequence: u64,
    pub timestamp: u32,
    pub payload_type: u8,
    pub ssrc: u32,
    pub marker: bool,
    pub rid: Option<String>,
    pub payload: Vec<u8>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    pub from: LayerId,
    pub to: LayerId,
    pub previous_sequence: u64,
    pub previous_timestamp: u32,
    pub sequence: u64,
    pub timestamp: u32,
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
    current: Option<(LayerId, u64, u32)>,
    switches: VecDeque<Switch>,
    switch_count: u64,
    window: VecDeque<ReceivedPacket>,
    capacity: usize,
}

impl TrackReceiver {
    pub(crate) fn new(mid: impl Into<String>, capacity: usize) -> Self {
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
            Some((current, sequence, timestamp)) if *current != layer => {
                let switch = Switch {
                    from: current.clone(),
                    to: layer.clone(),
                    previous_sequence: *sequence,
                    previous_timestamp: *timestamp,
                    sequence: packet.sequence,
                    timestamp: packet.timestamp,
                };

                self.switch_count += 1;
                if self.switches.len() == DEFAULT_SWITCH_HISTORY {
                    self.switches.pop_front();
                }
                self.switches.push_back(switch);
            }
            _ => {}
        }

        self.current = Some((layer, packet.sequence, packet.timestamp));
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
            window: self.window.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
#[path = "receiver_tests.rs"]
mod tests;
