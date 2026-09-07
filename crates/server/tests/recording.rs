//! Audio published to this server, over a real transport, ending up in a file.
//!
//! `crates/recording` proves what the recorder does with packets. This proves
//! the wiring in between: that a meeting starting binds the recorder, that a
//! published microphone opens a tap, that arriving RTP reaches it, and that a
//! participant leaving closes the file with the sidecar that lets it be
//! uploaded afterwards. None of that is visible from either side alone.
//!
//! Nothing here uploads anything: the object store these servers are pointed at
//! is a port with nothing listening on it, and the record store is a directory,
//! which cannot hand out work. Both are deliberate. What is being tested is the
//! half that has to work while the object store is unreachable, and pointing it
//! at a real one would make the test about the object store.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_client::{Client, ClientOptions, TrackSpec};
use mediacore_config::Config;
use mediacore_recording::ogg;
use mediacore_room::{Adoption, RoomOptions};
use mediacore_server::http;
use mediacore_server::state::AppState;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "standup";
const PATIENCE: Duration = Duration::from_secs(30);

struct Server {
    addr: SocketAddr,
    state: AppState,
    recordings: PathBuf,
}

impl Server {
    /// A server that records, into a directory of its own.
    ///
    /// `enabled` is the *default for rooms*; a test that wants the per-room
    /// setting to be the thing under test passes false and asks the room.
    async fn start(enabled: bool) -> Self {
        let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
        config.port = 0;
        config.rtc.udp_port = 0;
        config.rtc.port_range_start = 0;
        config.rtc.port_range_end = 0;
        config.rtc.bind_address = "127.0.0.1".to_owned();
        config.sessions.directory = tempfile::TempDir::new().unwrap().keep();

        let recordings = tempfile::TempDir::new().unwrap().keep();
        config.recording.enabled = enabled;
        config.recording.directory = recordings.clone();
        config.recording.s3.bucket = "meetings".to_owned();
        // Nothing is listening there and nothing is meant to be. A test that
        // needed an object store to prove a file was written would be a test
        // that fails when the object store is the thing that is broken, which
        // is the case this half of the system exists to survive.
        config.recording.s3.endpoint = "http://127.0.0.1:1".to_owned();
        config.recording.s3.access_key_id = "key".to_owned();
        config.recording.s3.secret_access_key = "secret".to_owned();

        let state = AppState::start(config)
            .await
            .expect("media transport starts");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = http::router(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            addr,
            state,
            recordings,
        }
    }

    fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    fn meeting(&self) -> Option<String> {
        self.state
            .registry
            .sessions()
            .live(ROOM)
            .map(|session| session.id)
    }
}

fn token(identity: &str) -> String {
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
            sub: identity.to_owned(),
            name: identity.to_owned(),
            account_id: format!("AC_{identity}"),
            video: VideoGrant {
                room_join: Some(true),
                room: ROOM.to_owned(),
                ..VideoGrant::default()
            },
            ..Claims::default()
        },
    )
    .unwrap()
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Every `.ogg` under the recording directory, and the sidecar beside each --
/// `None` while the sidecar has not been written or is being replaced.
fn files(directory: &Path) -> Vec<(PathBuf, Option<serde_json::Value>)> {
    let mut found = Vec::new();

    let Ok(meetings) = std::fs::read_dir(directory) else {
        return found;
    };

    for meeting in meetings.flatten() {
        let Ok(entries) = std::fs::read_dir(meeting.path()) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|kind| kind != "ogg") {
                continue;
            }

            let sidecar = std::fs::read(path.with_extension("json")).unwrap_or_default();
            found.push((path, serde_json::from_slice(&sidecar).ok()));
        }
    }

    found
}

