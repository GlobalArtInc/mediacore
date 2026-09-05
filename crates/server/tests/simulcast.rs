use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_client::{
    Client, ClientOptions, LayerId, PublishedTrack, TrackReport, TrackSettings, TrackSpec,
    default_layers, proto,
};
use mediacore_config::Config;
use mediacore_proto::livekit;
use mediacore_server::forward::SubscriptionSnapshot;
use mediacore_server::http;
use mediacore_server::state::AppState;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "demo";
const PATIENCE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const PAUSE_DELAY: Duration = Duration::from_millis(500);
const LOW: u8 = b'q';
const MEDIUM: u8 = b'h';
const HIGH: u8 = b'f';

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
        config.video.dynacast_pause_delay_ms = PAUSE_DELAY.as_millis() as u32;
        config.video.layer_upgrade_hold_ms = 200;

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

    async fn layer_plis(&self, track_sid: &str, rid: &str) -> u64 {
        let Some(source) = self.state.router.source(track_sid) else {
            return 0;
        };

        source
            .layer_stats()
            .await
            .into_iter()
            .find(|layer| layer.rid.map(|rid| rid.to_string()).as_deref() == Some(rid))
            .map(|layer| layer.plis)
            .unwrap_or_default()
    }

    fn missed_keyframe_requests(&self, track_sid: &str) -> u64 {
        self.state
            .router
            .source(track_sid)
            .map(|source| source.missed_keyframe_requests())
            .unwrap_or_default()
    }

    fn reported_layers(&self, track_sid: &str) -> Vec<livekit::VideoLayer> {
        self.state
            .registry
            .room(ROOM)
            .and_then(|room| room.track(track_sid))
            .map(|(_, track)| track.reported_layers().to_vec())
            .unwrap_or_default()
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

async fn publish_simulcast(client: &Client) -> PublishedTrack {
    let track = client
        .publish(TrackSpec::simulcast_video("camera"))
        .await
        .expect("the track is published");

    client
        .wait_connected(PATIENCE)
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    track
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

async fn report(client: &Client, track_sid: &str) -> Option<TrackReport> {
    client
        .received_track(track_sid)
        .await
        .expect("the client reports")
}

async fn on_layer(client: &Client, track_sid: &str, tag: u8, label: &str) -> TrackReport {
    let deadline = Instant::now() + PATIENCE;

    loop {
        if let Some(report) = report(client, track_sid).await
            && report.current_layer() == Some(LayerId::Tag(tag))
        {
            return report;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for {label} to receive layer {}",
            char::from(tag)
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
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

fn qualities(update: &livekit::SubscribedQualityUpdate) -> Vec<(i32, bool)> {
    update
        .subscribed_codecs
        .first()
        .map(|codec| {
            codec
                .qualities
                .iter()
                .map(|quality| (quality.quality, quality.enabled))
                .collect()
        })
        .unwrap_or_default()
}

fn enabled_up_to(
    update: &livekit::SubscribedQualityUpdate,
    quality: livekit::VideoQuality,
) -> bool {
    qualities(update)
        .into_iter()
        .all(|(reported, enabled)| enabled == (reported <= quality as i32))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_subscriber_of_one_simulcast_track_gets_the_layer_it_asked_for() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let carol = join(&server, "carol").await;
    let alice = join(&server, "alice").await;

    let camera = publish_simulcast(&alice).await;
    let sid = camera.sid().to_owned();

    assert_eq!(camera.rids(), vec!["q", "h", "f"]);

    let declared = until("the room to carry the declared ladder", || {
        let layers = server.reported_layers(&sid);
        (layers.len() == 3).then_some(layers)
    })
    .await;
    assert_eq!(
        declared
            .iter()
            .map(|layer| layer.rid.clone())
            .collect::<Vec<_>>(),
        vec!["q", "h", "f"],
        "the track carries the layers the publisher declared"
    );
    assert_eq!(declared[0].quality, livekit::VideoQuality::Low as i32);
    assert_eq!(declared[2].width, 1280);

    bob.request_quality([sid.as_str()], proto::VideoQuality::Low)
        .expect("bob asks for the small layer");
    carol
        .request_quality([sid.as_str()], proto::VideoQuality::High)
        .expect("carol asks for the big one");

    let seen_by_bob = on_layer(&bob, &sid, LOW, "bob").await;
    let seen_by_carol = on_layer(&carol, &sid, HIGH, "carol").await;

    let bob_after = grew(&bob, &sid, 30).await;
    let carol_after = grew(&carol, &sid, 30).await;

    assert_only_layer(&seen_by_bob, &bob_after, LOW, "bob");
    assert_only_layer(&seen_by_carol, &carol_after, HIGH, "carol");
    assert_contiguous(&bob_after, "bob's copy");
    assert_contiguous(&carol_after, "carol's copy");

    assert_eq!(
        server.subscription(&sid, bob.sid()).layer,
        Some(0),
        "the server thinks it is forwarding another layer to bob"
    );
    assert_eq!(server.subscription(&sid, carol.sid()).layer, Some(2));
    assert_ne!(
        bob_after.ssrcs, carol_after.ssrcs,
        "both subscribers were given the same outgoing stream"
    );

    assert_eq!(
        server.missed_keyframe_requests(&sid),
        0,
        "a keyframe was asked for without naming a layer, so it reached an arbitrary one"
    );

    let live = until("every layer to be seen arriving", || {
        let layers = server.reported_layers(&sid);
        layers.iter().all(|layer| layer.ssrc != 0).then_some(layers)
    })
    .await;
    assert_eq!(live.len(), 3);
    assert_eq!(
        live.iter().filter(|layer| layer.ssrc != 0).count(),
        3,
        "every arriving layer names the stream it arrived on"
    );

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
    carol.leave().await.expect("carol leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscriber_that_changes_quality_switches_without_breaking_its_stream() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;

    let camera = publish_simulcast(&alice).await;
    let sid = camera.sid().to_owned();

    let high = on_layer(&bob, &sid, HIGH, "bob").await;
    grew(&bob, &sid, 20).await;

    bob.request_quality([sid.as_str()], proto::VideoQuality::Low)
        .expect("bob asks for the small layer");

    let low = on_layer(&bob, &sid, LOW, "bob").await;
    let after = grew(&bob, &sid, 30).await;

    assert_contiguous(&after, "bob's copy across the switch");
    assert_only_layer(&low, &after, LOW, "bob after the switch");
    assert!(
        high.packets > 0,
        "bob never received the layer it started on"
    );

    let switch = after
        .switches
        .iter()
        .find(|switch| switch.to == LayerId::Tag(LOW))
        .expect("the switch onto the small layer was recorded");

    assert_eq!(switch.from, LayerId::Tag(HIGH));
    assert_eq!(
        switch.sequence_step(),
        1,
        "the sequence space broke across the switch: {switch:?}"
    );
    assert!(
        switch.timestamp_step() > 0,
        "the timestamp went backwards across the switch: {switch:?}"
    );
    assert!(switch.is_seamless());

    assert!(
        server.layer_plis(&sid, "q").await > 0,
        "the layer bob asked for was never asked for a keyframe"
    );
    assert_eq!(
        server.missed_keyframe_requests(&sid),
        0,
        "a keyframe was asked for without naming a layer"
    );

    let snapshot = server.subscription(&sid, bob.sid());
    assert_eq!(snapshot.layer, Some(0));
    assert_eq!(snapshot.target_layer, 0);
    assert!(snapshot.switches >= 1);

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layer_that_stops_arriving_makes_its_subscribers_fall_back() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;

    let camera = publish_simulcast(&alice).await;
    let sid = camera.sid().to_owned();

    let before = on_layer(&bob, &sid, HIGH, "bob").await;
    grew(&bob, &sid, 20).await;

    camera
        .stop_layer("f")
        .await
        .expect("the publisher stops its top layer");

    let fallen_back = on_layer(&bob, &sid, MEDIUM, "bob").await;
    let after = grew(&bob, &sid, 30).await;

    assert_contiguous(&after, "bob's copy after the top layer went quiet");
    assert_only_layer(&fallen_back, &after, MEDIUM, "bob after the fallback");
    assert!(before.packets > 0);
    assert_eq!(server.subscription(&sid, bob.sid()).layer, Some(1));

    let live = until(
        "the track to stop reporting the layer that went quiet",
        || {
            let layers = server.reported_layers(&sid);
            (layers.len() == 2).then_some(layers)
        },
    )
    .await;
    assert_eq!(
        live.iter()
            .map(|layer| layer.rid.clone())
            .collect::<Vec<_>>(),
        vec!["q", "h"],
        "the track still claims a layer nobody is sending"
    );

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layer_nobody_wants_stops_being_sent_and_comes_back_before_it_is_needed() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;
    let mut published = alice.responses();

    let camera = publish_simulcast(&alice).await;
    let sid = camera.sid().to_owned();

    let subscribed = sid.clone();
    let first = published
        .wait_for(PATIENCE, move |message| match message {
            livekit::signal_response::Message::SubscribedQualityUpdate(update)
                if update.track_sid == subscribed
                    && enabled_up_to(update, livekit::VideoQuality::High) =>
            {
                Some(update.clone())
            }
            _ => None,
        })
        .await
        .expect("the publisher is told the whole ladder is subscribed");

    assert_eq!(
        qualities(&first).len(),
        3,
        "the reference names every quality, enabled or not"
    );

    on_layer(&bob, &sid, HIGH, "bob").await;

    let asked_at = Instant::now();
    bob.request_quality([sid.as_str()], proto::VideoQuality::Low)
        .expect("bob asks for the small layer only");

    let subscribed = sid.clone();
    let stopped = published
        .wait_for(PATIENCE, move |message| match message {
            livekit::signal_response::Message::SubscribedQualityUpdate(update)
                if update.track_sid == subscribed
                    && !qualities(update).is_empty()
                    && qualities(update).iter().any(|(quality, enabled)| {
                        *quality == livekit::VideoQuality::High as i32 && !*enabled
                    }) =>
            {
                Some(update.clone())
            }
            _ => None,
        })
        .await
        .expect("the publisher is told to stop the layers nobody wants");

    assert!(
        asked_at.elapsed() >= PAUSE_DELAY,
        "the publisher was stopped after {:?}, before the pause delay of {PAUSE_DELAY:?} had run",
        asked_at.elapsed()
    );
    assert!(
        enabled_up_to(&stopped, livekit::VideoQuality::Low),
        "only the layer bob wants may stay on: {:?}",
        qualities(&stopped)
    );
    #[allow(deprecated)]
    let legacy = stopped.subscribed_qualities.len();
    assert_eq!(legacy, 3, "the deprecated field carries the same answer");

    bob.request_quality([sid.as_str()], proto::VideoQuality::High)
        .expect("bob asks for the big layer again");

    let subscribed = sid.clone();
    let resumed = published
        .wait_for(PATIENCE, move |message| match message {
            livekit::signal_response::Message::SubscribedQualityUpdate(update)
                if update.track_sid == subscribed
                    && qualities(update).iter().any(|(quality, enabled)| {
                        *quality == livekit::VideoQuality::High as i32 && *enabled
                    }) =>
            {
                Some(update.clone())
            }
            _ => None,
        })
        .await
        .expect("the publisher is told to send the top layer again");

    assert!(enabled_up_to(&resumed, livekit::VideoQuality::High));

    let back = on_layer(&bob, &sid, HIGH, "bob").await;
    let after = grew(&bob, &sid, 20).await;
    assert_contiguous(&after, "bob's copy after the layer came back");
    assert_only_layer(&back, &after, HIGH, "bob back on the top layer");

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscriber_that_asks_for_a_small_picture_is_given_the_small_layer() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;

    let camera = publish_simulcast(&alice).await;
    let sid = camera.sid().to_owned();
    let ladder = default_layers();

    on_layer(&bob, &sid, HIGH, "bob").await;

    bob.request_video_size([sid.as_str()], ladder[0].width, ladder[0].height)
        .expect("bob asks for a small picture");

    let small = on_layer(&bob, &sid, LOW, "bob").await;
    let after = grew(&bob, &sid, 20).await;
    assert_only_layer(&small, &after, LOW, "bob on the small picture");

    bob.request_video_size([sid.as_str()], ladder[1].width, ladder[1].height)
        .expect("bob asks for a bigger picture");

    let medium = on_layer(&bob, &sid, MEDIUM, "bob").await;
    let after = grew(&bob, &sid, 20).await;
    assert_only_layer(&medium, &after, MEDIUM, "bob on the middle picture");
    assert_contiguous(&after, "bob's copy across two size changes");

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_track_stops_arriving_and_resumes_where_it_left_off() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let mut states = bob.responses();
    let alice = join(&server, "alice").await;

    let camera = publish_simulcast(&alice).await;
    let sid = camera.sid().to_owned();

    grew(&bob, &sid, 20).await;

    bob.update_track_settings(&TrackSettings::for_tracks([sid.as_str()]).disabled(true))
        .expect("bob disables the track");

    let paused = states
        .wait_for(PATIENCE, |message| match message {
            livekit::signal_response::Message::StreamStateUpdate(update)
                if update
                    .stream_states
                    .iter()
                    .any(|state| state.state == livekit::StreamState::Paused as i32) =>
            {
                Some(update.clone())
            }
            _ => None,
        })
        .await
        .expect("bob is told the stream is paused");
    assert_eq!(paused.stream_states[0].track_sid, sid);
    assert_eq!(paused.stream_states[0].participant_sid, alice.sid());

    tokio::time::sleep(Duration::from_millis(300)).await;
    let frozen = report(&bob, &sid).await.expect("bob received before");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let still_frozen = report(&bob, &sid).await.expect("bob still reports");

    assert_eq!(
        frozen.packets, still_frozen.packets,
        "media kept flowing on a track the subscriber disabled"
    );
    assert!(server.subscription(&sid, bob.sid()).paused);

    bob.update_track_settings(&TrackSettings::for_tracks([sid.as_str()]).disabled(false))
        .expect("bob enables the track again");

    let resumed = grew(&bob, &sid, 20).await;
    assert_contiguous(&resumed, "bob's copy across a pause");
    assert!(
        resumed.sequence.expect("a sequence span").highest
            > still_frozen.sequence.expect("a sequence span").highest,
        "the resumed track did not continue the sequence space it had"
    );

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}
