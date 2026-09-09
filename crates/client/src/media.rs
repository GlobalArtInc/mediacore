use std::time::Duration;

use str0m::format::Codec as Str0mCodec;

const OPUS_TOC_CELT_FULLBAND_20MS_MONO: u8 = 0x78;
const VP8_DESCRIPTOR_START_OF_PARTITION: u8 = 0x10;
const VP8_KEYFRAME_START_CODE: [u8; 3] = [0x9d, 0x01, 0x2a];

pub const LAYER_TAG_TRAILER: usize = 4;

const LAYER_TAG_MAGIC: [u8; 2] = [0x4c, 0x59];

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
    pub layer_tag: Option<u8>,
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
            layer_tag: None,
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
            layer_tag: None,
        }
    }

    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn with_payload_size(mut self, payload_size: usize) -> Self {
        self.payload_size = payload_size;
        self
    }

    pub fn with_layer_tag(mut self, tag: u8) -> Self {
        self.layer_tag = Some(tag);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticPacket {
    pub payload: Vec<u8>,
    pub marker: bool,
    pub ticks: u32,
}

#[derive(Debug)]
pub struct SyntheticSource {
    profile: MediaProfile,
    index: u64,
    keyframe_requested: bool,
}

impl SyntheticSource {
    pub fn new(profile: MediaProfile) -> Self {
        Self {
            profile,
            index: 0,
            keyframe_requested: true,
        }
    }

    pub fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    pub fn next_packet(&mut self) -> SyntheticPacket {
        let index = self.index;
        self.index += 1;

        match self.profile.codec {
            Codec::Opus => {
                let mut payload = opus_payload(self.profile.payload_size);
                stamp_layer_tag(&mut payload, 1, self.profile.layer_tag);

                SyntheticPacket {
                    payload,
                    marker: index == 0,
                    ticks: self.profile.ticks_per_packet,
                }
            }
            Codec::Vp8 => {
                let periodic = self.profile.keyframe_interval > 0
                    && index.is_multiple_of(self.profile.keyframe_interval);
                let keyframe = periodic || self.keyframe_requested;
                self.keyframe_requested = false;

                let mut payload = vp8_payload(
                    keyframe,
                    self.profile.payload_size,
                    self.profile.width,
                    self.profile.height,
                );
                stamp_layer_tag(
                    &mut payload,
                    vp8_header_len(keyframe),
                    self.profile.layer_tag,
                );

                SyntheticPacket {
                    payload,
                    marker: true,
                    ticks: self.profile.ticks_per_packet,
                }
            }
        }
    }
}

pub fn layer_tag(payload: &[u8]) -> Option<u8> {
    let trailer = payload.len().checked_sub(LAYER_TAG_TRAILER)?;
    let trailer = &payload[trailer..];

    if trailer[..2] != LAYER_TAG_MAGIC || trailer[3] != !trailer[2] {
        return None;
    }

    Some(trailer[2])
}

pub(crate) fn stamp_layer_tag(payload: &mut [u8], protected: usize, tag: Option<u8>) {
    let Some(tag) = tag else {
        return;
    };

    let Some(at) = payload
        .len()
        .checked_sub(LAYER_TAG_TRAILER)
        .filter(|at| *at >= protected)
    else {
        return;
    };

    payload[at] = LAYER_TAG_MAGIC[0];
    payload[at + 1] = LAYER_TAG_MAGIC[1];
    payload[at + 2] = tag;
    payload[at + 3] = !tag;
}

pub(crate) fn vp8_header_len(keyframe: bool) -> usize {
    if keyframe { 11 } else { 4 }
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
    let header = vp8_header_len(keyframe);
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
#[path = "media_tests.rs"]
mod tests;
