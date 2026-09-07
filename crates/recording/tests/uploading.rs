//! The three steps after a file is closed, driven against a real object store
//! -- one that answers on a socket, refuses, hangs, and comes back.
//!
//! The queue underneath is a stand-in rather than Postgres, because what is
//! under test here is the ladder on top of it: claim, fail, come back later,
//! finish, announce. `crates/session/tests/postgres.rs` is where the queue's own
//! promises -- two pollers never taking the same row, a claim whose holder died
//! -- are held against real SQL, which is the only place they mean anything.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::any;
use mediacore_recording::{
    Announce, Queue, Recorder, S3, S3Settings, Settings, TrackSpec, Worker, WorkerSettings,
};
use mediacore_session::{Claim, Recording, Stage, StoreError, Work, WorkState};

// ---------------------------------------------------------------- the queue

/// One piece of work, as the stand-in keeps it.
#[derive(Clone)]
struct Row {
    state: WorkState,
    attempts: u32,
    due_at_ms: i64,
    /// When the claim on it stops being believed, the way the database keeps
    /// it: a claim is a time and not a flag, so a row whose holder has gone
    /// quiet comes back on its own.
    claim_ends_at_ms: i64,
    last_error: Option<String>,
}

#[derive(Default)]
struct Held {
    recordings: HashMap<String, Recording>,
    work: HashMap<(String, &'static str), Row>,
    uploaded: Vec<(String, String)>,
    announced: Vec<String>,
    /// How many times each stage has said "I am still working on this". The
    /// renewal is otherwise invisible from outside, and a lease that is never
    /// renewed is a claim handed to somebody else mid-flight.
    renewals: HashMap<&'static str, usize>,
}

/// The record store, in memory, behaving the way `crates/session` behaves
/// against Postgres: both stages exist from the moment a recording does, the
/// upload waits for the file to be closed and the announcement waits for the
/// upload.
#[derive(Default)]
struct Fake {
    held: Mutex<Held>,
    attempts_allowed: u32,
    /// How long a claim is believed here.
    lease_ms: i64,
    /// Whether renewing a claim works. A store that is away or too slow to
    /// answer refuses, and then a lease runs out under a task that is still
    /// inside the work -- which is when the row is offered again, to whoever
    /// asks, including the node that is still doing it.
    renewal_works: bool,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(Held::default()),
            attempts_allowed: 5,
            lease_ms: 30_000,
            renewal_works: true,
        })
    }

    fn row(&self, recording: &str, stage: &str) -> Option<Row> {
        self.held
            .lock()
            .expect("the queue")
            .work
            .get(&(recording.to_owned(), stage))
            .cloned()
    }

    fn renewals(&self, stage: Stage) -> usize {
        self.held
            .lock()
            .expect("the queue")
            .renewals
            .get(stage.as_str())
            .copied()
            .unwrap_or_default()
    }
}

impl Queue for Fake {
    fn note(&self, recording: &Recording, now_ms: i64) -> Result<String, StoreError> {
        let mut held = self.held.lock().expect("the queue");
        held.recordings
            .insert(recording.id.clone(), recording.clone());

        let upload = if recording.is_complete() {
            WorkState::Waiting
        } else {
            WorkState::Blocked
        };
        held.work
            .entry((recording.id.clone(), "upload"))
            .or_insert(Row {
                state: upload,
                attempts: 0,
                due_at_ms: now_ms,
                claim_ends_at_ms: 0,
                last_error: None,
            });
        held.work
            .entry((recording.id.clone(), "announce"))
            .or_insert(Row {
                state: WorkState::Blocked,
                attempts: 0,
                due_at_ms: now_ms,
                claim_ends_at_ms: 0,
                last_error: None,
            });

        Ok(recording.id.clone())
    }

