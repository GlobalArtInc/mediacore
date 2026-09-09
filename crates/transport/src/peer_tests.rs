use super::*;

use crate::negotiation::MediaSpec;
use crate::router::RouterConfig;

fn loopback_router_config() -> RouterConfig {
    RouterConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..RouterConfig::default()
    }
}

fn client_offer(crypto: &TransportCrypto) -> String {
    let mut rtc = Rtc::builder()
        .set_rtp_mode(true)
        .set_dtls_cert(crypto.dtls_cert())
        .build(Instant::now());

    rtc.add_local_candidate(
        Candidate::host(SocketAddr::from(([127, 0, 0, 1], 40000)), "udp")
            .expect("client host candidate"),
    );

    let mut api = rtc.sdp_api();
    api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
    api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);

    let (offer, _pending) = api.apply().expect("client offer");

    offer.to_sdp_string()
}

fn sdp_attribute(sdp: &str, name: &str) -> Option<String> {
    sdp.lines()
        .find_map(|line| line.trim().strip_prefix(name))
        .map(str::to_owned)
}

async fn peer_actor(
    crypto: &TransportCrypto,
    udp: &UdpRouter,
    config: PeerConfig,
) -> (PeerHandle, PeerActor, PeerEvents) {
    build_peer(crypto, udp, config).expect("a peer is built")
}

#[tokio::test]
async fn answers_a_client_offer_and_advertises_a_host_candidate() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();

    assert_eq!(udp.router().session_count(), 1);
    assert_eq!(
        udp.router()
            .session_for_ufrag(&peer.handle.ice_creds().ufrag),
        Some(peer.handle.id())
    );

    let answer = peer
        .handle
        .accept_offer(client_offer(&crypto))
        .await
        .unwrap();

    assert!(answer.contains(&format!("a=ice-ufrag:{}", peer.handle.ice_creds().ufrag)));
    assert!(answer.contains("a=ice-lite"));
    assert!(answer.contains("typ host"));
    assert!(answer.contains(&udp.local_addr().port().to_string()));
}

#[tokio::test]
async fn an_ice_restart_rekeys_the_route_to_the_new_credentials() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();

    let mut client = Rtc::builder()
        .set_rtp_mode(true)
        .set_dtls_cert(crypto.dtls_cert())
        .build(Instant::now());
    client.add_local_candidate(
        Candidate::host(SocketAddr::from(([127, 0, 0, 1], 40010)), "udp")
            .expect("client host candidate"),
    );

    let mut api = client.sdp_api();
    api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
    let (offer, pending) = api.apply().expect("client offer");

    let answer = peer
        .handle
        .accept_offer(offer.to_sdp_string())
        .await
        .unwrap();
    client
        .sdp_api()
        .accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap())
        .expect("the client applies our answer");

    let first = peer.handle.ice_creds();
    assert_eq!(
        udp.router().session_for_ufrag(&first.ufrag),
        Some(peer.handle.id())
    );

    let mut api = client.sdp_api();
    api.ice_restart(true);
    let (restart, _pending) = api.apply().expect("an ice restart offer");

    let answer = peer
        .handle
        .accept_offer(restart.to_sdp_string())
        .await
        .unwrap();

    let restarted = peer.handle.ice_creds();
    assert_ne!(restarted.ufrag, first.ufrag);
    assert_eq!(
        sdp_attribute(&answer, "a=ice-ufrag:").as_deref(),
        Some(restarted.ufrag.as_str())
    );
    assert_eq!(
        sdp_attribute(&answer, "a=ice-pwd:").as_deref(),
        Some(restarted.pass.as_str())
    );

    assert_eq!(udp.router().session_for_ufrag(&first.ufrag), None);
    assert_eq!(
        udp.router().session_for_ufrag(&restarted.ufrag),
        Some(peer.handle.id())
    );
    assert_eq!(udp.router().session_count(), 1);
}

