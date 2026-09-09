use super::*;

use std::ops::Deref;

/// A registry with a session store and an event bus of its own.
///
/// Every registry writes down what happens in its rooms and announces it,
/// so a test that wants a registry wants a directory to write into and
/// somewhere for the announcements to go. The directory travels with the
/// registry and goes when it does: two tests sharing one would read each
/// other's meetings back as their own.
///
/// The bus is the real one, with a listener attached: emitting is a
/// mutex and a queue push and needs no runtime, so a plain `#[test]` can
/// watch what a registry announced without an executor anywhere near it.
struct Harness {
    registry: Registry,
    events: mediacore_events::Listener,
    _records: tempfile::TempDir,
}

impl Deref for Harness {
    type Target = Registry;

    fn deref(&self) -> &Registry {
        &self.registry
    }
}

impl Harness {
    /// Everything announced since this was last asked, in order.
    fn announced(&mut self) -> Vec<Arc<Event>> {
        let mut seen = Vec::new();
        while let Some(event) = self.events.try_recv() {
            seen.push(event);
        }

        seen
    }

    /// The names of everything announced since this was last asked.
    fn announced_kinds(&mut self) -> Vec<&'static str> {
        self.announced()
            .iter()
            .map(|event| event.kind.as_str())
            .collect()
    }
}

fn registry(defaults: RoomDefaults) -> Harness {
    let records = tempfile::TempDir::new().expect("a directory for session records");
    let sessions =
        SessionStore::files(records.path(), 64 * 1024).expect("an empty session store opens");
    let events = Arc::new(Bus::new());
    let listener = events.listen();

    Harness {
        registry: Registry::new(defaults, Arc::new(sessions), events),
        events: listener,
        _records: records,
    }
}

fn displacing() -> RoomDefaults {
    RoomDefaults {
        duplicate_identity: DuplicateIdentity::Displace,
        ..RoomDefaults::default()
    }
}

fn session_policy(min_humans: u32, end_grace: u32) -> RoomDefaults {
    RoomDefaults {
        min_humans,
        end_grace,
        ..RoomDefaults::default()
    }
}

fn recorder(identity: &str) -> ParticipantSpec {
    ParticipantSpec {
        kind: ParticipantKind::Egress,
        ..spec(identity)
    }
}

fn leaves(rx: &mut SignalRx) -> Vec<mediacore::LeaveRequest> {
    let mut requests = Vec::new();

    while let Ok(response) = rx.try_recv() {
        if let Some(mediacore::signal_response::Message::Leave(request)) = response.message {
            requests.push(request);
        }
    }

    requests
}

fn expect_leave(rx: &mut SignalRx) -> mediacore::LeaveRequest {
    let mut requests = leaves(rx);
    assert_eq!(
        requests.len(),
        1,
        "expected one leave request, got {requests:?}"
    );

    requests.remove(0)
}

fn spec(identity: &str) -> ParticipantSpec {
    ParticipantSpec {
        identity: identity.to_owned(),
        name: identity.to_owned(),
        permission: mediacore::ParticipantPermission {
            can_subscribe: true,
            can_publish: true,
            can_publish_data: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn add_track(
    cid: &str,
    kind: mediacore::TrackType,
    source: mediacore::TrackSource,
) -> mediacore::AddTrackRequest {
    mediacore::AddTrackRequest {
        cid: cid.to_owned(),
        name: cid.to_owned(),
        r#type: kind as i32,
        source: source as i32,
        ..Default::default()
    }
}

fn camera(cid: &str) -> mediacore::AddTrackRequest {
    add_track(
        cid,
        mediacore::TrackType::Video,
        mediacore::TrackSource::Camera,
    )
}

fn drain(rx: &mut SignalRx) {
    while rx.try_recv().is_ok() {}
}

fn unpublished(rx: &mut SignalRx) -> Vec<String> {
    let mut track_sids = Vec::new();

    while let Ok(response) = rx.try_recv() {
        if let Some(mediacore::signal_response::Message::TrackUnpublished(message)) =
            response.message
        {
            track_sids.push(message.track_sid);
        }
    }

    track_sids
}

fn expect_update(rx: &mut SignalRx) -> mediacore::ParticipantUpdate {
    match rx.try_recv().expect("a signal was queued").message {
        Some(mediacore::signal_response::Message::Update(update)) => update,
        other => panic!("expected a participant update, got {other:?}"),
    }
}

#[test]
fn second_participant_sees_the_first() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    assert_eq!(bob.others.len(), 1);
    assert_eq!(bob.others[0].identity, "alice");
    assert_eq!(bob.room.participant_count(), 2);
    assert_eq!(expect_update(&mut alice_rx).participants[0].identity, "bob");
}

#[test]
fn leaving_notifies_others_and_closes_empty_room() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let _ = alice_rx.try_recv();

    registry.leave("demo", &bob.participant.sid);

    let update = expect_update(&mut alice_rx);
    assert_eq!(
        update.participants[0].state,
        mediacore::participant_info::State::Disconnected as i32
    );

    registry.leave("demo", &alice.participant.sid);
    assert!(registry.room("demo").is_none());
}

#[test]
fn rejoining_displaces_the_earlier_session() {
    let registry = registry(displacing());
    let (first_tx, mut first_rx) = signal_channel();
    let (second_tx, _second_rx) = signal_channel();

    let first = registry.join("demo", spec("alice"), first_tx).unwrap();
    let second = registry.join("demo", spec("alice"), second_tx).unwrap();

    assert_ne!(first.participant.sid, second.participant.sid);
    assert_eq!(second.room.participant_count(), 1);

    match first_rx.try_recv().expect("a signal was queued").message {
        Some(mediacore::signal_response::Message::Leave(request)) => {
            assert_eq!(
                request.reason,
                mediacore::DisconnectReason::DuplicateIdentity as i32
            );
        }
        other => panic!("expected a leave request, got {other:?}"),
    }
}

#[test]
fn displacing_a_session_never_tells_the_successor_it_disconnected() {
    let registry = registry(displacing());
    let (first_tx, _first_rx) = signal_channel();
    let (second_tx, mut second_rx) = signal_channel();

    registry.join("demo", spec("alice"), first_tx).unwrap();
    let second = registry.join("demo", spec("alice"), second_tx).unwrap();

    while let Ok(response) = second_rx.try_recv() {
        let Some(mediacore::signal_response::Message::Update(update)) = response.message else {
            continue;
        };

        for info in update.participants {
            assert!(
                info.state != mediacore::participant_info::State::Disconnected as i32
                    || info.sid != second.participant.sid,
                "the new session was told that it had disconnected"
            );
            assert_ne!(
                info.disconnect_reason,
                mediacore::DisconnectReason::DuplicateIdentity as i32,
                "the new session was handed the displacement notice meant for the room"
            );
        }
    }
}

#[test]
fn displacing_a_session_tears_its_tracks_out_of_every_subscription() {
    let registry = registry(displacing());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();
    let (again_tx, _again_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let published = alice
        .room
        .publish_track(&alice.participant.sid, &camera("cid-1"))
        .expect("alice may publish");
    bob.participant
        .add_desired_subscriptions([published.sid.clone()]);
    bob.participant.mark_subscription_active(&published.sid);
    drain(&mut bob_rx);

    let again = registry.join("demo", spec("alice"), again_tx).unwrap();
    assert_ne!(again.participant.sid, alice.participant.sid);

    assert!(
        !bob.participant
            .desired_subscriptions()
            .contains(&published.sid),
        "the subscription to a displaced session's track was left behind"
    );
    assert_eq!(
        bob.participant.subscription_diff().to_unsubscribe,
        vec![published.sid.clone()],
        "bob was never asked to tear the subscription down"
    );

    let update = expect_update(&mut bob_rx);
    assert_eq!(update.participants[0].sid, alice.participant.sid);
    assert_eq!(
        update.participants[0].state,
        mediacore::participant_info::State::Disconnected as i32
    );
    assert_eq!(
        update.participants[0].disconnect_reason,
        mediacore::DisconnectReason::DuplicateIdentity as i32
    );
}

#[test]
fn a_displaced_session_leaving_late_changes_nothing() {
    let registry = registry(displacing());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();
    let (again_tx, _again_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let again = registry.join("demo", spec("alice"), again_tx).unwrap();

    let published = again
        .room
        .publish_track(&again.participant.sid, &camera("cid-2"))
        .expect("the new session may publish");
    bob.participant
        .add_desired_subscriptions([published.sid.clone()]);
    drain(&mut bob_rx);

    registry.leave("demo", &alice.participant.sid);

    assert_eq!(
        bob.participant.desired_subscriptions(),
        vec![published.sid],
        "the stale session took its successor's tracks down with it"
    );
    assert!(
        bob_rx.try_recv().is_err(),
        "a stale leave must not announce anything"
    );
    assert_eq!(
        registry
            .room("demo")
            .expect("the room is still open")
            .participant_count(),
        2
    );
}

#[test]
fn a_displaced_session_cannot_evict_its_successor() {
    let registry = registry(displacing());
    let (first_tx, _first_rx) = signal_channel();
    let (second_tx, _second_rx) = signal_channel();

    let first = registry.join("demo", spec("alice"), first_tx).unwrap();
    let second = registry.join("demo", spec("alice"), second_tx).unwrap();

    registry.leave("demo", &first.participant.sid);

    let room = registry.room("demo").expect("room is still open");
    assert_eq!(room.participant_count(), 1);
    assert_eq!(
        room.participant("alice").unwrap().sid,
        second.participant.sid
    );
}

#[test]
fn honours_disabled_auto_create() {
    let registry = registry(RoomDefaults {
        auto_create: false,
        ..RoomDefaults::default()
    });
    let (tx, _rx) = signal_channel();
    let (again_tx, _again_rx) = signal_channel();

    let result = registry.join("demo", spec("alice"), tx);
    assert!(matches!(result, Err(JoinError::RoomNotFound(_))));

    registry.create("demo", RoomOptions::default(), Adoption::Always);
    assert!(
        registry.join("demo", spec("alice"), again_tx).is_ok(),
        "with auto-create off, creating the room is the only way anybody gets in"
    );
}

#[test]
fn publishing_surfaces_the_track_to_other_participants() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    drain(&mut alice_rx);

    let published = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .expect("bob may publish");

    assert!(published.sid.starts_with(TRACK_SID_PREFIX));
    assert_eq!(published.publisher_identity, "bob");
    assert!(!published.is_negotiated());

    let update = expect_update(&mut alice_rx);
    assert_eq!(update.participants[0].identity, "bob");
    assert_eq!(update.participants[0].tracks.len(), 1);
    assert_eq!(update.participants[0].tracks[0].sid, published.sid);
    assert_eq!(update.participants[0].tracks[0].name, "cid-1");
    assert_eq!(
        update.participants[0].tracks[0].r#type,
        mediacore::TrackType::Video as i32
    );

    assert_eq!(
        bob.room
            .track(&published.sid)
            .map(|(publisher, _)| publisher.sid.clone()),
        Some(bob.participant.sid.clone())
    );
}

#[test]
#[allow(deprecated)]
fn the_layers_a_track_reports_follow_the_ones_that_are_arriving() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let mut request = camera("cid-1");
    request.layers = vec![
        layer(mediacore::VideoQuality::Low, "q", 320, 180),
        layer(mediacore::VideoQuality::Medium, "h", 640, 360),
        layer(mediacore::VideoQuality::High, "f", 1280, 720),
    ];

    let published = bob
        .room
        .publish_track(&bob.participant.sid, &request)
        .expect("bob may publish");
    assert_eq!(published.to_proto().layers.len(), 3);
    assert!(published.to_proto().simulcast);
    drain(&mut alice_rx);

    let live = bob
        .room
        .set_track_live_layers(
            &bob.participant.sid,
            &published.sid,
            vec![
                layer(mediacore::VideoQuality::Low, "q", 320, 180),
                layer(mediacore::VideoQuality::Medium, "h", 640, 360),
            ],
        )
        .expect("the live layers are recorded");

    assert_eq!(live.live_layers.len(), 2);
    assert_eq!(live.layers.len(), 3, "the declared ladder is kept");
    assert!(
        live.to_proto().simulcast,
        "a track whose top layer went quiet is still a simulcast track"
    );

    let update = expect_update(&mut alice_rx);
    let reported = &update.participants[0].tracks[0].layers;
    assert_eq!(reported.len(), 2);
    assert_eq!(reported[1].rid, "h");
    assert!(live.version > published.version);

    assert!(
        bob.room
            .set_track_live_layers(
                &bob.participant.sid,
                &published.sid,
                vec![
                    layer(mediacore::VideoQuality::Low, "q", 320, 180),
                    layer(mediacore::VideoQuality::Medium, "h", 640, 360),
                ],
            )
            .is_none(),
        "an unchanged layer set is not announced again"
    );

    assert_eq!(alice.room.participant_count(), 2);
}

fn layer(
    quality: mediacore::VideoQuality,
    rid: &str,
    width: u32,
    height: u32,
) -> mediacore::VideoLayer {
    mediacore::VideoLayer {
        quality: quality as i32,
        rid: rid.to_owned(),
        width,
        height,
        ..Default::default()
    }
}

#[test]
fn the_publisher_does_not_hear_about_its_own_track() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    drain(&mut bob_rx);

    bob.room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .unwrap();

    assert!(bob_rx.try_recv().is_err());
}

