
use super::*;

fn request() -> mediacore::AddTrackRequest {
    mediacore::AddTrackRequest {
        cid: "cid-1".to_owned(),
        name: "camera".to_owned(),
        r#type: mediacore::TrackType::Video as i32,
        source: mediacore::TrackSource::Camera as i32,
        width: 1280,
        height: 720,
        layers: vec![
            mediacore::VideoLayer {
                quality: mediacore::VideoQuality::High as i32,
                width: 1280,
                height: 720,
                ..Default::default()
            },
            mediacore::VideoLayer {
                quality: mediacore::VideoQuality::Low as i32,
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
    let sid = generate_track_sid(TrackKind::Video, mediacore::TrackSource::Camera);
    assert!(sid.starts_with("TR_VC"), "unexpected sid {sid}");
}

#[test]
fn data_tracks_are_not_a_media_kind() {
    assert_eq!(
        TrackKind::from_proto(mediacore::TrackType::Audio as i32),
        Some(TrackKind::Audio)
    );
    assert_eq!(
        TrackKind::from_proto(mediacore::TrackType::Data as i32),
        None
    );
    assert_eq!(TrackKind::from_proto(42), None);
}

#[test]
#[allow(deprecated)]
fn proto_mirrors_the_client_declaration() {
    let mut track = PublishedTrack::new(
        "TR_VCabc".to_owned(),
        TrackKind::Video,
        mediacore::TrackSource::Camera,
        &request(),
        "PA_publisher",
        "alice",
    );
    track.mid = Some("0".to_owned());
    track.mime_type = Some("video/VP8".to_owned());

    let info = track.to_proto();
    assert_eq!(info.sid, "TR_VCabc");
    assert_eq!(info.r#type, mediacore::TrackType::Video as i32);
    assert_eq!(info.source, mediacore::TrackSource::Camera as i32);
    assert_eq!(info.mid, "0");
    assert_eq!(info.stream, "camera");
    assert!(info.simulcast);
    assert_eq!(
        info.layers[0].quality,
        mediacore::VideoQuality::Low as i32,
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
    let request = mediacore::AddTrackRequest {
        cid: "cid-2".to_owned(),
        r#type: mediacore::TrackType::Audio as i32,
        source: mediacore::TrackSource::Microphone as i32,
        stereo: true,
        disable_dtx: true,
        audio_features: vec![mediacore::AudioTrackFeature::TfStereo as i32],
        ..Default::default()
    };

    let track = PublishedTrack::new(
        "TR_AMabc".to_owned(),
        TrackKind::Audio,
        mediacore::TrackSource::Microphone,
        &request,
        "PA_publisher",
        "alice",
    );

    assert_eq!(
        track.audio_features,
        vec![
            mediacore::AudioTrackFeature::TfStereo as i32,
            mediacore::AudioTrackFeature::TfNoDtx as i32
        ]
    );
}

#[test]
fn versions_are_monotonic() {
    let first = TrackVersion::default().next();
    let second = first.next();
    assert!(second > first);
}