    fn claim(&self, stage: Stage, now_ms: i64, limit: usize) -> Result<Vec<Claim>, StoreError> {
        let mut held = self.held.lock().expect("the queue");
        let mut taken = Vec::new();

        // Due work, and work whose claim has run out -- which the database
        // offers to whoever asks next, and "whoever" includes the node that is
        // still inside it.
        let due: Vec<String> = held
            .work
            .iter()
            .filter(|((_, held_stage), row)| {
                *held_stage == stage.as_str()
                    && row.attempts < self.attempts_allowed
                    && ((row.state == WorkState::Waiting && row.due_at_ms <= now_ms)
                        || (row.state == WorkState::Claimed && row.claim_ends_at_ms <= now_ms))
            })
            .map(|((recording, _), _)| recording.clone())
            .take(limit)
            .collect();

        let ends_at_ms = now_ms + self.lease_ms;
        for recording in due {
            let attempts = {
                let row = held
                    .work
                    .get_mut(&(recording.clone(), stage.as_str()))
                    .expect("the row that was just found");
                row.state = WorkState::Claimed;
                row.claim_ends_at_ms = ends_at_ms;
                row.attempts += 1;
                row.attempts
            };

            if let Some(found) = held.recordings.get(&recording) {
                taken.push(Claim {
                    recording: found.clone(),
                    stage,
                    attempts,
                    ends_at_ms,
                });
            }
        }

        Ok(taken)
    }

    fn work(&self, recording: &str) -> Result<Vec<Work>, StoreError> {
        let held = self.held.lock().expect("the queue");

        Ok(held
            .work
            .iter()
            .filter(|((held_recording, _), _)| held_recording == recording)
            .map(|((held_recording, stage), row)| Work {
                recording: held_recording.clone(),
                stage: if *stage == "upload" {
                    Stage::Upload
                } else {
                    Stage::Announce
                },
                state: row.state,
                attempts: row.attempts,
                due_at_ms: row.due_at_ms,
                claimed_by: None,
                claim_ends_at_ms: None,
                done_at_ms: None,
                last_error: row.last_error.clone(),
            })
            .collect())
    }

    /// Only a claim that is still held can be renewed, which is what the
    /// database does -- and counted, so a test can see that it happened.
    fn renew(&self, recording: &str, stage: Stage, now_ms: i64) -> Result<bool, StoreError> {
        let mut held = self.held.lock().expect("the queue");
        *held.renewals.entry(stage.as_str()).or_default() += 1;

        if !self.renewal_works {
            return Err(StoreError::Unavailable(
                "the store did not answer in time".to_owned(),
            ));
        }

        let ends_at_ms = now_ms + self.lease_ms;
        let Some(row) = held.work.get_mut(&(recording.to_owned(), stage.as_str())) else {
            return Ok(false);
        };
        if row.state != WorkState::Claimed {
            return Ok(false);
        }
        row.claim_ends_at_ms = ends_at_ms;

        Ok(true)
    }

    fn uploaded(
        &self,
        recording: &str,
        object_key: &str,
        bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let mut held = self.held.lock().expect("the queue");

        if let Some(found) = held.recordings.get_mut(recording) {
            found.object_key = Some(object_key.to_owned());
            found.bytes = bytes.or(found.bytes);
        }
        if let Some(row) = held.work.get_mut(&(recording.to_owned(), "upload")) {
            row.state = WorkState::Done;
        }
        if let Some(row) = held.work.get_mut(&(recording.to_owned(), "announce"))
            && row.state == WorkState::Blocked
        {
            row.state = WorkState::Waiting;
            row.due_at_ms = now_ms;
        }
        held.uploaded
            .push((recording.to_owned(), object_key.to_owned()));

        Ok(())
    }

    fn announced(&self, recording: &str, _now_ms: i64) -> Result<(), StoreError> {
        let mut held = self.held.lock().expect("the queue");
        if let Some(row) = held.work.get_mut(&(recording.to_owned(), "announce")) {
            row.state = WorkState::Done;
        }
        held.announced.push(recording.to_owned());

        Ok(())
    }

