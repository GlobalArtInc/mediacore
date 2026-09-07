//! The store, against a real Postgres.
//!
//! **These are the tests that matter and they are the ones that need
//! something installed.** Everything in `src/lib.rs` runs against memory or a
//! directory and proves the part of this crate that decides where a meeting
//! begins and ends. None of it touches a line of SQL, and the interesting half
//! of a work queue -- two processes reaching for the same row, and what becomes
//! of a row whose holder died -- is not a thing a mock can be wrong about in
//! the same way a database is. So the queue is tested here, against a server
//! that really does `FOR UPDATE SKIP LOCKED`, or it is not tested at all.
//!
//! **How they run.** Set `MEDIACORE_TEST_POSTGRES` to a connection url for a
//! server that will let this create and drop databases:
//!
//! ```text
//! MEDIACORE_TEST_POSTGRES=postgres://mediacore:mediacore@127.0.0.1:5432/postgres \
//!     cargo test -p mediacore-session
//! ```
//!
//! Each test makes a database of its own and drops it afterwards, so they do
//! not see each other's meetings and can run at the same time. Without the
//! variable every test here does nothing and says so -- and because a test that
//! quietly does nothing is how a queue ends up untested,
//! `the_database_tests_are_not_quietly_skipped_in_ci` fails the build when the
//! variable is missing on CI. The workflow starts a Postgres service for
//! exactly that reason.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_session::{
    Kind, Member, PostgresSettings, Recording, SessionStore, Stage, WorkState, recording_id,
};
use postgres::NoTls;

const URL: &str = "MEDIACORE_TEST_POSTGRES";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

/// A database of this test's own, dropped when the test ends.
struct Scratch {
    base: String,
    name: String,
}

impl Scratch {
    /// `None` when no database was configured, which is every developer who has
    /// not asked for one.
    fn open(what: &str) -> Option<Self> {
        let base = std::env::var(URL)
            .ok()
            .filter(|url| !url.trim().is_empty())?;
        let name = format!("mediacore_test_{what}_{}", scratch_suffix());

        let mut admin = postgres::Client::connect(&base, NoTls).unwrap_or_else(|error| {
            panic!("{URL} is set and the server is not answering: {error}")
        });
        admin
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .unwrap_or_else(|error| panic!("creating a database for this test: {error}"));

        Some(Self { base, name })
    }

    fn url(&self) -> String {
        swap_database(&self.base, &self.name)
    }

    fn settings(&self, node: &str) -> PostgresSettings {
        PostgresSettings {
            url: self.url(),
            node: node.to_owned(),
            connections: 2,
            timeout: Duration::from_secs(10),
            lease: Duration::from_secs(30),
            attempts: 5,
        }
    }

    fn store(&self, node: &str) -> SessionStore {
        SessionStore::postgres(self.settings(node)).expect("the store opens")
    }

    /// A node in a cluster whose lease is short enough to watch run out and
    /// whose patience is short enough to watch run out too.
    fn impatient(&self, node: &str, attempts: u32) -> SessionStore {
        SessionStore::postgres(PostgresSettings {
            lease: Duration::from_millis(200),
            attempts,
            ..self.settings(node)
        })
        .expect("the store opens")
    }

    fn client(&self) -> postgres::Client {
        postgres::Client::connect(&self.url(), NoTls).expect("a connection of this test's own")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let Ok(mut admin) = postgres::Client::connect(&self.base, NoTls) else {
            return;
        };
        // FORCE, because a connection this test forgot to close would otherwise
        // leave a database behind on somebody's server for ever.
        let _ = admin.batch_execute(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ));
    }
}

