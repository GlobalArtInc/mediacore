use std::collections::{BTreeSet, VecDeque};

use crate::track::TrackKind;

pub const DEFAULT_PAYLOAD_WINDOW: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedPacket {
    pub sequence: u64,
    pub timestamp: u32,
    pub payload_type: u8,
    pub ssrc: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
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
    pub sequence: Option<Span<u64>>,
    pub timestamp: Option<Span<u32>>,
    pub window: Vec<ReceivedPacket>,
}

impl TrackReport {
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
    sequence: Option<Span<u64>>,
    timestamp: Option<Span<u32>>,
    missing: BTreeSet<u64>,
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
            sequence: None,
            timestamp: None,
            missing: BTreeSet::new(),
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
        self.ssrcs.insert(packet.ssrc);

        match self.sequence.as_mut() {
            None => self.sequence = Some(Span::new(packet.sequence)),
            Some(span) => {
                let highest = span.highest;

                if packet.sequence > highest {
                    if packet.sequence > highest + 1 {
                        self.gaps += 1;
                        for lost in (highest + 1)..packet.sequence {
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
            sequence: self.sequence,
            timestamp: self.timestamp,
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
        });

        let report = receiver.report();
        assert_eq!(report.window[0].payload, vec![0x10, 0x9d, 0x01, 0x2a]);
        assert!(report.window[0].marker);
        assert_eq!(report.payload_types, vec![96]);
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