/// The whole path, end to end: somebody joins, publishes a microphone, is
/// recorded, and leaves behind a file that can be uploaded.
#[tokio::test]
async fn a_published_microphone_is_recorded_into_a_file_that_can_be_uploaded() {
    let server = Server::start(true).await;
    let alice = join(&server, "alice").await;
    let meeting = until("a meeting to start", || server.meeting()).await;

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");
    alice
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    // Long enough for a few pages to be written, so that the file has audio in
    // it and not only its headers.
    let opened = until("a recording to be opened", || {
        files(&server.recordings)
            .into_iter()
            .find_map(|(path, sidecar)| sidecar.map(|sidecar| (path, sidecar)))
    })
    .await;
    assert_eq!(
        opened.1["complete"],
        serde_json::json!(false),
        "a recording that is still being written must not claim to be finished, or a node coming \
         back from a crash would upload half a file"
    );
    assert_eq!(opened.1["room"], serde_json::json!(ROOM));
    assert_eq!(opened.1["identity"], serde_json::json!("alice"));
    assert_eq!(opened.1["recording"]["meeting"], serde_json::json!(meeting));
    assert_eq!(
        opened.1["recording"]["accountId"],
        serde_json::json!("AC_alice"),
        "the account the token named has to be on the recording, or nothing downstream can say \
         whose voice this is once the meeting record has been pruned"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    alice.leave().await.expect("alice leaves");

    let finished = until("the recording to be finished", || {
        files(&server.recordings)
            .into_iter()
            .find_map(|(path, sidecar)| sidecar.map(|sidecar| (path, sidecar)))
            .filter(|(_, sidecar)| sidecar["complete"] == serde_json::json!(true))
    })
    .await;

    let duration = finished.1["recording"]["durationMs"]
        .as_i64()
        .expect("a finished recording has a length");
    assert!(
        duration > 0,
        "the recording has no audio in it, so the packets arriving at the server never reached \
         the recorder"
    );

    let bytes = std::fs::read(&finished.0).expect("the audio");
    let survey = ogg::survey(&bytes[..]).expect("a walk over the file");
    assert_eq!(
        survey.good_bytes,
        bytes.len() as u64,
        "every page of a finished recording has to check out; a file with a torn tail is one that \
         was never closed"
    );
    assert!(survey.complete, "and it has to say the stream ended");
    assert_eq!(
        survey.granule,
        duration as u64 * 48,
        "the length written into the sidecar has to be the length in the file, or a consumer \
         aligning against one of them is aligned against neither"
    );

    // The upload never happened and was never going to: the object store is a
    // closed port and the record store is a directory, which cannot hand out
    // work. The recording is on the disk with everything needed to upload it
    // whenever there is somewhere to upload it to, which is the point.
    assert!(
        finished.1["recording"]["objectKey"].is_null(),
        "nothing should have been uploaded to a port with nothing behind it"
    );
    assert_eq!(
        server
            .state
            .recording
            .as_ref()
            .expect("this server records")
            .dropped(),
        0,
        "audio was dropped between the media path and the writer during a test with one \
         participant in it, which means the queue is far too small or the writer is stuck"
    );
}

/// A room with one person in it, on a server that records nothing by default.
///
/// This is the case the conference service next door loses. Its recorder had a
/// minimum of two people that could not be set lower, so the meeting room --
/// where everybody sits together and joins from one account, and the human
/// count is therefore one -- was never recorded and its meetings were simply
/// missing afterwards. Here there is no second threshold to get wrong:
/// recording follows the meeting, `minHumans` decides where a meeting begins,
/// and one is a number it takes.
#[tokio::test]
async fn a_room_that_asks_to_be_recorded_with_one_person_in_it_is_recorded() {
    let server = Server::start(false).await;

    let room = server.state.registry.create(
        ROOM,
        RoomOptions {
            metadata: r#"{"sessionPolicy":{"minHumans":1,"record":true}}"#.to_owned(),
            min_humans: Some(1),
            record: Some(true),
            ..RoomOptions::default()
        },
        Adoption::Always,
    );
    assert!(room.records());

    let alice = join(&server, "alice").await;
    until("a meeting to start", || server.meeting()).await;

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");
    alice
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    until("the room's meeting to be recorded", || {
        files(&server.recordings).into_iter().next()
    })
    .await;

    alice.leave().await.expect("alice leaves");
}

/// And a room that does not ask, on the same server, is not.
#[tokio::test]
async fn a_room_that_does_not_ask_is_not_recorded() {
    let server = Server::start(false).await;
    let alice = join(&server, "alice").await;
    until("a meeting to start", || server.meeting()).await;

    let track = alice
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("the track is published");
    alice
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");
    track.start().await.expect("media starts");

    // Long enough that a recorder which was going to record this would have
    // opened a file several times over.
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert!(
        files(&server.recordings).is_empty(),
        "a room on a server whose default is off, that never asked to be recorded, was recorded \
         anyway -- which is somebody's meeting in an object store they did not agree to"
    );

    alice.leave().await.expect("alice leaves");
}