/// Unique per database this run makes. The process id alone is not enough --
/// these tests run at the same time in one process -- and a clock is not
/// either, because two of them can start in the same millisecond.
fn scratch_suffix() -> String {
    static MADE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    format!(
        "{}_{}",
        std::process::id(),
        MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// `postgres://user:pass@host:port/whatever?params` with the database swapped.
fn swap_database(base: &str, name: &str) -> String {
    let (scheme, rest) = base
        .split_once("://")
        .unwrap_or_else(|| panic!("{URL} has to be a postgres:// url, and this is `{base}`"));
    let (authority, tail) = rest.split_once('/').unwrap_or((rest, ""));

    match tail.split_once('?') {
        Some((_, query)) => format!("{scheme}://{authority}/{name}?{query}"),
        None => format!("{scheme}://{authority}/{name}"),
    }
}

/// Says once, loudly, that a test did nothing.
macro_rules! database {
    ($what:expr) => {
        match Scratch::open($what) {
            Some(scratch) => scratch,
            None => {
                eprintln!(
                    "skipping `{}`: set {URL} to run it against a real database",
                    $what
                );
                return;
            }
        }
    };
}

fn member(sid: &str, identity: &str, account: Option<&str>, joined_at_ms: i64) -> Member {
    Member {
        sid: sid.to_owned(),
        identity: identity.to_owned(),
        name: identity.to_owned(),
        kind: Kind::Standard,
        account_id: account.map(str::to_owned),
        joined_at_ms,
    }
}

fn recording(meeting: &str, participant: &str, segment: u32, complete: bool) -> Recording {
    let started = now_ms();

    Recording {
        id: recording_id(),
        meeting: meeting.to_owned(),
        participant: participant.to_owned(),
        account_id: Some("AC_alice".to_owned()),
        segment,
        local_path: PathBuf::from(format!(
            "/var/lib/mediacore/audio/{participant}-{segment}.ogg"
        )),
        // No node, so this recording's upload is offered to whoever asks --
        // which is what most of these tests are about. The test that is about
        // the node names one; see
        // `an_upload_is_only_offered_to_the_node_whose_disk_the_file_is_on`.
        node: None,
        object_key: None,
        bytes: complete.then_some(4_096),
        sample_rate: 48_000,
        channels: 1,
        started_at_ms: started,
        ended_at_ms: complete.then_some(started + 60_000),
        offset_ms: 0,
        duration_ms: complete.then_some(60_000),
    }
}

/// The upload has to go to the machine that has the file, and the announcement
/// has to go to anybody.
///
/// `local_path` names a file on one node's disk and means nothing anywhere
/// else, so an upload claimed by the wrong node is an attempt spent finding out
/// the file is not there -- and after `attempts` of those the recording is
/// `failed` with a file sitting perfectly intact on a disk nobody asked. The
/// announcement is the other way round: it needs no file, so a node that died
/// between uploading and announcing must not take the announcement with it.
#[test]
fn an_upload_goes_to_the_node_with_the_file_and_an_announcement_goes_to_anybody() {
    let scratch = database!("upload_node");
    let at = now_ms();

    let holder = scratch.store("ND_holder");
    holder.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = holder.live("demo").expect("a meeting").id;
    assert!(holder.flush(Duration::from_secs(30)));

    let mine = Recording {
        node: Some("ND_holder".to_owned()),
        ..recording(&meeting, "PA_1", 0, true)
    };
    let id = holder
        .note_recording(&mine, at)
        .expect("the recording is written down");

    let stranger = scratch.store("ND_stranger");
    assert!(
        stranger
            .claim(Stage::Upload, at, 10)
            .expect("the database answers")
            .is_empty(),
        "a node with none of this file on its disk was offered the upload of it; it would spend \
         an attempt discovering the file is not there, and after enough of those the recording is \
         given up on with the audio still sitting on the other node"
    );

    let taken = holder
        .claim(Stage::Upload, at, 10)
        .expect("the database answers");
    assert_eq!(
        taken.len(),
        1,
        "and the node that does have the file was not offered it either"
    );

    holder
        .uploaded(&id, "SE_x/PA_1-0.ogg", Some(4_096), now_ms())
        .expect("the upload is written down");

    let announcing = stranger
        .claim(Stage::Announce, now_ms(), 10)
        .expect("the database answers");
    assert_eq!(
        announcing.len(),
        1,
        "the announcement needs no file, so it has to be claimable by whichever node is up -- \
         otherwise a node that died between uploading and announcing takes the announcement with \
         it"
    );
}

/// A row that names no node is offered to everybody.
///
/// It is what a row written by hand looks like, and the honest end of one is a
/// node saying "the file is not here" into `last_error` rather than every node
/// staying silent and the recording never moving at all.
#[test]
fn a_recording_that_names_no_node_is_offered_to_whoever_asks() {
    let scratch = database!("upload_nowhere");
    let at = now_ms();

    let seeding = scratch.store("ND_seed");
    seeding.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = seeding.live("demo").expect("a meeting").id;
    assert!(seeding.flush(Duration::from_secs(30)));
    seeding
        .note_recording(&recording(&meeting, "PA_1", 0, true), at)
        .expect("the recording is written down");

    let anybody = scratch.store("ND_anybody");

    assert_eq!(
        anybody
            .claim(Stage::Upload, at, 10)
            .expect("the database answers")
            .len(),
        1
    );
}

/// A node that comes back with a new name takes its own files over.
///
/// The default node id is random per start, so a process that restarts is a
/// different node as far as the database is concerned. Writing the recording
/// down again -- which is what the startup scan does with everything it finds
/// on the disk -- has to move the file to whoever is holding it now, or a
/// restart would strand every recording that had not been uploaded yet.
#[test]
fn a_node_that_comes_back_under_a_new_name_adopts_its_own_recordings() {
    let scratch = database!("upload_adopt");
    let at = now_ms();

    let before = scratch.store("ND_before");
    before.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = before.live("demo").expect("a meeting").id;
    assert!(before.flush(Duration::from_secs(30)));

    let made = Recording {
        node: Some("ND_before".to_owned()),
        ..recording(&meeting, "PA_1", 0, true)
    };
    before
        .note_recording(&made, at)
        .expect("the recording is written down");

    // The process is killed and started again. Same disk, same files, new name.
    let after = scratch.store("ND_after");
    assert!(
        after
            .claim(Stage::Upload, at, 10)
            .expect("the database answers")
            .is_empty(),
        "before it has said anything, the upload still belongs to the name that is gone"
    );

    let readopted = after
        .note_recording(
            &Recording {
                node: Some("ND_after".to_owned()),
                ..made
            },
            now_ms(),
        )
        .expect("the recording is written down again");

    let taken = after
        .claim(Stage::Upload, now_ms(), 10)
        .expect("the database answers");
    assert_eq!(
        taken.len(),
        1,
        "a node that finds these files on its own disk and says so has to be able to upload them; \
         otherwise every restart strands whatever had not gone up yet"
    );
    assert_eq!(taken[0].recording.id, readopted);
    assert_eq!(taken[0].recording.node.as_deref(), Some("ND_after"));
}

/// A test that quietly does nothing is how a queue ends up with no test at all.
/// On a developer's machine that is a choice; on CI it is a hole, so it is a
/// failure there.
#[test]
fn the_database_tests_are_not_quietly_skipped_in_ci() {
    if std::env::var("CI").is_err() {
        return;
    }

    assert!(
        std::env::var(URL).is_ok_and(|url| !url.trim().is_empty()),
        "{URL} is not set on CI, so every test in this file did nothing and the build went green \
         anyway -- which is exactly the state this assertion exists to prevent. The workflow has \
         to start a Postgres service and point this at it."
    );
}

/// The whole of "how does the schema get there": start against an empty
/// database and it is there afterwards, and a second server against the same
/// database finds nothing to do.
#[test]
fn the_schema_arrives_on_an_empty_database() {
    let scratch = database!("schema");

    let first = scratch.store("ND_first");
    first.tick(now_ms());

    let mut client = scratch.client();
    let applied: Vec<i32> = client
        .query("SELECT version FROM mediacore_schema ORDER BY version", &[])
        .expect("the schema table is there")
        .iter()
        .map(|row| row.get(0))
        .collect();
    // Every migration in the binary, in order and each of them once. Written
    // as a shape rather than as a list of numbers so that adding a migration
    // does not mean editing this test -- which is how a test that watches the
    // schema stops watching it.
    assert_eq!(
        applied.first(),
        Some(&1),
        "an empty database did not get the schema, so this server would run all week and record \
         nothing"
    );
    assert!(
        applied.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "the applied versions are {applied:?}: one was skipped, or one was applied twice"
    );

    for table in [
        "mediacore_meeting",
        "mediacore_attendance",
        "mediacore_recording",
        "mediacore_recording_work",
    ] {
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .expect("the catalogue answers")
            .get(0);
        assert!(exists, "`{table}` was not created");
    }

    // A column from a later migration, so that "the schema arrived" means the
    // whole of it and not only the first step.
    let node_column: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_name = 'mediacore_recording' AND column_name = 'node'",
            &[],
        )
        .expect("the catalogue answers")
        .get(0);
    assert_eq!(
        node_column, 1,
        "a database that stopped at the first migration would offer every upload to every node, \
         and all but one of them would find no file"
    );

    // The second node is the ordinary case: three of them start at once and
    // two of them have nothing to do.
    let second = scratch.store("ND_second");
    second.tick(now_ms());

    let versions: i64 = client
        .query_one("SELECT count(*) FROM mediacore_schema", &[])
        .expect("the schema table is there")
        .get(0);
    assert_eq!(
        versions as usize,
        applied.len(),
        "a second server applied the migrations again, so three nodes starting together would \
         race each other through the schema"
    );
}

/// A meeting written by one node is a meeting another node can read. This is
/// the whole reason the store moved: with a directory each, neither of these
/// would ever see the other's.
#[test]
fn a_meeting_one_node_recorded_is_read_by_another() {
    let scratch = database!("shared");
    let at = now_ms();

    let writer = scratch.store("ND_writer");
    writer.note(
        "standup",
        &[
            member("PA_1", "alice", Some("AC_alice"), at),
            member("PA_2", "bob", None, at),
        ],
        0,
        0,
        at,
    );
    let id = writer.live("standup").expect("a meeting").id;
    writer.close("standup", at + 60_000);
    assert!(
        writer.flush(Duration::from_secs(30)),
        "the meeting never reached the database"
    );

    let reader = scratch.store("ND_reader");
    let session = reader
        .get(&id)
        .expect("the database answers")
        .expect("another node's meeting is not there to be read");

    assert_eq!(session.room, "standup");
    assert_eq!(session.ended_at_ms, Some(at + 60_000));
    assert_eq!(session.participants.len(), 2);
    assert_eq!(
        session.account_ids(),
        vec!["AC_alice"],
        "the account came off the token, went into the record, and did not come back out of the \
         database"
    );
    assert_eq!(
        session.participants[1].account_id, None,
        "a participant whose token carried no account has to read back as having none"
    );

    let page = reader
        .of_room("standup", 0, 10)
        .expect("the database answers");
    assert_eq!(page.total, 1);
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].id, id);
}

