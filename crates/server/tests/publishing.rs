use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_client::{Client, ClientOptions, TrackSpec};
use mediacore_config::Config;
use mediacore_proto::mediacore;
use mediacore_server::http;
use mediacore_server::publisher::{PublisherSnapshot, TrackSnapshot};
use mediacore_server::state::AppState;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "demo";
const PATIENCE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

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
        // A directory of this server's own for session records: a test must
        // never write into the tree, and two servers sharing a store would
        // read each other's meetings back.
        config.sessions.directory = tempfile::TempDir::new().unwrap().keep();

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

    fn participant(&self, identity: &str) -> std::sync::Arc<mediacore_room::Participant> {
        self.state
            .registry
            .room(ROOM)
            .expect("the room exists")
            .participant(identity)
            .expect("the participant is in the room")
    }

    fn publisher(&self, identity: &str) -> PublisherSnapshot {
        self.state
            .publishers
            .snapshot(&self.participant(identity).sid)
            .unwrap_or_default()
    }

    fn track_rtp(&self, identity: &str, track_sid: &str) -> TrackSnapshot {
        self.state
            .publishers
            .track_snapshot(&self.participant(identity).sid, track_sid)
            .unwrap_or_default()
    }

    fn attributed_tracks(&self, identity: &str) -> Vec<String> {
        self.state
            .publishers
            .attributed_tracks(&self.participant(identity).sid)
    }
}