    /// Only a claim that is still held can be given back, which is what the
    /// database does: a put-back that ignored the claim would undo whatever the
    /// row's new holder had already done with it.
    fn postpone(
        &self,
        recording: &str,
        stage: Stage,
        error: &str,
        due_at_ms: i64,
    ) -> Result<bool, StoreError> {
        let mut held = self.held.lock().expect("the queue");
        let allowed = self.attempts_allowed;

        let Some(row) = held.work.get_mut(&(recording.to_owned(), stage.as_str())) else {
            return Ok(false);
        };
        if row.state != WorkState::Claimed {
            return Ok(false);
        }

        row.state = if row.attempts >= allowed {
            WorkState::Failed
        } else {
            WorkState::Waiting
        };
        row.due_at_ms = due_at_ms;
        row.last_error = Some(error.to_owned());

        Ok(true)
    }
}

/// Where announcements go when nobody is listening but a test.
#[derive(Default)]
struct Said {
    said: Mutex<Vec<(String, String)>>,
    /// How long announcing takes. The real one reads the meeting out of the
    /// record store to find the room's name and the participant's identity, and
    /// that read waits for a connection out of a small pool -- so it is exactly
    /// as slow as the store is.
    slow: Duration,
}

impl Announce for Said {
    fn ready(&self, recording: &Recording, bucket: &str) -> Result<(), String> {
        std::thread::sleep(self.slow);

        self.said.lock().expect("the log").push((
            recording.id.clone(),
            format!(
                "{bucket}/{}",
                recording.object_key.clone().unwrap_or_default()
            ),
        ));

        Ok(())
    }
}

// ---------------------------------------------------------- the object store

/// How an object store behaves in one of these tests.
#[derive(Clone)]
struct Behaviour {
    /// How many requests are refused before it starts answering.
    refuse: Arc<AtomicUsize>,
    /// Never answer at all: a bucket behind a network that is gone.
    hang: bool,
    put: Arc<Mutex<Vec<(String, usize)>>>,
}

async fn object(
    State(behaviour): State<Behaviour>,
    request: axum::extract::Request,
) -> (StatusCode, String) {
    if behaviour.hang {
        // An object store that accepts the connection and never answers, which
        // is what a bucket behind a partition looks like from here -- and is
        // the case a timeout is for.
        tokio::time::sleep(Duration::from_secs(3_600)).await;
    }

    let path = request.uri().path().to_owned();
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap_or_default();

    if behaviour.refuse.load(Ordering::SeqCst) > 0 {
        behaviour.refuse.fetch_sub(1, Ordering::SeqCst);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "<Error><Code>InternalError</Code></Error>".to_owned(),
        );
    }

    behaviour
        .put
        .lock()
        .expect("what was put")
        .push((path, body.len()));

    (StatusCode::OK, String::new())
}

async fn spawn_store(behaviour: Behaviour) -> SocketAddr {
    let router = axum::Router::new()
        .route("/{*key}", any(object))
        .with_state(behaviour);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a socket");
    let addr = listener.local_addr().expect("an address");

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    addr
}

// --------------------------------------------------------------- the fixture

fn payload() -> Arc<[u8]> {
    let mut bytes = vec![0u8; 80];
    bytes[0] = 0b0111_1000;

    Arc::from(bytes.into_boxed_slice())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as i64
}

fn store(addr: SocketAddr) -> Arc<S3> {
    Arc::new(
        S3::new(
            S3Settings {
                endpoint: format!("http://{addr}"),
                region: "us-east-1".to_owned(),
                bucket: "meetings".to_owned(),
                access_key_id: "key".to_owned(),
                secret_access_key: "secret".to_owned(),
                path_style: true,
                prefix: String::new(),
            },
            Duration::from_secs(5),
        )
        .expect("a client"),
    )
}

fn worker_settings() -> WorkerSettings {
    WorkerSettings {
        poll: Duration::from_millis(20),
        batch: 8,
        backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(50),
        renew: Duration::from_millis(100),
        keep_local: false,
    }
}