/// Somebody who drops out and comes back is two stretches, and the second
/// cannot start inside the first -- through the database as well as in memory,
/// because the attendance list is stored as rows and their order is what makes
/// the two stretches tellable apart.
#[test]
fn a_participant_who_joins_twice_is_two_stretches_in_the_database() {
    let scratch = database!("twice");
    let at = now_ms();

    let writer = scratch.store("ND_writer");
    let alice = member("PA_alice", "alice", Some("AC_alice"), at);
    let bob = member("PA_bob", "bob", Some("AC_bob"), at);

    writer.note("demo", &[alice.clone(), bob.clone()], 2, 90_000, at);
    writer.note("demo", std::slice::from_ref(&alice), 2, 90_000, at + 5_000);
    writer.note("demo", &[alice, bob], 2, 90_000, at + 9_000);

    let id = writer.live("demo").expect("a meeting").id;
    writer.close("demo", at + 20_000);
    assert!(writer.flush(Duration::from_secs(30)));

    let reader = scratch.store("ND_reader");
    let session = reader
        .get(&id)
        .expect("the database answers")
        .expect("the meeting is there");

    let bobs: Vec<_> = session
        .participants
        .iter()
        .filter(|attendance| attendance.sid == "PA_bob")
        .collect();

    assert_eq!(
        bobs.len(),
        2,
        "bob left and came back and the database has him down as never having left"
    );
    assert_eq!(bobs[0].left_at_ms, Some(at + 5_000));
    assert!(
        bobs[0].left_at_ms.expect("the first stretch is closed") <= bobs[1].joined_at_ms,
        "bob's second stretch starts inside his first, so his time at this meeting counts the gap \
         twice"
    );
    assert_eq!(
        session.account_ids(),
        vec!["AC_alice", "AC_bob"],
        "one account per person however many stretches they have"
    );
}

