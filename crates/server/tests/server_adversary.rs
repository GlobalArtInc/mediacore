//! What an adversarial review of the room API found on the server side, kept as
//! the tests that would have caught it. Both of these passed against the broken
//! behaviour first.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_proto::mediacore;
use mediacore_room::{ParticipantSpec, ProfileUpdate};
use mediacore_server::http;
use mediacore_server::state::AppState;
use mediacore_server::subscriber::SubscriberSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const ROOM: &str = "demo";
const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";

async fn app_state() -> AppState {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();

    AppState::start(config).await.unwrap()
}

async fn serve(state: &AppState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(state.clone());

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    addr
}

fn admin(room: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    mediacore_auth::issue(
        API_SECRET,
        &Claims {
            exp,
            iss: API_KEY.to_owned(),
            video: VideoGrant {
                room_admin: Some(true),
                room: room.to_owned(),
                ..VideoGrant::default()
            },
            ..Claims::default()
        },
    )
    .unwrap()
}

/// One Twirp call over a real socket, because the point of these tests is the
/// path a moderator's button actually takes.
async fn post(addr: SocketAddr, method: &str, token: &str, body: serde_json::Value) -> u16 {
    let body = body.to_string();
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let head = format!(
            "POST {}{method} HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: application/json\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            mediacore_server::rooms::PATH_PREFIX,
            body.len()
        );

        socket.write_all(head.as_bytes()).await.expect("request");
        socket.write_all(body.as_bytes()).await.expect("body");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        raw
    };

    let raw = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("the room service did not answer in time");

    String::from_utf8_lossy(&raw)
        .split_whitespace()
        .nth(1)
        .expect("http status line")
        .parse()
        .expect("numeric http status")
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

fn permission(can_subscribe: bool) -> mediacore::ParticipantPermission {
    mediacore::ParticipantPermission {
        can_subscribe,
        can_publish: true,
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

/// A moderator who takes `canSubscribe` away and gives it back has to leave the
/// participant where they were, not blind and deaf until they reload.
#[tokio::test]
async fn restoring_the_subscribe_permission_brings_the_media_back() {
    let state = app_state().await;
    let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
    let (bob_tx, _bob_rx) = mediacore_room::signal_channel();
    let alice = state.registry.join(ROOM, spec("alice"), alice_tx).unwrap();
    let bob = state.registry.join(ROOM, spec("bob"), bob_tx).unwrap();

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &add_track("cid-camera"))
        .expect("alice publishes");

    let mut session = SubscriberSession::new(true);
    session.on_room_changed(&state, &bob).await;
    assert_eq!(
        bob.participant.desired_subscriptions(),
        vec![track.sid.clone()],
        "bob auto-subscribed to alice"
    );

    // A moderator takes bob's subscribe permission away.
    bob.room
        .update_participant(
            "bob",
            ProfileUpdate {
                permission: Some(permission(false)),
                ..ProfileUpdate::default()
            },
        )
        .expect("bob is in the room");
    session.on_room_changed(&state, &bob).await;
    assert!(
        bob.participant.desired_subscriptions().is_empty(),
        "revoking canSubscribe drops the intent"
    );

    // ...and gives it back a moment later.
    bob.room
        .update_participant(
            "bob",
            ProfileUpdate {
                permission: Some(permission(true)),
                ..ProfileUpdate::default()
            },
        )
        .expect("bob is in the room");
    assert!(bob.participant.can_subscribe());
    session.on_room_changed(&state, &bob).await;

    assert_eq!(
        bob.participant.desired_subscriptions(),
        vec![track.sid],
        "bob may subscribe again, and nothing re-added alice's track: the session counted it as \
         auto-subscribed once and bob's screen stays black until he reloads the page"
    );
}

/// Revoking `canPublish` takes the track out of the room. Only the router can
/// stop the packets that keep arriving for it.
#[tokio::test]
async fn a_revoked_track_is_taken_off_the_forwarder() {
    use mediacore_room::TrackKind;
    use mediacore_server::forward::TrackSource;
    use mediacore_server::simulcast::LayerMap;
    use mediacore_transport::str0m::media::Mid;

    let state = app_state().await;
    let addr = serve(&state).await;
    let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
    let alice = state.registry.join(ROOM, spec("alice"), alice_tx).unwrap();
    let publisher = state
        .media
        .spawn_publisher()
        .expect("a publisher transport");

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &add_track("cid-camera"))
        .expect("alice publishes");

    // What peer.rs does the moment alice's media goes live.
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
    assert!(state.router.source(&track.sid).is_some());

    // A moderator revokes publishing, through the API a moderator has.
    let status = post(
        addr,
        "UpdateParticipant",
        &admin(ROOM),
        serde_json::json!({
            "room": ROOM,
            "identity": "alice",
            "permission": {"canSubscribe": true, "canPublish": false},
        }),
    )
    .await;
    assert_eq!(status, 200);

    assert!(
        alice.room.track(&track.sid).is_none(),
        "the room forgot the track"
    );
    assert!(
        state.router.source(&track.sid).is_none(),
        "the SFU still has the source registered, so every RTP packet alice keeps sending is \
         still routed and ticked for the rest of her session; the client-initiated unpublish \
         path calls router.drop_track here and this one has to as well"
    );
    assert_eq!(
        state.router.totals().tracks,
        0,
        "/healthz still counts a track nobody may publish"
    );
}

