use std::time::Duration;

use str0m::format::Codec as Str0mCodec;

const OPUS_TOC_CELT_FULLBAND_20MS_MONO: u8 = 0x78;
const VP8_DESCRIPTOR_START_OF_PARTITION: u8 = 0x10;
const VP8_KEYFRAME_START_CODE: [u8; 3] = [0x9d, 0x01, 0x2a];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Opus,
    Vp8,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Opus => "opus",
            Codec::Vp8 => "vp8",
        }
    }

    pub(crate) fn str0m(self) -> Str0mCodec {
        match self {
            Codec::Opus => Str0mCodec::Opus,
            Codec::Vp8 => Str0mCodec::Vp8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaProfile {
    pub codec: Codec,
    pub packet_interval: Duration,
    pub clock_rate: u32,
    pub ticks_per_packet: u32,
    pub payload_size: usize,
    pub width: u32,
    pub height: u32,
    pub keyframe_interval: u64,
}

impl MediaProfile {
    pub fn opus() -> Self {
        Self {
            codec: Codec::Opus,
            packet_interval: Duration::from_millis(20),
            clock_rate: 48_000,
            ticks_per_packet: 960,
            payload_size: 80,
            width: 0,
            height: 0,
            keyframe_interval: 0,
        }
    }

    pub fn vp8() -> Self {
        Self {
            codec: Codec::Vp8,
            packet_interval: Duration::from_nanos(33_333_333),
            clock_rate: 90_000,
            ticks_per_packet: 3_000,
            payload_size: 900,
            width: 320,
            height: 240,
            keyframe_interval: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyntheticPacket {
    pub payload: Vec<u8>,
    pub marker: bool,
    pub ticks: u32,
}

#[derive(Debug)]
pub(crate) struct SyntheticSource {
    profile: MediaProfile,
    index: u64,
    keyframe_requested: bool,
}

impl SyntheticSource {
    pub(crate) fn new(profile: MediaProfile) -> Self {
        Self {
            profile,
            index: 0,
            keyframe_requested: true,
        }
    }

    pub(crate) fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    pub(crate) fn next(&mut self) -> SyntheticPacket {
        let index = self.index;
        self.index += 1;

        match self.profile.codec {
            Codec::Opus => SyntheticPacket {
                payload: opus_payload(self.profile.payload_size),
                marker: index == 0,
                ticks: self.profile.ticks_per_packet,
            },
            Codec::Vp8 => {
                let periodic = self.profile.keyframe_interval > 0
                    && index.is_multiple_of(self.profile.keyframe_interval);
                let keyframe = periodic || self.keyframe_requested;
                self.keyframe_requested = false;

                SyntheticPacket {
                    payload: vp8_payload(
                        keyframe,
                        self.profile.payload_size,
                        self.profile.width,
                        self.profile.height,
                    ),
                    marker: true,
                    ticks: self.profile.ticks_per_packet,
                }
            }
        }
    }
}

pub(crate) fn opus_payload(size: usize) -> Vec<u8> {
    let size = size.max(1);
    let mut payload = Vec::with_capacity(size);
    payload.push(OPUS_TOC_CELT_FULLBAND_20MS_MONO);
    for i in 1..size {
        payload.push((i % 251) as u8);
    }
    payload
}

pub(crate) fn vp8_payload(keyframe: bool, size: usize, width: u32, height: u32) -> Vec<u8> {
    let header = if keyframe { 11 } else { 4 };
    let size = size.max(header + 1);

    let mut payload = Vec::with_capacity(size);
    payload.push(VP8_DESCRIPTOR_START_OF_PARTITION);

    let first_partition = (size - header) as u32;
    let mut tag = (first_partition & 0x0007_ffff) << 5;
    tag |= 0x10;
    if !keyframe {
        tag |= 0x01;
    }
    payload.extend_from_slice(&tag.to_le_bytes()[..3]);

    if keyframe {
        payload.extend_from_slice(&VP8_KEYFRAME_START_CODE);
        payload.extend_from_slice(&((width as u16) & 0x3fff).to_le_bytes());
        payload.extend_from_slice(&((height as u16) & 0x3fff).to_le_bytes());
    }

    while payload.len() < size {
        payload.push((payload.len() % 251) as u8);
    }
    payload
}

#[cfg(test)]
mod tests {
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
        let first = source.next();
        let second = source.next();

        assert!(first.marker);
        assert!(!second.marker);
        assert_eq!(first.ticks, 960);
        assert_eq!(second.ticks, 960);
    }

    #[test]
    fn video_source_starts_with_a_keyframe_and_repeats_it() {
        let mut source = SyntheticSource::new(MediaProfile::vp8());

        let first = source.next();
        assert_eq!(first.payload[1] & 0x01, 0x00);
        assert!(first.marker);

        for _ in 1..30 {
            let packet = source.next();
            assert_eq!(packet.payload[1] & 0x01, 0x01);
        }

        assert_eq!(source.next().payload[1] & 0x01, 0x00);
    }

    #[test]
    fn a_keyframe_request_is_honoured_on_the_next_packet() {
        let mut source = SyntheticSource::new(MediaProfile::vp8());
        let _ = source.next();
        assert_eq!(source.next().payload[1] & 0x01, 0x01);

        source.request_keyframe();
        assert_eq!(source.next().payload[1] & 0x01, 0x00);
    }
}
