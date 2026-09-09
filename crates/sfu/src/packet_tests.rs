use super::*;
use str0m::media::{Mid, Rid};

#[test]
fn publisher_routing_extensions_never_reach_a_subscriber() {
    let mut ext = ExtensionValues {
        audio_level: Some(-30),
        voice_activity: Some(true),
        ..Default::default()
    };
    ext.mid = Some(Mid::from("2"));
    ext.rid = Some(Rid::from("h"));
    ext.rid_repair = Some(Rid::from("h"));
    ext.transport_cc = Some(17);
    ext.tx_time_offs = Some(5);

    let out = subscriber_extensions(&ext);

    assert_eq!(out.mid, None);
    assert_eq!(out.rid, None);
    assert_eq!(out.rid_repair, None);
    assert_eq!(out.transport_cc, None);
    assert_eq!(out.abs_send_time, None);
    assert_eq!(out.tx_time_offs, None);
    assert_eq!(out.audio_level, Some(-30));
    assert_eq!(out.voice_activity, Some(true));
}
