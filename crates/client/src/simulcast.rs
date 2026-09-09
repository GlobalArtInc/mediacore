use mediacore_proto::mediacore;

use crate::error::ClientError;
use crate::media::MediaProfile;

pub const RID_QUARTER: &str = "q";
pub const RID_HALF: &str = "h";
pub const RID_FULL: &str = "f";

pub const MAX_RID_LEN: usize = 8;

const FRAMES_PER_SECOND: u32 = 30;
const MIN_PAYLOAD: usize = 120;
const MAX_PAYLOAD: usize = 1_100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerSpec {
    pub rid: String,
    pub quality: mediacore::VideoQuality,
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub profile: MediaProfile,
}

impl LayerSpec {
    pub fn new(
        rid: impl Into<String>,
        quality: mediacore::VideoQuality,
        width: u32,
        height: u32,
        bitrate: u32,
    ) -> Self {
        let rid = rid.into();
        let profile = MediaProfile::vp8()
            .with_size(width, height)
            .with_payload_size(payload_size_for(bitrate))
            .with_layer_tag(tag_for(&rid));

        Self {
            rid,
            quality,
            width,
            height,
            bitrate,
            profile,
        }
    }

    pub fn quarter() -> Self {
        Self::new(RID_QUARTER, mediacore::VideoQuality::Low, 320, 180, 150_000)
    }

    pub fn half() -> Self {
        Self::new(RID_HALF, mediacore::VideoQuality::Medium, 640, 360, 500_000)
    }

    pub fn full() -> Self {
        Self::new(
            RID_FULL,
            mediacore::VideoQuality::High,
            1_280,
            720,
            1_700_000,
        )
    }

    pub fn with_profile(mut self, profile: MediaProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn tag(&self) -> Option<u8> {
        self.profile.layer_tag
    }

    pub fn to_video_layer(&self) -> mediacore::VideoLayer {
        mediacore::VideoLayer {
            quality: self.quality as i32,
            width: self.width,
            height: self.height,
            bitrate: self.bitrate,
            rid: self.rid.clone(),
            ..mediacore::VideoLayer::default()
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ClientError> {
        let usable = !self.rid.is_empty()
            && self.rid.len() <= MAX_RID_LEN
            && self.rid.chars().all(|c| c.is_ascii_alphanumeric());

        if usable {
            Ok(())
        } else {
            Err(ClientError::InvalidRid(self.rid.clone()))
        }
    }
}

pub fn default_layers() -> Vec<LayerSpec> {
    vec![LayerSpec::quarter(), LayerSpec::half(), LayerSpec::full()]
}

pub fn validate_layers(layers: &[LayerSpec]) -> Result<(), ClientError> {
    for (index, layer) in layers.iter().enumerate() {
        layer.validate()?;

        if layers[..index].iter().any(|seen| seen.rid == layer.rid) {
            return Err(ClientError::DuplicateRid(layer.rid.clone()));
        }
    }

    Ok(())
}

fn payload_size_for(bitrate: u32) -> usize {
    let bytes_per_frame = bitrate as usize / 8 / FRAMES_PER_SECOND as usize;
    bytes_per_frame.clamp(MIN_PAYLOAD, MAX_PAYLOAD)
}

fn tag_for(rid: &str) -> u8 {
    rid.as_bytes().first().copied().unwrap_or(b'?')
}

#[cfg(test)]
#[path = "simulcast_tests.rs"]
mod tests;