fn recorder(directory: &std::path::Path) -> Settings {
    Settings {
        node: "ND_one".to_owned(),
        directory: directory.to_path_buf(),
        max_segment_bytes: 64 * 1024 * 1024,
        reorder_hold: Duration::from_millis(5),
        queue: 8_192,
    }
}

fn spec() -> TrackSpec {
    TrackSpec {
        room: "standup".to_owned(),
        participant: "PA_000000000001".to_owned(),
        identity: "alice".to_owned(),
        account_id: Some("AC_alice".to_owned()),
        track: "TR_audio".to_owned(),
    }
}

/// Runs `check` every few milliseconds until it is true, or fails saying what
/// was being waited for.
async fn until(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("{what}");
}

// ----------------------------------------------------------------- the tests

/// The ordinary way through: a file is closed, written down, uploaded and
/// announced, and the local copy goes.
#[tokio::test]
async fn a_finished_recording_reaches_the_object_store_and_is_then_announced() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let behaviour = Behaviour {
        refuse: Arc::new(AtomicUsize::new(0)),
        hang: false,
        put: Arc::new(Mutex::new(Vec::new())),
    };
    let addr = spawn_store(behaviour.clone()).await;

    let (recorder_handle, finished) =
        Recorder::start(recorder(directory.path())).expect("a recorder");
    let queue = Fake::new();
    let said = Arc::new(Said::default());
    let _running = Worker::start(
        Arc::clone(&queue),
        store(addr),
        Arc::clone(&said),
        worker_settings(),
        Vec::new(),
        finished,
    );

    recorder_handle.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder_handle.track(spec());
    for index in 0..100u64 {
        tap.packet(index, index * 960, 48_000, payload(), Instant::now());
    }
    let local = {
        drop(tap);
        until("nothing was ever uploaded", || {
            !behaviour.put.lock().expect("what was put").is_empty()
        })
        .await;
        behaviour.put.lock().expect("what was put")[0].clone()
    };

    assert_eq!(
        local.0, "/meetings/SE_000000000001/PA_000000000001-0.ogg",
        "the object's key has to name the meeting and the participant, so that a row can be found \
         from an object and an object from a row"
    );
    assert!(local.1 > 0, "and it has to carry the audio");

    until("the recording was never announced", || {
        !said.said.lock().expect("the log").is_empty()
    })
    .await;

    let announced = said.said.lock().expect("the log")[0].clone();
    assert_eq!(
        announced.1, "meetings/SE_000000000001/PA_000000000001-0.ogg",
        "the announcement has to say where the object is, or a consumer has to guess"
    );

    until("the local copy was never removed", || {
        std::fs::read_dir(directory.path().join("SE_000000000001"))
            .map(|entries| entries.count() == 0)
            .unwrap_or(true)
    })
    .await;
}

/// An object store that refuses is tried again, and the row says why in the
/// meantime.
#[tokio::test]
async fn an_upload_that_fails_is_tried_again_and_says_why_while_it_waits() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let behaviour = Behaviour {
        refuse: Arc::new(AtomicUsize::new(2)),
        hang: false,
        put: Arc::new(Mutex::new(Vec::new())),
    };
    let addr = spawn_store(behaviour.clone()).await;

    let (recorder_handle, finished) =
        Recorder::start(recorder(directory.path())).expect("a recorder");
    let queue = Fake::new();
    let said = Arc::new(Said::default());
    let _running = Worker::start(
        Arc::clone(&queue),
        store(addr),
        Arc::clone(&said),
        worker_settings(),
        Vec::new(),
        finished,
    );

    recorder_handle.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder_handle.track(spec());
    for index in 0..50u64 {
        tap.packet(index, index * 960, 48_000, payload(), Instant::now());
    }
    drop(tap);

    let id = {
        until("the recording was never written down", || {
            !queue.held.lock().expect("the queue").recordings.is_empty()
        })
        .await;
        queue
            .held
            .lock()
            .expect("the queue")
            .recordings
            .keys()
            .next()
            .cloned()
            .expect("a recording")
    };

    until("the first refusal was never written onto the row", || {
        queue
            .row(&id, "upload")
            .is_some_and(|row| row.last_error.is_some())
    })
    .await;

    let waiting = queue.row(&id, "upload").expect("the upload row");
    assert_eq!(
        waiting.state,
        WorkState::Waiting,
        "an object store that said no is an outage and not a verdict; the work has to go back \
         into the queue"
    );
    assert!(
        waiting
            .last_error
            .as_deref()
            .is_some_and(|reason| reason.contains("500")),
        "and the row has to carry what it said, or an operator has a recording that is not moving \
         and no way to find out why: {:?}",
        waiting.last_error
    );

    until(
        "the upload never succeeded after the store came back",
        || {
            queue
                .row(&id, "upload")
                .is_some_and(|row| row.state == WorkState::Done)
        },
    )
    .await;

    let done = queue.row(&id, "upload").expect("the upload row");
    assert_eq!(
        done.attempts, 3,
        "two refusals and the one that worked; an attempt is spent on the claim, which is what \
         stops a recording that kills whatever tries it going round for ever"
    );

    until("the recording was never announced", || {
        !said.said.lock().expect("the log").is_empty()
    })
    .await;
}

