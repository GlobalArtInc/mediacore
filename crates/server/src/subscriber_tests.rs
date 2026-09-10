use super::*;

use mediacore_config::Config;
use mediacore_room::{ParticipantSpec, SignalRx};
use mediacore_transport::str0m::format::{CodecSpec, FormatParams};
use mediacore_transport::str0m::media::Frequency;

use crate::forward::TrackSource;
use crate::simulcast::LayerMap;

const ROOM: &str = "demo";

#[test]
fn a_republished_camera_does_not_rearm_the_estimator() {
    let mut session = SubscriberSession::new(true);

    assert!(
        session.claim_rearm(),
        "the first video subscription restarts the estimator"
    );
    assert!(
        !session.claim_rearm(),
        "a track republished under a new sid is not a new subscriber transport"
    );

    session.bwe_rearmed = false;

    assert!(
        session.claim_rearm(),
        "a subscriber transport built from scratch gets a fresh estimator"
    );
}

async fn app_state(negotiation_timeout: u32) -> AppState {
    let mut config =
        Config::parse("keys:\n  devkey: devsecretdevsecretdevsecretdevsecret\n").unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    // A directory of this server's own for session records: a test must
    // never write into the tree, and two servers sharing a store would
    // read each other's meetings back.
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();
    config.rtc.negotiation_timeout = negotiation_timeout;

    AppState::start(config)
        .await
        .expect("the media engine starts")
}

