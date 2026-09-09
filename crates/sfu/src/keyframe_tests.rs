
use super::*;

#[test]
fn mime_types_map_to_codecs() {
    assert_eq!(VideoCodec::from_mime("video/VP8"), Ok(VideoCodec::Vp8));
    assert_eq!(VideoCodec::from_mime("vp9"), Ok(VideoCodec::Vp9));
    assert_eq!(VideoCodec::from_mime("video/H264"), Ok(VideoCodec::H264));
    assert_eq!(VideoCodec::from_mime("video/AV1"), Ok(VideoCodec::Av1));
    assert!(VideoCodec::from_mime("audio/opus").is_err());
}

#[test]
fn an_empty_payload_is_never_a_keyframe() {
    assert_eq!(is_keyframe(VideoCodec::Vp8, &[]), Ok(false));
    assert_eq!(is_keyframe(VideoCodec::H264, &[]), Ok(false));
}

#[test]
fn vp8_reads_the_frame_type_after_the_descriptor() {
    assert_eq!(is_keyframe(VideoCodec::Vp8, &[0x10, 0x00]), Ok(true));
    assert_eq!(is_keyframe(VideoCodec::Vp8, &[0x10, 0x01]), Ok(false));
    assert_eq!(
        is_keyframe(VideoCodec::Vp8, &[0x90, 0x80, 0x81, 0x23, 0x00]),
        Ok(true)
    );
    assert_eq!(is_keyframe(VideoCodec::Vp8, &[0x00, 0x00]), Ok(false));
    assert_eq!(
        is_keyframe(VideoCodec::Vp8, &[0x90, 0x80]),
        Err(KeyframeError::Truncated {
            codec: VideoCodec::Vp8
        })
    );
}

#[test]
fn vp9_needs_a_frame_start_without_prediction() {
    assert_eq!(is_keyframe(VideoCodec::Vp9, &[0x08]), Ok(true));
    assert_eq!(is_keyframe(VideoCodec::Vp9, &[0x48]), Ok(false));
    assert_eq!(is_keyframe(VideoCodec::Vp9, &[0x04]), Ok(false));
}

#[test]
fn h264_recognises_idr_in_every_packetisation() {
    assert_eq!(is_keyframe(VideoCodec::H264, &[0x65, 0x88]), Ok(true));
    assert_eq!(is_keyframe(VideoCodec::H264, &[0x41, 0x88]), Ok(false));
    assert_eq!(
        is_keyframe(VideoCodec::H264, &[0x78, 0x00, 0x02, 0x67, 0x42]),
        Ok(true)
    );
    assert_eq!(
        is_keyframe(VideoCodec::H264, &[0x78, 0x00, 0x02, 0x41, 0x42]),
        Ok(false)
    );
    assert_eq!(is_keyframe(VideoCodec::H264, &[0x7c, 0x85]), Ok(true));
    assert_eq!(is_keyframe(VideoCodec::H264, &[0x7c, 0x05]), Ok(false));
    assert_eq!(
        is_keyframe(VideoCodec::H264, &[0x7c]),
        Err(KeyframeError::Truncated {
            codec: VideoCodec::H264
        })
    );
}

#[test]
fn av1_starts_a_coded_video_sequence() {
    assert_eq!(is_keyframe(VideoCodec::Av1, &[0x08]), Ok(true));
    assert_eq!(is_keyframe(VideoCodec::Av1, &[0x88]), Ok(false));
    assert_eq!(is_keyframe(VideoCodec::Av1, &[0x10]), Ok(false));
}