/// A node that stops does not take its meeting with it. The one that is still
/// up notices the lease has run out and closes the record at the last moment it
/// knows about -- which is the multi-process version of what a single-process
/// deployment does on its next start.
#[test]
fn a_meeting_spanning_a_restart_is_closed_by_the_node_that_is_still_up() {
    let scratch = database!("restart");
    let at = now_ms();

    let departed = {
        let dying = scratch.impatient("ND_dying", 5);

        dying.note(
            "demo",
            &[
                member("PA_1", "alice", Some("AC_alice"), at),
                member("PA_2", "bob", None, at),
            ],
            0,
            90_000,
            at,
        );
        // Somebody leaves, so the record knows about a moment later than the
        // meeting's start and the close has something to land on.
        dying.note(
            "demo",
            &[member("PA_1", "alice", Some("AC_alice"), at)],
            0,
            90_000,
            at + 30_000,
        );

        let id = dying.live("demo").expect("a meeting").id;
        assert!(dying.flush(Duration::from_secs(30)));
        id
        // and the node is gone, with the meeting still open
    };

    // Read straight out of the database, and before the survivor exists.
    //
    // Both halves matter. Through a store it would be a race: this deployment's
    // lease is two hundred milliseconds, a store's writer thread sweeps as soon
    // as it starts, and opening one takes a connection, an advisory lock and a
    // look at the schema -- so "the meeting is still open" would be a question
    // about how long that took on the day. What is being asked here is not
    // about the survivor at all: it is that a node going away does not by
    // itself close anything, which is true the instant `dying` is dropped.
    let mut client = scratch.client();
    let open: Option<i64> = client
        .query_one(
            "SELECT ended_at_ms FROM mediacore_meeting WHERE id = $1",
            &[&departed],
        )
        .expect("the meeting is in the database")
        .get(0);
    assert_eq!(
        open, None,
        "the meeting was closed by its node going away, rather than by another node deciding the \
         lease had run out. Nothing releases anything here -- the claim on a meeting is a time and \
         not a flag -- so a record closed at this point was closed by something that should not \
         have an opinion about it"
    );

    let survivor = scratch.impatient("ND_survivor", 5);
    survivor.tick(now_ms() + 10_000);

    let session = survivor
        .get(&departed)
        .expect("the database answers")
        .expect("the meeting is there");

    assert_eq!(
        session.ended_by,
        Some(mediacore_session::Ending::Restart),
        "a meeting whose node stopped is still being reported as the one happening now, for ever"
    );
    assert_eq!(
        session.ended_at_ms,
        Some(at + 30_000),
        "the end time has to be the last moment the record knows about; anything later is a \
         meeting invented to have gone on after the node did"
    );
    assert!(
        session
            .participants
            .iter()
            .all(|attendance| attendance.left_at_ms == Some(at + 30_000)),
        "everybody is still in a meeting that ended, so nobody's attendance can be measured"
    );
}

