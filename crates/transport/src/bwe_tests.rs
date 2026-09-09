use super::*;

fn entry(mid: &str, bytes: usize, packets: usize, oldest: Option<Instant>) -> SendQueueEntry {
    SendQueueEntry {
        mid: Mid::from(mid),
        rid: None,
        bytes,
        packets,
        oldest_unsent: oldest,
        sampled_at: oldest,
    }
}

fn report(queues: Vec<SendQueueEntry>, at: Instant) -> PeerReport {
    PeerReport {
        bwe_tx: None,
        egress_loss: None,
        ingress_loss: None,
        rtt: None,
        bytes_tx: 0,
        bytes_rx: 0,
        peer_bytes_tx: 0,
        peer_bytes_rx: 0,
        queues,
        at,
    }
}

#[test]
fn the_desired_bitrate_is_clamped_into_the_range_str0m_can_probe_for() {
    assert_eq!(clamp_desired(Bitrate::ZERO), MIN_DESIRED_BITRATE);
    assert_eq!(clamp_desired(Bitrate::gbps(1)), MAX_DESIRED_BITRATE);
    assert_eq!(clamp_desired(Bitrate::mbps(2)), Bitrate::mbps(2));

    assert_eq!(
        BweSettings::new(Bitrate::kbps(100)).desired,
        MIN_DESIRED_BITRATE,
        "a desired bitrate of zero makes str0m refuse to ever probe, so it has a floor"
    );
    assert_eq!(
        BweSettings::default().initial,
        DEFAULT_INITIAL_BITRATE,
        "the initial estimate is what the pacer starts at, it cannot be left unset"
    );
}

#[test]
fn the_queue_age_is_the_oldest_packet_still_waiting_anywhere() {
    let now = Instant::now();
    let old = now - Duration::from_millis(300);
    let fresh = now - Duration::from_millis(20);

    let report = report(
        vec![
            entry("video", 4_000, 4, Some(fresh)),
            entry("screen", 12_000, 9, Some(old)),
            entry("audio", 0, 0, None),
        ],
        now,
    );

    assert_eq!(report.queue_bytes(), 16_000);
    assert_eq!(report.queue_packets(), 13);
    assert_eq!(report.oldest_unsent(), Some(old));
    assert_eq!(report.queue_age(now), Duration::from_millis(300));
}

#[test]
fn an_empty_send_queue_has_no_age_at_all() {
    let now = Instant::now();
    let report = report(vec![entry("video", 0, 0, None)], now);

    assert_eq!(report.queue_age(now), Duration::ZERO);
    assert_eq!(report.oldest_unsent(), None);
    assert_eq!(
        SendQueueEntry::empty(Mid::from("video"), None).age(now),
        Duration::ZERO
    );
}

#[test]
fn the_overhead_is_the_gap_between_total_bytes_and_payload_bytes() {
    let now = Instant::now();
    let mut report = report(Vec::new(), now);
    report.peer_bytes_tx = 1_200_000;
    report.bytes_tx = 1_000_000;

    assert_eq!(report.overhead_bytes(), Some(200_000));

    report.bytes_tx = 1_300_000;
    assert_eq!(
        report.overhead_bytes(),
        None,
        "payload bytes above total bytes is nonsense and must not wrap"
    );
}