#[test]
fn unpublishing_removes_the_track() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let published = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .unwrap();
    drain(&mut alice_rx);

    let removed = bob
        .room
        .unpublish_track(&bob.participant.sid, &published.sid)
        .expect("the track was published");

    assert_eq!(removed.sid, published.sid);
    assert!(bob.participant.tracks().is_empty());
    assert!(bob.room.track(&published.sid).is_none());
    assert!(
        expect_update(&mut alice_rx).participants[0]
            .tracks
            .is_empty()
    );
    assert!(
        bob.room
            .unpublish_track(&bob.participant.sid, &published.sid)
            .is_none()
    );
}

#[test]
fn muting_updates_the_track() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let published = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .unwrap();
    assert!(!published.muted);
    drain(&mut alice_rx);

    let muted = bob
        .room
        .set_track_muted(&bob.participant.sid, &published.sid, true)
        .expect("the track was published");

    assert!(muted.muted);
    assert!(muted.version > published.version);
    assert!(bob.participant.track(&published.sid).unwrap().muted);

    let update = expect_update(&mut alice_rx);
    assert!(update.participants[0].tracks[0].muted);

    let unmuted = bob
        .room
        .set_track_muted(&bob.participant.sid, &published.sid, false)
        .unwrap();
    assert!(!unmuted.muted);
    assert!(!expect_update(&mut alice_rx).participants[0].tracks[0].muted);
}

#[test]
fn every_change_bumps_the_participant_version() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    drain(&mut alice_rx);

    let mut versions = Vec::new();
    let published = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .unwrap();
    versions.push(expect_update(&mut alice_rx).participants[0].version);

    bob.room
        .set_track_muted(&bob.participant.sid, &published.sid, true)
        .unwrap();
    versions.push(expect_update(&mut alice_rx).participants[0].version);

    bob.participant.set_track_mid(&published.sid, "0").unwrap();

    bob.room
        .unpublish_track(&bob.participant.sid, &published.sid)
        .unwrap();
    versions.push(expect_update(&mut alice_rx).participants[0].version);

    assert!(
        versions.windows(2).all(|pair| pair[1] > pair[0]),
        "versions did not increase: {versions:?}"
    );
}

#[test]
fn the_negotiated_mid_lands_on_the_track() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let published = alice.participant.publish_track(&camera("cid-1")).unwrap();

    let negotiated = alice
        .participant
        .set_track_mid(&published.sid, "0")
        .expect("the track was published");
    alice
        .participant
        .set_track_mime_type(&published.sid, "video/VP8");

    assert!(negotiated.is_negotiated());
    let info = &alice.participant.track_infos()[0];
    assert_eq!(info.mid, "0");
    assert_eq!(info.mime_type, "video/VP8");
    assert_eq!(
        alice
            .participant
            .track_by_cid("cid-1")
            .map(|track| track.sid),
        Some(published.sid)
    );
}

#[test]
fn republishing_the_same_client_id_keeps_the_track_sid() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let first = alice.participant.publish_track(&camera("cid-1")).unwrap();
    let second = alice.participant.publish_track(&camera("cid-1")).unwrap();

    assert_eq!(first.sid, second.sid);
    assert!(second.version > first.version);
    assert_eq!(alice.participant.tracks().len(), 1);
}

#[test]
fn a_participant_without_the_publish_grant_is_refused() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let mut listener = spec("bob");
    listener.permission.can_publish = false;

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", listener, bob_tx).unwrap();

    let refused = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .expect_err("bob may not publish");
    assert!(matches!(refused, PublishError::NotAllowed(_)));
    assert_eq!(
        refused.reason(),
        mediacore::request_response::Reason::NotAllowed
    );
    assert!(bob.participant.tracks().is_empty());

    let mut screen_only = spec("carol");
    screen_only.permission.can_publish_sources = vec![mediacore::TrackSource::ScreenShare as i32];
    let (carol_tx, _carol_rx) = signal_channel();
    let carol = registry.join("demo", screen_only, carol_tx).unwrap();

    assert!(matches!(
        carol.participant.publish_track(&camera("cid-2")),
        Err(PublishError::NotAllowed(_))
    ));
    assert!(
        carol
            .participant
            .publish_track(&add_track(
                "cid-3",
                mediacore::TrackType::Video,
                mediacore::TrackSource::ScreenShare
            ))
            .is_ok()
    );

    let data = add_track(
        "cid-4",
        mediacore::TrackType::Data,
        mediacore::TrackSource::Unknown,
    );
    assert!(matches!(
        alice.participant.publish_track(&data),
        Err(PublishError::UnsupportedKind(_))
    ));
}