#[tokio::test]
async fn offers_outgoing_media_and_applies_the_client_answer() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber(None)).unwrap();

    assert!(matches!(
        peer.handle.negotiate(Negotiation::default()).await,
        Err(TransportError::NothingToNegotiate)
    ));
    assert!(matches!(
        peer.handle.accept_answer(String::new()).await,
        Err(TransportError::NoPendingOffer)
    ));

    let local = peer
        .handle
        .negotiate(Negotiation::add(MediaSpec {
            kind: MediaKind::Video,
            direction: Direction::SendOnly,
            stream_id: Some("stream".to_owned()),
            track_id: Some("track".to_owned()),
            simulcast: None,
        }))
        .await
        .unwrap();

    let mid = local.mids[0];
    let offer = local.sdp;
    assert!(offer.contains(&format!("a=mid:{mid}")));

    let mut client = Rtc::builder()
        .set_rtp_mode(true)
        .set_dtls_cert(crypto.dtls_cert())
        .build(Instant::now());
    client.add_local_candidate(
        Candidate::host(SocketAddr::from(([127, 0, 0, 1], 40001)), "udp")
            .expect("client host candidate"),
    );

    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer).expect("our offer parses"))
        .expect("client answers our offer");

    peer.handle
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();
}

#[tokio::test]
async fn closing_a_peer_deregisters_it_from_the_router() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();
    let ufrag = peer.handle.ice_creds().ufrag;

    assert_eq!(udp.router().session_count(), 1);

    peer.handle.close();
    peer.task.await.unwrap();

    assert_eq!(udp.router().session_count(), 0);
    assert_eq!(udp.router().address_count(), 0);
    assert_eq!(udp.router().session_for_ufrag(&ufrag), None);
}

#[tokio::test]
async fn dropping_the_handle_stops_the_peer_and_frees_the_ufrag() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();
    let ufrag = peer.handle.ice_creds().ufrag;

    let SpawnedPeer {
        handle,
        events: _events,
        task,
    } = peer;
    drop(handle);
    task.await.unwrap();

    assert_eq!(udp.router().session_for_ufrag(&ufrag), None);
}

#[test]
fn a_wildcard_socket_reports_the_advertised_address_to_the_session() {
    let bound = SocketAddr::from(([0, 0, 0, 0], 7882));
    let advertised = SocketAddr::from(([203, 0, 113, 7], 7882));

    assert_eq!(
        receiving_addr(bound, &[advertised]),
        advertised,
        "a packet that arrived on a wildcard socket must be attributed to the candidate we \
             advertised, or ice never finds a pair and stays in checking for ever"
    );

    let explicit = SocketAddr::from(([127, 0, 0, 1], 7882));
    assert_eq!(receiving_addr(explicit, &[advertised]), explicit);
    assert_eq!(receiving_addr(bound, &[]), bound);
    assert_eq!(
        receiving_addr(bound, &[SocketAddr::from(([0, 0, 0, 0], 7882)), advertised]),
        advertised
    );
}

#[tokio::test]
async fn refuses_to_start_without_a_usable_advertised_address() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(RouterConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        advertise: vec![SocketAddr::from(([0, 0, 0, 0], 7881))],
        ..RouterConfig::default()
    })
    .await
    .unwrap();

    let error = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap_err();

    assert!(matches!(error, TransportError::NoLocalCandidate));
    assert_eq!(udp.router().session_count(), 0);
}

#[tokio::test]
async fn an_empty_trickle_candidate_is_ignored() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();

    peer.handle
        .add_remote_candidate(String::new())
        .await
        .unwrap();

    let error = peer
        .handle
        .add_remote_candidate("not a candidate".to_owned())
        .await
        .unwrap_err();

    assert!(matches!(error, TransportError::Candidate(_)));
}

#[tokio::test]
async fn a_flapping_ice_connection_still_expires() {
    let mut ice = IceWatch::new(Duration::from_secs(10));
    let start = Instant::now();

    ice.observe(IceConnectionState::Disconnected, start);
    ice.observe(IceConnectionState::Checking, start + Duration::from_secs(1));
    ice.observe(
        IceConnectionState::Disconnected,
        start + Duration::from_secs(2),
    );

    assert!(
        ice.expired(start + Duration::from_secs(11)),
        "a peer that keeps flapping must still be reaped"
    );

    ice.observe(
        IceConnectionState::Connected,
        start + Duration::from_secs(12),
    );
    assert_eq!(ice.deadline(), None);
}

#[tokio::test]
async fn a_refused_rekey_closes_the_session() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) = peer_actor(&crypto, &udp, PeerConfig::publisher()).await;
    let (occupier, _occupier_actor, _occupier_events) =
        peer_actor(&crypto, &udp, PeerConfig::publisher()).await;

    let taken = occupier.ice_creds();
    actor
        .rtc
        .direct_api()
        .set_local_ice_credentials(taken.clone());

    let error = actor
        .sync_ice_credentials()
        .expect_err("the rekey is refused");

    assert!(matches!(error, TransportError::RouteRekey(ufrag) if ufrag == taken.ufrag));
    assert!(
        !actor.rtc.is_alive(),
        "a session the router cannot reach must not stay alive"
    );
}

