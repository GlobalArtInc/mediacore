use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_client::{Client, ClientEvent, PublishedTrack, TrackReport, TrackSpec};
use mediacore_config::Config;
use mediacore_proto::livekit;
use mediacore_server::forward::SubscriptionSnapshot;
use mediacore_server::http;
use mediacore_server::state::AppState;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "demo";
const PATIENCE: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const OUR_SEQUENCE_SPACE: std::ops::Range<u64> = (1 << 15)..(1 << 16);

struct Server {
    addr: SocketAddr,
    state: AppState,
}

impl Server {
    async fn start() -> Self {
        let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
        config.port = 0;
        config.rtc.udp_port = 0;
        config.rtc.port_range_start = 0;
        config.rtc.port_range_end = 0;
        config.rtc.bind_address = "127.0.0.1".to_owned();

        let state = AppState::start(config)
            .await
            .expect("media transport starts");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = http::router(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self { addr, state }
    }

    fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    fn subscription(&self, track_sid: &str, subscriber_sid: &str) -> SubscriptionSnapshot {
        self.state
            .router
            .subscription(track_sid, subscriber_sid)
            .unwrap_or_default()
    }

    fn subscriber_count(&self, track_sid: &str) -> usize {
        self.state.router.subscriber_count(track_sid)
    }
}

fn token(identity: &str, grant: VideoGrant) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    let claims = Claims {
        exp,
        iss: API_KEY.to_owned(),
        sub: identity.to_owned(),
        name: identity.to_owned(),
        video: VideoGrant {
            room_join: Some(true),
            room: ROOM.to_owned(),
            ..grant
        },
        ..Claims::default()
    };

    mediacore_auth::issue(API_SECRET, &claims).unwrap()
}

async fn join(server: &Server, identity: &str) -> Client {
    join_with(server, identity, VideoGrant::default()).await
}

async fn join_with(server: &Server, identity: &str, grant: VideoGrant) -> Client {
    Client::connect(&server.url(), &token(identity, grant))
        .await
        .expect("the client joins")
}

async fn publish(client: &Client, spec: TrackSpec) -> PublishedTrack {
    let track = client.publish(spec).await.expect("the track is published");

    client
        .wait_connected(PATIENCE)
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    track
}