fn spec(identity: &str) -> ParticipantSpec {
    ParticipantSpec {
        identity: identity.to_owned(),
        name: identity.to_owned(),
        permission: mediacore::ParticipantPermission {
            can_subscribe: true,
            can_publish: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn add_track(cid: &str) -> mediacore::AddTrackRequest {
    mediacore::AddTrackRequest {
        cid: cid.to_owned(),
        name: cid.to_owned(),
        r#type: mediacore::TrackType::Video as i32,
        source: mediacore::TrackSource::Camera as i32,
        ..Default::default()
    }
}

fn drain(rx: &mut SignalRx) {
    while rx.try_recv().is_ok() {}
}

fn offers(rx: &mut SignalRx) -> Vec<mediacore::SessionDescription> {
    let mut offers = Vec::new();

    while let Ok(response) = rx.try_recv() {
        if let Some(signal_response::Message::Offer(offer)) = response.message {
            offers.push(offer);
        }
    }

    offers
}

fn params(pt: u8, codec: Codec, clock_rate: Frequency) -> PayloadParams {
    PayloadParams::new(
        pt.into(),
        None,
        CodecSpec {
            codec,
            clock_rate,
            channels: None,
            format: FormatParams::default(),
        },
    )
}

#[test]
fn the_payload_type_is_the_one_the_subscriber_negotiated_for_that_codec() {
    let offered = [
        params(111, Codec::Opus, Frequency::FORTY_EIGHT_KHZ),
        params(96, Codec::Vp8, Frequency::NINETY_KHZ),
        params(97, Codec::Rtx, Frequency::NINETY_KHZ),
    ];

    assert_eq!(
        payload_type(&offered, "video/VP8"),
        Some((96u8.into(), 90_000))
    );
    assert_eq!(
        payload_type(&offered, "audio/opus"),
        Some((111u8.into(), 48_000))
    );
    assert_eq!(payload_type(&offered, "video/H264"), None);
    assert_eq!(payload_type(&[], "audio/opus"), None);
}

#[tokio::test]
async fn an_unanswered_subscriber_offer_never_moves_the_track_to_another_mid() {
    let state = app_state(0).await;
    let publisher = state
        .media
        .spawn_publisher()
        .expect("a publisher transport");

    let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
    let (bob_tx, mut bob_rx) = mediacore_room::signal_channel();
    let alice = state
        .registry
        .join(ROOM, spec("alice"), alice_tx)
        .expect("alice joins");
    let bob = state
        .registry
        .join(ROOM, spec("bob"), bob_tx)
        .expect("bob joins");

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &add_track("cid-camera"))
        .expect("alice publishes");
    state.router.register_source(
        &track.sid,
        TrackSource::new(
            publisher.handle.clone(),
            Mid::from("0"),
            TrackKind::Video,
            "video/VP8",
            &alice.participant.sid,
            LayerMap::build(&track.layers, &[]),
            state.source_config(track.source),
        ),
    );

    let mut session = SubscriberSession::new(false);
    bob.participant
        .add_desired_subscriptions([track.sid.clone()]);
    drain(&mut bob_rx);

    session.reconcile(&state, &bob).await;

    let offered = offers(&mut bob_rx);
    assert_eq!(offered.len(), 1, "the subscription was never offered");
    let first_mid = session
        .pending
        .as_ref()
        .expect("an offer is outstanding")
        .mid;

    session.reconcile(&state, &bob).await;

    assert!(
        offers(&mut bob_rx).is_empty(),
        "the track was re-offered on a media section the client never saw, \
             leaving mid {first_mid} orphaned in the client's peer connection"
    );
    assert!(session.pending.is_none());

    session.reconcile(&state, &bob).await;
    assert!(offers(&mut bob_rx).is_empty());

    publisher.handle.close();
}

#[tokio::test]
async fn a_negotiation_failure_after_the_answer_landed_is_ignored() {
    let state = app_state(15).await;
    let (tx, mut rx) = mediacore_room::signal_channel();
    let joined = state
        .registry
        .join(ROOM, spec("bob"), tx)
        .expect("bob joins");

    let mut session = SubscriberSession::new(false);
    assert!(session.pending.is_none());
    drain(&mut rx);

    assert!(session.on_event(&state, &joined, PeerEvent::NegotiationFailed));

    assert!(
        !session.failed,
        "a stale negotiation failure must not condemn a healthy session"
    );
    assert!(
        rx.try_recv().is_err(),
        "the session was told to reconnect after its answer had already landed"
    );
}

#[tokio::test]
async fn a_subscriber_that_never_answers_is_asked_to_reconnect() {
    let state = app_state(0).await;
    let (tx, mut rx) = mediacore_room::signal_channel();
    let joined = state
        .registry
        .join(ROOM, spec("bob"), tx)
        .expect("bob joins");

    let mut session = SubscriberSession::new(false);
    let mid = Mid::from("0");
    session.tracks_by_mid.insert(mid, "TR_camera".to_owned());
    session.pending = Some(Pending {
        track_sid: "TR_camera".to_owned(),
        publisher_sid: "PA_alice".to_owned(),
        mid,
        kind: TrackKind::Video,
        mime_type: "video/VP8".to_owned(),
        id: 1,
        since: Instant::now(),
    });
    drain(&mut rx);

    session.reconcile(&state, &joined).await;

    match rx
        .try_recv()
        .expect("the session is told to reconnect")
        .message
    {
        Some(signal_response::Message::Leave(request)) => {
            assert!(request.can_reconnect);
            assert_eq!(
                request.action,
                mediacore::leave_request::Action::Reconnect as i32
            );
            assert_eq!(
                request.reason,
                mediacore::DisconnectReason::StateMismatch as i32
            );
        }
        other => panic!("expected a leave request, got {other:?}"),
    }

    assert!(rx.try_recv().is_err(), "nothing follows the leave request");
    assert!(session.pending.is_none());
}

/// The `msid` a subscription is offered under carries the participant and the
/// track, joined by a pipe. It is what a client takes apart to learn who is
/// talking, and `@livekit/rtc-node` drops the track on the floor when it does
/// not come apart: `unpack_stream_id` returns nothing for a stream without the
/// separator and the receiver returns before the room ever hears about it. The
/// publication sits on the roster with `subscribed` false for ever and no
/// frame arrives, with nothing said about why on either side of the wire. The
/// browser survives it only because it falls back to the track id.
#[tokio::test]
async fn a_subscription_is_offered_under_the_participant_and_the_track_together() {
    let state = app_state(0).await;
    let publisher = state
        .media
        .spawn_publisher()
        .expect("a publisher transport");

    let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
    let (bob_tx, mut bob_rx) = mediacore_room::signal_channel();
    let alice = state
        .registry
        .join(ROOM, spec("alice"), alice_tx)
        .expect("alice joins");
    let bob = state
        .registry
        .join(ROOM, spec("bob"), bob_tx)
        .expect("bob joins");

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &add_track("cid-camera"))
        .expect("alice publishes");
    state.router.register_source(
        &track.sid,
        TrackSource::new(
            publisher.handle.clone(),
            Mid::from("0"),
            TrackKind::Video,
            "video/VP8",
            &alice.participant.sid,
            LayerMap::build(&track.layers, &[]),
            state.source_config(track.source),
        ),
    );

    let mut session = SubscriberSession::new(false);
    bob.participant
        .add_desired_subscriptions([track.sid.clone()]);
    drain(&mut bob_rx);

    session.reconcile(&state, &bob).await;

    let offered = offers(&mut bob_rx);
    assert_eq!(offered.len(), 1, "the subscription was never offered");

    let stream = format!("{}|{}", alice.participant.sid, track.sid);
    assert!(
        offered[0].sdp.contains(&format!("a=msid:{stream} ")),
        "the offer does not name the participant and the track together, so a client that \
         takes the stream apart learns nothing and never surfaces the track:\n{}",
        offered[0].sdp
    );
    assert_eq!(
        offered[0].mid_to_track_id.values().collect::<Vec<&String>>(),
        vec![&track.sid],
        "the mid the track was offered on is not the one it was announced under"
    );

    publisher.handle.close();
}

/// Both halves, and only one separator: the sids carry none of their own, so a
/// client splitting on the first pipe gets exactly the two it was promised.
#[test]
fn a_stream_name_comes_apart_into_the_two_sids_it_was_made_from() {
    let packed = stream_id("PA_a1b2c3d4e5f6", "TR_AM9f8e7d6c5b4a");

    assert_eq!(packed, "PA_a1b2c3d4e5f6|TR_AM9f8e7d6c5b4a");
    assert_eq!(
        packed.split(STREAM_SEPARATOR).collect::<Vec<&str>>(),
        vec!["PA_a1b2c3d4e5f6", "TR_AM9f8e7d6c5b4a"]
    );
}
