use super::*;

use std::sync::OnceLock;

use str0m::ice::{StunMessageBuilder, TransId};

const LOCAL_UFRAG: &str = "localufrag00";
const REMOTE_UFRAG: &str = "remoteufrag0";
const PASSWORD: &str = "thepassword0000000000";

fn creds(ufrag: &str) -> IceCreds {
    IceCreds {
        ufrag: ufrag.to_owned(),
        pass: PASSWORD.to_owned(),
    }
}

fn sha1_hmac(key: &[u8], payloads: &[&[u8]]) -> [u8; 20] {
    static PROVIDER: OnceLock<str0m::crypto::CryptoProvider> = OnceLock::new();
    let provider = PROVIDER.get_or_init(str0m::crypto::from_feature_flags);
    provider.sha1_hmac_provider.sha1_hmac(key, payloads)
}

fn stun_binding_request(local_ufrag: &str) -> Vec<u8> {
    signed_stun_binding_request(local_ufrag, PASSWORD)
}

fn signed_stun_binding_request(local_ufrag: &str, password: &str) -> Vec<u8> {
    let username = format!("{local_ufrag}:{REMOTE_UFRAG}");
    let message = StunMessageBuilder::new()
        .binding()
        .request()
        .username(&username)
        .prio(1)
        .ice_controlling(42)
        .build(TransId::new());

    let mut buf = vec![0u8; 512];
    let len = message
        .to_bytes(Some(password.as_bytes()), &mut buf, sha1_hmac)
        .expect("serialising a stun binding request");
    buf.truncate(len);
    buf
}

fn stun_binding_response() -> Vec<u8> {
    let message = StunMessageBuilder::new()
        .binding()
        .success()
        .xor_mapped_address(addr(6000))
        .build(TransId::new());

    let mut buf = vec![0u8; 512];
    let len = message
        .to_bytes(Some(PASSWORD.as_bytes()), &mut buf, sha1_hmac)
        .expect("serialising a stun binding response");
    buf.truncate(len);
    buf
}

fn unsigned_stun_binding_request(local_ufrag: &str) -> Vec<u8> {
    let username = format!("{local_ufrag}:{REMOTE_UFRAG}");
    let message = StunMessageBuilder::new()
        .binding()
        .request()
        .username(&username)
        .prio(1)
        .ice_controlling(42)
        .build(TransId::new());

    let mut buf = vec![0u8; 512];
    let len = message
        .to_bytes(None, &mut buf, sha1_hmac)
        .expect("serialising an unsigned stun binding request");
    buf.truncate(len);
    buf
}

fn media_datagram() -> Vec<u8> {
    let mut buf = vec![0u8; 40];
    buf[0] = 0x80;
    buf[1] = 96;
    buf
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn routes_stun_to_the_session_owning_the_local_ufrag() {
    let router = Router::new();
    let mut first = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut second = router.register(&creds("otherufrag00"), 8).unwrap();
    let mut first_rx = first.take_receiver().unwrap();
    let mut second_rx = second.take_receiver().unwrap();

    let packet = stun_binding_request(LOCAL_UFRAG);
    let dispatch = router.dispatch(Instant::now(), addr(5000), &packet);

    assert_eq!(dispatch, Dispatch::Routed(first.id()));
    let received = first_rx.try_recv().expect("first session got the datagram");
    assert_eq!(received.source, addr(5000));
    assert_eq!(&received.data[..], &packet[..]);
    assert!(second_rx.try_recv().is_err());
    assert_eq!(router.stats().routed, 1);
}

#[test]
fn drops_malformed_stun_datagrams() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut rx = route.take_receiver().unwrap();

    let mut packet = stun_binding_request(LOCAL_UFRAG);
    packet[4] = 0x00;

    let dispatch = router.dispatch(Instant::now(), addr(5001), &packet);

    assert_eq!(dispatch, Dispatch::StunMalformed);
    assert!(rx.try_recv().is_err());
    assert_eq!(router.stats().stun_malformed, 1);
    assert_eq!(router.address_count(), 0);
}

#[test]
fn drops_stun_for_an_unknown_ufrag() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut rx = route.take_receiver().unwrap();

    let packet = stun_binding_request("nobodysufrag");
    let dispatch = router.dispatch(Instant::now(), addr(5002), &packet);

    assert_eq!(dispatch, Dispatch::UnknownUfrag);
    assert!(rx.try_recv().is_err());
    assert_eq!(router.stats().unknown_ufrag, 1);
    assert_eq!(router.address_count(), 0);
}

