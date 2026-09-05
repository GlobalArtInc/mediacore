use mediacore_proto::livekit;
use str0m::media::MediaKind;

use crate::media::MediaProfile;
use crate::simulcast::{LayerSpec, default_layers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
}

impl TrackKind {
    pub(crate) fn media_kind(self) -> MediaKind {
        match self {
            TrackKind::Audio => MediaKind::Audio,
            TrackKind::Video => MediaKind::Video,
        }
    }

    pub(crate) fn track_type(self) -> livekit::TrackType {
        match self {
            TrackKind::Audio => livekit::TrackType::Audio,
            TrackKind::Video => livekit::TrackType::Video,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackSpec {
    pub cid: String,
    pub name: String,
    pub kind: TrackKind,
    pub source: livekit::TrackSource,
    pub stream_id: String,
    pub profile: MediaProfile,
    pub layers: Vec<LayerSpec>,
}

impl TrackSpec {
    pub fn audio(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            cid: new_cid(),
            stream_id: new_stream_id(),
            name,
            kind: TrackKind::Audio,
            source: livekit::TrackSource::Microphone,
            profile: MediaProfile::opus(),
            layers: Vec::new(),
        }
    }

    pub fn video(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            cid: new_cid(),
            stream_id: new_stream_id(),
            name,
            kind: TrackKind::Video,
            source: livekit::TrackSource::Camera,
            profile: MediaProfile::vp8(),
            layers: Vec::new(),
        }
    }

    pub fn simulcast_video(name: impl Into<String>) -> Self {
        Self::video(name).with_layers(default_layers())
    }

    pub fn with_layers(mut self, layers: Vec<LayerSpec>) -> Self {
        if let Some(top) = layers.last() {
            self.profile = top.profile;
        }
        self.layers = layers;
        self
    }

    pub fn is_simulcast(&self) -> bool {
        !self.layers.is_empty()
    }

    pub fn layer(&self, rid: &str) -> Option<&LayerSpec> {
        self.layers.iter().find(|layer| layer.rid == rid)
    }

    pub fn rids(&self) -> Vec<&str> {
        self.layers.iter().map(|layer| layer.rid.as_str()).collect()
    }

    pub fn with_cid(mut self, cid: impl Into<String>) -> Self {
        self.cid = cid.into();
        self
    }

    pub fn with_stream_id(mut self, stream_id: impl Into<String>) -> Self {
        self.stream_id = stream_id.into();
        self
    }

    pub fn with_source(mut self, source: livekit::TrackSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_profile(mut self, profile: MediaProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn to_request(&self) -> livekit::AddTrackRequest {
        let layers = match self.kind {
            TrackKind::Audio => Vec::new(),
            TrackKind::Video if self.is_simulcast() => {
                self.layers.iter().map(LayerSpec::to_video_layer).collect()
            }
            TrackKind::Video => vec![livekit::VideoLayer {
                quality: livekit::VideoQuality::High as i32,
                width: self.profile.width,
                height: self.profile.height,
                bitrate: 0,
                ..livekit::VideoLayer::default()
            }],
        };

        livekit::AddTrackRequest {
            cid: self.cid.clone(),
            name: self.name.clone(),
            r#type: self.kind.track_type() as i32,
            width: self.profile.width,
            height: self.profile.height,
            source: self.source as i32,
            layers,
            stream: self.stream_id.clone(),
            simulcast_codecs: vec![livekit::SimulcastCodec {
                codec: self.profile.codec.name().to_owned(),
                cid: self.cid.clone(),
                ..livekit::SimulcastCodec::default()
            }],
            ..livekit::AddTrackRequest::default()
        }
    }
}

fn new_cid() -> String {
    format!("TR_{}", uuid::Uuid::new_v4().simple())
}

fn new_stream_id() -> String {
    format!("ST_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    #[test]
    fn an_audio_spec_asks_for_a_microphone_track() {
        let spec = TrackSpec::audio("mic").with_cid("TR_fixed");
        let request = spec.to_request();

        assert_eq!(request.cid, "TR_fixed");
        assert_eq!(request.name, "mic");
        assert_eq!(request.r#type, livekit::TrackType::Audio as i32);
        assert_eq!(request.source, livekit::TrackSource::Microphone as i32);
        assert!(request.layers.is_empty());
        assert_eq!(request.simulcast_codecs[0].codec, "opus");
    }

    #[test]
    fn a_video_spec_declares_one_layer_with_its_dimensions() {
        let request = TrackSpec::video("cam").to_request();

        assert_eq!(request.r#type, livekit::TrackType::Video as i32);
        assert_eq!(request.source, livekit::TrackSource::Camera as i32);
        assert_eq!(request.width, 320);
        assert_eq!(request.height, 240);
        assert_eq!(request.layers.len(), 1);
        assert_eq!(
            request.layers[0].quality,
            livekit::VideoQuality::High as i32
        );
        assert_eq!(request.simulcast_codecs[0].codec, "vp8");
    }

    #[test]
    fn a_simulcast_spec_declares_one_layer_per_encoding() {
        let spec = TrackSpec::simulcast_video("cam");
        let request = spec.to_request();

        assert_eq!(spec.rids(), vec!["q", "h", "f"]);
        assert_eq!(request.layers.len(), 3);
        assert_eq!(request.layers[0].rid, "q");
        assert_eq!(request.layers[0].quality, livekit::VideoQuality::Low as i32);
        assert_eq!(request.layers[1].rid, "h");
        assert_eq!(
            request.layers[1].quality,
            livekit::VideoQuality::Medium as i32
        );
        assert_eq!(request.layers[2].rid, "f");
        assert_eq!(
            request.layers[2].quality,
            livekit::VideoQuality::High as i32
        );
        assert_eq!(request.layers[2].bitrate, 1_700_000);
    }

    #[test]
    fn a_simulcast_spec_reports_the_top_layer_as_the_track_size() {
        let request = TrackSpec::simulcast_video("cam").to_request();

        assert_eq!(request.width, 1_280);
        assert_eq!(request.height, 720);
    }

    #[test]
    fn each_layer_of_a_spec_can_be_looked_up_by_rid() {
        let spec = TrackSpec::simulcast_video("cam");

        assert_eq!(spec.layer("h").map(|layer| layer.height), Some(360));
        assert_eq!(spec.layer("nope"), None);
        assert!(spec.is_simulcast());
        assert!(!TrackSpec::video("cam").is_simulcast());
    }

    #[test]
    fn cids_are_unique_per_spec() {
        assert_ne!(TrackSpec::audio("a").cid, TrackSpec::audio("a").cid);
    }

    #[test]
    fn an_add_track_request_survives_a_protobuf_round_trip() {
        let request = TrackSpec::video("cam").with_cid("TR_fixed").to_request();
        let frame = livekit::SignalRequest {
            message: Some(livekit::signal_request::Message::AddTrack(request.clone())),
        };

        let bytes = frame.encode_to_vec();
        let decoded = livekit::SignalRequest::decode(bytes.as_slice()).unwrap();

        match decoded.message.unwrap() {
            livekit::signal_request::Message::AddTrack(decoded) => {
                assert_eq!(decoded.cid, request.cid);
                assert_eq!(decoded.r#type, request.r#type);
                assert_eq!(decoded.layers, request.layers);
            }
            other => panic!("expected an add_track request, got {other:?}"),
        }
    }
}