#[tokio::test]
async fn a_transient_ice_disconnect_keeps_the_session_alive() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, mut events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig {
            ice_disconnected_grace: Duration::from_secs(30),
            ..PeerConfig::publisher()
        },
    )
    .await;

    let start = Instant::now();

    assert!(actor.on_event(Event::IceConnectionStateChange(
        IceConnectionState::Disconnected
    )));
    assert!(
        actor.rtc.is_alive(),
        "a transient ice disconnect must not close the rtc session"
    );
    assert!(actor.ice.deadline().is_some());
    assert!(!actor.ice.expired(start + Duration::from_secs(29)));

    assert!(matches!(
        events.recv_control().await,
        Some(PeerEvent::IceConnectionState(
            IceConnectionState::Disconnected
        ))
    ));

    assert!(actor.on_event(Event::IceConnectionStateChange(
        IceConnectionState::Connected
    )));
    assert!(actor.rtc.is_alive());
    assert_eq!(actor.ice.deadline(), None);
    assert!(!actor.ice.expired(start + Duration::from_secs(600)));
}

#[tokio::test]
async fn an_ice_disconnect_that_never_recovers_expires() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig {
            ice_disconnected_grace: Duration::from_millis(50),
            ..PeerConfig::publisher()
        },
    )
    .await;

    let start = Instant::now();
    actor.on_event(Event::IceConnectionStateChange(
        IceConnectionState::Disconnected,
    ));

    assert!(!actor.ice.expired(start));
    assert!(actor.ice.expired(start + Duration::from_millis(51)));
    assert!(actor.rtc.is_alive());
}

#[tokio::test]
async fn the_ice_grace_period_survives_repeated_disconnected_reports() {
    let mut ice = IceWatch::new(Duration::from_secs(10));
    let start = Instant::now();

    ice.observe(IceConnectionState::Disconnected, start);
    ice.observe(
        IceConnectionState::Disconnected,
        start + Duration::from_secs(9),
    );

    assert_eq!(ice.deadline(), Some(start + Duration::from_secs(10)));
    assert!(ice.expired(start + Duration::from_secs(10)));
}

#[tokio::test]
async fn a_full_media_queue_drops_media_and_never_blocks_control() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (handle, mut actor, mut events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig {
            media_queue: 4,
            ..PeerConfig::publisher()
        },
    )
    .await;

    let filled = tokio::time::timeout(Duration::from_secs(1), async {
        for _ in 0..64 {
            assert!(actor.sink.emit_media(PeerEvent::Connected));
        }
    })
    .await;
    assert!(filled.is_ok(), "emitting media must never block the actor");

    assert_eq!(handle.dropped_media_events(), 60);

    let control = tokio::time::timeout(Duration::from_secs(1), async {
        assert!(actor.sink.emit_control(PeerEvent::Connected));
        events.recv_control().await
    })
    .await
    .expect("control events flow while the media queue is full");

    assert!(matches!(control, Some(PeerEvent::Connected)));
}

fn answering_client(crypto: &TransportCrypto, port: u16) -> Rtc {
    let mut client = Rtc::builder()
        .set_rtp_mode(true)
        .set_dtls_cert(crypto.dtls_cert())
        .build(Instant::now());

    client.add_local_candidate(
        Candidate::host(SocketAddr::from(([127, 0, 0, 1], port)), "udp")
            .expect("client host candidate"),
    );

    client
}

fn outgoing(kind: MediaKind, track: &str) -> Negotiation {
    Negotiation::add(MediaSpec {
        kind,
        direction: Direction::SendOnly,
        stream_id: Some("stream".to_owned()),
        track_id: Some(track.to_owned()),
        simulcast: None,
    })
}

#[tokio::test]
async fn declaring_a_send_stream_for_an_unknown_mid_is_refused() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber(None)).unwrap();

    let missing = Mid::from("nosuch");
    let error = peer
        .handle
        .declare_send_stream(SendStream::new(missing))
        .await
        .expect_err("a mid the session never had cannot carry a stream");

    assert!(matches!(error, TransportError::UnknownMid(mid) if mid == missing));

    assert!(matches!(
        peer.handle.media(missing).await,
        Err(TransportError::UnknownMid(_))
    ));
    assert!(!peer.handle.remove_send_stream(missing, None).await.unwrap());

    peer.handle
        .add_remote_candidate(String::new())
        .await
        .expect("the peer survives the refusal and keeps answering commands");
}

