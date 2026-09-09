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