#[test]
fn desired_subscriptions_are_diffed_against_the_active_ones() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let camera_track = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .unwrap();
    let microphone_track = bob
        .room
        .publish_track(
            &bob.participant.sid,
            &add_track(
                "cid-2",
                mediacore::TrackType::Audio,
                mediacore::TrackSource::Microphone,
            ),
        )
        .unwrap();

    let mut wanted = bob.room.subscribable_track_sids(&alice.participant.sid);
    wanted.sort();
    let mut published = vec![camera_track.sid.clone(), microphone_track.sid.clone()];
    published.sort();
    assert_eq!(wanted, published);

    alice.participant.set_desired_subscriptions(wanted.clone());
    let diff = alice.participant.subscription_diff();
    assert_eq!(diff.to_subscribe, wanted);
    assert!(diff.to_unsubscribe.is_empty());

    alice
        .participant
        .mark_subscription_active(&camera_track.sid);
    let diff = alice.participant.subscription_diff();
    assert_eq!(diff.to_subscribe, vec![microphone_track.sid.clone()]);
    assert!(diff.to_unsubscribe.is_empty());

    alice
        .participant
        .mark_subscription_active(&microphone_track.sid);
    assert!(alice.participant.subscription_diff().is_empty());

    bob.room
        .unpublish_track(&bob.participant.sid, &camera_track.sid)
        .unwrap();
    let diff = alice.participant.subscription_diff();
    assert!(diff.to_subscribe.is_empty());
    assert_eq!(
        diff.to_unsubscribe,
        vec![camera_track.sid.clone()],
        "an unpublished track stays active until the transport tears it down"
    );

    alice
        .participant
        .mark_subscription_inactive(&camera_track.sid);
    assert!(alice.participant.subscription_diff().is_empty());
    assert_eq!(
        alice.participant.active_subscriptions(),
        vec![microphone_track.sid]
    );
}

#[test]
fn a_departing_publisher_takes_its_tracks_out_of_the_intent() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let track = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .unwrap();
    alice
        .participant
        .set_desired_subscriptions([track.sid.clone()]);
    alice.participant.mark_subscription_active(&track.sid);

    registry.leave("demo", &bob.participant.sid);

    assert!(alice.participant.desired_subscriptions().is_empty());
    assert_eq!(
        alice.participant.subscription_diff().to_unsubscribe,
        vec![track.sid]
    );
}

#[test]
fn a_publisher_decides_who_may_listen_and_the_gate_outlasts_every_intent() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();
    let (carol_tx, _carol_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let carol = registry.join("demo", spec("carol"), carol_tx).unwrap();

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &camera("cid-1"))
        .unwrap();

    bob.participant
        .set_desired_subscriptions([track.sid.clone()]);
    assert!(alice.room.can_subscribe(&bob.participant.sid, &track.sid));
    assert!(alice.room.can_subscribe(&carol.participant.sid, &track.sid));

    let cut = alice
        .room
        .set_subscription_permission(
            &alice.participant.sid,
            &mediacore::SubscriptionPermission {
                all_participants: false,
                track_permissions: vec![mediacore::TrackPermission {
                    participant_identity: "carol".to_owned(),
                    all_tracks: true,
                    ..Default::default()
                }],
            },
        )
        .expect("alice is in the room");

    assert_eq!(cut, 1, "only bob was subscribed and only bob is cut");
    assert!(!alice.room.can_subscribe(&bob.participant.sid, &track.sid));
    assert!(alice.room.can_subscribe(&carol.participant.sid, &track.sid));
    assert!(
        alice
            .room
            .subscribable_track_sids(&bob.participant.sid)
            .is_empty(),
        "auto-subscription must not put the track back"
    );
    assert!(
        !bob.participant
            .subscription_diff()
            .to_subscribe
            .contains(&track.sid),
        "the intent behind the running subscription is gone"
    );

    let told = std::iter::from_fn(|| bob_rx.try_recv().ok())
        .filter_map(|response| response.message)
        .any(|message| {
            matches!(
                message,
                mediacore::signal_response::Message::SubscriptionPermissionUpdate(update)
                    if update.track_sid == track.sid && !update.allowed
            )
        });
    assert!(told, "bob is told the track is no longer his to hear");

    alice
        .room
        .set_subscription_permission(
            &alice.participant.sid,
            &mediacore::SubscriptionPermission {
                all_participants: true,
                track_permissions: Vec::new(),
            },
        )
        .expect("alice is in the room");

    assert!(alice.room.can_subscribe(&bob.participant.sid, &track.sid));
    assert_eq!(
        alice.room.subscribable_track_sids(&bob.participant.sid),
        vec![track.sid.clone()],
        "opening the audience lets auto-subscription find the track again"
    );
}

#[test]
fn a_data_packet_goes_to_the_people_it_named_and_never_back_to_its_sender() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (second_tx, _second_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let second = registry.join("demo", spec("alice"), second_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    assert_eq!(second.participant.identity, "alice#2");

    let everyone = alice
        .room
        .data_recipients(Some(&alice.participant.sid), &[]);
    assert_eq!(everyone.len(), 2);
    assert!(
        !everyone
            .iter()
            .any(|participant| participant.sid == alice.participant.sid)
    );

    let to_alice = alice
        .room
        .data_recipients(Some(&bob.participant.sid), &["alice".to_owned()]);
    assert_eq!(
        to_alice.len(),
        2,
        "a name is a person, so both of alice's connections are named"
    );

    let named_twice = alice.room.data_recipients(
        Some(&bob.participant.sid),
        &["alice".to_owned(), "alice#2".to_owned()],
    );
    assert_eq!(named_twice.len(), 2, "naming a connection twice sends once");

    let to_nobody = alice.room.data_recipients(None, &["dave".to_owned()]);
    assert!(to_nobody.is_empty());

    let to_self = alice
        .room
        .data_recipients(Some(&bob.participant.sid), &["bob".to_owned()]);
    assert!(
        to_self.is_empty(),
        "a sender never receives their own packet"
    );
}

#[test]
fn the_room_answers_who_may_subscribe_to_what() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let mut deaf = spec("bob");
    deaf.permission.can_subscribe = false;

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", deaf, bob_tx).unwrap();

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &camera("cid-1"))
        .unwrap();

    assert!(!alice.room.can_subscribe(&bob.participant.sid, &track.sid));
    assert!(
        !alice.room.can_subscribe(&alice.participant.sid, &track.sid),
        "a participant does not subscribe to its own track"
    );
    assert!(!alice.room.can_subscribe("PA_unknown", &track.sid));
    assert!(
        !alice
            .room
            .can_subscribe(&bob.participant.sid, "TR_VCmissing")
    );
    assert!(
        alice
            .room
            .subscribable_track_sids(&bob.participant.sid)
            .is_empty()
    );

    let request = mediacore::UpdateSubscription {
        track_sids: vec![track.sid.clone()],
        subscribe: true,
        participant_tracks: vec![mediacore::ParticipantTracks {
            participant_sid: alice.participant.sid.clone(),
            track_sids: vec![track.sid.clone()],
        }],
    };
    assert_eq!(
        alice
            .room
            .update_subscriptions(&bob.participant.sid, &request),
        vec![track.sid.clone()]
    );
    assert!(bob.participant.desired_subscriptions().is_empty());
}

#[test]
fn a_subscription_request_records_intent_for_allowed_tracks() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &camera("cid-1"))
        .unwrap();

    let subscribe = mediacore::UpdateSubscription {
        track_sids: vec![track.sid.clone(), "TR_VCmissing".to_owned()],
        subscribe: true,
        participant_tracks: Vec::new(),
    };
    assert_eq!(
        alice
            .room
            .update_subscriptions(&bob.participant.sid, &subscribe),
        vec!["TR_VCmissing".to_owned()]
    );
    assert_eq!(
        bob.participant.desired_subscriptions(),
        vec![track.sid.clone()]
    );

    let unsubscribe = mediacore::UpdateSubscription {
        track_sids: vec![track.sid.clone()],
        subscribe: false,
        participant_tracks: Vec::new(),
    };
    assert!(
        alice
            .room
            .update_subscriptions(&bob.participant.sid, &unsubscribe)
            .is_empty()
    );
    assert!(bob.participant.desired_subscriptions().is_empty());
}

fn pong() -> mediacore::signal_response::Message {
    mediacore::signal_response::Message::Pong(1)
}

fn answer() -> mediacore::signal_response::Message {
    mediacore::signal_response::Message::Answer(mediacore::SessionDescription {
        r#type: "answer".to_owned(),
        sdp: "v=0\r\n".to_owned(),
        ..Default::default()
    })
}

#[test]
fn a_droppable_message_is_dropped_when_the_client_cannot_keep_up() {
    let registry = registry(RoomDefaults::default());
    let (tx, mut rx) = signal_channel();
    let alice = registry.join("demo", spec("alice"), tx).unwrap();

    for _ in 0..SIGNAL_CHANNEL_CAPACITY {
        assert!(alice.participant.send(pong()));
    }

    assert!(!alice.participant.send(pong()));
    assert_eq!(alice.participant.close_reason(), None);
    assert!(rx.try_recv().is_ok(), "the session is still open");
}

#[test]
fn a_control_message_that_cannot_be_queued_ends_the_session() {
    let registry = registry(RoomDefaults::default());
    let (tx, mut rx) = signal_channel();
    let alice = registry.join("demo", spec("alice"), tx).unwrap();

    for _ in 0..SIGNAL_CHANNEL_CAPACITY {
        assert!(alice.participant.send(pong()));
    }

    assert!(!alice.participant.send(answer()));
    assert_eq!(
        alice.participant.close_reason(),
        Some(mediacore::DisconnectReason::SignalClose)
    );

    assert!(!alice.participant.send(answer()));
    assert!(!alice.participant.send(participant_update(Vec::new())));

    let mut delivered = 0;
    while rx.try_recv().is_ok() {
        delivered += 1;
    }
    assert_eq!(delivered, SIGNAL_CHANNEL_CAPACITY);
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)),
        "the signal stream must end so the session is torn down"
    );
}

