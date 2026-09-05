use mediacore_proto::livekit;

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
        match livekit::TrackType::try_from(value).ok()? {
            livekit::TrackType::Audio => Some(Self::Audio),
            livekit::TrackType::Video => Some(Self::Video),
            livekit::TrackType::Data => None,
        }
    }

    pub fn to_proto(self) -> livekit::TrackType {
        match self {
            Self::Audio => livekit::TrackType::Audio,
            Self::Video => livekit::TrackType::Video,
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
    NotAllowed(livekit::TrackSource),
    #[error("track type `{0}` cannot be published")]
    UnsupportedKind(i32),
    #[error("already publishing {0} tracks")]
    TooManyTracks(usize),
}

impl PublishError {
    pub fn reason(&self) -> livekit::request_response::Reason {
        match self {
            Self::UnknownParticipant(_) => livekit::request_response::Reason::NotFound,
            Self::NotAllowed(_) => livekit::request_response::Reason::NotAllowed,
            Self::UnsupportedKind(_) => livekit::request_response::Reason::UnsupportedType,
            Self::TooManyTracks(_) => livekit::request_response::Reason::LimitExceeded,
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

    pub fn to_proto(self) -> livekit::TimedVersion {
        livekit::TimedVersion {
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
    pub source: livekit::TrackSource,
    pub muted: bool,
    pub width: u32,
    pub height: u32,
    pub layers: Vec<livekit::VideoLayer>,
    pub stream: String,
    pub encryption: livekit::encryption::Type,
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
        source: livekit::TrackSource,
        request: &livekit::AddTrackRequest,
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
            stream,
            encryption: livekit::encryption::Type::try_from(request.encryption)
                .unwrap_or(livekit::encryption::Type::None),
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

    #[allow(deprecated)]
    pub fn to_proto(&self) -> livekit::TrackInfo {
        let mime_type = self.mime_type.clone().unwrap_or_default();
        let mid = self.mid.clone().unwrap_or_default();

        livekit::TrackInfo {
            sid: self.sid.clone(),
            r#type: self.kind.to_proto() as i32,
            name: self.name.clone(),
            muted: self.muted,
            width: self.width,
            height: self.height,
            simulcast: self.layers.len() > 1,
            source: self.source as i32,
            layers: self.layers.clone(),
            mime_type: mime_type.clone(),
            mid: mid.clone(),
            codecs: vec![livekit::SimulcastCodecInfo {
                mime_type,
                mid,
                cid: self.cid.clone(),
                layers: self.layers.clone(),
                video_layer_mode: livekit::video_layer::Mode::OneSpatialLayerPerStream as i32,
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

pub(crate) fn generate_track_sid(kind: TrackKind, source: livekit::TrackSource) -> String {
    format!(
        "{TRACK_SID_PREFIX}{}{}{}",
        kind.tag(),
        source_tag(source),
        random_id()
    )
}

fn source_tag(source: livekit::TrackSource) -> char {
    match source {
        livekit::TrackSource::Camera => 'C',
        livekit::TrackSource::Microphone => 'M',
        livekit::TrackSource::ScreenShare => 'S',
        livekit::TrackSource::ScreenShareAudio => 's',
        livekit::TrackSource::Unknown => 'U',
    }
}

fn stream_from_source(source: livekit::TrackSource) -> &'static str {
    match source {
        livekit::TrackSource::Camera | livekit::TrackSource::Microphone => "camera",
        livekit::TrackSource::ScreenShare | livekit::TrackSource::ScreenShareAudio => "screen",
        livekit::TrackSource::Unknown => "unknown",
    }
}

fn sorted_layers(layers: &[livekit::VideoLayer]) -> Vec<livekit::VideoLayer> {
    let mut layers = layers.to_vec();
    layers.sort_by_key(|layer| layer.quality);
    layers
}

#[allow(deprecated)]
fn audio_features(request: &livekit::AddTrackRequest) -> Vec<i32> {
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
        push(livekit::AudioTrackFeature::TfStereo as i32);
    }
    if request.disable_dtx {
        push(livekit::AudioTrackFeature::TfNoDtx as i32);
    }

    features
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> livekit::AddTrackRequest {
        livekit::AddTrackRequest {
            cid: "cid-1".to_owned(),
            name: "camera".to_owned(),
            r#type: livekit::TrackType::Video as i32,
            source: livekit::TrackSource::Camera as i32,
            width: 1280,
            height: 720,
            layers: vec![
                livekit::VideoLayer {
                    quality: livekit::VideoQuality::High as i32,
                    width: 1280,
                    height: 720,
                    ..Default::default()
                },
                livekit::VideoLayer {
                    quality: livekit::VideoQuality::Low as i32,
                    width: 320,
                    height: 180,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn track_sid_carries_kind_and_source() {
        let sid = generate_track_sid(TrackKind::Video, livekit::TrackSource::Camera);
        assert!(sid.starts_with("TR_VC"), "unexpected sid {sid}");
    }

    #[test]
    fn data_tracks_are_not_a_media_kind() {
        assert_eq!(
            TrackKind::from_proto(livekit::TrackType::Audio as i32),
            Some(TrackKind::Audio)
        );
        assert_eq!(TrackKind::from_proto(livekit::TrackType::Data as i32), None);
        assert_eq!(TrackKind::from_proto(42), None);
    }

    #[test]
    #[allow(deprecated)]
    fn proto_mirrors_the_client_declaration() {
        let mut track = PublishedTrack::new(
            "TR_VCabc".to_owned(),
            TrackKind::Video,
            livekit::TrackSource::Camera,
            &request(),
            "PA_publisher",
            "alice",
        );
        track.mid = Some("0".to_owned());
        track.mime_type = Some("video/VP8".to_owned());

        let info = track.to_proto();
        assert_eq!(info.sid, "TR_VCabc");
        assert_eq!(info.r#type, livekit::TrackType::Video as i32);
        assert_eq!(info.source, livekit::TrackSource::Camera as i32);
        assert_eq!(info.mid, "0");
        assert_eq!(info.stream, "camera");
        assert!(info.simulcast);
        assert_eq!(
            info.layers[0].quality,
            livekit::VideoQuality::Low as i32,
            "layers are ordered by ascending quality"
        );
        assert_eq!(info.codecs.len(), 1);
        assert_eq!(info.codecs[0].cid, "cid-1");
        assert_eq!(info.codecs[0].mime_type, "video/VP8");
        assert!(info.version.is_some());
    }

    #[test]
    fn stereo_and_dtx_become_audio_features() {
        #[allow(deprecated)]
        let request = livekit::AddTrackRequest {
            cid: "cid-2".to_owned(),
            r#type: livekit::TrackType::Audio as i32,
            source: livekit::TrackSource::Microphone as i32,
            stereo: true,
            disable_dtx: true,
            audio_features: vec![livekit::AudioTrackFeature::TfStereo as i32],
            ..Default::default()
        };

        let track = PublishedTrack::new(
            "TR_AMabc".to_owned(),
            TrackKind::Audio,
            livekit::TrackSource::Microphone,
            &request,
            "PA_publisher",
            "alice",
        );

        assert_eq!(
            track.audio_features,
            vec![
                livekit::AudioTrackFeature::TfStereo as i32,
                livekit::AudioTrackFeature::TfNoDtx as i32
            ]
        );
    }

    #[test]
    fn versions_are_monotonic() {
        let first = TrackVersion::default().next();
        let second = first.next();
        assert!(second > first);
    }
}