async fn until<T>(label: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        if let Some(found) = poll() {
            return found;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn receive(client: &Client, track_sid: &str, packets: u64) -> TrackReport {
    client
        .wait_for_packets(track_sid, packets, PATIENCE)
        .await
        .unwrap_or_else(|error| panic!("no media on {track_sid}: {error}"))
}

fn assert_contiguous(report: &TrackReport, label: &str) {
    let span = report
        .sequence
        .expect("the subscriber saw sequence numbers");

    assert_eq!(
        span.first, span.lowest,
        "{label} started in the middle of its own sequence space"
    );
    assert_eq!(
        report.expected(),
        report.unique(),
        "{label} lost or reordered packets: {report:?}"
    );
    assert_eq!(report.missing, 0, "{label} is missing packets");
    assert_eq!(report.gaps, 0, "{label} saw a gap");
    assert_eq!(report.duplicates, 0, "{label} saw a duplicate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_participants_in_a_room_see_and_hear_each_other() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let mut subscribed_states = bob.responses();
    let alice = join(&server, "alice").await;
    let mut published = alice.responses();

    let mic = publish(&alice, TrackSpec::audio("mic")).await;
    let camera = publish(&alice, TrackSpec::video("camera")).await;

    let audio = receive(&bob, mic.sid(), 20).await;
    let video = receive(&bob, camera.sid(), 20).await;

    assert_eq!(audio.kind, Some(mediacore_client::TrackKind::Audio));
    assert_eq!(video.kind, Some(mediacore_client::TrackKind::Video));
    assert_contiguous(&audio, "the audio track");
    assert_contiguous(&video, "the video track");

    for (track_sid, report) in [(mic.sid(), &audio), (camera.sid(), &video)] {
        let snapshot = server.subscription(track_sid, bob.sid());
        let ours = snapshot
            .first_sequence
            .expect("the server forwarded from a sequence of its own");
        let theirs = snapshot
            .source_sequence
            .expect("the server saw the publisher sequence");

        assert!(
            OUR_SEQUENCE_SPACE.contains(&ours),
            "{track_sid} was forwarded from {ours}, which is not in our own space"
        );
        assert_eq!(
            report.sequence.expect("a sequence span").lowest & 0xffff,
            ours & 0xffff,
            "{track_sid} did not start where the server said it would"
        );
        assert!(
            snapshot.packets >= report.packets,
            "the server forwarded fewer packets than the subscriber received"
        );
        assert_ne!(
            ours & 0xffff,
            theirs & 0xffff,
            "{track_sid} was forwarded in the publisher's own sequence space"
        );
    }

    assert!(video.payload_types.len() == 1, "{video:?}");
    assert!(
        server.subscription(camera.sid(), bob.sid()).keyframes > 0,
        "the subscriber never saw a keyframe"
    );

    let subscribed = published
        .wait_for(PATIENCE, |message| match message {
            livekit::signal_response::Message::TrackSubscribed(subscribed) => {
                Some(subscribed.track_sid.clone())
            }
            _ => None,
        })
        .await
        .expect("the publisher is told its track has a subscriber");
    assert!(
        subscribed == mic.sid() || subscribed == camera.sid(),
        "unexpected track {subscribed}"
    );

    let camera_sid = camera.sid().to_owned();
    let quality = published
        .wait_for(PATIENCE, move |message| match message {
            livekit::signal_response::Message::SubscribedQualityUpdate(update)
                if update.track_sid == camera_sid =>
            {
                Some(update.clone())
            }
            _ => None,
        })
        .await
        .expect("the publisher is told which qualities are subscribed");

    assert_eq!(quality.subscribed_codecs.len(), 1);
    assert_eq!(quality.subscribed_codecs[0].codec, "vp8");
    assert!(
        !quality.subscribed_codecs[0].qualities.is_empty()
            && quality.subscribed_codecs[0]
                .qualities
                .iter()
                .all(|quality| quality.enabled),
        "{quality:?}"
    );

    let states = subscribed_states
        .wait_for(PATIENCE, |message| match message {
            livekit::signal_response::Message::StreamStateUpdate(update) => Some(update.clone()),
            _ => None,
        })
        .await
        .expect("the subscriber is told the stream is active");

    assert_eq!(states.stream_states.len(), 1);
    assert_eq!(
        states.stream_states[0].state,
        livekit::StreamState::Active as i32
    );
    assert_eq!(states.stream_states[0].participant_sid, alice.sid());

    bob.unsubscribe([mic.sid(), camera.sid()])
        .expect("the unsubscribe request is sent");

    until("the server to drop both subscriptions", || {
        (server.subscriber_count(mic.sid()) == 0 && server.subscriber_count(camera.sid()) == 0)
            .then_some(())
    })
    .await;

    let stopped = bob
        .received_track(mic.sid())
        .await
        .expect("the client reports")
        .expect("the track was received before");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still_stopped = bob
        .received_track(mic.sid())
        .await
        .expect("the client reports")
        .expect("the track is still reported");

    assert_eq!(
        stopped.packets, still_stopped.packets,
        "media kept flowing after the unsubscribe"
    );

    bob.subscribe([mic.sid()])
        .expect("the resubscribe request is sent");

    let resumed = receive(&bob, mic.sid(), still_stopped.packets + 20).await;
    assert_contiguous(&resumed, "the resumed audio track");
    assert!(
        resumed.sequence.expect("a sequence span").highest
            > still_stopped.sequence.expect("a sequence span").highest,
        "the resumed track did not continue the sequence space it had"
    );

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_subscribers_of_one_track_own_their_sequence_spaces() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let carol = join(&server, "carol").await;
    let alice = join(&server, "alice").await;

    let camera = publish(&alice, TrackSpec::video("camera")).await;

    let seen_by_bob = receive(&bob, camera.sid(), 20).await;
    let seen_by_carol = receive(&carol, camera.sid(), 20).await;

    assert_contiguous(&seen_by_bob, "bob's copy");
    assert_contiguous(&seen_by_carol, "carol's copy");
    assert_eq!(server.subscriber_count(camera.sid()), 2);

    let to_bob = server.subscription(camera.sid(), bob.sid());
    let to_carol = server.subscription(camera.sid(), carol.sid());

    assert_ne!(
        to_bob.first_sequence, to_carol.first_sequence,
        "both subscribers were given the same sequence space"
    );
    assert_ne!(
        seen_by_bob.ssrcs, seen_by_carol.ssrcs,
        "both subscribers were given the same ssrc"
    );

    for start in [to_bob.first_sequence, to_carol.first_sequence] {
        let start = start.expect("a forwarded sequence");
        assert!(OUR_SEQUENCE_SPACE.contains(&start), "{start} is not ours");
    }

    carol.leave().await.expect("carol leaves");

    until("the server to drop carol's subscription", || {
        (server.subscriber_count(camera.sid()) == 1).then_some(())
    })
    .await;

    let after = receive(&bob, camera.sid(), seen_by_bob.packets + 20).await;
    assert_contiguous(&after, "bob's copy after carol left");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_participant_without_the_subscribe_grant_is_refused_and_receives_nothing() {
    let server = Server::start().await;

    let listener = join_with(
        &server,
        "listener",
        VideoGrant {
            can_subscribe: Some(false),
            ..VideoGrant::default()
        },
    )
    .await;
    let mut responses = listener.responses();

    let alice = join(&server, "alice").await;
    let mic = publish(&alice, TrackSpec::audio("mic")).await;

    until("the track to reach the room", || {
        server
            .state
            .registry
            .room(ROOM)
            .and_then(|room| room.track(mic.sid()))
            .map(|_| ())
    })
    .await;

    listener
        .subscribe([mic.sid()])
        .expect("the subscribe request is sent");

    let refusal = responses
        .wait_for(PATIENCE, |message| match message {
            livekit::signal_response::Message::SubscriptionResponse(response) => {
                Some(response.clone())
            }
            _ => None,
        })
        .await
        .expect("the server refuses the subscription");

    assert_eq!(refusal.track_sid, mic.sid());
    assert_eq!(
        refusal.err,
        livekit::SubscriptionError::SeUnknown as i32,
        "a track that exists but may not be received is not a missing track"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(server.subscriber_count(mic.sid()), 0);
    assert!(
        listener
            .received()
            .await
            .expect("the client reports")
            .is_empty(),
        "a participant without the grant received media"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_survives_the_publisher_leaving_and_a_new_one_arriving() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;
    let camera = publish(&alice, TrackSpec::video("camera")).await;

    let seen = receive(&bob, camera.sid(), 20).await;
    assert_contiguous(&seen, "alice's camera");

    alice.leave().await.expect("alice leaves");

    until(
        "the subscription to be torn down with its publisher",
        || (server.subscriber_count(camera.sid()) == 0).then_some(()),
    )
    .await;

    assert_eq!(server.state.router.totals().subscriptions, 0);
    assert!(
        server
            .state
            .registry
            .room(ROOM)
            .expect("the room still exists")
            .track(camera.sid())
            .is_none(),
        "the departed publisher left its track behind"
    );

    let dave = join(&server, "dave").await;
    let mic = publish(&dave, TrackSpec::audio("mic")).await;

    let after = receive(&bob, mic.sid(), 20).await;
    assert_contiguous(&after, "dave's microphone");
    assert_eq!(server.subscriber_count(mic.sid()), 1);

    let snapshot = server.subscription(mic.sid(), bob.sid());
    assert!(
        OUR_SEQUENCE_SPACE.contains(&snapshot.first_sequence.expect("a forwarded sequence")),
        "{snapshot:?}"
    );

    dave.leave().await.expect("dave leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpublishing_a_track_tears_every_subscription_of_it_down() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let mut bob_events = bob.events();
    let alice = join(&server, "alice").await;
    let mut alice_events = alice.events();

    let mic = publish(&alice, TrackSpec::audio("mic")).await;
    let camera = publish(&alice, TrackSpec::video("camera")).await;

    let heard = receive(&bob, mic.sid(), 20).await;
    receive(&bob, camera.sid(), 20).await;
    assert_eq!(server.subscriber_count(mic.sid()), 1);

    alice.unpublish(&mic).await.expect("alice unpublishes");

    let unpublished = alice_events
        .wait_for(PATIENCE, |event| match event {
            ClientEvent::LocalTrackUnpublished { track_sid } => Some(track_sid.clone()),
            _ => None,
        })
        .await
        .expect("the publisher is told its track is gone");
    assert_eq!(unpublished, mic.sid());

    let seen = bob_events
        .wait_for(PATIENCE, |event| match event {
            ClientEvent::TrackUnpublished { track_sid, .. } => Some(track_sid.clone()),
            _ => None,
        })
        .await
        .expect("the subscriber is told the track is gone");
    assert_eq!(seen, mic.sid());

    until("the server to drop the unpublished track", || {
        (server.subscriber_count(mic.sid()) == 0 && server.state.router.source(mic.sid()).is_none())
            .then_some(())
    })
    .await;

    assert!(
        server
            .state
            .registry
            .room(ROOM)
            .expect("the room still exists")
            .track(mic.sid())
            .is_none(),
        "an unpublished track stayed in the room"
    );
    assert!(
        !server
            .state
            .registry
            .room(ROOM)
            .expect("the room still exists")
            .participant("bob")
            .expect("bob is in the room")
            .desired_subscriptions()
            .contains(&mic.sid().to_owned()),
        "bob is still trying to subscribe to a track nobody publishes"
    );

    let frozen = bob
        .received_track(mic.sid())
        .await
        .expect("the client reports")
        .expect("the track was received before");
    assert!(frozen.packets >= heard.packets);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still_frozen = bob
        .received_track(mic.sid())
        .await
        .expect("the client reports")
        .expect("the track is still reported");
    assert_eq!(
        frozen.packets, still_frozen.packets,
        "media kept flowing on an unpublished track"
    );

    let camera_after = receive(&bob, camera.sid(), 20).await;
    assert_contiguous(&camera_after, "the surviving camera track");
    assert_eq!(server.subscriber_count(camera.sid()), 1);

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_under_the_same_identity_takes_the_old_subscriptions_with_it() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;
    let camera = publish(&alice, TrackSpec::video("camera")).await;

    let seen = receive(&bob, camera.sid(), 20).await;
    assert_contiguous(&seen, "alice's camera");
    assert_eq!(server.subscriber_count(camera.sid()), 1);

    let alice_again = join(&server, "alice").await;

    until(
        "the displaced session's subscriptions to be torn down",
        || (server.subscriber_count(camera.sid()) == 0).then_some(()),
    )
    .await;

    until("the router to forget the displaced session", || {
        (server.state.router.totals().subscriptions == 0).then_some(())
    })
    .await;

    assert!(
        !server
            .state
            .registry
            .room(ROOM)
            .expect("the room still exists")
            .participant("bob")
            .expect("bob is in the room")
            .desired_subscriptions()
            .contains(&camera.sid().to_owned()),
        "bob still wants a track the displaced session took with it"
    );

    let mic = publish(&alice_again, TrackSpec::audio("mic")).await;
    let after = receive(&bob, mic.sid(), 20).await;
    assert_contiguous(&after, "the reconnected alice's microphone");
    assert_eq!(server.subscriber_count(mic.sid()), 1);

    alice_again.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}