#[test]
fn honours_the_participant_limit() {
    let registry = registry(RoomDefaults {
        max_participants: 1,
        ..RoomDefaults::default()
    });
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let result = registry.join("demo", spec("bob"), bob_tx);

    assert!(matches!(result, Err(JoinError::RoomFull(_))));
}

#[test]
fn creating_a_room_that_exists_returns_the_one_that_exists() {
    let registry = registry(RoomDefaults::default());

    let first = registry.create(
        "demo",
        RoomOptions {
            max_participants: Some(2),
            ..RoomOptions::default()
        },
        Adoption::Always,
    );
    let again = registry.create(
        "demo",
        RoomOptions {
            max_participants: Some(9),
            ..RoomOptions::default()
        },
        Adoption::Always,
    );

    assert_eq!(first.sid, again.sid);
    assert_eq!(registry.rooms().len(), 1);
    assert_eq!(
        again.settings().max_participants,
        2,
        "a retried create rewrote the room somebody is already sitting in"
    );
}

#[test]
fn a_created_room_carries_settings_of_its_own() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let room = registry.create(
        "demo",
        RoomOptions {
            metadata: "{\"minHumans\":2}".to_owned(),
            max_participants: Some(1),
            empty_timeout: Some(60),
            ..RoomOptions::default()
        },
        Adoption::Always,
    );

    let proto = room.to_proto();
    assert_eq!(proto.metadata, "{\"minHumans\":2}");
    assert_eq!(proto.max_participants, 1);
    assert_eq!(proto.empty_timeout, 60);
    assert_eq!(
        proto.departure_timeout, 20,
        "a setting the room did not override must still come from the registry"
    );

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    assert!(
        matches!(
            registry.join("demo", spec("bob"), bob_tx),
            Err(JoinError::RoomFull(_))
        ),
        "the room's own limit was ignored in favour of the registry's"
    );
    drain(&mut alice_rx);

    room.set_metadata("{\"minHumans\":3}");
    match alice_rx.try_recv().expect("a signal was queued").message {
        Some(mediacore::signal_response::Message::RoomUpdate(update)) => {
            assert_eq!(update.room.unwrap().metadata, "{\"minHumans\":3}");
        }
        other => panic!("expected a room update, got {other:?}"),
    }

    room.set_metadata("{\"minHumans\":3}");
    assert!(
        alice_rx.try_recv().is_err(),
        "metadata that did not change was announced anyway"
    );
}

/// A room made by hand is somebody's intention, and it outlives the first
/// person to walk out of it. A room that only exists because somebody
/// walked in has nothing behind it once they walk out.
#[test]
fn a_room_that_was_created_outlives_its_participants() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.create("standup", RoomOptions::default(), Adoption::Always);
    let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();
    registry.leave("standup", &alice.participant.sid);

    assert!(
        registry.room("standup").is_some(),
        "a room somebody created was thrown away behind their back"
    );

    let bob = registry.join("ad-hoc", spec("bob"), bob_tx).unwrap();
    registry.leave("ad-hoc", &bob.participant.sid);
    assert!(registry.room("ad-hoc").is_none());

    assert!(registry.delete("standup").is_some());
    assert!(registry.room("standup").is_none());
    assert!(registry.delete("standup").is_none());
}

/// A conference service makes a room per meeting. Without a timeout that
/// somebody enforces, the registry and every `ListRooms` after it grow for
/// as long as the process lives.
#[test]
fn a_created_room_nobody_ever_joined_is_collected() {
    let registry = registry(RoomDefaults::default());
    let room = registry.create(
        "standup",
        RoomOptions {
            empty_timeout: Some(60),
            ..RoomOptions::default()
        },
        Adoption::Always,
    );
    let emptied_at = room.emptied_at_ms().expect("nobody is in it");

    assert!(
        registry.sweep(emptied_at + 59_000).is_empty(),
        "the room was collected before the timeout it reports to its callers"
    );
    assert_eq!(
        registry.sweep(emptied_at + 60_000).closed,
        vec!["standup".to_owned()],
        "empty_timeout is carried on the room and reported to every client; a room that \
             outlives it is a promise the server does not keep"
    );
    assert!(registry.room("standup").is_none());
}

/// `empty_timeout` waits for a room nobody has been in; `departure_timeout`
/// waits after the last person leaves. LiveKit divides them the same way,
/// and a meeting that is over should not be held open for the five minutes
/// that were meant for one that has not started.
#[test]
fn a_created_room_is_collected_after_the_last_person_walks_out() {
    let registry = registry(RoomDefaults::default());
    let (tx, _rx) = signal_channel();

    let room = registry.create("standup", RoomOptions::default(), Adoption::Always);
    let alice = registry.join("standup", spec("alice"), tx).unwrap();
    assert_eq!(
        room.emptied_at_ms(),
        None,
        "somebody is in the room, so nothing is counting down"
    );

    registry.leave("standup", &alice.participant.sid);
    let emptied_at = room.emptied_at_ms().expect("the room is empty again");

    assert!(registry.sweep(emptied_at + 19_000).is_empty());
    assert_eq!(
        registry.sweep(emptied_at + 20_000).closed,
        vec!["standup".to_owned()],
        "a room people have been in and left waits departure_timeout, not empty_timeout"
    );
}

/// A room whose settings a caller could not choose is the trap `auto_create`
/// leaves, and adoption is what closes it -- but `roomCreate` names no
/// room, so the caller has to.
#[test]
fn a_create_that_names_no_room_leaves_an_occupied_one_alone() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();
    drain(&mut alice_rx);

    let room = registry.create(
        "standup",
        RoomOptions {
            metadata: "the daily".to_owned(),
            max_participants: Some(5),
            min_humans: Some(2),
            ..RoomOptions::default()
        },
        Adoption::WhenEmpty,
    );

    assert_eq!(room.sid, alice.room.sid);
    assert_eq!(
        room.settings().max_participants,
        0,
        "a token with no idea which room it is talking about re-pointed the settings of a \
             meeting that was already running"
    );
    assert_eq!(room.settings().min_humans, 0, "and its session policy");
    assert_eq!(room.metadata(), "");
    assert!(!room.is_persistent());
    assert!(
        alice_rx.try_recv().is_err(),
        "nothing changed, so the room was told nothing"
    );
}

#[test]
fn deleting_a_room_disconnects_everybody_in_it() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    registry.join("demo", spec("bob"), bob_tx).unwrap();
    drain(&mut alice_rx);
    drain(&mut bob_rx);

    let deleted = registry.delete("demo").expect("the room existed");

    assert!(registry.room("demo").is_none());
    assert_eq!(
        deleted.participant_count(),
        0,
        "a deleted room still holds the people it was carrying"
    );

    for (who, rx) in [("alice", &mut alice_rx), ("bob", &mut bob_rx)] {
        let leave = expect_leave(rx);
        assert_eq!(
            leave.reason,
            mediacore::DisconnectReason::RoomDeleted as i32,
            "{who} was cut off without being told the room was deleted"
        );
        assert!(
            !leave.can_reconnect,
            "{who} was invited back into a room that no longer exists"
        );
    }

    registry.leave("demo", &alice.participant.sid);
    assert!(registry.room("demo").is_none());
}

#[test]
fn removing_a_participant_tells_it_and_the_room() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    let published = bob
        .room
        .publish_track(&bob.participant.sid, &camera("cid-1"))
        .expect("bob may publish");
    alice
        .participant
        .add_desired_subscriptions([published.sid.clone()]);
    alice.participant.mark_subscription_active(&published.sid);
    drain(&mut alice_rx);
    drain(&mut bob_rx);

    let removed = registry
        .remove_participant("demo", "bob")
        .expect("bob was in the room");

    assert_eq!(removed.sid, bob.participant.sid);
    assert_eq!(
        expect_leave(&mut bob_rx).reason,
        mediacore::DisconnectReason::ParticipantRemoved as i32,
        "bob was dropped without being told why"
    );

    let update = expect_update(&mut alice_rx);
    assert_eq!(update.participants[0].sid, bob.participant.sid);
    assert_eq!(
        update.participants[0].state,
        mediacore::participant_info::State::Disconnected as i32
    );
    assert_eq!(
        update.participants[0].disconnect_reason,
        mediacore::DisconnectReason::ParticipantRemoved as i32
    );

    assert!(
        alice.participant.desired_subscriptions().is_empty(),
        "alice is still subscribed to a track whose publisher was removed"
    );
    assert_eq!(
        alice.participant.subscription_diff().to_unsubscribe,
        vec![published.sid]
    );
    assert_eq!(alice.room.participant_count(), 1);
    assert!(registry.remove_participant("demo", "bob").is_none());
}

