use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_client::{Client, ClientOptions, LayerId, PublishedTrack, TrackReport, TrackSpec};
use mediacore_config::Config;
use mediacore_proto::mediacore;
use mediacore_server::forward::SubscriptionSnapshot;
use mediacore_server::http;
use mediacore_server::state::AppState;
use mediacore_sfu::{Bitrate, LayerSet};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "demo";
const PATIENCE: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const LOW: u8 = b'q';
const HIGH: u8 = b'f';
const ROOMY: Bitrate = Bitrate::mbps(5);
const CRAMPED: Bitrate = Bitrate::kbps(100);
const HEALTHY_SAMPLES: u64 = 2;
/// The index of the `f` rung, as the allocator counts them.
const TOP_LAYER: u8 = 2;
const SETTLE: Duration = Duration::from_millis(500);

struct Server {
    addr: SocketAddr,
    state: AppState,
}

impl Server {
    async fn start(shape: impl FnOnce(&mut Config)) -> Self {
        let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
        config.port = 0;
        config.rtc.udp_port = 0;
        config.rtc.port_range_start = 0;
        config.rtc.port_range_end = 0;
        config.rtc.bind_address = "127.0.0.1".to_owned();
        // A directory of this server's own for session records: a test must
        // never write into the tree, and two servers sharing a store would
        // read each other's meetings back.
        config.sessions.directory = tempfile::TempDir::new().unwrap().keep();
        config.video.dynacast_pause_delay_ms = 500;
        config.video.layer_upgrade_hold_ms = 200;
        config.video.bwe.enabled = true;
        config.video.bwe.allocation_interval_ms = 100;
        config.video.bwe.recovery_hold_ms = 0;
        config.video.bwe.recovery_step_ms = 0;
        shape(&mut config);

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

    fn pin(&self, subscriber_sid: &str, estimate: Bitrate) {
        self.state
            .bandwidth
            .of(subscriber_sid)
            .pin_estimate(Some(estimate));
    }

    fn subscription(&self, track_sid: &str, subscriber_sid: &str) -> SubscriptionSnapshot {
        self.state
            .router
            .subscription(track_sid, subscriber_sid)
            .unwrap_or_default()
    }

    fn committed_layers(&self, track_sid: &str) -> Option<LayerSet> {
        self.state.router.source(track_sid)?.committed_layers()
    }
}

fn token(identity: &str) -> String {
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
            ..VideoGrant::default()
        },
        ..Claims::default()
    };

    mediacore_auth::issue(API_SECRET, &claims).unwrap()
}

