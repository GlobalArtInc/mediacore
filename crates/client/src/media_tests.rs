use super::*;

#[test]
fn opus_payload_starts_with_a_plausible_toc_byte() {
    let payload = opus_payload(80);
    assert_eq!(payload.len(), 80);
    assert_eq!(payload[0], 0x78);
}

#[test]
fn vp8_keyframe_carries_the_start_code_and_dimensions() {
    let payload = vp8_payload(true, 900, 320, 240);
    assert_eq!(payload.len(), 900);
    assert_eq!(payload[0] & 0x10, 0x10);
    assert_eq!(payload[1] & 0x01, 0x00);
    assert_eq!(&payload[4..7], &[0x9d, 0x01, 0x2a]);
    assert_eq!(u16::from_le_bytes([payload[7], payload[8]]) & 0x3fff, 320);
    assert_eq!(u16::from_le_bytes([payload[9], payload[10]]) & 0x3fff, 240);
}

#[test]
fn vp8_interframe_sets_the_frame_type_bit() {
    let payload = vp8_payload(false, 900, 320, 240);
    assert_eq!(payload.len(), 900);
    assert_eq!(payload[1] & 0x01, 0x01);
}

#[test]
fn opus_source_paces_one_packet_per_twenty_milliseconds() {
    let mut source = SyntheticSource::new(MediaProfile::opus());
    let first = source.next_packet();
    let second = source.next_packet();

    assert!(first.marker);
    assert!(!second.marker);
    assert_eq!(first.ticks, 960);
    assert_eq!(second.ticks, 960);
}

#[test]
fn video_source_starts_with_a_keyframe_and_repeats_it() {
    let mut source = SyntheticSource::new(MediaProfile::vp8());

    let first = source.next_packet();
    assert_eq!(first.payload[1] & 0x01, 0x00);
    assert!(first.marker);

    for _ in 1..30 {
        let packet = source.next_packet();
        assert_eq!(packet.payload[1] & 0x01, 0x01);
    }

    assert_eq!(source.next_packet().payload[1] & 0x01, 0x00);
}

#[test]
fn an_untagged_profile_leaves_the_payload_unmarked() {
    let mut source = SyntheticSource::new(MediaProfile::vp8());
    assert_eq!(layer_tag(&source.next_packet().payload), None);
}

#[test]
fn a_tagged_profile_stamps_every_packet_keyframe_or_not() {
    let mut source = SyntheticSource::new(MediaProfile::vp8().with_layer_tag(b'h'));

    for _ in 0..40 {
        let packet = source.next_packet();
        assert_eq!(layer_tag(&packet.payload), Some(b'h'));
    }
}

#[test]
fn the_tag_does_not_disturb_the_vp8_header() {
    let mut source = SyntheticSource::new(MediaProfile::vp8().with_layer_tag(0xff));

    let keyframe = source.next_packet();
    assert_eq!(keyframe.payload[1] & 0x01, 0x00);
    assert_eq!(&keyframe.payload[4..7], &[0x9d, 0x01, 0x2a]);

    let interframe = source.next_packet();
    assert_eq!(interframe.payload[1] & 0x01, 0x01);
    assert_eq!(layer_tag(&interframe.payload), Some(0xff));
}

#[test]
fn two_layers_are_told_apart_by_their_tag() {
    let mut low = SyntheticSource::new(MediaProfile::vp8().with_layer_tag(b'q'));
    let mut high = SyntheticSource::new(MediaProfile::vp8().with_layer_tag(b'f'));

    assert_eq!(layer_tag(&low.next_packet().payload), Some(b'q'));
    assert_eq!(layer_tag(&high.next_packet().payload), Some(b'f'));
}

#[test]
fn a_payload_too_short_to_hold_a_tag_is_left_alone() {
    let mut payload = vec![0x10, 0x00, 0x00, 0x00, 0x00];
    stamp_layer_tag(&mut payload, 4, Some(b'q'));

    assert_eq!(payload, vec![0x10, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(layer_tag(&payload), None);
}

#[test]
fn a_payload_that_only_looks_tagged_is_not_read_as_one() {
    assert_eq!(layer_tag(&[0x4c, 0x59, 0x10, 0x10]), None);
    assert_eq!(layer_tag(&[0x00, 0x00, 0x10, 0xef]), None);
    assert_eq!(layer_tag(&[0x59]), None);
}

#[test]
fn an_audio_profile_can_carry_a_tag_too() {
    let mut source = SyntheticSource::new(MediaProfile::opus().with_layer_tag(7));
    let packet = source.next_packet();

    assert_eq!(packet.payload[0], 0x78);
    assert_eq!(layer_tag(&packet.payload), Some(7));
}

#[test]
fn a_keyframe_request_is_honoured_on_the_next_packet() {
    let mut source = SyntheticSource::new(MediaProfile::vp8());
    let _ = source.next_packet();
    assert_eq!(source.next_packet().payload[1] & 0x01, 0x01);

    source.request_keyframe();
    assert_eq!(source.next_packet().payload[1] & 0x01, 0x00);
}