#[test]
fn the_session_ends_when_too_few_humans_remain() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    assert_eq!(
        alice.room.session_ends_at_ms(),
        None,
        "a room with the humans it asked for is not on its way out"
    );

    registry.leave("demo", &bob.participant.sid);
    drain(&mut alice_rx);
    let ends_at = alice
        .room
        .session_ends_at_ms()
        .expect("the grace period is running");

    assert!(
        registry.sweep(ends_at - 1).is_empty(),
        "the room ended before the grace period it promised was over"
    );
    assert!(registry.room("demo").is_some());

    assert_eq!(registry.sweep(ends_at).ended, vec!["demo".to_owned()]);
    assert!(registry.room("demo").is_none());
    assert_eq!(
        expect_leave(&mut alice_rx).reason,
        mediacore::DisconnectReason::RoomClosed as i32,
        "the last person left in the room was cut off without being told the session was over"
    );
    assert_eq!(alice.room.participant_count(), 0);
}

#[test]
fn a_rejoin_inside_the_grace_period_calls_it_off() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();
    let (again_tx, _again_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    registry.leave("demo", &bob.participant.sid);

    let ends_at = alice
        .room
        .session_ends_at_ms()
        .expect("the grace period is running");
    drain(&mut alice_rx);

    registry.join("demo", spec("bob"), again_tx).unwrap();

    assert_eq!(
        alice.room.session_ends_at_ms(),
        None,
        "bob came back and the room is still counting down to its own end"
    );
    assert!(registry.sweep(ends_at + 60_000).is_empty());
    assert!(registry.room("demo").is_some());
    assert!(
        leaves(&mut alice_rx).is_empty(),
        "alice was thrown out of a room that filled up again"
    );
}

/// The policy counts people, not connections. A recorder sitting in the
/// room keeps it from being empty and must not keep it from ending, which
/// is the whole reason `ParticipantInfo.kind` is carried at all.
#[test]
fn a_recorder_is_not_one_of_the_humans_the_policy_counts() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();
    let (recorder_tx, mut recorder_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    registry.leave("demo", &bob.participant.sid);

    let scribe = registry
        .join("demo", recorder("scribe"), recorder_tx)
        .unwrap();
    assert_eq!(
        scribe
            .participant
            .to_proto(mediacore::participant_info::State::Active)
            .kind,
        mediacore::participant_info::Kind::Egress as i32
    );

    let ends_at = alice
        .room
        .session_ends_at_ms()
        .expect("the grace period is still running");
    assert_eq!(alice.room.participant_count(), 2);
    assert_eq!(
        alice.room.human_count(),
        1,
        "the recorder was counted as somebody who came to talk"
    );
    drain(&mut alice_rx);
    drain(&mut recorder_rx);

    assert_eq!(
        registry.sweep(ends_at).ended,
        vec!["demo".to_owned()],
        "a recorder in the room held the session open past its grace period"
    );
    assert_eq!(
        expect_leave(&mut alice_rx).reason,
        mediacore::DisconnectReason::RoomClosed as i32
    );
    assert_eq!(
        expect_leave(&mut recorder_rx).reason,
        mediacore::DisconnectReason::RoomClosed as i32
    );
}

/// A room that has not filled yet is waiting for people, not losing them.
/// Arming the grace on the way up would end every scheduled meeting ninety
/// seconds before the second person arrived.
#[test]
fn the_grace_period_does_not_start_before_the_room_has_filled() {
    let registry = registry(session_policy(2, 90));
    let (tx, mut rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), tx).unwrap();

    assert_eq!(alice.room.session_ends_at_ms(), None);
    assert!(registry.sweep(now_ms() + 3_600_000).ended.is_empty());
    assert!(registry.room("demo").is_some());
    assert!(leaves(&mut rx).is_empty());
}

/// Every session record the store holds for a room, in the order it
/// reports them.
fn recorded(registry: &Registry, room: &str) -> Vec<mediacore_session::Session> {
    registry
        .sessions()
        .of_room(room, 0, mediacore_session::MAX_PAGE)
        .expect("the session store answers")
        .sessions
}

/// One meeting by id, out of whatever store the registry was given.
fn read(registry: &Registry, id: &str) -> Option<mediacore_session::Session> {
    registry
        .sessions()
        .get(id)
        .expect("the session store answers")
}

fn attendees(session: &mediacore_session::Session) -> Vec<String> {
    let mut identities: Vec<String> = session
        .participants
        .iter()
        .map(|attendance| attendance.identity.clone())
        .collect();
    identities.sort();
    identities
}

/// A room with a floor of two is waiting for people until the second one
/// arrives. Opening a meeting on the first would write down a meeting
/// every time somebody opened the page, saw they were alone and closed it
/// again -- which is the noise that makes a history worth nothing.
#[test]
fn a_meeting_starts_when_the_room_fills_and_not_before() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    assert!(
        registry.sessions().live("demo").is_none(),
        "one person waiting for a second is not a meeting"
    );
    assert!(recorded(&registry, "demo").is_empty());

    registry.join("demo", spec("bob"), bob_tx).unwrap();

    let session = registry
        .sessions()
        .live("demo")
        .expect("the room reached its floor and no meeting was written down");
    assert_eq!(session.room, "demo");
    assert_eq!(attendees(&session), vec!["alice", "bob"]);
    assert!(session.is_live());
    assert!(
        session
            .participants
            .iter()
            .any(|attendance| attendance.identity == "alice"
                && attendance.joined_at_ms <= session.started_at_ms),
        "alice arrived before the meeting did, and the record has to say so: how long the \
             first person waited is a question somebody asks"
    );
    assert_eq!(alice.room.name, "demo");
}

/// The policy counts people. A recorder keeps a room from being empty and
/// must not make it a meeting -- but it was in the room, so a record that
/// leaves it out cannot answer whether the meeting was recorded.
#[test]
fn a_recorder_does_not_make_a_meeting_and_is_still_written_into_one() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, _alice_rx) = signal_channel();
    let (scribe_tx, _scribe_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    registry
        .join("demo", recorder("scribe"), scribe_tx)
        .unwrap();

    assert!(
        registry.sessions().live("demo").is_none(),
        "a recorder was counted as somebody who came to talk, and one person plus a bot was \
             written down as a meeting of two"
    );

    registry.join("demo", spec("bob"), bob_tx).unwrap();
    let session = registry
        .sessions()
        .live("demo")
        .expect("two people are in the room");

    assert_eq!(attendees(&session), vec!["alice", "bob", "scribe"]);
    assert!(
        session.participants.iter().any(|attendance| {
            attendance.identity == "scribe" && attendance.kind == mediacore_session::Kind::Egress
        }),
        "the recorder is in the record as a recorder, which is what answers `was this \
             meeting recorded` a month later"
    );
}

/// The end of a meeting is the end of the grace period, on the tick the
/// grace period named -- not on the tick the sweep happened to notice, and
/// not while somebody could still have come back.
#[test]
fn a_meeting_ends_when_the_grace_period_runs_out() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let id = registry.sessions().live("demo").expect("a meeting").id;

    registry.leave("demo", &bob.participant.sid);
    drain(&mut alice_rx);
    let ends_at = alice
        .room
        .session_ends_at_ms()
        .expect("the grace period is running");

    assert!(
        registry.sweep(ends_at - 1).sessions.is_empty(),
        "the meeting was written down as over before the grace period it promised was"
    );
    assert!(registry.sessions().live("demo").is_some());

    assert_eq!(registry.sweep(ends_at).sessions, vec![id.clone()]);

    let session = read(&registry, &id).expect("the record is kept");
    assert_eq!(
        session.ended_at_ms,
        Some(ends_at),
        "a meeting ends when its grace period said it would, not when the sweep next ran: a \
             record nobody can reconcile with the promise is a record nobody trusts"
    );
    assert_eq!(session.ended_by, Some(mediacore_session::Ending::Grace));
    assert!(
        session
            .participants
            .iter()
            .all(|attendance| attendance.left_at_ms.is_some()),
        "alice was in the room when it closed and her record says she is in it still"
    );
    assert!(registry.sessions().live("demo").is_none());
}

/// Somebody whose network blinked and who came back before the grace ran
/// out did not hold two meetings. The record is one meeting with a gap in
/// it, and the gap is theirs: two stretches of attendance, the first one
/// closed.
#[test]
fn coming_back_inside_the_grace_period_is_one_meeting_with_a_gap_in_it() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();
    let (again_tx, _again_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let id = registry.sessions().live("demo").expect("a meeting").id;

    registry.leave("demo", &bob.participant.sid);
    let ends_at = alice
        .room
        .session_ends_at_ms()
        .expect("the grace period is running");

    registry.join("demo", spec("bob"), again_tx).unwrap();

    assert!(registry.sweep(ends_at + 60_000).sessions.is_empty());
    let session = registry
        .sessions()
        .live("demo")
        .expect("the meeting is still running");
    assert_eq!(
        session.id, id,
        "bob came back to the meeting he had just left and was given a second one; the room \
             now reads as two short meetings instead of one interrupted one"
    );
    assert_eq!(recorded(&registry, "demo").len(), 1);

    let stretches: Vec<&mediacore_session::Attendance> = session
        .participants
        .iter()
        .filter(|attendance| attendance.identity == "bob")
        .collect();
    assert_eq!(
        stretches.len(),
        2,
        "bob left and came back, and the record shows one unbroken attendance: the gap is the \
             part somebody is asking about"
    );
    assert!(stretches[0].left_at_ms.is_some());
    assert!(stretches[1].left_at_ms.is_none());
}