#[tokio::test]
async fn a_declared_stream_keeps_the_ssrc_that_was_offered() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber(None)).unwrap();

    let local = peer
        .handle
        .negotiate(outgoing(MediaKind::Video, "video"))
        .await
        .unwrap();
    let mid = local.mids[0];

    let mut client = answering_client(&crypto, 40100);
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&local.sdp).expect("our offer parses"))
        .expect("the client answers");
    peer.handle
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let declared = peer
        .handle
        .declare_send_stream(SendStream::new(mid))
        .await
        .expect("the negotiated mid accepts a send stream");

    assert_eq!(declared.mid, mid);
    assert!(declared.negotiated, "the sdp already allocated the ssrc");
    assert!(declared.rtx.is_some(), "video needs an rtx ssrc to resend");
    assert!(
        local.sdp.contains(&format!("a=ssrc:{}", *declared.ssrc)),
        "the declared ssrc must be the one we put in the offer"
    );
    assert!(
        local
            .sdp
            .contains(&format!("a=ssrc:{}", *declared.rtx.unwrap()))
    );

    let media = peer.handle.media(mid).await.unwrap();
    assert_eq!(media.kind, MediaKind::Video);
    assert_eq!(media.direction, Direction::SendOnly);
    assert!(
        !media.params.is_empty(),
        "the answer must leave us at least one payload type"
    );

    assert!(peer.handle.remove_send_stream(mid, None).await.unwrap());
    assert!(!peer.handle.remove_send_stream(mid, None).await.unwrap());
}

#[tokio::test]
async fn a_second_negotiation_waits_for_the_answer_to_the_first() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) =
        peer_actor(&crypto, &udp, PeerConfig::subscriber(None)).await;

    let (reply, first) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "video")),
        reply,
    });
    let first = first.await.unwrap().expect("the first offer is created");

    let (reply, mut second) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Audio, "audio")),
        reply,
    });

    assert!(
        second.try_recv().is_err(),
        "a second offer must not be created while the first is unanswered"
    );
    assert_eq!(actor.renegotiation.queued(), 1);
    assert!(actor.renegotiation.is_pending());

    let mut client = answering_client(&crypto, 40101);
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&first.sdp).expect("our offer parses"))
        .expect("the client answers the first offer");

    let (reply, applied) = oneshot::channel();
    actor.on_command(PeerCommand::AcceptAnswer {
        sdp: answer.to_sdp_string(),
        reply,
    });
    applied.await.unwrap().expect("the answer applies");

    let second = second.await.unwrap().expect("the queued offer runs next");

    assert_ne!(second.mids[0], first.mids[0]);
    assert_eq!(actor.renegotiation.queued(), 0);
    assert!(actor.renegotiation.is_pending());
    assert!(
        actor
            .declare_send_stream(SendStream::new(first.mids[0]))
            .is_ok(),
        "the stream from the first negotiation stays declarable"
    );
}

#[tokio::test]
async fn an_unanswered_offer_is_held_rather_than_rolled_back() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, mut events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig {
            negotiation_timeout: Duration::from_millis(10),
            ..PeerConfig::subscriber(None)
        },
    )
    .await;

    let (reply, first) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "video")),
        reply,
    });
    let first = first.await.unwrap().expect("the first offer is created");

    let (reply, second) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Audio, "audio")),
        reply,
    });

    actor.expire_negotiation(Instant::now() + Duration::from_millis(11));

    assert!(
        matches!(
            second.await.unwrap(),
            Err(TransportError::NegotiationFailed)
        ),
        "a queued offer must never take the place of one the remote may have applied"
    );
    assert!(
        actor.renegotiation.is_pending(),
        "the unanswered offer must stay held so str0m never releases mid {}",
        first.mids[0]
    );
    assert!(actor.renegotiation.has_failed());
    assert_eq!(
        actor.renegotiation.deadline(),
        None,
        "a failed negotiation must not keep waking the peer up"
    );

    let (reply, later) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "later")),
        reply,
    });
    assert!(matches!(
        later.await.unwrap(),
        Err(TransportError::NegotiationFailed)
    ));

    assert!(
        matches!(
            events.recv_control().await,
            Some(PeerEvent::NegotiationFailed)
        ),
        "the session must be told its negotiation is beyond repair"
    );
}