/// Two processes polling one queue. Neither waits for the other and neither
/// gets the other's work.
#[test]
fn two_claimants_never_take_the_same_work() {
    let scratch = database!("claimants");
    let at = now_ms();

    let seeding = scratch.store("ND_seed");
    seeding.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = seeding.live("demo").expect("a meeting").id;
    assert!(seeding.flush(Duration::from_secs(30)));

    let wanted: HashSet<String> = (0..20)
        .map(|segment| {
            seeding
                .note_recording(&recording(&meeting, "PA_1", segment, true), at)
                .expect("the recording is written down")
        })
        .collect();

    let first = Arc::new(scratch.store("ND_first"));
    let second = Arc::new(scratch.store("ND_second"));
    let gate = Arc::new(Barrier::new(2));

    let mut taken = Vec::new();
    std::thread::scope(|scope| {
        let mut hands = Vec::new();
        for store in [Arc::clone(&first), Arc::clone(&second)] {
            let gate = Arc::clone(&gate);
            hands.push(scope.spawn(move || {
                gate.wait();
                let mut mine: Vec<String> = Vec::new();
                loop {
                    let claimed = store
                        .claim(Stage::Upload, now_ms(), 7)
                        .expect("the database answers");
                    if claimed.is_empty() {
                        return mine;
                    }
                    mine.extend(claimed.into_iter().map(|claim| claim.recording.id));
                }
            }));
        }
        for hand in hands {
            taken.push(hand.join().expect("a claimant finished"));
        }
    });

    let mine: HashSet<&String> = taken[0].iter().collect();
    let theirs: HashSet<&String> = taken[1].iter().collect();

    assert_eq!(
        taken[0].len(),
        mine.len(),
        "one claimant was handed the same recording twice"
    );
    assert!(
        mine.is_disjoint(&theirs),
        "two claimants were handed the same recording, so it would be uploaded twice and \
         announced twice: {:?}",
        mine.intersection(&theirs).collect::<Vec<_>>()
    );
    assert_eq!(
        taken[0].len() + taken[1].len(),
        wanted.len(),
        "between them the two claimants did not get every recording, so some file is in the queue \
         that nobody will ever pick up"
    );

    for id in &wanted {
        let work = first.work(id).expect("the database answers");
        let upload = work
            .iter()
            .find(|work| work.stage == Stage::Upload)
            .expect("every recording has an upload");
        assert_eq!(upload.state, WorkState::Claimed);
        assert_eq!(
            upload.attempts, 1,
            "an attempt was spent twice on one claim, so a recording is thrown away after half as \
             many tries as this deployment asked for"
        );
    }
}

/// A claimant that stops without saying anything. Nothing has to be released,
/// because the claim is a time and not a flag -- so it comes back on its own,
/// and the attempt it spent is not given back.
#[test]
fn a_claim_whose_holder_disappeared_comes_back_and_then_is_left_alone() {
    let scratch = database!("dead_claim");
    let at = now_ms();

    let seeding = scratch.impatient("ND_seed", 2);

    seeding.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = seeding.live("demo").expect("a meeting").id;
    assert!(seeding.flush(Duration::from_secs(30)));

    let id = seeding
        .note_recording(&recording(&meeting, "PA_1", 0, true), at)
        .expect("the recording is written down");

    let taken = seeding
        .claim(Stage::Upload, at, 10)
        .expect("the database answers");
    assert_eq!(taken.len(), 1, "the only piece of work was not offered");
    assert_eq!(taken[0].attempts, 1);
    assert_eq!(taken[0].recording.id, id);

    assert!(
        seeding
            .claim(Stage::Upload, at, 10)
            .expect("the database answers")
            .is_empty(),
        "work that somebody is already doing was handed out again while their claim was still \
         good"
    );

    // The holder is gone. Nothing releases anything; the lease simply runs out.
    // The node that picks it up has the same patience as the one that died --
    // two nodes in one cluster reading one deployment's settings.
    let next = scratch.impatient("ND_next", 2);
    let again = next
        .claim(Stage::Upload, at + 1_000, 10)
        .expect("the database answers");
    assert_eq!(
        again.len(),
        1,
        "the node holding this stopped and the work never came back, so the recording sits in the \
         queue for ever"
    );
    assert_eq!(
        again[0].attempts, 2,
        "the attempt spent by the claimant that died was given back, so a recording that kills \
         whatever tries it would go round the cluster for ever"
    );

    // Two attempts is what this deployment allows, and both are spent.
    assert!(
        next.claim(Stage::Upload, at + 2_000, 10)
            .expect("the database answers")
            .is_empty(),
        "a recording that has used up its attempts was handed out again"
    );

    let work = next.work(&id).expect("the database answers");
    let upload = work
        .iter()
        .find(|work| work.stage == Stage::Upload)
        .expect("the upload is on the record");
    assert_eq!(upload.state, WorkState::Failed);
    assert!(
        upload
            .last_error
            .as_deref()
            .is_some_and(|reason| reason.contains("stopped")),
        "a row that stopped moving has to say why; an operator reading `failed` with no reason \
         cannot tell a dead claimant from an object store saying no, and it is {:?}",
        upload.last_error
    );
}