/// The deployment that is running has no session policy: `min_humans` is
/// zero and always has been. A record that only appears once somebody opts
/// into a policy they have never heard of is a record nobody has -- so a
/// meeting is one person arriving, and it ends when the room empties and
/// the grace period runs out.
///
/// The room here is one nobody created, which is the ordinary case: it is
/// thrown away the moment the last person leaves, and what happened in it
/// is not.
#[test]
fn an_auto_created_rooms_record_outlives_the_room_itself() {
    let registry = registry(RoomDefaults::default());
    let (tx, _rx) = signal_channel();

    assert_eq!(
        RoomDefaults::default().min_humans,
        0,
        "this test is about the deployment that configured nothing, and it is testing \
             something else"
    );

    let alice = registry.join("demo", spec("alice"), tx).unwrap();
    let session = registry
        .sessions()
        .live("demo")
        .expect("a room with no policy still holds meetings, or nobody has this feature");

    registry.leave("demo", &alice.participant.sid);
    assert!(
        registry.room("demo").is_none(),
        "a room nobody created goes when the last person does, as it always has"
    );
    assert_eq!(
        registry
            .sessions()
            .live("demo")
            .map(|live| live.id)
            .as_deref(),
        Some(session.id.as_str()),
        "the meeting is serving out its grace period; the room it happened in is gone and \
             that is not the same thing"
    );

    let swept = registry.sweep(now_ms() + i64::from(RoomDefaults::default().end_grace) * 1_000);
    assert_eq!(swept.sessions, vec![session.id.clone()]);
    assert!(
        swept.ended.is_empty(),
        "no room ended here -- there was no room left to end"
    );

    let record = read(&registry, &session.id)
        .expect("the record of a room that no longer exists is still there to be asked about");
    assert_eq!(record.room, "demo");
    assert!(record.ended_at_ms.is_some());
    assert_eq!(attendees(&record), vec!["alice"]);
    assert_eq!(recorded(&registry, "demo").len(), 1);
}

/// Finding A, first path. A backoffice raises the floor over a room
/// somebody is sitting in. The store's countdown and the room's used to be
/// two decisions armed on two conditions: only the store's fired, so the
/// record closed with alice still in the room, the endpoint answered "no
/// meeting" while she was talking, and the rest of her time was written
/// down nowhere.
#[test]
fn raising_the_floor_over_an_occupied_room_ends_the_room_with_the_meeting() {
    let registry = registry(RoomDefaults::default());
    let (tx, mut rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), tx).unwrap();
    let id = registry.sessions().live("demo").expect("a meeting").id;
    assert_eq!(alice.room.settings().min_humans, 0);

    registry.set_session_policy(&alice.room, Some(2), Some(90), None);
    let ends_at = alice
        .room
        .session_ends_at_ms()
        .expect("one person under a floor of two is a meeting on its way out");
    drain(&mut rx);

    let swept = registry.sweep(ends_at);
    assert_eq!(swept.sessions, vec![id.clone()]);
    assert_eq!(
        swept.ended,
        vec!["demo".to_owned()],
        "the meeting was written down as over and the room it was held in carried on, so \
             alice is in a room the record says nothing is happening in"
    );
    assert_eq!(
        alice.room.participant_count(),
        0,
        "the record says the meeting ended and alice is still sitting in the room"
    );
    assert_eq!(
        expect_leave(&mut rx).reason,
        mediacore::DisconnectReason::RoomClosed as i32,
        "alice's meeting was closed under her and nobody told her"
    );

    let record = read(&registry, &id).expect("the record is kept");
    assert!(
        record
            .participants
            .iter()
            .all(|attendance| attendance.left_at_ms == record.ended_at_ms),
        "the record has to close everybody at the moment the room let them go"
    );
}

/// Finding A, second path. The room alice comes back into is not the room
/// she left -- a room nobody created is thrown away with the last person
/// and built again for the first one back -- so a countdown kept on the
/// `Room` object was never armed on the new one, while the meeting's own
/// ran on and closed the record over her head.
#[test]
fn coming_back_alone_inside_the_grace_does_not_leave_a_meeting_running_over_you() {
    let registry = registry(session_policy(2, 90));
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let id = registry.sessions().live("demo").expect("a meeting").id;

    registry.leave("demo", &bob.participant.sid);
    registry.leave("demo", &alice.participant.sid);
    assert!(
        registry.room("demo").is_none(),
        "a room nobody created goes with the last person to leave it"
    );

    let (again_tx, mut again_rx) = signal_channel();
    let back = registry.join("demo", spec("alice"), again_tx).unwrap();
    assert_eq!(
        registry.sessions().live("demo").map(|live| live.id),
        Some(id.clone()),
        "alice came back inside the grace and was given a second meeting"
    );

    let ends_at = back.room.session_ends_at_ms().expect(
        "one person under a floor of two is still a meeting on its way out, whichever room \
             object she came back into",
    );
    drain(&mut again_rx);

    let swept = registry.sweep(ends_at);
    assert_eq!(swept.sessions, vec![id]);
    assert_eq!(
        swept.ended,
        vec!["demo".to_owned()],
        "the meeting ended and the room alice was sitting in was left standing"
    );
    assert_eq!(
        back.room.participant_count(),
        0,
        "the record says the meeting ended and alice is still in the room"
    );
}

/// Finding D. Two threads moving people in and out of one room used to
/// snapshot a roster each and then race to hand it over, so an older
/// snapshot landed on a newer one: the record grew stretches nobody sat
/// through, saying people left and came back at moments they did not move.
#[test]
fn concurrent_arrivals_are_recorded_as_the_room_actually_was() {
    let registry = registry(RoomDefaults::default());
    let (anchor_tx, _anchor_rx) = signal_channel();
    let rounds = 60;

    // Somebody who never leaves, so the meeting is one meeting throughout
    // and every stretch below belongs to the same record.
    registry.join("demo", spec("anchor"), anchor_tx).unwrap();

    std::thread::scope(|scope| {
        for thread in 0..2 {
            let registry = &registry;
            scope.spawn(move || {
                for round in 0..rounds {
                    let (tx, _rx) = signal_channel();
                    let identity = format!("guest{thread}-{round}");
                    let joined = registry.join("demo", spec(&identity), tx).unwrap();
                    registry.leave("demo", &joined.participant.sid);
                }
            });
        }
    });

    let session = registry
        .sessions()
        .live("demo")
        .expect("the anchor never left, so the meeting never ended");

    let mut per_sid: HashMap<&str, usize> = HashMap::new();
    for attendance in &session.participants {
        *per_sid.entry(attendance.sid.as_str()).or_default() += 1;
    }
    let doubled: Vec<&str> = per_sid
        .iter()
        .filter(|(_, stretches)| **stretches > 1)
        .map(|(sid, _)| *sid)
        .collect();

    assert!(
        doubled.is_empty(),
        "{} connections joined once and the record says they left and came back: {doubled:?}. \
             Nobody moved at those moments; an older view of the room landed on top of a newer \
             one",
        doubled.len()
    );
    assert_eq!(
        session.participants.len(),
        1 + 2 * rounds,
        "one stretch per arrival is {} and the record has {}",
        1 + 2 * rounds,
        session.participants.len()
    );
    assert_eq!(
        session
            .participants
            .iter()
            .filter(|attendance| attendance.left_at_ms.is_none())
            .count(),
        1,
        "somebody who left is still open in the record, so their attendance runs to the end \
             of the meeting"
    );
}

/// A room somebody created outlives its meetings and holds them one after
/// another, which is why the endpoint that lists them is plural.
///
/// `end_grace: 0` is the escape hatch for a deployment that wants a
/// meeting to end the instant its room empties rather than be held open
/// for anybody coming back.
#[test]
fn a_created_room_holds_one_meeting_after_another() {
    let registry = registry(RoomDefaults {
        end_grace: 0,
        ..RoomDefaults::default()
    });
    registry.create("standup", RoomOptions::default(), Adoption::Always);

    let mut held = Vec::new();
    for identity in ["alice", "bob", "carol"] {
        let (tx, _rx) = signal_channel();
        let joined = registry.join("standup", spec(identity), tx).unwrap();
        held.push(
            registry
                .sessions()
                .live("standup")
                .expect("somebody is in the room")
                .id,
        );

        registry.leave("standup", &joined.participant.sid);
        registry.sweep(now_ms());
    }

    assert!(
        registry.room("standup").is_some(),
        "a room somebody created outlives the people in it, which is the point of creating one"
    );

    let sessions = recorded(&registry, "standup");
    assert_eq!(
        sessions.len(),
        3,
        "three meetings were held in one room and the room has {} of them",
        sessions.len()
    );
    assert!(
        sessions.iter().all(|session| session.ended_at_ms.is_some()),
        "a meeting nobody is at is still open in the record"
    );

    let mut ids: Vec<String> = sessions.iter().map(|session| session.id.clone()).collect();
    ids.sort();
    held.sort();
    assert_eq!(
        ids, held,
        "the three meetings the room reports are not the three it held"
    );
    assert!(
        sessions.iter().all(|session| attendees(session).len() == 1),
        "somebody who was at one meeting is written into another"
    );
}