/// The one that matters most: an object store that is not answering at all
/// costs uploads and costs nothing else.
///
/// The recorder goes on taking packets and closing files at full speed while
/// every upload is stuck in a request that will never be answered, because the
/// two are a thread and a task and share nothing that either can hold.
#[tokio::test]
async fn a_dead_object_store_never_reaches_the_path_a_packet_arrives_on() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let behaviour = Behaviour {
        refuse: Arc::new(AtomicUsize::new(0)),
        hang: true,
        put: Arc::new(Mutex::new(Vec::new())),
    };
    let addr = spawn_store(behaviour.clone()).await;

    let (recorder_handle, finished) = Recorder::start(Settings {
        // Deep enough to hold the whole burst below, which hands over forty
        // seconds of nine people's audio as fast as a loop can run -- far
        // faster than a meeting produces it. What the bound is for is a disk
        // that has stopped, and that is the test in `crates/recording/src/lib.rs`.
        queue: 65_536,
        ..recorder(directory.path())
    })
    .expect("a recorder");
    let queue = Fake::new();
    let _running = Worker::start(
        Arc::clone(&queue),
        store(addr),
        Arc::new(Said::default()),
        worker_settings(),
        Vec::new(),
        finished,
    );

    recorder_handle.meeting_started("standup", "SE_000000000001", now_ms());

    // One recording finishes, so that an upload is claimed and stuck before the
    // rest of the audio arrives.
    let first = recorder_handle.track(spec());
    for index in 0..50u64 {
        first.packet(index, index * 960, 48_000, payload(), Instant::now());
    }
    drop(first);

    until("no upload was ever claimed", || {
        queue
            .held
            .lock()
            .expect("the queue")
            .work
            .values()
            .any(|row| row.state == WorkState::Claimed)
    })
    .await;

    // Now nine more participants, all talking, while that upload hangs.
    let mut taps = Vec::new();
    for number in 1..10u32 {
        taps.push(recorder_handle.track(TrackSpec {
            participant: format!("PA_00000000000{number}"),
            ..spec()
        }));
    }

    let started = Instant::now();
    for index in 0..2_000u64 {
        for tap in &taps {
            tap.packet(index, index * 960, 48_000, payload(), Instant::now());
        }
    }
    let handing_over = started.elapsed();
    drop(taps);

    assert!(
        handing_over < Duration::from_secs(5),
        "handing eighteen thousand packets to the recorder took {handing_over:?} while the object \
         store was a black hole; nothing on the path a packet arrives on may wait for object \
         storage, and something now does"
    );

    // And every one of those recordings is a whole file on the disk, written
    // while not one byte could be uploaded.
    until("the nine recordings were never finished", || {
        queue.held.lock().expect("the queue").recordings.len() >= 10
    })
    .await;

    let recordings: Vec<Recording> = queue
        .held
        .lock()
        .expect("the queue")
        .recordings
        .values()
        .cloned()
        .collect();
    for recording in &recordings {
        assert!(
            recording.is_complete(),
            "a recording written during an object-store outage is a finished file waiting for the \
             store to come back, not a half-written one"
        );
    }
    assert_eq!(
        recorder_handle.dropped(),
        0,
        "nothing should have been dropped: the queue had room, and an object store that is not \
         answering has no way to reach it"
    );
    assert_eq!(
        recordings
            .iter()
            .filter(|recording| recording.duration_ms == Some(40_000))
            .count(),
        9,
        "each of the nine should hold all forty seconds of its audio, written while not a byte \
         could be uploaded"
    );
    assert!(
        behaviour.put.lock().expect("what was put").is_empty(),
        "and not one of them reached the object store, which is the point of the test"
    );
}