/// The order the two stages have to happen in, and what each of them writes
/// down. A half-written file in an object store is worse than a late one,
/// because nothing downstream can tell it is half a file.
#[test]
fn nothing_is_uploaded_before_it_is_finished_and_nothing_is_announced_before_it_is_up() {
    let scratch = database!("stages");
    let at = now_ms();

    let store = scratch.store("ND_only");
    store.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = store.live("demo").expect("a meeting").id;
    assert!(store.flush(Duration::from_secs(30)));

    let open = recording(&meeting, "PA_1", 0, false);
    let id = store
        .note_recording(&open, at)
        .expect("the recording is written down");

    assert!(
        store
            .claim(Stage::Upload, at, 10)
            .expect("the database answers")
            .is_empty(),
        "a file that is still being written was offered for upload"
    );

    // The same segment again, now that the file is closed. A recorder that was
    // restarted writes it down under a new id and has to land on the same row.
    let closed = Recording {
        id: recording_id(),
        ..recording(&meeting, "PA_1", 0, true)
    };
    let same = store
        .note_recording(&closed, at)
        .expect("the recording is written down");
    assert_eq!(
        same, id,
        "the same segment written down twice became two rows, so the file would be uploaded twice"
    );
    assert_eq!(
        store
            .recordings(&meeting)
            .expect("the database answers")
            .len(),
        1
    );

    let taken = store
        .claim(Stage::Upload, at, 10)
        .expect("the database answers");
    assert_eq!(taken.len(), 1, "a finished file was not offered for upload");
    assert_eq!(taken[0].recording.local_path, closed.local_path);
    assert_eq!(taken[0].recording.sample_rate, 48_000);
    assert_eq!(taken[0].recording.channels, 1);
    assert_eq!(taken[0].recording.account_id.as_deref(), Some("AC_alice"));

    assert!(
        store
            .claim(Stage::Announce, at, 10)
            .expect("the database answers")
            .is_empty(),
        "a recording that is not in the object store yet was offered to be announced"
    );

    assert!(
        store
            .renew(&id, Stage::Upload, at + 100)
            .expect("the database answers"),
        "a claimant that is still working could not say so"
    );

    store
        .uploaded(&id, "meetings/demo/alice-0.ogg", Some(8_192), at + 1_000)
        .expect("the upload is written down");

    let announce = store
        .claim(Stage::Announce, at + 1_000, 10)
        .expect("the database answers");
    assert_eq!(
        announce.len(),
        1,
        "a file went into the object store and nothing was ever going to say so"
    );
    assert_eq!(
        announce[0].recording.object_key.as_deref(),
        Some("meetings/demo/alice-0.ogg")
    );
    assert_eq!(announce[0].recording.bytes, Some(8_192));

    store
        .announced(&id, at + 2_000)
        .expect("the announcement is written down");

    let work = store.work(&id).expect("the database answers");
    assert_eq!(
        work.len(),
        2,
        "a recording has an upload and an announcement"
    );
    for stage in &work {
        assert_eq!(
            stage.state,
            WorkState::Done,
            "`{}` is not done",
            stage.stage
        );
    }
    assert_eq!(
        work.iter()
            .find(|work| work.stage == Stage::Upload)
            .and_then(|work| work.done_at_ms),
        Some(at + 1_000),
        "when the file went up is not on the record"
    );
    assert_eq!(
        work.iter()
            .find(|work| work.stage == Stage::Announce)
            .and_then(|work| work.done_at_ms),
        Some(at + 2_000),
        "when it was announced is not on the record"
    );

    assert!(
        store
            .claim(Stage::Upload, at + 3_000, 10)
            .expect("the database answers")
            .is_empty()
    );
}

/// Work that failed for a reason says the reason, waits, and comes back.
#[test]
fn a_postponed_upload_says_why_and_comes_back_when_it_is_due() {
    let scratch = database!("postpone");
    let at = now_ms();

    let store = scratch.store("ND_only");
    store.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = store.live("demo").expect("a meeting").id;
    assert!(store.flush(Duration::from_secs(30)));

    let id = store
        .note_recording(&recording(&meeting, "PA_1", 0, true), at)
        .expect("the recording is written down");

    store
        .claim(Stage::Upload, at, 10)
        .expect("the database answers");
    assert!(
        store
            .postpone(&id, Stage::Upload, "the object store said 503", at + 5_000)
            .expect("the database answers"),
        "the node that holds a claim has to be able to give it back"
    );

    assert!(
        store
            .claim(Stage::Upload, at + 1_000, 10)
            .expect("the database answers")
            .is_empty(),
        "work that asked to be tried again in five seconds was handed out after one"
    );

    let again = store
        .claim(Stage::Upload, at + 5_000, 10)
        .expect("the database answers");
    assert_eq!(again.len(), 1, "work that was due was not handed out");
    assert_eq!(again[0].attempts, 2);

    let work = store.work(&id).expect("the database answers");
    assert_eq!(
        work.iter()
            .find(|work| work.stage == Stage::Upload)
            .and_then(|work| work.last_error.clone())
            .as_deref(),
        Some("the object store said 503"),
        "the reason the last attempt failed is the one thing an operator looking at a stuck queue \
         needs, and it is not there"
    );
}