#[tokio::test]
async fn a_late_answer_puts_the_session_back_in_business() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig {
            negotiation_timeout: Duration::from_millis(10),
            ..PeerConfig::subscriber(None)
        },
    )
    .await;

    let (reply, first) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "video")),
        reply,
    });
    let first = first.await.unwrap().expect("the first offer is created");

    actor.expire_negotiation(Instant::now() + Duration::from_millis(11));
    assert!(actor.renegotiation.has_failed());

    let mut client = answering_client(&crypto, 40102);
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&first.sdp).expect("our offer parses"))
        .expect("the client answers late");

    let (reply, applied) = oneshot::channel();
    actor.on_command(PeerCommand::AcceptAnswer {
        sdp: answer.to_sdp_string(),
        reply,
    });
    applied
        .await
        .unwrap()
        .expect("the late answer still applies");

    assert!(!actor.renegotiation.has_failed());
    assert!(
        actor
            .declare_send_stream(SendStream::new(first.mids[0]))
            .is_ok(),
        "the media section the remote answered must be the one we offered"
    );
}

#[tokio::test]
async fn a_backlog_of_renegotiations_is_refused_rather_than_queued_forever() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig {
            negotiation_backlog: 1,
            ..PeerConfig::subscriber(None)
        },
    )
    .await;

    let (reply, first) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "one")),
        reply,
    });
    let _first = first.await.unwrap().expect("the first offer is created");

    let (reply, _queued) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "two")),
        reply,
    });

    let (reply, refused) = oneshot::channel();
    actor.on_command(PeerCommand::Negotiate {
        request: Box::new(outgoing(MediaKind::Video, "three")),
        reply,
    });

    assert!(matches!(
        refused.await.unwrap(),
        Err(TransportError::NegotiationBacklog)
    ));
}

fn settings(initial: Bitrate) -> BweSettings {
    BweSettings::new(initial)
}

#[tokio::test]
async fn a_publisher_is_never_built_with_an_estimator_or_a_real_pacer() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();

    let error = spawn_peer(
        &crypto,
        &udp,
        PeerConfig {
            bwe: Some(settings(Bitrate::kbps(600))),
            ..PeerConfig::publisher()
        },
    )
    .expect_err("a publisher sends nothing but rtcp and must keep the null pacer");

    assert!(matches!(error, TransportError::BweOnPublisher));
    assert_eq!(udp.router().session_count(), 0);

    let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();
    assert!(!peer.handle.estimates_bandwidth());
    assert_eq!(peer.handle.bwe(), None);
}

#[tokio::test]
async fn a_subscriber_carries_the_estimator_settings_it_was_built_with() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();

    let plain = spawn_peer(&crypto, &udp, PeerConfig::subscriber(None)).unwrap();
    assert!(!plain.handle.estimates_bandwidth());

    let bwe = settings(Bitrate::kbps(900));
    let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber(Some(bwe))).unwrap();

    assert!(peer.handle.estimates_bandwidth());
    assert_eq!(peer.handle.bwe(), Some(bwe));
    assert_eq!(
        PeerConfig::subscriber(None).stats_interval,
        Some(SUBSCRIBER_STATS_INTERVAL),
        "the congestion interpreter samples once a second, not once every two"
    );
    assert_eq!(
        PeerConfig::publisher().stats_interval,
        Some(PUBLISHER_STATS_INTERVAL)
    );
}

#[tokio::test]
async fn steering_an_estimator_that_was_never_built_is_refused() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber(None)).unwrap();

    assert!(matches!(
        peer.handle.set_desired_bitrate(Bitrate::mbps(3)),
        Err(TransportError::BweDisabled)
    ));
    assert!(matches!(
        peer.handle.reset_bwe(Bitrate::kbps(600)),
        Err(TransportError::BweDisabled)
    ));

    assert!(
        peer.handle.send_queues().await.unwrap().is_empty(),
        "a peer with no declared send streams has nothing queued"
    );
}

#[tokio::test]
async fn a_desired_bitrate_reaches_str0m_and_leaves_the_peer_serving() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) = peer_actor(
        &crypto,
        &udp,
        PeerConfig::subscriber(Some(settings(Bitrate::kbps(600)))),
    )
    .await;

    actor.on_command(PeerCommand::SetDesiredBitrate(Bitrate::mbps(4)));
    actor.on_command(PeerCommand::ResetBwe(Bitrate::kbps(800)));

    let (reply, queues) = oneshot::channel();
    actor.on_command(PeerCommand::QueueQuery { reply });

    assert!(
        queues.await.unwrap().is_empty(),
        "str0m accepted both bwe commands and the actor kept answering"
    );
}