#[test]
fn drops_a_truncated_datagram_without_panicking() {
    let router = Router::new();
    let _route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();

    assert_eq!(
        router.dispatch(Instant::now(), addr(5003), &[]),
        Dispatch::UnknownSource
    );
    assert_eq!(
        router.dispatch(Instant::now(), addr(5003), &[0x00, 0x01]),
        Dispatch::UnknownSource
    );
    assert_eq!(router.stats().unknown_source, 2);
}

#[test]
fn learns_the_source_address_from_stun_and_routes_media_by_it() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut rx = route.take_receiver().unwrap();
    let peer = addr(5004);

    let media = media_datagram();
    assert_eq!(
        router.dispatch(Instant::now(), peer, &media),
        Dispatch::UnknownSource
    );

    let packet = stun_binding_request(LOCAL_UFRAG);
    router.dispatch(Instant::now(), peer, &packet);
    assert_eq!(router.session_for_address(&peer), Some(route.id()));

    assert_eq!(
        router.dispatch(Instant::now(), peer, &media),
        Dispatch::Routed(route.id())
    );

    let first = rx.try_recv().unwrap();
    assert!(looks_like_stun(&first.data));
    let second = rx.try_recv().unwrap();
    assert_eq!(&second.data[..], &media[..]);
}

#[test]
fn a_stun_datagram_with_a_bad_integrity_never_moves_the_address() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut rx = route.take_receiver().unwrap();
    let owner = addr(5010);
    let attacker = addr(5011);

    router.dispatch(Instant::now(), owner, &stun_binding_request(LOCAL_UFRAG));
    assert_eq!(router.session_for_address(&owner), Some(route.id()));
    assert!(rx.try_recv().is_ok());

    let forged = signed_stun_binding_request(LOCAL_UFRAG, "guessedpassword000000");
    assert_eq!(
        router.dispatch(Instant::now(), attacker, &forged),
        Dispatch::StunUnauthenticated
    );

    let unsigned = unsigned_stun_binding_request(LOCAL_UFRAG);
    assert!(matches!(
        router.dispatch(Instant::now(), attacker, &unsigned),
        Dispatch::StunUnauthenticated | Dispatch::StunMalformed
    ));

    assert_eq!(router.session_for_address(&attacker), None);
    assert_eq!(router.session_for_address(&owner), Some(route.id()));
    assert_eq!(router.address_count(), 1);
    assert_eq!(router.stats().stun_unauthenticated, 1);
    assert!(rx.try_recv().is_err());

    assert_eq!(
        router.dispatch(Instant::now(), attacker, &media_datagram()),
        Dispatch::UnknownSource
    );
}

#[test]
fn a_stun_response_reaches_the_session_that_owns_its_source_address() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut rx = route.take_receiver().unwrap();
    let peer = addr(5013);
    let stranger = addr(5014);

    let response = stun_binding_response();
    assert_eq!(
        router.dispatch(Instant::now(), peer, &response),
        Dispatch::StunWithoutUsername
    );

    router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG));
    assert!(rx.try_recv().is_ok());

    assert_eq!(
        router.dispatch(Instant::now(), peer, &response),
        Dispatch::Routed(route.id())
    );
    assert!(rx.try_recv().is_ok());

    assert_eq!(
        router.dispatch(Instant::now(), stranger, &response),
        Dispatch::StunWithoutUsername
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_later_session_takes_over_a_shared_source_address() {
    let router = Router::new();
    let mut first = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut second = router.register(&creds("secondufrag0"), 8).unwrap();
    let mut first_rx = first.take_receiver().unwrap();
    let mut second_rx = second.take_receiver().unwrap();
    let peer = addr(5005);

    router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG));
    assert_eq!(router.session_for_address(&peer), Some(first.id()));
    assert!(first_rx.try_recv().is_ok());

    router.dispatch(Instant::now(), peer, &stun_binding_request("secondufrag0"));
    assert_eq!(router.session_for_address(&peer), Some(second.id()));
    assert!(second_rx.try_recv().is_ok());

    let media = media_datagram();
    assert_eq!(
        router.dispatch(Instant::now(), peer, &media),
        Dispatch::Routed(second.id())
    );
    assert!(first_rx.try_recv().is_err());
    assert_eq!(&second_rx.try_recv().unwrap().data[..], &media[..]);
    assert_eq!(router.address_count(), 1);
}

