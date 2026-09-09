use mediacore_proto::mediacore;
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

    pub(crate) fn track_type(self) -> mediacore::TrackType {
        match self {
            TrackKind::Audio => mediacore::TrackType::Audio,
            TrackKind::Video => mediacore::TrackType::Video,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackSpec {
    pub cid: String,
    pub name: String,
    pub kind: TrackKind,
    pub source: mediacore::TrackSource,
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
            source: mediacore::TrackSource::Microphone,
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
            source: mediacore::TrackSource::Camera,
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

    pub fn with_source(mut self, source: mediacore::TrackSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_profile(mut self, profile: MediaProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn to_request(&self) -> mediacore::AddTrackRequest {
        let layers = match self.kind {
            TrackKind::Audio => Vec::new(),
            TrackKind::Video if self.is_simulcast() => {
                self.layers.iter().map(LayerSpec::to_video_layer).collect()
            }
            TrackKind::Video => vec![mediacore::VideoLayer {
                quality: mediacore::VideoQuality::High as i32,
                width: self.profile.width,
                height: self.profile.height,
                bitrate: 0,
                ..mediacore::VideoLayer::default()
            }],
        };

        mediacore::AddTrackRequest {
            cid: self.cid.clone(),
            name: self.name.clone(),
            r#type: self.kind.track_type() as i32,
            width: self.profile.width,
            height: self.profile.height,
            source: self.source as i32,
            layers,
            stream: self.stream_id.clone(),
            simulcast_codecs: vec![mediacore::SimulcastCodec {
                codec: self.profile.codec.name().to_owned(),
                cid: self.cid.clone(),
                ..mediacore::SimulcastCodec::default()
            }],
            ..mediacore::AddTrackRequest::default()
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
#[path = "track_tests.rs"]
mod tests;