/// A room's history, out of the database, newest first and a page at a time --
/// including the meeting happening right now, which the store is still carrying
/// and may not have written yet.
#[test]
fn a_rooms_history_comes_back_a_page_at_a_time_from_the_database() {
    let scratch = database!("paging");
    let at = now_ms();

    let store = scratch.store("ND_only");
    for index in 0..5i64 {
        let when = at + index * 10_000;
        let sid = format!("PA_{index}");
        store.note(
            "standup",
            &[member(&sid, "alice", Some("AC_alice"), when)],
            0,
            0,
            when,
        );
        store.close("standup", when + 1_000);
    }
    assert!(store.flush(Duration::from_secs(30)));

    let first = store
        .of_room("standup", 0, 2)
        .expect("the database answers");
    assert_eq!(
        first.total, 5,
        "the caller cannot tell what it has not seen"
    );
    assert_eq!(first.sessions.len(), 2);
    assert_eq!(
        first.sessions[0].started_at_ms,
        at + 40_000,
        "the page does not start at the newest meeting"
    );

    let last = store
        .of_room("standup", 4, 2)
        .expect("the database answers");
    assert_eq!(
        last.sessions.len(),
        1,
        "the last page is short, and that is the end"
    );
    assert_eq!(last.sessions[0].started_at_ms, at);

    // A meeting that is happening now belongs at the top of the first page even
    // though the writer may not have reached it.
    store.note(
        "standup",
        &[member("PA_now", "alice", Some("AC_alice"), at + 60_000)],
        0,
        0,
        at + 60_000,
    );
    let live = store.live("standup").expect("a meeting is running");
    let now = store
        .of_room("standup", 0, 2)
        .expect("the database answers");
    assert_eq!(
        now.sessions[0].id, live.id,
        "the meeting happening right now is not at the top of its own room's history"
    );
}

/// A node that is still here does not have its own meeting closed under it.
///
/// **The mechanism this pins is the renewal**, the
/// `UPDATE mediacore_meeting SET kept_alive_at_ms = ...` inside the sweep. It
/// is what the whole lease rests on, and it was untested: replacing the
/// statement with a no-op left every test in this crate green, because the only
/// test that watched a lease was about the node that had *stopped* -- and the
/// survivor there holds no meetings, so it took the renewal branch with an
/// empty list and never exercised it.
///
/// What the missing statement costs is not subtle: a node that is up, holding a
/// meeting and ticking three times per lease closes its own live meeting the
/// moment the lease elapses. With the default thirty seconds that is every
/// meeting longer than half a minute, closed with people still talking in it.
#[test]
fn a_node_that_is_still_here_does_not_close_its_own_meeting() {
    let scratch = database!("renewal");
    let at = now_ms();

    // A two-hundred-millisecond lease, so a lease can be watched running out
    // rather than waited for.
    let store = scratch.impatient("ND_live", 5);
    store.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        90_000,
        at,
    );
    let id = store.live("demo").expect("a meeting").id;
    assert!(store.flush(Duration::from_secs(30)));

    // Six leases of wall clock, ticked from this node's own clock the way the
    // writer thread does it. Nothing else touches the record, so the only thing
    // that can keep it alive is the renewal.
    let until = std::time::Instant::now() + Duration::from_millis(1_200);
    while std::time::Instant::now() < until {
        store.tick(now_ms());
        std::thread::sleep(Duration::from_millis(50));
    }

    // Asked of the database and not of the store. `get` answers out of memory
    // while this node is holding the meeting, so it would say the meeting is
    // running whatever the row says -- and the row is the thing the renewal
    // writes and the sweep reads.
    let mut client = scratch.client();
    let row = client
        .query_one(
            "SELECT ended_at_ms, ended_by, kept_alive_at_ms FROM mediacore_meeting WHERE id = $1",
            &[&id],
        )
        .expect("the meeting is in the database");
    let ended_at_ms: Option<i64> = row.get(0);
    let ended_by: Option<String> = row.get(1);
    let kept_alive_at_ms: i64 = row.get(2);

    assert_eq!(
        ended_at_ms, None,
        "a node that is up, holding this meeting and saying so three times per lease closed it \
         anyway, at {ended_at_ms:?} with `{ended_by:?}`. Every meeting on this deployment longer \
         than a lease ends itself with people still talking in it, and the record says the node \
         went away"
    );
    assert!(
        kept_alive_at_ms > at + 500,
        "the meeting was left alive only because nothing got round to reaping it: its \
         kept_alive_at_ms is still {kept_alive_at_ms}, from when the record was first written, \
         rather than from one of the last second's heartbeats"
    );
}