#[test]
fn rekeying_moves_a_session_to_its_restarted_credentials() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    let mut rx = route.take_receiver().unwrap();
    let peer = addr(5012);

    router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG));
    assert!(rx.try_recv().is_ok());

    let restarted = IceCreds {
        ufrag: "restartufrag".to_owned(),
        pass: "restartedpassword0000".to_owned(),
    };
    assert!(route.rekey(&restarted));

    assert_eq!(route.ufrag(), restarted.ufrag);
    assert_eq!(router.session_for_ufrag(LOCAL_UFRAG), None);
    assert_eq!(router.session_for_ufrag(&restarted.ufrag), Some(route.id()));
    assert_eq!(router.session_count(), 1);

    assert_eq!(
        router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG)),
        Dispatch::UnknownUfrag
    );
    assert_eq!(
        router.dispatch(
            Instant::now(),
            peer,
            &signed_stun_binding_request(&restarted.ufrag, &restarted.pass)
        ),
        Dispatch::Routed(route.id())
    );
    assert!(rx.try_recv().is_ok());

    assert_eq!(
        router.dispatch(
            Instant::now(),
            peer,
            &signed_stun_binding_request(&restarted.ufrag, PASSWORD)
        ),
        Dispatch::StunUnauthenticated
    );
}

#[test]
fn rekeying_frees_the_previous_ufrag_for_a_new_session() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();

    assert!(route.rekey(&IceCreds {
        ufrag: "restartufrag".to_owned(),
        pass: PASSWORD.to_owned(),
    }));

    let reused = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
    assert_ne!(reused.id(), route.id());
    assert_eq!(router.session_for_ufrag(LOCAL_UFRAG), Some(reused.id()));
}

#[test]
fn closing_a_session_clears_both_tables() {
    let router = Router::new();
    let peer = addr(5006);

    {
        let route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
        router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG));

        assert_eq!(router.session_for_ufrag(LOCAL_UFRAG), Some(route.id()));
        assert_eq!(router.session_for_address(&peer), Some(route.id()));
    }

    assert_eq!(router.session_count(), 0);
    assert_eq!(router.address_count(), 0);
    assert_eq!(router.session_for_ufrag(LOCAL_UFRAG), None);
    assert_eq!(router.session_for_address(&peer), None);

    assert_eq!(
        router.dispatch(Instant::now(), peer, &media_datagram()),
        Dispatch::UnknownSource
    );
    assert_eq!(
        router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG)),
        Dispatch::UnknownUfrag
    );
}

#[test]
fn closing_one_session_leaves_the_other_addresses_alone() {
    let router = Router::new();
    let mut survivor = router.register(&creds("survivor0000"), 8).unwrap();
    let mut survivor_rx = survivor.take_receiver().unwrap();
    let survivor_peer = addr(5007);
    let doomed_peer = addr(5008);

    router.dispatch(
        Instant::now(),
        survivor_peer,
        &stun_binding_request("survivor0000"),
    );

    {
        let _doomed = router.register(&creds(LOCAL_UFRAG), 8).unwrap();
        router.dispatch(
            Instant::now(),
            doomed_peer,
            &stun_binding_request(LOCAL_UFRAG),
        );
        assert_eq!(router.address_count(), 2);
    }

    assert_eq!(router.address_count(), 1);
    assert_eq!(
        router.session_for_address(&survivor_peer),
        Some(survivor.id())
    );

    let media = media_datagram();
    assert_eq!(
        router.dispatch(Instant::now(), survivor_peer, &media),
        Dispatch::Routed(survivor.id())
    );
    assert!(survivor_rx.try_recv().is_ok());
    assert!(survivor_rx.try_recv().is_ok());
}

#[test]
fn a_ufrag_cannot_be_registered_twice() {
    let router = Router::new();
    let _route = router.register(&creds(LOCAL_UFRAG), 8).unwrap();

    assert!(matches!(
        router.register(&creds(LOCAL_UFRAG), 8),
        Err(TransportError::DuplicateUfrag(_))
    ));
}

#[test]
fn a_full_queue_drops_instead_of_blocking() {
    let router = Router::new();
    let mut route = router.register(&creds(LOCAL_UFRAG), 1).unwrap();
    let mut rx = route.take_receiver().unwrap();
    let peer = addr(5009);

    router.dispatch(Instant::now(), peer, &stun_binding_request(LOCAL_UFRAG));
    assert_eq!(
        router.dispatch(Instant::now(), peer, &media_datagram()),
        Dispatch::QueueFull(route.id())
    );
    assert_eq!(router.stats().queue_full, 1);

    assert!(rx.try_recv().is_ok());
    assert_eq!(
        router.dispatch(Instant::now(), peer, &media_datagram()),
        Dispatch::Routed(route.id())
    );
}