/// The announcement keeps its claim alive while it runs, exactly as the upload
/// does.
///
/// **It looks like a function call and it is not.** `Announce::ready` reads the
/// meeting out of the record store -- the event has to carry the room's name and
/// the participant's identity, and a recording row carries neither -- and that
/// read waits for a connection out of a pool of four. So the announcement is
/// slow precisely when the store is slow, which is precisely when a lease is
/// most likely to run out under it. A stage that does not renew has its row
/// re-offered while it is still working, and the event is emitted twice; and
/// because announcing reads the store, every duplicate makes the store slower
/// and the next duplicate more likely.
#[tokio::test]
async fn an_announcement_keeps_its_claim_alive_while_it_reads_the_store() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let behaviour = Behaviour {
        refuse: Arc::new(AtomicUsize::new(0)),
        hang: false,
        put: Arc::new(Mutex::new(Vec::new())),
    };
    let addr = spawn_store(behaviour).await;

    let (recorder_handle, finished) =
        Recorder::start(recorder(directory.path())).expect("a recorder");
    let queue = Fake::new();
    // Four times the worker's renewal interval, so a renewing announcement says
    // so several times and one that does not renew says nothing at all.
    let said = Arc::new(Said {
        said: Mutex::new(Vec::new()),
        slow: Duration::from_millis(400),
    });
    let _running = Worker::start(
        Arc::clone(&queue),
        store(addr),
        Arc::clone(&said),
        worker_settings(),
        Vec::new(),
        finished,
    );

    recorder_handle.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder_handle.track(spec());
    for index in 0..50u64 {
        tap.packet(index, index * 960, 48_000, payload(), Instant::now());
    }
    drop(tap);

    until("the recording was never announced", || {
        !said.said.lock().expect("the log").is_empty()
    })
    .await;

    assert!(
        queue.renewals(Stage::Announce) > 0,
        "the announcement ran for four renewal intervals and never once said it was still \
         working. Its claim expires under it, the row goes back into the queue, and whoever \
         claims it next emits the same recording_ready a second time -- and does it by reading \
         the same store that was already too slow"
    );
}