/// A claimant whose lease ran out cannot undo what the next one did.
///
/// The put-back used to be `WHERE recording = ... AND stage = ...` and nothing
/// else, which makes it a statement that edits other people's rows. The
/// sequence is ordinary: a node claims the announcement, its lease expires
/// while it is inside `ready()` -- which reads the record store, so a slow store
/// is exactly when this happens -- another node claims it, announces it and
/// marks it done, and then the first node finally comes back to say it failed.
/// A `done` announcement went back into the queue to be emitted a second time,
/// and because `attempts` kept climbing, a recording that really was announced
/// could end as `failed` with `done_at_ms` set: a row that reads as nonsense to
/// whoever has to fix it.
#[test]
fn a_claim_that_has_gone_cannot_be_given_back_by_the_node_that_lost_it() {
    let scratch = database!("stale_postpone");
    let at = now_ms();

    let slow = scratch.impatient("ND_slow", 5);
    slow.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = slow.live("demo").expect("a meeting").id;
    assert!(slow.flush(Duration::from_secs(30)));

    let id = slow
        .note_recording(&recording(&meeting, "PA_1", 0, true), at)
        .expect("the recording is written down");
    slow.uploaded(&id, "SE_x/PA_1-0.ogg", Some(4_096), now_ms())
        .expect("the upload is written down");

    let taken = slow
        .claim(Stage::Announce, now_ms(), 10)
        .expect("the database answers");
    assert_eq!(taken.len(), 1, "the announcement was not offered");

    // The lease runs out while `ready()` is still inside the store.
    std::thread::sleep(Duration::from_millis(250));

    let quick = scratch.impatient("ND_quick", 5);
    let again = quick
        .claim(Stage::Announce, now_ms(), 10)
        .expect("the database answers");
    assert_eq!(
        again.len(),
        1,
        "a claim whose lease ran out has to come back to the cluster"
    );
    quick
        .announced(&id, now_ms())
        .expect("the announcement is written down");

    // And now the node that lost the claim comes back, having failed.
    let given = slow
        .postpone(&id, Stage::Announce, "the bus said no", now_ms() + 60_000)
        .expect("the database answers");

    assert!(
        !given,
        "a node whose claim had already gone was allowed to give it back, which puts a finished \
         announcement into the queue to be emitted a second time"
    );

    let work = quick.work(&id).expect("the database answers");
    let announce = work
        .iter()
        .find(|work| work.stage == Stage::Announce)
        .expect("the announcement is on the record");

    assert_eq!(
        announce.state,
        WorkState::Done,
        "the announcement was finished and a stale claimant put it back into the queue; it will \
         be emitted again, and the row now says {:?} with done_at_ms {:?}",
        announce.state,
        announce.done_at_ms
    );
    assert!(
        announce.done_at_ms.is_some(),
        "and when it was announced has to stay on the record"
    );
    assert_eq!(
        announce.last_error, None,
        "a finished announcement must not end up carrying the reason a stale claimant failed"
    );

    // The same hole was in the upload stage, where a stale put-back clears a
    // live claimant's lease and pushes its due time out by the *stale* node's
    // backoff while it is still uploading.
    let held = quick
        .claim(Stage::Upload, now_ms(), 10)
        .expect("the database answers");
    assert!(
        held.is_empty(),
        "this upload is already done and was offered again"
    );
    assert!(
        !slow
            .postpone(&id, Stage::Upload, "and the same for the upload", now_ms())
            .expect("the database answers"),
        "a node that holds nothing was allowed to put an upload back"
    );
}

/// Work nobody can pick up is counted and said out loud, rather than sitting in
/// the queue for ever in silence.
///
/// An upload is offered only to the node whose disk holds the file -- nothing
/// else can do it -- so a node that never comes back leaves a row that is not
/// `failed`, is not claimed, and that every other node walks straight past. The
/// `failed` sweep only fires on rows that are `claimed`, so it never fires
/// either, and the announcement behind it stays `blocked` for ever. Nothing
/// logged it and nothing counted it; `unwritten_recordings` on `/healthz` is
/// this node's own in-memory backlog and says nothing about the shared queue.
///
/// The row is deliberately *not* handed round the cluster: no other node can do
/// the work, so redistributing it would only spend its attempts and end in
/// `failed` with the audio still on a disk nobody has. What changes is that it
/// is visible -- and the log line says how an operator frees it.
#[test]
fn an_upload_waiting_for_a_node_that_never_came_back_is_counted() {
    let scratch = database!("stranded");
    let at = now_ms();

    let seeding = scratch.store("ND_seed");
    seeding.note(
        "demo",
        &[member("PA_1", "alice", Some("AC_alice"), at)],
        0,
        0,
        at,
    );
    let meeting = seeding.live("demo").expect("a meeting").id;
    assert!(seeding.flush(Duration::from_secs(30)));

    // A finished recording on a node that is gone for good: an ephemeral volume,
    // a pod rescheduled onto another machine, or simply the random node id a
    // restart hands out. Written down an hour ago and never picked up since.
    let stranded = Recording {
        node: Some("ND_gone".to_owned()),
        ..recording(&meeting, "PA_1", 0, true)
    };
    seeding
        .note_recording(&stranded, at - 3_600_000)
        .expect("the recording is written down");

    let survivor = scratch.store("ND_survivor");
    assert!(
        survivor
            .claim(Stage::Upload, now_ms(), 10)
            .expect("the database answers")
            .is_empty(),
        "the file is on a disk this node does not have, so it must not be offered the upload"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        survivor.tick(now_ms());
        if survivor.stuck() > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an upload that has been waiting an hour for a node that is never coming back is not \
             counted anywhere. It is not `failed`, nobody has claimed it, every other node walks \
             past it, and the announcement behind it is blocked for ever -- and there is no \
             number an operator could have alerted on"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
