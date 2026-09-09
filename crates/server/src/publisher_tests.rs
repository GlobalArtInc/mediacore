
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
