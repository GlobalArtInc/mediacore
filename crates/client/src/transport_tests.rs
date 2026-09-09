use super::*;
use str0m::change::SdpOffer;

fn host(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn offering_rtc(port: u16) -> Rtc {
    let mut rtc = publisher_rtc().expect("a publisher rtc");
    let candidate = Candidate::host(host(port), "udp").expect("a host candidate");
    assert!(rtc.add_local_candidate(candidate).is_some());
    rtc
}

fn answering_rtc(port: u16) -> Rtc {
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .set_dtls_cert(dtls_cert().expect("a dtls certificate"))
        .build(Instant::now());
    let candidate = Candidate::host(host(port), "udp").expect("a host candidate");
    assert!(rtc.add_local_candidate(candidate).is_some());
    rtc
}

#[test]
fn the_offer_describes_a_send_only_track_with_ice_and_dtls() {
    let mut rtc = offering_rtc(40001);

    let mut api = rtc.sdp_api();
    let mid = api.add_media(
        MediaKind::Audio,
        Direction::SendOnly,
        Some("stream".to_owned()),
        Some("TR_test".to_owned()),
        None,
    );
    let (offer, _pending) = api.apply().expect("an offer");
    let sdp = offer.to_sdp_string();

    assert!(sdp.contains("m=audio"), "{sdp}");
    assert!(sdp.contains("a=sendonly"), "{sdp}");
    assert!(sdp.contains(&format!("a=mid:{mid}")), "{sdp}");
    assert!(sdp.contains("a=ice-ufrag:"), "{sdp}");
    assert!(sdp.contains("a=ice-pwd:"), "{sdp}");
    assert!(sdp.contains("a=fingerprint:"), "{sdp}");
    assert!(sdp.contains("a=candidate:"), "{sdp}");
    assert!(sdp.contains("127.0.0.1 40001"), "{sdp}");
    assert!(sdp.contains("opus/48000/2"), "{sdp}");
    assert!(sdp.contains("a=msid:stream TR_test"), "{sdp}");
    assert!(!sdp.contains("a=ice-lite"), "{sdp}");
}

#[test]
fn a_video_offer_advertises_vp8_and_nothing_we_cannot_send() {
    let mut rtc = offering_rtc(40002);

    let mut api = rtc.sdp_api();
    api.add_media(
        MediaKind::Video,
        Direction::SendOnly,
        Some("stream".to_owned()),
        Some("TR_video".to_owned()),
        None,
    );
    let (offer, _pending) = api.apply().expect("an offer");
    let sdp = offer.to_sdp_string().to_lowercase();

    assert!(sdp.contains("m=video"), "{sdp}");
    assert!(sdp.contains("vp8/90000"), "{sdp}");
    assert!(!sdp.contains("h264"), "{sdp}");
    assert!(!sdp.contains("vp9"), "{sdp}");
}

#[test]
fn a_peer_accepts_the_offer_and_we_learn_the_payload_type() {
    let mut ours = offering_rtc(40003);
    let mut theirs = answering_rtc(40004);

    let mut api = ours.sdp_api();
    let mid = api.add_media(
        MediaKind::Audio,
        Direction::SendOnly,
        Some("stream".to_owned()),
        Some("TR_test".to_owned()),
        None,
    );
    let (offer, pending) = api.apply().expect("an offer");

    let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("a parseable offer");
    let answer = theirs.sdp_api().accept_offer(offer).expect("an answer");

    let answer = SdpAnswer::from_sdp_string(&answer.to_sdp_string()).expect("a parseable answer");
    ours.sdp_api()
        .accept_answer(pending, answer)
        .expect("the answer applies");

    let pt = negotiated_pt(&ours, mid, MediaProfile::opus()).expect("an opus payload type");
    assert_eq!(
        ours.codec_config()
            .params()
            .iter()
            .find(|params| params.pt() == pt)
            .map(|params| params.spec().codec),
        Some(str0m::format::Codec::Opus)
    );

    assert!(ours.direct_api().stream_tx_by_mid(mid, None).is_some());
}

fn sources(rids: &[&str]) -> Vec<PublishSource> {
    rids.iter()
        .map(|rid| PublishSource {
            rid: Some(Rid::from(*rid)),
            profile: MediaProfile::vp8(),
        })
        .collect()
}

#[test]
fn a_track_without_layers_offers_no_simulcast() {
    let plain = vec![PublishSource {
        rid: None,
        profile: MediaProfile::vp8(),
    }];

    assert!(simulcast_of(&plain).is_none());
    assert!(simulcast_of(&[]).is_none());

    let simulcast = simulcast_of(&sources(&["q", "h", "f"])).expect("a simulcast config");
    assert_eq!(simulcast.send.len(), 3);
    assert!(simulcast.recv.is_empty());
    assert_eq!(simulcast.send[0].rid, Rid::from("q"));
}

#[test]
fn a_simulcast_offer_names_every_rid_the_browser_way() {
    let mut rtc = offering_rtc(40007);
    let sources = sources(&["q", "h", "f"]);

    let mut api = rtc.sdp_api();
    api.add_media(
        MediaKind::Video,
        Direction::SendOnly,
        Some("stream".to_owned()),
        Some("TR_cam".to_owned()),
        simulcast_of(&sources),
    );
    let (offer, _pending) = api.apply().expect("an offer");
    let sdp = offer.to_sdp_string();

    assert!(sdp.contains("a=rid:q send"), "{sdp}");
    assert!(sdp.contains("a=rid:h send"), "{sdp}");
    assert!(sdp.contains("a=rid:f send"), "{sdp}");
    assert!(sdp.contains("a=simulcast:send q;h;f"), "{sdp}");
    assert!(sdp.to_lowercase().contains("rtp-stream-id"), "{sdp}");
}

#[test]
fn a_simulcast_answer_leaves_us_one_send_stream_per_rid() {
    let mut ours = offering_rtc(40009);
    let mut theirs = answering_rtc(40010);
    let sources = sources(&["q", "h", "f"]);

    let mut api = ours.sdp_api();
    let mid = api.add_media(
        MediaKind::Video,
        Direction::SendOnly,
        Some("stream".to_owned()),
        Some("TR_cam".to_owned()),
        simulcast_of(&sources),
    );
    let (offer, pending) = api.apply().expect("an offer");

    let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("a parseable offer");
    let answer = theirs.sdp_api().accept_offer(offer).expect("an answer");
    let answer = SdpAnswer::from_sdp_string(&answer.to_sdp_string()).expect("a parseable answer");
    ours.sdp_api()
        .accept_answer(pending, answer)
        .expect("the answer applies");

    let mut ssrcs = Vec::new();
    for rid in ["q", "h", "f"] {
        let mut direct = ours.direct_api();
        let stream = direct
            .stream_tx_by_mid(mid, Some(Rid::from(rid)))
            .unwrap_or_else(|| panic!("a send stream for rid {rid}"));

        assert_eq!(stream.rid(), Some(Rid::from(rid)));
        ssrcs.push(stream.ssrc());
    }

    ssrcs.sort();
    ssrcs.dedup();
    assert_eq!(ssrcs.len(), 3, "every layer needs its own ssrc");
}

#[test]
fn asking_for_a_codec_the_peer_did_not_accept_is_an_error() {
    let mut ours = offering_rtc(40005);
    let mut theirs = answering_rtc(40006);

    let mut api = ours.sdp_api();
    let mid = api.add_media(
        MediaKind::Audio,
        Direction::SendOnly,
        Some("stream".to_owned()),
        Some("TR_test".to_owned()),
        None,
    );
    let (offer, pending) = api.apply().expect("an offer");

    let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("a parseable offer");
    let answer = theirs.sdp_api().accept_offer(offer).expect("an answer");
    let answer = SdpAnswer::from_sdp_string(&answer.to_sdp_string()).expect("a parseable answer");
    ours.sdp_api()
        .accept_answer(pending, answer)
        .expect("the answer applies");

    let error = negotiated_pt(&ours, mid, MediaProfile::vp8()).unwrap_err();
    assert!(matches!(error, ClientError::CodecNotNegotiated { .. }));
}
