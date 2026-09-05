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
mod tests {
    use super::*;

    fn packet(sequence: u64, timestamp: u32) -> ReceivedPacket {
        ReceivedPacket {
            sequence,
            timestamp,
            payload_type: 111,
            ssrc: 0xdead_beef,
            marker: sequence == 0,
            payload: vec![(sequence % 251) as u8; 4],
            ..ReceivedPacket::default()
        }
    }

    fn tagged(sequence: u64, timestamp: u32, tag: u8) -> ReceivedPacket {
        let mut payload = vec![0x10_u8; 32];
        crate::media::stamp_layer_tag(&mut payload, 4, Some(tag));

        ReceivedPacket {
            sequence,
            timestamp,
            payload_type: 96,
            ssrc: 0x1234_5678,
            marker: true,
            payload,
            ..ReceivedPacket::default()
        }
    }

    fn feed(sequences: &[u64]) -> TrackReport {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        for sequence in sequences {
            receiver.accept(packet(*sequence, (*sequence as u32) * 960));
        }
        receiver.report()
    }

    #[test]
    fn a_contiguous_run_reports_nothing_unusual() {
        let report = feed(&[0, 1, 2, 3, 4]);

        assert_eq!(report.packets, 5);
        assert_eq!(report.payload_bytes, 20);
        assert_eq!(report.unique(), 5);
        assert_eq!(report.expected(), 5);
        assert!(report.is_clean());
        assert_eq!(report.payload_types, vec![111]);
        assert_eq!(report.ssrcs, vec![0xdead_beef]);
    }

    #[test]
    fn the_sequence_and_timestamp_spans_cover_what_arrived() {
        let report = feed(&[10, 11, 12]);

        let sequence = report.sequence.expect("a sequence span");
        assert_eq!(sequence.first, 10);
        assert_eq!(sequence.last, 12);
        assert_eq!(sequence.lowest, 10);
        assert_eq!(sequence.highest, 12);

        let timestamp = report.timestamp.expect("a timestamp span");
        assert_eq!(timestamp.first, 9_600);
        assert_eq!(timestamp.last, 11_520);
    }

    #[test]
    fn a_hole_in_the_sequence_is_reported_as_a_gap() {
        let report = feed(&[0, 1, 4, 5]);

        assert_eq!(report.gaps, 1);
        assert_eq!(report.missing, 2);
        assert_eq!(report.duplicates, 0);
        assert_eq!(report.reordered, 0);
        assert_eq!(report.expected(), 6);
        assert_eq!(report.unique(), 4);
    }

    #[test]
    fn a_repeated_sequence_number_is_a_duplicate() {
        let report = feed(&[0, 1, 1, 2]);

        assert_eq!(report.duplicates, 1);
        assert_eq!(report.gaps, 0);
        assert_eq!(report.reordered, 0);
        assert_eq!(report.packets, 4);
        assert_eq!(report.unique(), 3);
    }

    #[test]
    fn a_late_packet_fills_its_hole_and_counts_as_a_reorder() {
        let report = feed(&[0, 1, 3, 2, 4]);

        assert_eq!(report.gaps, 1);
        assert_eq!(report.reordered, 1);
        assert_eq!(report.missing, 0);
        assert_eq!(report.duplicates, 0);
        assert_eq!(report.unique(), 5);
        assert_eq!(report.expected(), 5);
    }