async fn join(server: &Server, identity: &str) -> Client {
    Client::connect_with(
        &server.url(),
        &token(identity),
        ClientOptions {
            connect_timeout: PATIENCE,
            response_timeout: PATIENCE,
            ..ClientOptions::default()
        },
    )
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

async fn report(client: &Client, track_sid: &str) -> Option<TrackReport> {
    client
        .received_track(track_sid)
        .await
        .expect("the client reports")
}

async fn on_layer(client: &Client, track_sid: &str, tag: u8, label: &str) -> TrackReport {
    let deadline = Instant::now() + PATIENCE;

    loop {
        if let Some(found) = report(client, track_sid).await
            && found.current_layer() == Some(LayerId::Tag(tag))
        {
            return found;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for {label} to receive layer {}",
            char::from(tag)
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn until<T>(label: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;

    loop {
        if let Some(found) = poll() {
            return found;
        }

        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn sampled(server: &Server, subscriber_sid: &str, count: u64) {
    until(
        "the interpreter to have watched the link for a while",
        || {
            let seen = server.state.bandwidth.of(subscriber_sid).snapshot().samples;

            (seen >= count).then_some(())
        },
    )
    .await;
}

/// Wait until the allocator has settled this subscription on a layer.
///
/// A subscriber's first packets are whatever the forwarder had before the
/// estimator had run once -- the top rung -- and the allocator steps it down a
/// few milliseconds later, once its first real budget arrives. So the first
/// packet of a layer is as likely to be that transient as it is to be a
/// decision, and a test that starts measuring on it can be measuring across a
/// step down it never asked for. The server's own view changes when the
/// allocator decides and not before, which is the precondition these tests
/// actually want.
async fn settled_on(server: &Server, track_sid: &str, subscriber_sid: &str, layer: u8) {
    until("the allocator to settle on a layer", || {
        (server.subscription(track_sid, subscriber_sid).layer == Some(layer)).then_some(())
    })
    .await;
}

async fn grew(client: &Client, track_sid: &str, by: u64) -> TrackReport {
    let now = report(client, track_sid)
        .await
        .map(|report| report.packets)
        .unwrap_or_default();

    client
        .wait_for_packets(track_sid, now + by, PATIENCE)
        .await
        .unwrap_or_else(|error| panic!("no media on {track_sid}: {error}"))
}

async fn packets(client: &Client, track_sid: &str) -> u64 {
    report(client, track_sid)
        .await
        .map(|report| report.packets)
        .unwrap_or_default()
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

fn layer_packets(report: &TrackReport, tag: u8) -> u64 {
    report
        .layer(&LayerId::Tag(tag))
        .map(|layer| layer.packets)
        .unwrap_or_default()
}

fn assert_only_layer(before: &TrackReport, after: &TrackReport, tag: u8, label: &str) {
    let total = after.packets - before.packets;
    let of_layer = layer_packets(after, tag) - layer_packets(before, tag);

    assert!(total > 0, "{label} received nothing to judge");
    assert_eq!(
        total,
        of_layer,
        "{label} received {} packets of another layer, layers are {:?}",
        total - of_layer,
        after.layer_ids()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscriber_whose_budget_collapses_steps_down_and_keeps_its_sequence() {
    let server = Server::start(|_| {}).await;

    let bob = join(&server, "bob").await;
    server.pin(bob.sid(), ROOMY);
    let alice = join(&server, "alice").await;

    let camera = publish(&alice, TrackSpec::simulcast_video("camera")).await;
    let sid = camera.sid().to_owned();

    sampled(&server, bob.sid(), HEALTHY_SAMPLES).await;
    settled_on(&server, &sid, bob.sid(), TOP_LAYER).await;

    let top = on_layer(&bob, &sid, HIGH, "bob").await;
    assert_only_layer(
        &top,
        &grew(&bob, &sid, 20).await,
        HIGH,
        "bob before the squeeze",
    );

    server.pin(bob.sid(), CRAMPED);

    let stepped = on_layer(&bob, &sid, LOW, "bob").await;
    let after = grew(&bob, &sid, 30).await;

    assert_contiguous(&after, "bob's copy across the step down");
    assert_only_layer(&stepped, &after, LOW, "bob after the squeeze");
    assert_eq!(
        server.subscription(&sid, bob.sid()).layer,
        Some(0),
        "the server thinks it is still forwarding another layer"
    );
    assert!(
        !server.subscription(&sid, bob.sid()).bandwidth_paused,
        "a squeeze must step down rather than pause while pausing is not allowed"
    );

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_keeps_flowing_while_video_is_paused_for_bandwidth() {
    let server = Server::start(|config| {
        config.video.bwe.allow_pause = true;
        config.video.bwe.min_budget_kbps = 1;
    })
    .await;

    let bob = join(&server, "bob").await;
    server.pin(bob.sid(), ROOMY);
    let alice = join(&server, "alice").await;

    let mic = publish(&alice, TrackSpec::audio("mic")).await;
    let camera = publish(&alice, TrackSpec::simulcast_video("camera")).await;
    let video_sid = camera.sid().to_owned();
    let audio_sid = mic.sid().to_owned();

    on_layer(&bob, &video_sid, HIGH, "bob").await;
    grew(&bob, &audio_sid, 20).await;

    sampled(&server, bob.sid(), HEALTHY_SAMPLES).await;
    settled_on(&server, &video_sid, bob.sid(), TOP_LAYER).await;
    assert!(
        !server.subscription(&video_sid, bob.sid()).bandwidth_paused,
        "the squeeze has not happened yet and the video is already paused"
    );

    // Subscribed here rather than before alice publishes: with pausing allowed
    // and a floor of one kbps, the allocator's first pass over a subscriber it
    // has no estimate for can pause the video on its own, and a listener opened
    // earlier matches that update instead of the one the squeeze below causes.
    let mut states = bob.responses();
    server.pin(bob.sid(), Bitrate::kbps(1));

    let paused = states
        .wait_for(PATIENCE, |message| match message {
            mediacore::signal_response::Message::StreamStateUpdate(update)
                if update.stream_states.iter().any(|state| {
                    state.track_sid == video_sid
                        && state.state == mediacore::StreamState::Paused as i32
                }) =>
            {
                Some(update.clone())
            }
            _ => None,
        })
        .await
        .expect("bob is told the video stream is paused for bandwidth");

    assert_eq!(paused.stream_states[0].participant_sid, alice.sid());
    assert!(server.subscription(&video_sid, bob.sid()).bandwidth_paused);
    assert!(
        !server.subscription(&video_sid, bob.sid()).paused,
        "a bandwidth pause is not the subscriber's own pause"
    );

    let mut resumes = bob.responses();
    tokio::time::sleep(SETTLE).await;

    let video = packets(&bob, &video_sid).await;
    let audio = packets(&bob, &audio_sid).await;
    grew(&bob, &audio_sid, 20).await;

    assert_eq!(
        packets(&bob, &video_sid).await,
        video,
        "video kept flowing on a track that was paused for bandwidth"
    );
    assert!(
        packets(&bob, &audio_sid).await > audio,
        "audio stopped with the video it was never enrolled with"
    );

    server.pin(bob.sid(), ROOMY);

    resumes
        .wait_for(PATIENCE, |message| match message {
            mediacore::signal_response::Message::StreamStateUpdate(update)
                if update.stream_states.iter().any(|state| {
                    state.track_sid == video_sid
                        && state.state == mediacore::StreamState::Active as i32
                }) =>
            {
                Some(())
            }
            _ => None,
        })
        .await
        .expect("bob is told the video stream is active again");

    let resumed = grew(&bob, &video_sid, 30).await;
    assert_contiguous(&resumed, "bob's copy across a bandwidth pause");
    assert!(!server.subscription(&video_sid, bob.sid()).bandwidth_paused);

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_budget_that_returns_restores_the_layer_the_subscriber_asked_for() {
    let server = Server::start(|_| {}).await;

    let bob = join(&server, "bob").await;
    server.pin(bob.sid(), ROOMY);
    let alice = join(&server, "alice").await;

    let camera = publish(&alice, TrackSpec::simulcast_video("camera")).await;
    let sid = camera.sid().to_owned();

    bob.request_quality([sid.as_str()], mediacore_client::proto::VideoQuality::High)
        .expect("bob asks for the big picture");
    on_layer(&bob, &sid, HIGH, "bob").await;

    sampled(&server, bob.sid(), HEALTHY_SAMPLES).await;
    server.pin(bob.sid(), CRAMPED);
    on_layer(&bob, &sid, LOW, "bob").await;

    server.pin(bob.sid(), ROOMY);

    let back = on_layer(&bob, &sid, HIGH, "bob").await;
    let after = grew(&bob, &sid, 30).await;

    assert_only_layer(&back, &after, HIGH, "bob back on the layer it asked for");
    assert_contiguous(&after, "bob's copy across a squeeze and a recovery");

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_subscribers_of_one_track_are_allocated_independently() {
    let server = Server::start(|_| {}).await;

    let bob = join(&server, "bob").await;
    let carol = join(&server, "carol").await;
    server.pin(bob.sid(), ROOMY);
    server.pin(carol.sid(), ROOMY);
    let alice = join(&server, "alice").await;

    let camera = publish(&alice, TrackSpec::simulcast_video("camera")).await;
    let sid = camera.sid().to_owned();

    on_layer(&bob, &sid, HIGH, "bob").await;
    on_layer(&carol, &sid, HIGH, "carol").await;

    sampled(&server, bob.sid(), HEALTHY_SAMPLES).await;
    sampled(&server, carol.sid(), HEALTHY_SAMPLES).await;

    let carol_top = grew(&carol, &sid, 5).await;
    server.pin(bob.sid(), CRAMPED);
    on_layer(&bob, &sid, LOW, "bob").await;

    let carol_after = grew(&carol, &sid, 30).await;

    assert_only_layer(
        &carol_top,
        &carol_after,
        HIGH,
        "carol while bob was squeezed",
    );
    assert_contiguous(&carol_after, "carol's copy");
    assert_eq!(
        server.subscription(&sid, carol.sid()).layer,
        Some(2),
        "carol was stepped down by a squeeze that was not hers"
    );
    assert_eq!(server.subscription(&sid, bob.sid()).layer, Some(0));

    let committed = until("the publisher to be told which layers are wanted", || {
        server.committed_layers(&sid)
    })
    .await;
    assert_eq!(
        committed,
        LayerSet::all(),
        "a squeezed subscriber pulled the publisher's encoder down and degraded the room"
    );

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
    carol.leave().await.expect("carol leaves");
}