/// Deleting a room ends the meeting in it now. The grace period is a
/// promise that whoever comes back inside it finds the same meeting, and a
/// deleted room is the one case where there is nothing to come back to.
#[test]
fn deleting_a_room_ends_its_meeting_and_keeps_the_record() {
    let registry = registry(RoomDefaults::default());
    let (tx, _rx) = signal_channel();

    registry.join("demo", spec("alice"), tx).unwrap();
    let id = registry.sessions().live("demo").expect("a meeting").id;

    registry.delete("demo");

    assert!(
        registry.sessions().live("demo").is_none(),
        "the room was deleted and its meeting is still being held open for somebody to come \
             back to a room that is not there"
    );
    let record = read(&registry, &id).expect("the record survives");
    assert_eq!(record.ended_by, Some(mediacore_session::Ending::Closed));
    assert!(
        record
            .participants
            .iter()
            .all(|attendance| attendance.left_at_ms.is_some())
    );
}

/// `livekit-client` keys its remote participants by identity, so two
/// connections sharing one are invisible to every client in the room. The
/// server hands the second one an identity of its own and keeps the
/// display name, which is the half a person actually reads.
/// A hidden participant, which is what a recorder joins as: invisible to
/// the people in the room and not to whoever is listening outside it.
fn hidden(identity: &str) -> ParticipantSpec {
    ParticipantSpec {
        kind: ParticipantKind::Egress,
        permission: mediacore::ParticipantPermission {
            hidden: true,
            ..spec(identity).permission
        },
        ..spec(identity)
    }
}

/// The whole life of a room that one person walked into and out of, as it
/// was announced. The order is the point: a consumer builds a timeline out
/// of these and cannot reorder what it is given, so somebody must not
/// arrive in a room that has not been announced, and a meeting must not
/// start before its first attendee is in it.
#[test]
fn a_rooms_whole_life_is_announced_in_the_order_it_happened() {
    let mut registry = registry(RoomDefaults::default());
    let (tx, _rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), tx).unwrap();

    let announced = registry.announced();
    assert_eq!(
        announced
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<&str>>(),
        vec!["room_started", "participant_joined", "session_started"],
        "somebody joined a room nobody was told about, or a meeting started before anybody \
             was announced as being at it"
    );
    assert_eq!(
        announced[1]
            .participant
            .as_ref()
            .expect("a participant_joined with nobody on it")
            .identity,
        "alice"
    );
    assert_eq!(
        announced[1]
            .room
            .as_ref()
            .expect("a participant_joined with no room on it")
            .name,
        "demo"
    );
    assert_eq!(
        announced[2]
            .session
            .as_ref()
            .expect("a session_started with no meeting on it")
            .room,
        "demo",
        "a meeting outlives its room, so the room's name has to travel inside the meeting"
    );

    registry.leave("demo", &alice.participant.sid);
    assert_eq!(
        registry.announced_kinds(),
        vec!["participant_left", "room_finished"],
        "a room nobody created goes with the last person to leave it, and that has to be \
             announced or a consumer counts a room that is not there any more"
    );

    // The meeting is not over: the grace period is holding it open for
    // whoever comes back. Nothing else exists to notice when it runs out
    // -- the room was thrown away two lines ago -- which is why the sweep
    // is the one clock and why this event has to come from it.
    let swept = registry.sweep(now_ms() + 91_000);
    assert_eq!(swept.sessions.len(), 1);
    assert_eq!(
        registry.announced_kinds(),
        vec!["session_finished"],
        "a meeting ended after the room it was in had been thrown away, and nobody outside \
             this process would ever have heard about it"
    );
}

/// `CreateRoom` is a retry as often as it is a first attempt -- an SDK
/// whose socket timed out sends it again. A consumer counting rooms must
/// not count one twice for that.
#[test]
fn a_room_is_announced_once_however_many_times_it_is_created() {
    let mut registry = registry(RoomDefaults::default());

    registry.create("demo", RoomOptions::default(), Adoption::WhenEmpty);
    registry.create("demo", RoomOptions::default(), Adoption::WhenEmpty);
    registry.create("demo", RoomOptions::default(), Adoption::Always);

    assert_eq!(
        registry.announced_kinds(),
        vec!["room_started"],
        "a room that already existed was announced as new"
    );
}

/// Deleting a room ends everything inside it at once, and the
/// announcements have to come out in the order those things happened:
/// everybody left, then the meeting they were at ended, then the place it
/// was in stopped existing.
#[test]
fn deleting_a_room_announces_the_departures_the_meeting_and_the_room() {
    let mut registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    registry.create("demo", RoomOptions::default(), Adoption::WhenEmpty);
    registry.join("demo", spec("alice"), alice_tx).unwrap();
    registry.join("demo", spec("bob"), bob_tx).unwrap();
    registry.announced();

    registry
        .delete("demo")
        .expect("the room is there to delete");

    assert_eq!(
        registry.announced_kinds(),
        vec![
            "participant_left",
            "participant_left",
            "session_finished",
            "room_finished"
        ],
        "a deleted room was announced in an order a consumer would have to undo to make a \
             timeline out of"
    );
}

/// A recorder is hidden from the people in the room on purpose. It is not
/// hidden from whoever is billing for the recording, and "was this meeting
/// recorded" is asked a month later by somebody who was not there.
#[test]
fn a_recorder_is_announced_although_nobody_in_the_room_was_told_about_it() {
    let mut registry = registry(RoomDefaults::default());
    let (scribe_tx, _scribe_rx) = signal_channel();
    let (alice_tx, mut alice_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    registry.announced();
    drain(&mut alice_rx);

    let scribe = registry.join("demo", hidden("scribe"), scribe_tx).unwrap();
    assert!(
        scribe.participant.is_hidden(),
        "the participant under test is not hidden, so this proves nothing"
    );
    assert!(
        expect_update_or_none(&mut alice_rx).is_none(),
        "the room was told a hidden participant arrived"
    );

    let announced = registry.announced();
    assert_eq!(
        announced
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<&str>>(),
        vec!["participant_joined"],
        "the recorder arrived and nothing outside this process was told"
    );

    registry.leave("demo", &scribe.participant.sid);
    assert_eq!(
        registry.announced_kinds(),
        vec!["participant_left"],
        "the recording stopped and nothing outside this process was told"
    );
}

/// The one message a participant would have got, or nothing.
fn expect_update_or_none(rx: &mut SignalRx) -> Option<mediacore::ParticipantUpdate> {
    match rx.try_recv() {
        Ok(response) => match response.message {
            Some(mediacore::signal_response::Message::Update(update)) => Some(update),
            other => panic!("expected a participant update or nothing, got {other:?}"),
        },
        Err(_) => None,
    }
}

#[test]
fn a_second_connection_from_the_same_person_gets_an_identity_of_its_own() {
    let registry = registry(RoomDefaults::default());
    let (first_tx, mut first_rx) = signal_channel();
    let (second_tx, _second_rx) = signal_channel();

    let mut alice = spec("alice");
    alice.name = "Alice".to_owned();

    let first = registry.join("demo", alice.clone(), first_tx).unwrap();
    let second = registry.join("demo", alice, second_tx).unwrap();

    assert_eq!(first.participant.identity, "alice");
    assert_eq!(second.participant.identity, "alice#2");
    assert_ne!(first.participant.sid, second.participant.sid);
    assert_eq!(
        second.participant.name(),
        "Alice",
        "the person's name belongs to the person, not to the connection"
    );
    assert_eq!(second.room.participant_count(), 2);
    assert_eq!(second.others.len(), 1);
    assert_eq!(second.others[0].identity, "alice");

    let update = expect_update(&mut first_rx);
    assert_eq!(update.participants[0].identity, "alice#2");
    assert_eq!(update.participants[0].name, "Alice");
    assert_eq!(
        update.participants[0].state,
        mediacore::participant_info::State::Joined as i32,
        "the first tab was told its own person had left"
    );
    assert!(
        leaves(&mut first_rx).is_empty(),
        "opening a second tab threw the first one out"
    );
    assert_eq!(first.participant.close_reason(), None);
}

#[test]
fn a_suffixed_identity_never_lands_on_one_that_is_taken() {
    let registry = registry(RoomDefaults::default());
    let (first_tx, _first_rx) = signal_channel();
    let (taken_tx, _taken_rx) = signal_channel();
    let (third_tx, _third_rx) = signal_channel();

    registry.join("demo", spec("alice"), first_tx).unwrap();
    registry.join("demo", spec("alice#2"), taken_tx).unwrap();
    let third = registry.join("demo", spec("alice"), third_tx).unwrap();

    assert_eq!(
        third.participant.identity, "alice#3",
        "a token that already carries a suffix was displaced by a generated one"
    );
    assert_eq!(third.room.participant_count(), 3);
}

/// A client that reconnects sends the sid of the session it thinks it
/// still has. Without honouring it, every dropped socket would come back
/// as a new person and leave its own ghost sitting in the room until the
/// signal timeout collected it.
#[test]
fn a_resuming_session_takes_back_the_identity_it_had() {
    let registry = registry(RoomDefaults::default());
    let (first_tx, mut first_rx) = signal_channel();
    let (second_tx, mut second_rx) = signal_channel();
    let (again_tx, _again_rx) = signal_channel();

    let first = registry.join("demo", spec("alice"), first_tx).unwrap();
    let second = registry.join("demo", spec("alice"), second_tx).unwrap();
    assert_eq!(second.participant.identity, "alice#2");

    let resume = ParticipantSpec {
        replaces: Some(second.participant.sid.clone()),
        ..spec("alice")
    };
    let again = registry.join("demo", resume, again_tx).unwrap();

    assert_eq!(again.participant.identity, "alice#2");
    assert_eq!(
        again.room.participant_count(),
        2,
        "a session that came back was counted as a third person"
    );
    assert_eq!(
        expect_leave(&mut second_rx).reason,
        mediacore::DisconnectReason::DuplicateIdentity as i32
    );
    assert_eq!(
        registry
            .room("demo")
            .unwrap()
            .participant("alice#2")
            .unwrap()
            .sid,
        again.participant.sid
    );
    assert!(
        leaves(&mut first_rx).is_empty(),
        "the other tab was thrown out by a session that was not it"
    );
    assert_eq!(first.participant.identity, "alice");
}

#[test]
fn a_resume_claim_cannot_name_somebody_elses_session() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();
    let (mallory_tx, _mallory_rx) = signal_channel();

    registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    drain(&mut bob_rx);

    let claim = ParticipantSpec {
        replaces: Some(bob.participant.sid.clone()),
        ..spec("mallory")
    };
    let mallory = registry.join("demo", claim, mallory_tx).unwrap();

    assert_eq!(mallory.participant.identity, "mallory");
    assert_eq!(mallory.room.participant_count(), 3);
    assert!(
        leaves(&mut bob_rx).is_empty(),
        "a token for one identity evicted the session of another"
    );
    assert_eq!(
        registry
            .room("demo")
            .unwrap()
            .participant("bob")
            .unwrap()
            .sid,
        bob.participant.sid
    );
}