fn patient_options() -> ClientOptions {
    ClientOptions {
        connect_timeout: PATIENCE,
        response_timeout: PATIENCE,
        ..ClientOptions::default()
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
    Client::connect_with(
        &server.url(),
        &token(identity, VideoGrant::default()),
        patient_options(),
    )
    .await
    .expect("the client joins")
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

#[tokio::test]
async fn a_published_audio_track_reaches_the_server_and_the_room() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");

    assert!(track.sid().starts_with("TR_A"), "sid was {}", track.sid());

    alice
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    let snapshot = until("rtp to arrive at the server", || {
        let snapshot = server.publisher("alice");
        (snapshot.connected && snapshot.rtp_packets > 0).then_some(snapshot)
    })
    .await;

    assert!(snapshot.rtp_bytes > 0);
    assert_eq!(snapshot.bound_tracks, 1);
    assert_eq!(snapshot.rtp_unattributed, 0);

    let attributed = until("the server to credit the track with the packets", || {
        let stats = server.track_rtp("alice", track.sid());
        (stats.rtp_packets > 0).then_some(stats)
    })
    .await;
    assert!(attributed.rtp_bytes > 0);
    assert_eq!(
        server.attributed_tracks("alice"),
        vec![track.sid().to_owned()]
    );

    let published = until("the track to be bound to its media section", || {
        server
            .participant("alice")
            .track(track.sid())
            .filter(|published| published.is_negotiated())
    })
    .await;

    assert_eq!(published.cid, track.cid());
    assert_eq!(published.mid.as_deref(), Some(track.mid().as_str()));
    assert_eq!(published.mime_type.as_deref(), Some("audio/opus"));
    assert_eq!(published.kind, mediacore_room::TrackKind::Audio);

    let seen = until("bob to see the track in a participant update", || {
        bob.participants()
            .into_iter()
            .find(|participant| participant.identity == "alice")
            .and_then(|participant| participant.tracks.first().cloned())
    })
    .await;

    assert_eq!(seen.sid, track.sid());
    assert_eq!(seen.name, "mic");
    assert_eq!(seen.r#type, mediacore::TrackType::Audio as i32);
    assert_eq!(seen.mime_type, "audio/opus");

    alice.leave().await.expect("alice leaves");
    bob.leave().await.expect("bob leaves");
}

#[tokio::test]
async fn rtp_keeps_arriving_once_the_publisher_stops_repeating_the_mid() {
    let server = Server::start().await;
    let alice = join(&server, "alice").await;

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");

    alice
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    let before = until("the publisher to stop repeating the mid extension", || {
        let snapshot = server.publisher("alice");
        (snapshot.rtp_without_mid_extension > 0).then_some(snapshot)
    })
    .await;

    assert!(before.rtp_packets > 0, "no rtp arrived at all");
    assert!(
        before.rtp_packets > before.rtp_without_mid_extension,
        "the publisher never carried the mid extension in the first place"
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = server.publisher("alice");

    assert!(
        after.rtp_without_mid_extension > before.rtp_without_mid_extension,
        "rtp intake stalled once the mid extension went away, \
         {} of {} packets carried no mid",
        after.rtp_without_mid_extension,
        after.rtp_packets
    );
    assert_eq!(
        after.rtp_unattributed, 0,
        "packets without a mid extension were not attributed to any track"
    );

    let attributed = server.track_rtp("alice", track.sid());
    assert!(
        attributed.rtp_without_mid_extension > 0,
        "the track saw no packet that lacked the mid extension"
    );
    assert_eq!(attributed.rtp_packets, after.rtp_packets);

    let sent = track.stats().await.expect("client statistics").packets;
    assert!(
        after.rtp_packets * 10 >= sent * 9,
        "the server saw {} of the {sent} packets the client sent",
        after.rtp_packets
    );
}

#[tokio::test]
async fn rtp_is_attributed_to_the_track_it_belongs_to() {
    let server = Server::start().await;
    let alice = join(&server, "alice").await;

    let voice = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the first track is published");

    alice
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");

    let screen = alice
        .publish(TrackSpec::audio("screen-audio"))
        .await
        .expect("the renegotiated track is published");

    assert_ne!(voice.sid(), screen.sid());
    assert_ne!(voice.mid(), screen.mid());

    voice.start().await.expect("the first track starts");

    let only_voice = until("rtp on the first track", || {
        let stats = server.track_rtp("alice", voice.sid());
        (stats.rtp_packets > 0).then_some(stats)
    })
    .await;

    assert!(only_voice.rtp_bytes > 0);
    assert_eq!(
        server.track_rtp("alice", screen.sid()),
        TrackSnapshot::default(),
        "a silent track must not be credited with another track's packets"
    );

    screen.start().await.expect("the second track starts");

    let on_screen = until("rtp on the renegotiated track", || {
        let stats = server.track_rtp("alice", screen.sid());
        (stats.rtp_packets > 0).then_some(stats)
    })
    .await;
    assert!(on_screen.rtp_bytes > 0);

    let mut attributed = server.attributed_tracks("alice");
    attributed.sort();
    let mut published = vec![voice.sid().to_owned(), screen.sid().to_owned()];
    published.sort();
    assert_eq!(attributed, published);

    let totals = server.publisher("alice");
    assert_eq!(totals.rtp_unattributed, 0);
    assert_eq!(
        totals.rtp_packets,
        server.track_rtp("alice", voice.sid()).rtp_packets
            + server.track_rtp("alice", screen.sid()).rtp_packets
    );
    assert_eq!(totals.bound_tracks, 2);

    for track in [&voice, &screen] {
        let published = server
            .participant("alice")
            .track(track.sid())
            .expect("the track is in the room");
        assert!(published.is_negotiated());
        assert_eq!(published.mid.as_deref(), Some(track.mid().as_str()));
        assert_eq!(published.mime_type.as_deref(), Some("audio/opus"));
    }
}

#[tokio::test]
async fn every_trickled_candidate_is_a_candidate_a_client_can_parse() {
    let server = Server::start().await;
    let alice = join(&server, "alice").await;
    let mut responses = alice.responses();

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");

    let mut trickles = Vec::new();

    loop {
        let message = responses
            .next(PATIENCE)
            .await
            .expect("the server keeps signalling");

        if let mediacore::signal_response::Message::Trickle(trickle) = message {
            let last = trickle.r#final;
            trickles.push(trickle);
            if last {
                break;
            }
        }
    }

    assert!(!trickles.is_empty(), "the server trickled nothing at all");
    assert_eq!(
        trickles.iter().filter(|trickle| trickle.r#final).count(),
        1,
        "the end of candidates must be signalled exactly once"
    );
    assert!(
        trickles.last().expect("a candidate").r#final,
        "the final flag rides on the last candidate"
    );

    for trickle in &trickles {
        assert_eq!(trickle.target, mediacore::SignalTarget::Publisher as i32);
        assert!(
            !trickle.candidate_init.is_empty(),
            "an empty candidateInit is not parseable json"
        );

        let init: serde_json::Value = serde_json::from_str(&trickle.candidate_init)
            .expect("candidateInit is the json a browser parses");

        let candidate = init["candidate"].as_str().expect("a candidate string");
        assert!(candidate.starts_with("candidate:"), "was {candidate}");
        assert_eq!(
            init["sdpMid"].as_str(),
            Some(track.mid().as_str()),
            "sdpMid must name a media section that exists"
        );
        assert_eq!(init["sdpMLineIndex"].as_u64(), Some(0));
    }
}

#[tokio::test]
async fn muting_a_published_track_is_seen_by_the_other_participants() {
    let server = Server::start().await;

    let bob = join(&server, "bob").await;
    let alice = join(&server, "alice").await;

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");

    alice
        .send(mediacore::signal_request::Message::Mute(
            mediacore::MuteTrackRequest {
                sid: track.sid().to_owned(),
                muted: true,
            },
        ))
        .expect("the mute request is sent");

    let muted = until("bob to see the track muted", || {
        bob.participants()
            .into_iter()
            .find(|participant| participant.identity == "alice")
            .and_then(|participant| participant.tracks.first().cloned())
            .filter(|info| info.muted)
    })
    .await;

    assert_eq!(muted.sid, track.sid());
    assert!(
        server
            .participant("alice")
            .track(track.sid())
            .unwrap()
            .muted
    );
}

#[tokio::test]
async fn a_participant_without_the_grant_is_told_why_it_cannot_publish() {
    let server = Server::start().await;

    let client = Client::connect(
        &server.url(),
        &token(
            "listener",
            VideoGrant {
                can_publish: Some(false),
                ..VideoGrant::default()
            },
        ),
    )
    .await
    .expect("the client joins");

    let mut responses = client.responses();
    let spec = TrackSpec::audio("mic");
    client
        .send(mediacore::signal_request::Message::AddTrack(
            spec.to_request(),
        ))
        .expect("the request is sent");

    let refusal = responses
        .wait_for(PATIENCE, |message| match message {
            mediacore::signal_response::Message::RequestResponse(response) => {
                Some(response.clone())
            }
            _ => None,
        })
        .await
        .expect("the server answers the request");

    assert_eq!(
        refusal.reason,
        mediacore::request_response::Reason::NotAllowed as i32
    );
    assert!(server.participant("listener").tracks().is_empty());
}

#[tokio::test]
async fn a_publisher_that_never_offers_leaves_no_transport_behind() {
    let server = Server::start().await;
    let client = join(&server, "alice").await;

    assert_eq!(server.state.publishers.count(), 1);
    assert_eq!(server.publisher("alice"), PublisherSnapshot::default());

    client.leave().await.expect("alice leaves");

    until("the publisher registration to be dropped", || {
        (server.state.publishers.count() == 0).then_some(())
    })
    .await;
}