/// An unsubscribe that arrives before the subscriber session has got round to
/// auto-subscribing has to survive the pass that follows it.
#[tokio::test]
async fn an_unsubscribe_that_arrives_early_is_not_undone_by_auto_subscribe() {
    let state = app_state().await;
    let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
    let (bob_tx, _bob_rx) = mediacore_room::signal_channel();
    let alice = state.registry.join(ROOM, spec("alice"), alice_tx).unwrap();
    let bob = state.registry.join(ROOM, spec("bob"), bob_tx).unwrap();

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &add_track("cid-camera"))
        .expect("alice publishes");

    // The backoffice unsubscribes bob before his own session has ever looked
    // at the room, which is what happens when the call lands in the seconds
    // between his join and his first reconcile.
    let mut session = SubscriberSession::new(true);
    bob.room.update_subscriptions(
        &bob.participant.sid,
        &mediacore::UpdateSubscription {
            track_sids: vec![track.sid.clone()],
            subscribe: false,
            ..Default::default()
        },
    );
    session.on_room_changed(&state, &bob).await;

    assert!(
        bob.participant.desired_subscriptions().is_empty(),
        "the auto-subscribe pass put back a track the backoffice had just taken away, because \
         the session had not offered it yet and counted it as new"
    );

    // Asking for it again is how somebody takes that back.
    bob.room.update_subscriptions(
        &bob.participant.sid,
        &mediacore::UpdateSubscription {
            track_sids: vec![track.sid.clone()],
            subscribe: true,
            ..Default::default()
        },
    );
    assert_eq!(bob.participant.desired_subscriptions(), vec![track.sid]);
}

/// The sweep needs a driver that does not depend on somebody being in the room:
/// an empty room has no session to tick, which is exactly when it has to be
/// collected.
#[tokio::test]
async fn an_empty_room_is_collected_with_nobody_on_the_server() {
    let mut config = Config::parse(&format!(
        "keys:\n  {API_KEY}: {API_SECRET}\nroom:\n  empty_timeout: 1\n  sweep_interval_ms: 50\n"
    ))
    .unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    let state = AppState::start(config).await.unwrap();

    state.registry.create(
        "meeting",
        mediacore_room::RoomOptions::default(),
        mediacore_room::Adoption::Always,
    );
    assert!(state.registry.room("meeting").is_some());

    let collected = tokio::time::timeout(Duration::from_secs(10), async {
        while state.registry.room("meeting").is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;

    assert!(
        collected.is_ok(),
        "the room outlived its empty timeout because the only thing that sweeps rooms is a \
         signalling session, and there is nobody on the server"
    );
}

/// The same recovery, without the session ever seeing the permission go: two
/// updates can land between one reconcile and the next, and the participant
/// still has to be able to see the room afterwards.
#[tokio::test]
async fn a_revoke_and_a_restore_between_two_reconciles_still_come_back() {
    let state = app_state().await;
    let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
    let (bob_tx, _bob_rx) = mediacore_room::signal_channel();
    let alice = state.registry.join(ROOM, spec("alice"), alice_tx).unwrap();
    let bob = state.registry.join(ROOM, spec("bob"), bob_tx).unwrap();

    let track = alice
        .room
        .publish_track(&alice.participant.sid, &add_track("cid-camera"))
        .expect("alice publishes");

    let mut session = SubscriberSession::new(true);
    session.on_room_changed(&state, &bob).await;
    assert_eq!(
        bob.participant.desired_subscriptions(),
        vec![track.sid.clone()]
    );

    for granted in [false, true] {
        bob.room
            .update_participant(
                "bob",
                ProfileUpdate {
                    permission: Some(permission(granted)),
                    ..ProfileUpdate::default()
                },
            )
            .expect("bob is in the room");
    }

    session.on_room_changed(&state, &bob).await;

    assert_eq!(
        bob.participant.desired_subscriptions(),
        vec![track.sid],
        "bob's subscriptions were dropped by the revocation and never re-offered, because the \
         session woke up after the permission had already come back and had nothing left to \
         tell it something had happened"
    );
}
