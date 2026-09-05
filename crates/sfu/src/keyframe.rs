#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp8,
    Vp9,
    H264,
    Av1,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyframeError {
    #[error("`{0}` is not a video codec this server can gate on")]
    UnsupportedCodec(String),
    #[error("{codec:?} payload is shorter than its descriptor")]
    Truncated { codec: VideoCodec },
}

impl VideoCodec {
    pub fn from_mime(mime: &str) -> Result<Self, KeyframeError> {
        let name = mime.rsplit('/').next().unwrap_or(mime);

        if name.eq_ignore_ascii_case("vp8") {
            Ok(Self::Vp8)
        } else if name.eq_ignore_ascii_case("vp9") {
            Ok(Self::Vp9)
        } else if name.eq_ignore_ascii_case("h264") {
            Ok(Self::H264)
        } else if name.eq_ignore_ascii_case("av1") || name.eq_ignore_ascii_case("av1x") {
            Ok(Self::Av1)
        } else {
            Err(KeyframeError::UnsupportedCodec(mime.to_owned()))
        }
    }
}

pub fn is_keyframe(codec: VideoCodec, payload: &[u8]) -> Result<bool, KeyframeError> {
    if payload.is_empty() {
        return Ok(false);
    }

    match codec {
        VideoCodec::Vp8 => vp8(payload),
        VideoCodec::Vp9 => vp9(payload),
        VideoCodec::H264 => h264(payload),
        VideoCodec::Av1 => av1(payload),
    }
}

fn truncated(codec: VideoCodec) -> KeyframeError {
    KeyframeError::Truncated { codec }
}

fn vp8(payload: &[u8]) -> Result<bool, KeyframeError> {
    let descriptor = payload[0];
    let start_of_partition = descriptor & 0x10 != 0;
    let partition_index = descriptor & 0x07;
    let mut offset = 1usize;

    if descriptor & 0x80 != 0 {
        let extended = *payload
            .get(offset)
            .ok_or_else(|| truncated(VideoCodec::Vp8))?;
        offset += 1;

        if extended & 0x80 != 0 {
            let first = *payload
                .get(offset)
                .ok_or_else(|| truncated(VideoCodec::Vp8))?;
            offset += if first & 0x80 != 0 { 2 } else { 1 };
        }
        if extended & 0x40 != 0 {
            offset += 1;
        }
        if extended & 0x30 != 0 {
            offset += 1;
        }
    }

    if !start_of_partition || partition_index != 0 {
        return Ok(false);
    }

    let header = *payload
        .get(offset)
        .ok_or_else(|| truncated(VideoCodec::Vp8))?;
    Ok(header & 0x01 == 0)
}

fn vp9(payload: &[u8]) -> Result<bool, KeyframeError> {
    let descriptor = payload[0];
    let inter_picture_predicted = descriptor & 0x40 != 0;
    let start_of_frame = descriptor & 0x08 != 0;

    Ok(!inter_picture_predicted && start_of_frame)
}

fn h264(payload: &[u8]) -> Result<bool, KeyframeError> {
    const SINGLE_NALU_MAX: u8 = 23;
    const STAP_A: u8 = 24;
    const FU_A: u8 = 28;

    let unit = payload[0] & 0x1f;

    if unit > 0 && unit <= SINGLE_NALU_MAX {
        return Ok(is_h264_key_unit(unit));
    }

    if unit == STAP_A {
        let mut offset = 1usize;

        while offset + 2 <= payload.len() {
            let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
            offset += 2;

            let unit = *payload
                .get(offset)
                .ok_or_else(|| truncated(VideoCodec::H264))?
                & 0x1f;
            if is_h264_key_unit(unit) {
                return Ok(true);
            }

            offset = offset.saturating_add(size);
        }

        return Ok(false);
    }

    if unit == FU_A {
        let header = *payload.get(1).ok_or_else(|| truncated(VideoCodec::H264))?;
        let start = header & 0x80 != 0;

        return Ok(start && is_h264_key_unit(header & 0x1f));
    }

    Ok(false)
}

fn is_h264_key_unit(unit: u8) -> bool {
    matches!(unit, 5 | 7 | 8)
}

fn av1(payload: &[u8]) -> Result<bool, KeyframeError> {
    let aggregation = payload[0];
    let continues_previous = aggregation & 0x80 != 0;
    let new_coded_video_sequence = aggregation & 0x08 != 0;

    Ok(!continues_previous && new_coded_video_sequence)
}

#[cfg(test)]
mod tests {
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
}