#[test]
fn a_room_the_first_arrival_made_is_settled_by_a_create() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();

    assert!(!alice.room.is_persistent());

    let room = registry.create(
        "standup",
        RoomOptions {
            metadata: "the daily".to_owned(),
            max_participants: Some(5),
            ..RoomOptions::default()
        },
        Adoption::Always,
    );

    assert_eq!(
        room.sid, alice.room.sid,
        "the create made a second room beside the one alice is in"
    );
    assert_eq!(room.settings().max_participants, 5);
    assert_eq!(room.metadata(), "the daily");

    match alice_rx.try_recv().expect("a signal was queued").message {
        Some(mediacore::signal_response::Message::RoomUpdate(update)) => {
            assert_eq!(update.room.unwrap().metadata, "the daily")
        }
        other => panic!("expected a room update, got {other:?}"),
    }

    registry.leave("standup", &alice.participant.sid);
    assert_eq!(
        registry.rooms().len(),
        1,
        "a room somebody asked for by name has to outlive the arrival who happened to open it, \
             or the settings and the session policy vanish the moment the room empties"
    );
}

#[test]
fn one_person_holding_two_connections_is_found_by_either_name() {
    let registry = registry(RoomDefaults::default());
    let (first_tx, _first_rx) = signal_channel();
    let (second_tx, _second_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let first = registry.join("demo", spec("alice"), first_tx).unwrap();
    let second = registry.join("demo", spec("alice"), second_tx).unwrap();
    registry.join("demo", spec("bob"), bob_tx).unwrap();

    let both: Vec<String> = first
        .room
        .participants_of("alice")
        .into_iter()
        .map(|participant| participant.identity.clone())
        .collect();
    assert_eq!(
        both,
        vec!["alice".to_owned(), "alice#2".to_owned()],
        "a caller who names the person means every session that person opened"
    );

    let one: Vec<String> = first
        .room
        .participants_of("alice#2")
        .into_iter()
        .map(|participant| participant.identity.clone())
        .collect();
    assert_eq!(one, vec![second.participant.identity.clone()]);
    assert!(first.room.participants_of("alic").is_empty());
    assert!(
        first.room.participants_of("alice#").is_empty(),
        "the suffix is a separator and an ordinal, and half of it names nobody"
    );
}

#[test]
fn revoking_the_right_to_publish_takes_the_tracks_with_it() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let track = alice
        .room
        .publish_track(&alice.participant.sid, &camera("cam"))
        .unwrap();
    bob.participant
        .add_desired_subscriptions([track.sid.clone()]);
    drain(&mut alice_rx);
    drain(&mut bob_rx);

    let updated = alice
        .room
        .update_participant(
            "alice",
            ProfileUpdate {
                permission: Some(mediacore::ParticipantPermission {
                    can_subscribe: true,
                    can_publish: false,
                    ..Default::default()
                }),
                ..ProfileUpdate::default()
            },
        )
        .expect("alice is in the room");

    assert_eq!(
        updated.unpublished,
        vec![track.sid.clone()],
        "the room forgot the track without telling the caller, so nothing can tell the \
             forwarder to stop sending the packets that are still arriving for it"
    );
    assert!(
        alice.participant.tracks().is_empty(),
        "a participant who may no longer publish keeps sending until the track is taken away \
             from them, so revoking the permission alone changes nothing anybody can hear"
    );
    assert_eq!(
        unpublished(&mut alice_rx),
        vec![track.sid.clone()],
        "alice's own client was never told to stop, so it keeps the camera on"
    );
    assert!(
        bob.participant.desired_subscriptions().is_empty(),
        "bob is still waiting for a track that no longer exists"
    );
}

#[test]
fn revoking_the_right_to_subscribe_drops_what_was_wanted() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    let track = alice
        .room
        .publish_track(&alice.participant.sid, &camera("cam"))
        .unwrap();
    bob.participant.add_desired_subscriptions([track.sid]);

    alice.room.update_participant(
        "bob",
        ProfileUpdate {
            permission: Some(mediacore::ParticipantPermission {
                can_subscribe: false,
                can_publish: true,
                ..Default::default()
            }),
            ..ProfileUpdate::default()
        },
    );

    assert!(
        bob.participant.desired_subscriptions().is_empty(),
        "bob keeps receiving what he already asked for unless the intent behind it is dropped"
    );
}

#[test]
fn an_update_that_changes_nothing_is_not_announced() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    registry.join("demo", spec("bob"), bob_tx).unwrap();
    drain(&mut bob_rx);

    alice.room.update_participant(
        "alice",
        ProfileUpdate {
            name: Some("alice".to_owned()),
            ..ProfileUpdate::default()
        },
    );

    assert!(
        bob_rx.try_recv().is_err(),
        "a backoffice that resends the same profile on every render would flood the room with \
             updates that say nothing"
    );
}

#[test]
fn an_attribute_set_to_nothing_is_deleted() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, _alice_rx) = signal_channel();
    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();

    alice.room.update_participant(
        "alice",
        ProfileUpdate {
            attributes: HashMap::from([
                ("role".to_owned(), "host".to_owned()),
                ("seat".to_owned(), "front".to_owned()),
            ]),
            ..ProfileUpdate::default()
        },
    );
    assert_eq!(alice.participant.attributes().len(), 2);

    alice.room.update_participant(
        "alice",
        ProfileUpdate {
            attributes: HashMap::from([("role".to_owned(), String::new())]),
            ..ProfileUpdate::default()
        },
    );

    let attributes = alice.participant.attributes();
    assert_eq!(
        attributes.get("seat").map(String::as_str),
        Some("front"),
        "an update names the attributes it touches and leaves the rest alone"
    );
    assert!(
        !attributes.contains_key("role"),
        "an empty value is how LiveKit spells a deletion, not how it spells a blank"
    );
}

#[test]
fn a_hidden_participant_hears_about_themselves_and_nobody_else_does() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (ghost_tx, mut ghost_rx) = signal_channel();

    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let ghost = ParticipantSpec {
        permission: mediacore::ParticipantPermission {
            hidden: true,
            can_subscribe: true,
            ..Default::default()
        },
        ..spec("recorder")
    };
    registry.join("demo", ghost, ghost_tx).unwrap();
    drain(&mut alice_rx);
    drain(&mut ghost_rx);

    alice.room.update_participant(
        "recorder",
        ProfileUpdate {
            metadata: Some("recording to s3".to_owned()),
            ..ProfileUpdate::default()
        },
    );

    assert_eq!(
        expect_update(&mut ghost_rx).participants[0].metadata,
        "recording to s3"
    );
    assert!(
        alice_rx.try_recv().is_err(),
        "announcing the update would put a participant the room is not supposed to see into \
             everybody's roster"
    );
}

#[test]
fn a_policy_that_arrives_after_the_room_did_still_ends_the_session() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();

    let room = registry.create("standup", RoomOptions::default(), Adoption::Always);
    let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("standup", spec("bob"), bob_tx).unwrap();
    assert_eq!(room.settings().min_humans, 0);

    registry.set_session_policy(&room, Some(2), Some(0), None);
    assert!(
        room.session_ends_at_ms().is_none(),
        "the room is full enough, there is nothing to end"
    );

    registry.leave("standup", &bob.participant.sid);
    assert!(
        room.session_ends_at_ms().is_some(),
        "a policy set on a room that already reached its quorum has to count that quorum as \
             reached, or the first departure after it arrives is never noticed"
    );
    drain(&mut alice_rx);

    assert_eq!(registry.sweep(now_ms()).ended, vec!["standup"]);
    assert_eq!(
        expect_leave(&mut alice_rx).reason,
        mediacore::DisconnectReason::RoomClosed as i32
    );
    assert!(registry.rooms().is_empty());
    let _ = alice;
}