#[tokio::test]
async fn steering_commands_on_a_peer_without_bwe_are_ignored_rather_than_fatal() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let (_handle, mut actor, _events) = peer_actor(&crypto, &udp, PeerConfig::publisher()).await;

    actor.on_command(PeerCommand::SetDesiredBitrate(Bitrate::mbps(4)));
    actor.on_command(PeerCommand::ResetBwe(Bitrate::kbps(800)));

    let (reply, queues) = oneshot::channel();
    actor.on_command(PeerCommand::QueueQuery { reply });

    assert!(queues.await.unwrap().is_empty());
}

#[tokio::test]
async fn the_desired_bitrate_is_clamped_before_it_leaves_the_handle() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(
        &crypto,
        &udp,
        PeerConfig::subscriber(Some(settings(Bitrate::kbps(600)))),
    )
    .unwrap();

    peer.handle
        .set_desired_bitrate(Bitrate::ZERO)
        .expect("a zero wish is floored rather than refused");

    assert_eq!(
        settings(Bitrate::kbps(600)).desired,
        Bitrate::kbps(600),
        "the build time wish arms str0m's prober, which refuses a first ever zero"
    );
}

#[tokio::test]
async fn a_subscriber_offers_transport_wide_congestion_control_on_every_m_line() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(
        &crypto,
        &udp,
        PeerConfig::subscriber(Some(settings(Bitrate::kbps(600)))),
    )
    .unwrap();

    let mut request = outgoing(MediaKind::Video, "video");
    request.add_media.push(MediaSpec {
        kind: MediaKind::Audio,
        direction: Direction::SendOnly,
        stream_id: Some("stream".to_owned()),
        track_id: Some("audio".to_owned()),
        simulcast: None,
    });

    let local = peer.handle.negotiate(request).await.unwrap();

    assert!(
        local.sdp.contains("transport-wide-cc-extensions-01"),
        "without the twcc header extension str0m never registers a sent packet, \
             and the estimator has nothing to work from: {}",
        local.sdp
    );

    let feedback = local
        .sdp
        .lines()
        .filter(|line| line.trim().ends_with("transport-cc"))
        .count();

    assert!(
        feedback >= 2,
        "both the video and the audio m-line must ask the client for twcc feedback: {}",
        local.sdp
    );
}

#[tokio::test]
async fn a_declared_send_stream_is_visible_to_a_queue_query_until_it_is_removed() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(
        &crypto,
        &udp,
        PeerConfig::subscriber(Some(settings(Bitrate::kbps(600)))),
    )
    .unwrap();

    let local = peer
        .handle
        .negotiate(outgoing(MediaKind::Video, "video"))
        .await
        .unwrap();
    let mid = local.mids[0];

    let mut client = answering_client(&crypto, 40120);
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&local.sdp).expect("our offer parses"))
        .expect("the client answers");
    peer.handle
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let declared = peer
        .handle
        .declare_send_stream(SendStream::new(mid))
        .await
        .unwrap();

    assert!(
        declared.rtx.is_some(),
        "without an rtx ssrc str0m has no padding queue and never probes"
    );

    let queues = peer.handle.send_queues().await.unwrap();
    assert_eq!(queues.len(), 1);
    assert_eq!(queues[0].mid, mid);
    assert_eq!(queues[0].bytes, 0);
    assert_eq!(queues[0].packets, 0);

    assert!(peer.handle.remove_send_stream(mid, None).await.unwrap());
    assert!(peer.handle.send_queues().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_audio_stream_gets_an_rtx_ssrc_only_when_the_codec_negotiated_one() {
    let crypto = TransportCrypto::new().unwrap();
    let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
    let peer = spawn_peer(
        &crypto,
        &udp,
        PeerConfig::subscriber(Some(settings(Bitrate::kbps(600)))),
    )
    .unwrap();

    let local = peer
        .handle
        .negotiate(outgoing(MediaKind::Audio, "audio"))
        .await
        .unwrap();
    let mid = local.mids[0];

    let mut client = answering_client(&crypto, 40121);
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&local.sdp).expect("our offer parses"))
        .expect("the client answers");
    peer.handle
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let media = peer.handle.media(mid).await.unwrap();
    let resendable = media.params.iter().any(|params| params.resend().is_some());

    let declared = peer
        .handle
        .declare_send_stream(SendStream::new(mid))
        .await
        .unwrap();

    assert_eq!(
        declared.rtx.is_some(),
        resendable,
        "an rtx ssrc without an rtx payload type is dead weight, and with one it is \
             what lets an audio only subscriber probe at all"
    );
}
