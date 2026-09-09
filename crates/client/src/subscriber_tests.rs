use super::*;

const CAPTURED_OFFER: &str = concat!(
    "v=0\r\n",
    "o=- 5228595090268964687 1 IN IP4 0.0.0.0\r\n",
    "s=-\r\n",
    "t=0 0\r\n",
    "a=fingerprint:sha-256 \
         6D:F5:6C:9E:16:9E:5D:AF:6E:E8:4C:7A:2B:16:2E:76:7B:33:8F:F2:0A:1E:8F:D0:8E:24:2D:1B:57:3E:66:11\r\n",
    "a=group:BUNDLE 0 1\r\n",
    "a=msid-semantic:WMS *\r\n",
    "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
    "c=IN IP4 0.0.0.0\r\n",
    "a=setup:actpass\r\n",
    "a=mid:0\r\n",
    "a=ice-ufrag:CvGkYCbFvSKGxIWq\r\n",
    "a=ice-pwd:CmLdgVzTPAjEcVBRJhTxRxOMKtIOwpKw\r\n",
    "a=rtcp-mux\r\n",
    "a=rtcp-rsize\r\n",
    "a=rtpmap:111 opus/48000/2\r\n",
    "a=fmtp:111 minptime=10;useinbandfec=1\r\n",
    "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
    "a=ssrc:2541098696 cname:bob\r\n",
    "a=ssrc:2541098696 msid:ST_bob TR_bobmic\r\n",
    "a=sendonly\r\n",
    "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
    "c=IN IP4 0.0.0.0\r\n",
    "a=setup:actpass\r\n",
    "a=mid:1\r\n",
    "a=ice-ufrag:CvGkYCbFvSKGxIWq\r\n",
    "a=ice-pwd:CmLdgVzTPAjEcVBRJhTxRxOMKtIOwpKw\r\n",
    "a=rtcp-mux\r\n",
    "a=rtcp-rsize\r\n",
    "a=rtpmap:96 VP8/90000\r\n",
    "a=rtcp-fb:96 nack\r\n",
    "a=rtcp-fb:96 nack pli\r\n",
    "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
    "a=ssrc:1815724087 cname:bob\r\n",
    "a=ssrc:1815724087 msid:ST_bob TR_bobcam\r\n",
    "a=sendonly\r\n",
);

#[test]
fn the_media_sections_of_a_captured_offer_are_read_out() {
    let sections = media_sections(CAPTURED_OFFER);

    assert_eq!(sections.len(), 2);

    assert_eq!(sections[0].mid, "0");
    assert_eq!(sections[0].kind, Some(TrackKind::Audio));
    assert_eq!(sections[0].stream_id.as_deref(), Some("ST_bob"));
    assert_eq!(sections[0].track_id.as_deref(), Some("TR_bobmic"));
    assert_eq!(sections[0].direction.as_deref(), Some("sendonly"));

    assert_eq!(sections[1].mid, "1");
    assert_eq!(sections[1].kind, Some(TrackKind::Video));
    assert_eq!(sections[1].track_id.as_deref(), Some("TR_bobcam"));
}

#[test]
fn an_explicit_msid_line_wins_over_the_ssrc_attribute() {
    let sdp = concat!(
        "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
        "a=mid:0\r\n",
        "a=msid:ST_named TR_named\r\n",
        "a=ssrc:1 msid:ST_other TR_other\r\n",
    );

    let sections = media_sections(sdp);
    assert_eq!(sections[0].track_id.as_deref(), Some("TR_named"));
    assert_eq!(sections[0].stream_id.as_deref(), Some("ST_named"));
}

#[test]
fn a_section_without_a_mid_is_dropped() {
    let sdp = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    assert!(media_sections(sdp).is_empty());
}

#[test]
fn a_captured_offer_is_answered_with_a_receiving_session() {
    let mut rtc = subscriber_rtc().expect("a subscriber rtc");
    let candidate = Candidate::host(SocketAddr::from(([127, 0, 0, 1], 41001)), "udp")
        .expect("a host candidate");
    assert!(rtc.add_local_candidate(candidate).is_some());

    let answer = answer_offer(&mut rtc, CAPTURED_OFFER).expect("an answer");

    assert!(answer.starts_with("v=0"), "{answer}");
    assert!(answer.contains("a=mid:0"), "{answer}");
    assert!(answer.contains("a=mid:1"), "{answer}");
    assert_eq!(answer.matches("a=recvonly").count(), 2, "{answer}");
    assert!(answer.contains("a=ice-ufrag:"), "{answer}");
    assert!(answer.contains("a=ice-pwd:"), "{answer}");
    assert!(answer.contains("a=fingerprint:"), "{answer}");
    assert!(answer.contains("a=setup:passive"), "{answer}");
    assert!(answer.contains("opus/48000/2"), "{answer}");
    assert!(answer.to_lowercase().contains("vp8/90000"), "{answer}");
    assert!(answer.contains("a=group:BUNDLE 0 1"), "{answer}");
}

#[test]
fn answering_the_same_offer_twice_is_stable() {
    let mut rtc = subscriber_rtc().expect("a subscriber rtc");
    let first = answer_offer(&mut rtc, CAPTURED_OFFER).expect("a first answer");
    let second = answer_offer(&mut rtc, CAPTURED_OFFER).expect("a second answer");

    assert_eq!(
        first.matches("a=mid:").count(),
        second.matches("a=mid:").count()
    );
}

#[test]
fn a_malformed_offer_is_an_error_and_not_a_panic() {
    let mut rtc = subscriber_rtc().expect("a subscriber rtc");
    let error = answer_offer(&mut rtc, "not an sdp at all").unwrap_err();
    assert!(matches!(error, ClientError::Rtc(_)), "{error}");
}