    #[test]
    fn a_gap_a_duplicate_and_a_reorder_are_told_apart_in_one_run() {
        let report = feed(&[0, 1, 2, 5, 4, 4, 6, 7]);

        assert_eq!(report.packets, 8);
        assert_eq!(report.gaps, 1);
        assert_eq!(report.reordered, 1);
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.missing, 1);
        assert_eq!(report.unique(), 7);
        assert_eq!(report.expected(), 8);
        assert!(!report.is_clean());
    }

    #[test]
    fn the_payload_window_keeps_the_most_recent_packets() {
        let mut receiver = TrackReceiver::new("0", 3);
        for sequence in 0..10 {
            receiver.accept(packet(sequence, sequence as u32));
        }

        let report = receiver.report();
        assert_eq!(report.window.len(), 3);
        assert_eq!(
            report
                .window
                .iter()
                .map(|packet| packet.sequence)
                .collect::<Vec<_>>(),
            vec![7, 8, 9]
        );
        assert_eq!(report.packets, 10);
        assert_eq!(report.payloads().count(), 3);
    }

    #[test]
    fn payloads_are_kept_byte_for_byte() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        receiver.accept(ReceivedPacket {
            sequence: 0,
            timestamp: 0,
            payload_type: 96,
            ssrc: 1,
            marker: true,
            payload: vec![0x10, 0x9d, 0x01, 0x2a],
            ..ReceivedPacket::default()
        });

        let report = receiver.report();
        assert_eq!(report.window[0].payload, vec![0x10, 0x9d, 0x01, 0x2a]);
        assert!(report.window[0].marker);
        assert_eq!(report.payload_types, vec![96]);
    }

    #[test]
    fn an_untagged_stream_is_one_layer_that_never_switches() {
        let report = feed(&[0, 1, 2]);

        assert_eq!(report.layer_ids(), vec![LayerId::Unknown]);
        assert_eq!(report.switch_count, 0);
        assert!(report.switches.is_empty());
        assert_eq!(report.layers[0].packets, 3);
    }

    #[test]
    fn packets_are_split_per_layer_by_their_tag() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        for sequence in 0..4 {
            receiver.accept(tagged(sequence, sequence as u32 * 3_000, b'q'));
        }
        for sequence in 4..10 {
            receiver.accept(tagged(sequence, sequence as u32 * 3_000, b'f'));
        }

        let report = receiver.report();
        assert_eq!(report.packets, 10);
        assert_eq!(report.tags(), vec![b'q', b'f']);

        let low = report.layer(&LayerId::Tag(b'q')).expect("the low layer");
        assert_eq!(low.packets, 4);
        assert_eq!(low.sequence.expect("a span").last, 3);

        let high = report.layer(&LayerId::Tag(b'f')).expect("the high layer");
        assert_eq!(high.packets, 6);
        assert_eq!(high.sequence.expect("a span").first, 4);
        assert_eq!(high.timestamp.expect("a span").first, 12_000);
    }

    #[test]
    fn a_change_of_layer_is_reported_as_a_switch() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        for sequence in 0..3 {
            receiver.accept(tagged(sequence, sequence as u32 * 3_000, b'q'));
        }
        receiver.accept(tagged(3, 9_000, b'f'));

        let report = receiver.report();
        assert_eq!(report.switch_count, 1);
        assert_eq!(report.current_layer(), Some(LayerId::Tag(b'f')));

        let switch = report.last_switch().expect("a switch");
        assert_eq!(switch.from, LayerId::Tag(b'q'));
        assert_eq!(switch.to, LayerId::Tag(b'f'));
        assert_eq!(switch.previous_sequence, 2);
        assert_eq!(switch.sequence, 3);
        assert_eq!(switch.sequence_step(), 1);
        assert_eq!(switch.timestamp_step(), 3_000);
        assert!(switch.is_seamless());
    }

    #[test]
    fn a_switch_that_breaks_the_sequence_is_not_seamless() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        receiver.accept(tagged(0, 0, b'q'));
        receiver.accept(tagged(9, 90_000, b'f'));

        let switch = receiver.report().last_switch().cloned().expect("a switch");
        assert_eq!(switch.sequence_step(), 9);
        assert!(!switch.is_seamless());
    }

    #[test]
    fn switching_back_and_forth_counts_every_move() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        for (index, tag) in (*b"qffqh").into_iter().enumerate() {
            receiver.accept(tagged(index as u64, index as u32 * 3_000, tag));
        }

        let report = receiver.report();
        assert_eq!(report.switch_count, 3);
        assert_eq!(report.switches.len(), 3);
        assert_eq!(report.tags(), vec![b'q', b'f', b'h']);
        assert_eq!(
            report.layer(&LayerId::Tag(b'q')).map(|layer| layer.packets),
            Some(2)
        );
    }

    #[test]
    fn the_switch_history_keeps_the_most_recent_moves() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        let total = DEFAULT_SWITCH_HISTORY as u64 + 10;

        for sequence in 0..=total {
            let tag = if sequence % 2 == 0 { b'q' } else { b'f' };
            receiver.accept(tagged(sequence, sequence as u32, tag));
        }

        let report = receiver.report();
        assert_eq!(report.switch_count, total);
        assert_eq!(report.switches.len(), DEFAULT_SWITCH_HISTORY);
        assert_eq!(report.switches[0].previous_sequence, total - 64);
    }

    #[test]
    fn a_rid_names_the_layer_when_the_payload_carries_no_tag() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);

        receiver.accept(ReceivedPacket {
            sequence: 0,
            rid: Some("q".to_owned()),
            payload: vec![0x10; 8],
            ..ReceivedPacket::default()
        });
        receiver.accept(ReceivedPacket {
            sequence: 1,
            rid: Some("f".to_owned()),
            payload: vec![0x10; 8],
            ..ReceivedPacket::default()
        });

        let report = receiver.report();
        assert_eq!(report.rids, vec!["f".to_owned(), "q".to_owned()]);
        assert_eq!(
            report.layer_ids(),
            vec![LayerId::Rid("q".to_owned()), LayerId::Rid("f".to_owned())]
        );
        assert_eq!(report.switch_count, 1);
    }

    #[test]
    fn a_tag_outranks_a_rid_that_the_forwarder_stops_sending() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);

        let mut first = tagged(0, 0, b'f');
        first.rid = Some("f".to_owned());
        receiver.accept(first);
        receiver.accept(tagged(1, 3_000, b'f'));

        let report = receiver.report();
        assert_eq!(report.layer_ids(), vec![LayerId::Tag(b'f')]);
        assert_eq!(report.switch_count, 0);
        assert_eq!(report.layers[0].rids, vec!["f".to_owned()]);
    }

    #[test]
    fn a_wild_jump_in_the_sequence_does_not_run_away() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);
        receiver.accept(packet(0, 0));
        receiver.accept(packet(u32::MAX as u64 * 4, 0));

        let report = receiver.report();
        assert_eq!(report.gaps, 1);
        assert_eq!(report.missing, MAX_TRACKED_MISSING as u64);
        assert_eq!(report.packets, 2);
    }

    #[test]
    fn a_peer_inventing_rids_and_ssrcs_cannot_grow_the_report_forever() {
        let mut receiver = TrackReceiver::new("0", DEFAULT_PAYLOAD_WINDOW);

        for sequence in 0..(MAX_LAYERS as u64 * 4) {
            receiver.accept(ReceivedPacket {
                sequence,
                ssrc: sequence as u32,
                rid: Some(format!("r{sequence}")),
                payload: vec![0x10; 8],
                ..ReceivedPacket::default()
            });
        }

        let report = receiver.report();
        assert_eq!(report.ssrcs.len(), MAX_LAYERS);
        assert_eq!(report.rids.len(), MAX_LAYERS);
        assert_eq!(report.layers.len(), MAX_LAYERS);
        assert_eq!(report.packets, MAX_LAYERS as u64 * 4);
    }

    #[test]
    fn a_layer_id_reads_back_in_a_log_line() {
        assert_eq!(LayerId::Tag(b'q').to_string(), "tag:q");
        assert_eq!(LayerId::Tag(0x01).to_string(), "tag:0x01");
        assert_eq!(LayerId::Rid("h".to_owned()).to_string(), "rid:h");
        assert_eq!(LayerId::Unknown.to_string(), "unknown");
    }

    #[test]
    fn a_track_learns_its_sid_and_kind_once() {
        let mut receiver = TrackReceiver::new("1", DEFAULT_PAYLOAD_WINDOW);
        receiver.describe(Some("TR_one".to_owned()), Some(TrackKind::Video));
        receiver.describe(Some(String::new()), None);

        let report = receiver.report();
        assert_eq!(report.track_sid, "TR_one");
        assert_eq!(report.kind, Some(TrackKind::Video));
        assert_eq!(report.mid, "1");
    }
}
