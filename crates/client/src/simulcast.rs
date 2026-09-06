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
mod tests {
    use super::*;

    #[test]
    fn the_default_ladder_climbs_from_a_quarter_to_a_full_frame() {
        let layers = default_layers();

        assert_eq!(layers.len(), 3);
        assert_eq!(
            layers.iter().map(|l| l.rid.as_str()).collect::<Vec<_>>(),
            vec!["q", "h", "f"]
        );
        assert_eq!(
            layers.iter().map(|l| l.quality).collect::<Vec<_>>(),
            vec![
                mediacore::VideoQuality::Low,
                mediacore::VideoQuality::Medium,
                mediacore::VideoQuality::High
            ]
        );
        assert_eq!(layers[0].height, 180);
        assert_eq!(layers[2].width, 1_280);
    }

    #[test]
    fn every_default_layer_carries_its_own_tag_and_size() {
        let layers = default_layers();

        assert_eq!(layers[0].tag(), Some(b'q'));
        assert_eq!(layers[1].tag(), Some(b'h'));
        assert_eq!(layers[2].tag(), Some(b'f'));

        assert_eq!(layers[0].profile.width, 320);
        assert_eq!(layers[0].profile.height, 180);
        assert!(layers[0].profile.payload_size < layers[1].profile.payload_size);
    }

    #[test]
    fn a_layer_becomes_the_video_layer_the_server_reads() {
        let layer = LayerSpec::half().to_video_layer();

        assert_eq!(layer.quality, mediacore::VideoQuality::Medium as i32);
        assert_eq!(layer.width, 640);
        assert_eq!(layer.height, 360);
        assert_eq!(layer.bitrate, 500_000);
        assert_eq!(layer.rid, "h");
    }

    #[test]
    fn a_payload_never_grows_past_a_single_packet() {
        assert_eq!(payload_size_for(1_700_000), MAX_PAYLOAD);
        assert_eq!(payload_size_for(1_000), MIN_PAYLOAD);
        assert_eq!(payload_size_for(150_000), 625);
    }

    #[test]
    fn a_rid_str0m_would_silently_mangle_is_rejected() {
        assert!(matches!(
            LayerSpec::new("", mediacore::VideoQuality::Low, 320, 180, 1).validate(),
            Err(ClientError::InvalidRid(_))
        ));
        assert!(matches!(
            LayerSpec::new("far-too-long", mediacore::VideoQuality::Low, 320, 180, 1).validate(),
            Err(ClientError::InvalidRid(_))
        ));
        assert!(matches!(
            LayerSpec::new("a b", mediacore::VideoQuality::Low, 320, 180, 1).validate(),
            Err(ClientError::InvalidRid(_))
        ));
        assert!(
            LayerSpec::new("hi720", mediacore::VideoQuality::High, 1, 1, 1)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn the_same_rid_twice_on_one_track_is_refused() {
        let layers = vec![LayerSpec::quarter(), LayerSpec::quarter()];

        assert!(matches!(
            validate_layers(&layers),
            Err(ClientError::DuplicateRid(rid)) if rid == "q"
        ));
        assert!(validate_layers(&default_layers()).is_ok());
    }
}
