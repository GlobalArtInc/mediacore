
use super::*;

fn mid() -> Mid {
    Mid::from("video")
}

#[test]
fn a_negotiated_rid_set_is_read_back_the_way_str0m_holds_it() {
    let low = Rid::from("q");
    let high = Rid::from("f");

    assert_eq!(LayerSet::from(&Rids::None), LayerSet::None);
    assert_eq!(LayerSet::from(&Rids::Any), LayerSet::Any);

    let negotiated = LayerSet::from(&Rids::Specific(vec![low, high]));

    assert!(negotiated.is_specific());
    assert_eq!(negotiated.len(), 2);
    assert!(negotiated.contains(low));
    assert!(!negotiated.contains(Rid::from("h")));
    assert!(!LayerSet::None.contains(low));
    assert!(LayerSet::Any.contains(low));
}

#[test]
fn every_rid_counts_into_its_own_layer() {
    let mut table = LayerTable::default();
    let now = Instant::now();
    let low = Rid::from("q");
    let high = Rid::from("f");

    assert!(table.observe(mid(), Some(low), 1.into(), 100, true, now));
    assert!(!table.observe(mid(), Some(low), 1.into(), 100, false, now));
    assert!(table.observe(mid(), Some(high), 2.into(), 900, false, now));

    let layers = table.snapshot(mid());
    assert_eq!(layers.len(), 2);

    let low = layers
        .iter()
        .find(|layer| layer.rid == Some(low))
        .expect("the low layer is tracked");
    let high = layers
        .iter()
        .find(|layer| layer.rid == Some(high))
        .expect("the high layer is tracked");

    assert_eq!((low.packets, low.bytes, low.keyframes), (2, 200, 1));
    assert_eq!((high.packets, high.bytes, high.keyframes), (1, 900, 0));
    assert_eq!(low.ssrc, Some(1.into()));
    assert_eq!(high.ssrc, Some(2.into()));
}

#[test]
fn a_layer_that_goes_silent_keeps_everything_it_counted() {
    let mut table = LayerTable::default();
    let start = Instant::now();
    let rid = Rid::from("h");

    table.observe(mid(), Some(rid), 7.into(), 500, true, start);

    let silent = start + Duration::from_secs(2);
    table.note_activity(
        LayerActivity {
            mid: mid(),
            rid: Some(rid),
            ssrc: 7.into(),
            active: false,
        },
        silent,
    );

    let layer = table.snapshot(mid()).remove(0);
    assert!(!layer.active);
    assert_eq!(layer.silent_since, Some(silent));
    assert_eq!(layer.packets, 1);
    assert_eq!(layer.keyframes, 1);
    assert_eq!(layer.first_arrival, Some(start));

    let back = silent + Duration::from_secs(1);
    table.observe(mid(), Some(rid), 7.into(), 500, false, back);

    let layer = table.snapshot(mid()).remove(0);
    assert!(layer.active);
    assert_eq!(layer.silent_since, None);
    assert_eq!(layer.packets, 2, "the counters must not restart");
    assert_eq!(layer.first_arrival, Some(start));
    assert_eq!(layer.last_arrival, Some(back));
}

#[test]
fn a_layer_can_be_marked_unwanted_before_it_has_ever_arrived() {
    let mut table = LayerTable::default();
    let rid = Rid::from("f");

    assert!(table.wanted(mid(), Some(rid)));
    assert!(table.snapshot(mid()).is_empty());

    assert!(table.set_wanted(mid(), Some(rid), false));
    assert!(!table.set_wanted(mid(), Some(rid), false));
    assert!(!table.wanted(mid(), Some(rid)));
    assert_eq!(table.snapshot(mid()).len(), 1);

    table.observe(mid(), Some(rid), 3.into(), 10, false, Instant::now());
    assert!(
        !table.wanted(mid(), Some(rid)),
        "arriving packets must not undo what we asked the publisher for"
    );

    assert!(table.set_wanted(mid(), Some(rid), true));
    assert!(table.wanted(mid(), Some(rid)));
}

#[test]
fn keyframe_requests_are_counted_on_the_layer_they_targeted() {
    let mut table = LayerTable::default();
    let low = Rid::from("q");
    let high = Rid::from("f");

    table.note_keyframe_request(mid(), Some(low), KeyframeRequestKind::Pli);
    table.note_keyframe_request(mid(), Some(low), KeyframeRequestKind::Fir);
    table.note_keyframe_request(mid(), Some(high), KeyframeRequestKind::Pli);

    let layers = table.snapshot(mid());
    let low = layers.iter().find(|layer| layer.rid == Some(low)).unwrap();
    let high = layers.iter().find(|layer| layer.rid == Some(high)).unwrap();

    assert_eq!((low.plis, low.firs), (1, 1));
    assert_eq!((high.plis, high.firs), (1, 0));
}

#[test]
fn ingress_reports_land_on_the_layer_they_belong_to() {
    let mut table = LayerTable::default();
    let rid = Rid::from("h");
    let now = Instant::now();

    table.merge_ingress(&IngressReport {
        mid: mid(),
        rid: Some(rid),
        packets: 10,
        bytes: 1000,
        plis: 1,
        firs: 0,
        nacks: 4,
        jitter: 12,
        rtt: Some(Duration::from_millis(30)),
        loss: Some(0.5),
        at: now,
    });

    let layer = table.snapshot(mid()).remove(0);
    assert_eq!(layer.rid, Some(rid));
    assert_eq!(layer.nacks, 4);
    assert_eq!(layer.jitter, Some(12));
    assert_eq!(layer.loss, Some(0.5));
}