/// A recording that was already in the object store when this node found it on
/// its disk is removed from the disk.
///
/// **Nothing else would ever remove it.** `disk::forget` runs in the
/// successful-upload arm and nowhere else, so a process that died between
/// `uploaded` and that deletion leaves the `.ogg` and its sidecar behind with
/// both work rows `done` -- and a row that is done is never offered to anybody
/// again. Every later start finds the same two files, writes the same row down,
/// and leaves them exactly where they are. The volume fills a recording at a
/// time, nothing says why, and a node whose volume is full stops recording.
#[tokio::test]
async fn a_recording_already_in_the_object_store_is_taken_off_the_disk_when_it_is_found_again() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let behaviour = Behaviour {
        refuse: Arc::new(AtomicUsize::new(0)),
        hang: false,
        put: Arc::new(Mutex::new(Vec::new())),
    };
    let addr = spawn_store(behaviour.clone()).await;

    // A node records something and stops.
    {
        let (recorder_handle, _finished) =
            Recorder::start(recorder(directory.path())).expect("a recorder");
        recorder_handle.meeting_started("standup", "SE_000000000001", now_ms());
        let tap = recorder_handle.track(spec());
        for index in 0..50u64 {
            tap.packet(index, index * 960, 48_000, payload(), Instant::now());
        }
    }

    let found = mediacore_recording::disk::scan(directory.path(), "ND_two");
    assert_eq!(found.len(), 1, "the recording was not left on the disk");
    let left = found[0].clone();
    let sidecar = left.local_path.with_extension("json");
    assert!(left.local_path.exists() && sidecar.exists());

    // It went up and was announced, and the process died before the local copy
    // was removed.
    let queue = Fake::new();
    queue.note(&left, now_ms()).expect("the row");
    queue
        .uploaded(&left.id, "meetings/x.ogg", Some(4_096), now_ms())
        .expect("the upload is written down");
    queue
        .announced(&left.id, now_ms())
        .expect("the announcement");

    // The node comes back, scans its disk and hands over what it found. The
    // sender is kept alive because a worker whose recorder is gone stops.
    let (_recorder, arriving) = tokio::sync::mpsc::unbounded_channel();
    let _running = Worker::start(
        Arc::clone(&queue),
        store(addr),
        Arc::new(Said::default()),
        worker_settings(),
        found,
        arriving,
    );

    until("the local copy was never removed", || {
        !left.local_path.exists() && !sidecar.exists()
    })
    .await;

    assert!(
        behaviour.put.lock().expect("what was put").is_empty(),
        "a recording the store already had was uploaded again"
    );
}

/// A node never starts a second copy of work it is already doing.
///
/// **The queue cannot tell the two cases apart and this can.** A claim whose
/// lease has run out is offered to whoever asks next, and "whoever" includes the
/// node that is still inside the work — which is right for a node that crashed
/// and came back under the same name, because its uploads are on its own disk
/// and nobody else can do them, and wrong for a node that is merely slow. Here
/// the renewal cannot reach the store, so the lease runs out under an
/// announcement that is still running, and the poller is handed its own work
/// back every twenty milliseconds until the attempts are gone. Without the
/// guard that is the same `recording_ready` emitted once per re-offer — and
/// because announcing reads the store, every extra copy makes the store slower
/// and the next re-offer more likely.
#[tokio::test]
async fn a_node_never_starts_a_second_copy_of_work_it_is_already_doing() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let behaviour = Behaviour {
        refuse: Arc::new(AtomicUsize::new(0)),
        hang: false,
        put: Arc::new(Mutex::new(Vec::new())),
    };
    let addr = spawn_store(behaviour).await;

    let (recorder_handle, finished) =
        Recorder::start(recorder(directory.path())).expect("a recorder");
    // A fiftieth of the announcement's length, and a renewal that cannot reach
    // the store: the claim is gone long before the work is.
    let queue = Arc::new(Fake {
        held: Mutex::new(Held::default()),
        attempts_allowed: 5,
        lease_ms: 40,
        renewal_works: false,
    });
    let said = Arc::new(Said {
        said: Mutex::new(Vec::new()),
        slow: Duration::from_millis(500),
    });
    let _running = Worker::start(
        Arc::clone(&queue),
        store(addr),
        Arc::clone(&said),
        worker_settings(),
        Vec::new(),
        finished,
    );

    recorder_handle.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder_handle.track(spec());
    for index in 0..50u64 {
        tap.packet(index, index * 960, 48_000, payload(), Instant::now());
    }
    drop(tap);

    until("the recording was never announced", || {
        !said.said.lock().expect("the log").is_empty()
    })
    .await;

    // Long enough for any duplicate that started while the first was running to
    // finish and say so.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let announced = said.said.lock().expect("the log").len();
    assert_eq!(
        announced, 1,
        "one recording was announced {announced} times. Its claim expired under an announcement \
         that was still running, the queue offered the row back, and this node took its own work \
         a second time — so every consumer downstream sees the same recording_ready again, and \
         the copies pile up for as long as the store stays slow"
    );
}
