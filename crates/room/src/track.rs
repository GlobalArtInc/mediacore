use mediacore_proto::mediacore;

use crate::{now_ms, random_id};

pub const TRACK_SID_PREFIX: &str = "TR_";
pub const MAX_PUBLISHED_TRACKS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Audio,
    Video,
}

impl TrackKind {
    pub fn from_proto(value: i32) -> Option<Self> {
        match mediacore::TrackType::try_from(value).ok()? {
            mediacore::TrackType::Audio => Some(Self::Audio),
            mediacore::TrackType::Video => Some(Self::Video),
            mediacore::TrackType::Data => None,
        }
    }

    pub fn to_proto(self) -> mediacore::TrackType {
        match self {
            Self::Audio => mediacore::TrackType::Audio,
            Self::Video => mediacore::TrackType::Video,
        }
    }

    fn tag(self) -> char {
        match self {
            Self::Audio => 'A',
            Self::Video => 'V',
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("participant `{0}` is not in the room")]
    UnknownParticipant(String),
    #[error("no permission to publish a {0:?} track")]
    NotAllowed(mediacore::TrackSource),
    #[error("track type `{0}` cannot be published")]
    UnsupportedKind(i32),
    #[error("already publishing {0} tracks")]
    TooManyTracks(usize),
}

impl PublishError {
    pub fn reason(&self) -> mediacore::request_response::Reason {
        match self {
            Self::UnknownParticipant(_) => mediacore::request_response::Reason::NotFound,
            Self::NotAllowed(_) => mediacore::request_response::Reason::NotAllowed,
            Self::UnsupportedKind(_) => mediacore::request_response::Reason::UnsupportedType,
            Self::TooManyTracks(_) => mediacore::request_response::Reason::LimitExceeded,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackVersion {
    unix_micro: i64,
    ticks: i32,
}

impl TrackVersion {
    pub(crate) fn next(self) -> Self {
        let unix_micro = now_ms().saturating_mul(1000);
        if unix_micro > self.unix_micro {
            Self {
                unix_micro,
                ticks: 0,
            }
        } else {
            Self {
                unix_micro: self.unix_micro,
                ticks: self.ticks.saturating_add(1),
            }
        }
    }

    pub fn to_proto(self) -> mediacore::TimedVersion {
        mediacore::TimedVersion {
            unix_micro: self.unix_micro,
            ticks: self.ticks,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PublishedTrack {
    pub sid: String,
    pub cid: String,
    pub name: String,
    pub kind: TrackKind,
    pub source: mediacore::TrackSource,
    pub muted: bool,
    pub width: u32,
    pub height: u32,
    pub layers: Vec<mediacore::VideoLayer>,
    pub live_layers: Vec<mediacore::VideoLayer>,
    pub stream: String,
    pub encryption: mediacore::encryption::Type,
    pub audio_features: Vec<i32>,
    pub disable_red: bool,
    pub backup_codec_policy: i32,
    pub mid: Option<String>,
    pub mime_type: Option<String>,
    pub publisher_sid: String,
    pub publisher_identity: String,
    pub version: TrackVersion,
}

impl PublishedTrack {
    pub(crate) fn new(
        sid: String,
        kind: TrackKind,
        source: mediacore::TrackSource,
        request: &mediacore::AddTrackRequest,
        publisher_sid: &str,
        publisher_identity: &str,
    ) -> Self {
        let stream = if request.stream.is_empty() {
            stream_from_source(source).to_owned()
        } else {
            request.stream.clone()
        };

        Self {
            sid,
            cid: request.cid.clone(),
            name: request.name.clone(),
            kind,
            source,
            muted: request.muted,
            width: request.width,
            height: request.height,
            layers: sorted_layers(&request.layers),
            live_layers: Vec::new(),
            stream,
            encryption: mediacore::encryption::Type::try_from(request.encryption)
                .unwrap_or(mediacore::encryption::Type::None),
            audio_features: audio_features(request),
            disable_red: request.disable_red,
            backup_codec_policy: request.backup_codec_policy,
            mid: None,
            mime_type: None,
            publisher_sid: publisher_sid.to_owned(),
            publisher_identity: publisher_identity.to_owned(),
            version: TrackVersion::default().next(),
        }
    }

    pub(crate) fn touch(&mut self) {
        self.version = self.version.next();
    }

    pub fn is_negotiated(&self) -> bool {
        self.mid.is_some()
    }

    pub fn is_simulcast(&self) -> bool {
        self.layers.len() > 1
    }

    pub fn reported_layers(&self) -> &[mediacore::VideoLayer] {
        if self.live_layers.is_empty() {
            &self.layers
        } else {
            &self.live_layers
        }
    }

    pub(crate) fn set_live_layers(&mut self, layers: Vec<mediacore::VideoLayer>) -> bool {
        let layers = sorted_layers(&layers);

        if self.live_layers == layers {
            return false;
        }

        self.live_layers = layers;
        true
    }

    #[allow(deprecated)]
    pub fn to_proto(&self) -> mediacore::TrackInfo {
        let mime_type = self.mime_type.clone().unwrap_or_default();
        let mid = self.mid.clone().unwrap_or_default();
        let layers = self.reported_layers().to_vec();

        mediacore::TrackInfo {
            sid: self.sid.clone(),
            r#type: self.kind.to_proto() as i32,
            name: self.name.clone(),
            muted: self.muted,
            width: self.width,
            height: self.height,
            simulcast: self.is_simulcast(),
            source: self.source as i32,
            layers: layers.clone(),
            mime_type: mime_type.clone(),
            mid: mid.clone(),
            codecs: vec![mediacore::SimulcastCodecInfo {
                mime_type,
                mid,
                cid: self.cid.clone(),
                layers,
                video_layer_mode: mediacore::video_layer::Mode::OneSpatialLayerPerStream as i32,
                sdp_cid: String::new(),
            }],
            disable_red: self.disable_red,
            encryption: self.encryption as i32,
            stream: self.stream.clone(),
            version: Some(self.version.to_proto()),
            audio_features: self.audio_features.clone(),
            backup_codec_policy: self.backup_codec_policy,
            ..Default::default()
        }
    }
}

pub(crate) fn generate_track_sid(kind: TrackKind, source: mediacore::TrackSource) -> String {
    format!(
        "{TRACK_SID_PREFIX}{}{}{}",
        kind.tag(),
        source_tag(source),
        random_id()
    )
}

fn source_tag(source: mediacore::TrackSource) -> char {
    match source {
        mediacore::TrackSource::Camera => 'C',
        mediacore::TrackSource::Microphone => 'M',
        mediacore::TrackSource::ScreenShare => 'S',
        mediacore::TrackSource::ScreenShareAudio => 's',
        mediacore::TrackSource::Unknown => 'U',
    }
}

fn stream_from_source(source: mediacore::TrackSource) -> &'static str {
    match source {
        mediacore::TrackSource::Camera | mediacore::TrackSource::Microphone => "camera",
        mediacore::TrackSource::ScreenShare | mediacore::TrackSource::ScreenShareAudio => "screen",
        mediacore::TrackSource::Unknown => "unknown",
    }
}

fn sorted_layers(layers: &[mediacore::VideoLayer]) -> Vec<mediacore::VideoLayer> {
    let mut layers = layers.to_vec();
    layers.sort_by_key(|layer| layer.quality);
    layers
}

#[allow(deprecated)]
fn audio_features(request: &mediacore::AddTrackRequest) -> Vec<i32> {
    let mut features: Vec<i32> = Vec::with_capacity(request.audio_features.len() + 2);
    let mut push = |feature: i32| {
        if !features.contains(&feature) {
            features.push(feature);
        }
    };

    for feature in &request.audio_features {
        push(*feature);
    }
    if request.stereo {
        push(mediacore::AudioTrackFeature::TfStereo as i32);
    }
    if request.disable_dtx {
        push(mediacore::AudioTrackFeature::TfNoDtx as i32);
    }

    features
}

#[cfg(test)]
#[path = "track_tests.rs"]
mod tests;
