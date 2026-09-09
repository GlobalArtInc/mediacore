use super::*;

const BUNDLED_OFFER: &str = "v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
m=video 0 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:1\r\n\
a=bundle-only\r\n\
a=sendonly\r\n\
m=video 0 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:2\r\n\
a=sendonly\r\n";

#[test]
fn a_bundle_only_section_is_still_sending() {
    let sections = sending_sections(BUNDLED_OFFER);

    assert_eq!(sections.get("0"), Some(&true));
    assert_eq!(
        sections.get("1"),
        Some(&true),
        "a bundle-only section carries media even though its port is zero"
    );
    assert_eq!(
        sections.get("2"),
        Some(&false),
        "a section with a zero port and no bundle-only is rejected"
    );
}

use mediacore_transport::str0m::format::{CodecSpec, FormatParams};
use mediacore_transport::str0m::media::Frequency;

const OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:aBc\r\n\
a=sendonly\r\n\
a=msid:stream-1 cid-audio\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:dEf\r\n\
a=sendonly\r\n\
a=ssrc:42 cname:x\r\n\
a=ssrc:42 msid:stream-1 cid-video\r\n";

fn offer(sdp: &str) -> mediacore::SessionDescription {
    mediacore::SessionDescription {
        r#type: "offer".to_owned(),
        sdp: sdp.to_owned(),
        ..Default::default()
    }
}

#[test]
fn tracks_are_read_from_the_msid_of_every_media_section() {
    assert_eq!(
        offered_tracks(&offer(OFFER)),
        vec![
            ("aBc".to_owned(), "cid-audio".to_owned()),
            ("dEf".to_owned(), "cid-video".to_owned()),
        ]
    );
}

#[test]
fn a_declared_mid_to_track_map_wins_over_the_sdp() {
    let mut description = offer(OFFER);
    description
        .mid_to_track_id
        .insert("aBc".to_owned(), "declared".to_owned());

    assert_eq!(
        offered_tracks(&description),
        vec![("aBc".to_owned(), "declared".to_owned())]
    );
}

#[test]
fn a_media_section_without_a_track_is_skipped() {
    let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";

    assert!(offered_tracks(&offer(sdp)).is_empty());
}

#[test]
fn a_media_section_the_publisher_stopped_sending_on_is_read_as_such() {
    let sending = sending_sections(OFFER);

    assert_eq!(sending.get("aBc"), Some(&true));
    assert_eq!(sending.get("dEf"), Some(&true));

    let stopped = OFFER.replace("a=mid:aBc\r\na=sendonly", "a=mid:aBc\r\na=inactive");
    let sending = sending_sections(&stopped);
    assert_eq!(sending.get("aBc"), Some(&false));
    assert_eq!(
        sending.get("dEf"),
        Some(&true),
        "one silenced section must not silence the next one"
    );

    let rejected = OFFER.replace(
        "m=video 9 UDP/TLS/RTP/SAVPF 96",
        "m=video 0 UDP/TLS/RTP/SAVPF 96",
    );
    assert_eq!(sending_sections(&rejected).get("dEf"), Some(&false));

    let bare = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";
    assert_eq!(
        sending_sections(bare).get("0"),
        Some(&true),
        "a section without a direction is sendrecv by default"
    );
}

#[test]
fn a_candidate_is_unwrapped_from_its_json_envelope() {
    let init = candidate_init("candidate:1 1 udp 100 127.0.0.1 40000 typ host", "aBc");

    assert_eq!(
        candidate_from_init(&init).as_deref(),
        Some("candidate:1 1 udp 100 127.0.0.1 40000 typ host")
    );
    assert_eq!(candidate_from_init(""), None);
    assert_eq!(candidate_from_init("{\"candidate\":\"\"}"), None);
    assert_eq!(candidate_from_init("not json"), None);
}

#[test]
fn a_trickled_candidate_names_the_bundled_media_section() {
    let init = candidate_init("candidate:1 1 udp 100 127.0.0.1 40000 typ host", "aBc");
    let parsed: serde_json::Value = serde_json::from_str(&init).expect("the envelope is json");

    assert_eq!(parsed["sdpMid"], serde_json::json!("aBc"));
    assert_eq!(parsed["sdpMLineIndex"], serde_json::json!(0));
    assert_eq!(
        parsed["candidate"],
        serde_json::json!("candidate:1 1 udp 100 127.0.0.1 40000 typ host")
    );
}

#[test]
fn the_bundle_mid_is_the_first_media_section_of_the_answer() {
    assert_eq!(bundle_mid(OFFER), "aBc");
    assert_eq!(bundle_mid("v=0\r\nm=audio 9 RTP/AVP 0\r\n"), "0");
}

fn params(pt: u8, codec: Codec, clock_rate: Frequency, channels: Option<u8>) -> PayloadParams {
    PayloadParams::new(
        pt.into(),
        None,
        CodecSpec {
            codec,
            clock_rate,
            channels,
            format: FormatParams::default(),
        },
    )
}

#[test]
fn the_mime_type_follows_the_negotiated_codec() {
    let opus = params(111, Codec::Opus, Frequency::FORTY_EIGHT_KHZ, Some(2));
    let vp8 = params(96, Codec::Vp8, Frequency::NINETY_KHZ, None);

    assert_eq!(
        mime_type(MediaKind::Audio, &[opus]).as_deref(),
        Some("audio/opus")
    );
    assert_eq!(
        mime_type(MediaKind::Video, &[vp8]).as_deref(),
        Some("video/VP8")
    );
    assert_eq!(mime_type(MediaKind::Video, &[]), None);
}
