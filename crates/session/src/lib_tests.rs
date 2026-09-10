use super::*;

fn member(sid: &str, identity: &str, joined_at_ms: i64) -> Member {
    Member {
        sid: sid.to_owned(),
        identity: identity.to_owned(),
        name: identity.to_owned(),
        kind: Kind::Standard,
        attributes: HashMap::new(),
        joined_at_ms,
    }
}

fn carrying(sid: &str, identity: &str, attributes: &[(&str, &str)], joined_at_ms: i64) -> Member {
    Member {
        attributes: attributes
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        ..member(sid, identity, joined_at_ms)
    }
}

/// Most of what is below is about where the boundary of a meeting is and
/// not about what the platform above hangs off one, so it goes through a
/// room with no metadata. The tests that are about the metadata call
/// [`SessionStore::note`] itself.
trait Noting {
    fn noted(
        &self,
        room: &str,
        roster: &[Member],
        floor: u32,
        grace_ms: i64,
        now_ms: i64,
    ) -> Option<Session>;
}

impl Noting for SessionStore {
    fn noted(
        &self,
        room: &str,
        roster: &[Member],
        floor: u32,
        grace_ms: i64,
        now_ms: i64,
    ) -> Option<Session> {
        self.note(room, roster, floor, grace_ms, "", now_ms)
    }
}

fn recorder(sid: &str, identity: &str, joined_at_ms: i64) -> Member {
    Member {
        kind: Kind::Egress,
        ..member(sid, identity, joined_at_ms)
    }
}

fn directory() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("a directory for session records")
}

/// A store whose opaque limit is far larger than any of these tests uses,
/// so that a test about the limit is the only one the limit can fail.
fn store(directory: &tempfile::TempDir) -> SessionStore {
    SessionStore::files(directory.path(), 64 * 1024).expect("the session records open")
}

/// Everything a room has held, which is what `of_room` used to hand back
/// in one piece.
fn history(store: &SessionStore, room: &str) -> Vec<Session> {
    store
        .of_room(room, 0, MAX_PAGE)
        .expect("the store answers")
        .sessions
}

fn read(store: &SessionStore, id: &str) -> Option<Session> {
    store.get(id).expect("the store answers")
}

/// The reason these records are kept at all. A consumer that was down
/// when a meeting ended has to be able to come back and ask what it
/// missed, and it cannot ask a process that has been restarted since.
#[test]
fn a_meeting_outlives_the_process_that_recorded_it() {
    let directory = directory();

    let id = {
        let store = store(&directory);
        store.noted("demo", &[member("PA_1", "alice", 1_000)], 0, 90_000, 1_000);
        let id = store.live("demo").expect("a meeting started").id;

        store.noted("demo", &[], 0, 90_000, 5_000);
        assert_eq!(store.expire(95_000).len(), 1);
        id
    };

    let store = store(&directory);
    let session = read(&store, &id)
        .expect("a meeting held by the last process is not there to be asked about");

    assert_eq!(session.room, "demo");
    assert_eq!(session.started_at_ms, 1_000);
    assert_eq!(session.ended_at_ms, Some(95_000));
    assert_eq!(session.ended_by, Some(Ending::Grace));
    assert_eq!(session.participants.len(), 1);
    assert_eq!(session.participants[0].identity, "alice");
    assert_eq!(session.participants[0].left_at_ms, Some(5_000));
    assert_eq!(history(&store, "demo").len(), 1);
}

/// What a restart costs, stated as a test. Nobody saw this meeting end, so
/// the record is closed at the last thing it knows and says why -- rather
/// than being left open, which would have a meeting from last March
/// reported as the one happening now.
#[test]
fn a_meeting_the_process_died_during_is_closed_at_the_last_thing_it_knows() {
    let directory = directory();

    let id = {
        let store = store(&directory);
        store.noted("demo", &[member("PA_1", "alice", 1_000)], 0, 90_000, 1_000);
        store.noted(
            "demo",
            &[member("PA_1", "alice", 1_000), member("PA_2", "bob", 4_000)],
            0,
            90_000,
            4_000,
        );
        store.live("demo").expect("a meeting is running").id
    };

    let store = store(&directory);

    assert!(
        store.live("demo").is_none(),
        "a meeting held by a process that is gone was reported as the one happening now"
    );
    let session = read(&store, &id).expect("the record is kept");
    assert_eq!(
        session.ended_at_ms,
        Some(4_000),
        "the end time has to be the last moment the record knows about; anything later is a \
             meeting invented to have gone on after the server did"
    );
    assert_eq!(session.ended_by, Some(Ending::Restart));
    assert!(
        session
            .participants
            .iter()
            .all(|attendance| attendance.left_at_ms == Some(4_000)),
        "everybody is still in a meeting that ended, so nobody's attendance can be measured"
    );
}

#[test]
fn a_rooms_meetings_come_back_newest_first_after_a_reload() {
    let directory = directory();

    {
        let store = store(&directory);
        for start in [1_000, 10_000, 20_000] {
            let sid = format!("PA_{start}");
            store.noted("standup", &[member(&sid, "alice", start)], 0, 0, start);
            store.noted("standup", &[], 0, 0, start + 1_000);
            assert_eq!(store.expire(start + 1_000).len(), 1);
        }
    }

    let store = store(&directory);
    let sessions = history(&store, "standup");

    assert_eq!(
        sessions
            .iter()
            .map(|session| session.started_at_ms)
            .collect::<Vec<i64>>(),
        vec![20_000, 10_000, 1_000],
        "a room's history came back in whatever order the directory listed its files; the \
             question is almost always about the last meeting, so it belongs at the top"
    );
}

/// One unreadable file is one meeting nobody can look up. Refusing to
/// start over it takes every other meeting down with it, and deleting it
/// destroys whatever is still legible in there.
#[test]
fn a_file_that_is_not_a_record_costs_one_meeting_and_not_the_server() {
    let directory = directory();

    let id = {
        let store = store(&directory);
        store.noted("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
        let id = store.live("demo").expect("a meeting").id;
        store.close("demo", 2_000);
        id
    };

    std::fs::write(directory.path().join("SE_ruined.json"), b"{ not json").unwrap();
    std::fs::write(directory.path().join("notes.txt"), b"an operator's note").unwrap();
    std::fs::write(
        directory.path().join("SE_halfway.json.writing"),
        b"{ interrupted",
    )
    .unwrap();

    let store = SessionStore::files(directory.path(), 64 * 1024)
        .expect("one unreadable file took every other meeting on this server with it");

    assert_eq!(
        store.count(),
        1,
        "a file that is not a record was read back as a meeting"
    );
    assert!(read(&store, &id).is_some());
}

/// The store is told the whole roster on every change, so it is told the
/// same roster many times. Writing a record each time would turn a quiet
/// room into a stream of identical writes.
#[test]
fn a_roster_that_has_not_moved_changes_nothing() {
    let directory = directory();
    let store = store(&directory);
    let roster = [member("PA_1", "alice", 1_000)];

    store.noted("demo", &roster, 0, 90_000, 1_000);
    let first = store.live("demo").expect("a meeting");

    store.noted("demo", &roster, 0, 90_000, 2_000);
    store.noted("demo", &roster, 0, 90_000, 3_000);

    assert_eq!(
        store.live("demo").expect("the same meeting"),
        first,
        "nothing happened in the room and the record changed anyway"
    );
    assert_eq!(store.count(), 1);
}

/// One unfinished record per room is what a crash leaves behind, and the
/// next load closes it. Two is what a crash plus a close that never
/// reached the disk leaves behind -- and if the load only closed one of
/// them, the other would be reported as the meeting happening now, for
/// ever.
#[test]
fn two_unfinished_records_for_one_room_are_both_closed() {
    let directory = directory();

    for (id, started) in [("SE_first0000000", 1_000), ("SE_second000000", 9_000)] {
        let session = Session {
            id: id.to_owned(),
            room: "demo".to_owned(),
            started_at_ms: started,
            ended_at_ms: None,
            ended_by: None,
            participants: vec![Attendance::open(
                &member("PA_1", "alice", started),
                i64::MIN,
                id,
                64 * 1024,
            )],
            metadata: None,
        };
        files::write_record(directory.path(), &session).unwrap();
    }

    let store = store(&directory);

    assert!(
        store.live("demo").is_none(),
        "a meeting nobody has been at since the last restart is being reported as the one \
             happening now"
    );
    for id in ["SE_first0000000", "SE_second000000"] {
        let session = read(&store, id).expect("the record is kept");
        assert_eq!(
            session.ended_by,
            Some(Ending::Restart),
            "`{id}` is still open"
        );
    }

    store.noted("demo", &[member("PA_2", "bob", 20_000)], 0, 0, 20_000);
    assert!(
        store.live("demo").is_some(),
        "the room could not hold another meeting, because it is still pointing at one that \
             ended when the last process did"
    );
    assert_eq!(history(&store, "demo").len(), 3);
}

#[test]
fn two_rooms_hold_two_meetings() {
    let directory = directory();
    let store = store(&directory);

    store.noted("standup", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
    store.noted("payroll", &[member("PA_2", "bob", 1_000)], 0, 0, 1_000);

    let standup = store.live("standup").expect("a meeting in standup");
    let payroll = store.live("payroll").expect("a meeting in payroll");

    assert_ne!(standup.id, payroll.id);
    assert_eq!(history(&store, "standup").len(), 1);
    assert_eq!(history(&store, "payroll").len(), 1);
    assert_eq!(store.live_count(), 2);
    assert_eq!(store.unwritten(), 0);

    assert_eq!(
        store.expire(1_000).len(),
        0,
        "both rooms have somebody in them and neither meeting is counting down"
    );
}

/// Finding B. A recorder holds a room open across two meetings; its
/// arrival time is the same both times, so the second record's stretch
/// used to cover the whole of the first, and anybody adding up "time in
/// meetings" counted that hour twice.
#[test]
fn one_connection_is_never_in_two_meetings_at_once() {
    let directory = directory();
    let store = store(&directory);

    let scribe = recorder("PA_scribe", "scribe", 1_000);
    let alice = member("PA_alice", "alice", 2_000);
    let bob = member("PA_bob", "bob", 200_000);

    store.noted("demo", std::slice::from_ref(&scribe), 0, 90_000, 1_000);
    store.noted("demo", &[scribe.clone(), alice], 0, 90_000, 2_000);
    store.noted("demo", std::slice::from_ref(&scribe), 0, 90_000, 10_000);
    assert_eq!(store.expire(100_000).len(), 1, "the first meeting ended");

    store.noted("demo", &[scribe.clone(), bob], 0, 90_000, 200_000);
    assert!(store.live("demo").is_some(), "the second meeting started");

    let mut stretches: Vec<(i64, i64)> = history(&store, "demo")
        .iter()
        .flat_map(|session| session.participants.clone())
        .filter(|attendance| attendance.sid == scribe.sid)
        .map(|attendance| {
            (
                attendance.joined_at_ms,
                attendance.left_at_ms.unwrap_or(i64::MAX),
            )
        })
        .collect();
    stretches.sort();

    assert_eq!(stretches.len(), 2, "the recorder was at both meetings");
    assert!(
        stretches[0].1 <= stretches[1].0,
        "one connection is recorded as being at two meetings over the same minutes ({:?} and \
             {:?}); adding up somebody's attendance is the obvious thing to do with this list and \
             it now double counts",
        stretches[0],
        stretches[1]
    );
}

/// The same invariant inside one record: somebody who left and came back
/// during a meeting has two stretches, and the second cannot start before
/// the first ended.
#[test]
fn a_gap_in_one_meeting_is_two_stretches_that_do_not_overlap() {
    let directory = directory();
    let store = store(&directory);

    let alice = member("PA_alice", "alice", 1_000);
    let bob = member("PA_bob", "bob", 1_000);

    store.noted("demo", &[alice.clone(), bob.clone()], 2, 90_000, 1_000);
    store.noted("demo", std::slice::from_ref(&alice), 2, 90_000, 5_000);
    // The same connection comes back: the room reports the sid it always
    // had, with the arrival time it always had.
    store.noted("demo", &[alice, bob.clone()], 2, 90_000, 9_000);

    let session = store.live("demo").expect("one meeting with a gap in it");
    let bobs: Vec<&Attendance> = session
        .participants
        .iter()
        .filter(|attendance| attendance.sid == bob.sid)
        .collect();

    assert_eq!(
        bobs.len(),
        2,
        "bob left and came back and it shows as one stretch"
    );
    assert!(
        bobs[0].left_at_ms.expect("the first stretch is closed") <= bobs[1].joined_at_ms,
        "bob's second stretch starts inside his first, so his time at this meeting counts \
             the gap twice"
    );
}

/// Finding G. A server two years into a deployment must not hold every
/// meeting it ever held in memory to answer a question about one of them.
#[test]
fn a_history_on_disk_is_not_a_history_in_memory() {
    let directory = directory();
    let held = 200;

    {
        let store = store(&directory);
        for index in 0..held {
            let at = 1_000 + index as i64 * 10_000;
            let sid = format!("PA_{index}");
            store.noted("standup", &[member(&sid, "alice", at)], 0, 0, at);
            store.noted("standup", &[], 0, 0, at + 1_000);
            store.expire(at + 1_000);
        }
        assert!(
            store.flush(Duration::from_secs(30)),
            "the records did not reach the disk"
        );
    }

    let store = store(&directory);

    assert_eq!(store.count(), held, "the meetings were not all indexed");
    assert_eq!(
        store.resident(),
        0,
        "{} finished meetings are being held in memory; nothing is running, nothing is owed \
             to the store, and this is the number that grows for every meeting the server ever \
             holds",
        store.resident()
    );

    let newest = store
        .of_room("standup", 0, 1)
        .expect("the store answers")
        .sessions
        .pop()
        .expect("the newest meeting");
    assert!(
        read(&store, &newest.id).is_some(),
        "a meeting that is only in the store cannot be read back by id"
    );
    assert_eq!(
        store.resident(),
        0,
        "reading a meeting back put it in memory for good"
    );
}

/// A room a service has been meeting in for two years answers with a page,
/// and says how many pages there were.
#[test]
fn a_rooms_history_comes_back_a_page_at_a_time() {
    let directory = directory();
    let store = store(&directory);

    for index in 0..25i64 {
        let at = 1_000 + index * 10_000;
        let sid = format!("PA_{index}");
        store.noted("standup", &[member(&sid, "alice", at)], 0, 0, at);
        store.noted("standup", &[], 0, 0, at + 1_000);
        store.expire(at + 1_000);
    }
    assert!(store.flush(Duration::from_secs(30)));

    let first = store.of_room("standup", 0, 10).expect("the store answers");
    assert_eq!(
        first.sessions.len(),
        10,
        "a page longer than it was asked for"
    );
    assert_eq!(
        first.total, 25,
        "the caller cannot tell how much it has not seen"
    );
    assert_eq!(
        first.sessions[0].started_at_ms, 241_000,
        "the page does not start at the newest meeting"
    );

    let second = store.of_room("standup", 10, 10).expect("the store answers");
    assert_eq!(second.sessions.len(), 10);
    assert_eq!(second.sessions[0].started_at_ms, 141_000);

    let last = store.of_room("standup", 20, 10).expect("the store answers");
    assert_eq!(
        last.sessions.len(),
        5,
        "the last page is short and that is the end"
    );

    assert_eq!(
        store
            .of_room("standup", 0, MAX_PAGE + 500)
            .expect("the store answers")
            .sessions
            .len(),
        25,
        "a caller asking for more than the cap got more than the cap"
    );
}

/// The freshness the writer trades away has a bottom: a store that is
/// dropped has written everything it was owed, so a planned stop loses
/// nothing.
#[test]
fn a_planned_stop_leaves_nothing_owed_to_the_store() {
    let directory = directory();
    let store = store(&directory);

    store.noted("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
    let id = store.live("demo").expect("a meeting").id;
    store.close("demo", 2_000);

    assert!(
        store.flush(Duration::from_secs(30)),
        "a flush returned with records still owed"
    );
    assert_eq!(store.owed(), 0);

    let path = directory.path().join(format!("{id}.json"));
    let written: Session =
        serde_json::from_slice(&std::fs::read(&path).expect("the record is on the disk"))
            .expect("the record on disk is a record");
    assert_eq!(
        written.ended_at_ms,
        Some(2_000),
        "the meeting ended and the file still says it is running"
    );
}

/// A record that is owed to the store still answers, or a backoffice
/// polling for the end of a meeting would see it un-end for a moment.
#[test]
fn a_record_answers_before_it_reaches_the_store() {
    let directory = directory();
    let store = store(&directory);

    store.noted("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
    let session = store.close("demo", 2_000).expect("the meeting ended");

    assert_eq!(
        read(&store, &session.id).map(|record| record.ended_at_ms),
        Some(Some(2_000)),
        "a meeting that has ended reads as still running until its record lands"
    );
    assert_eq!(history(&store, "demo").len(), 1);
}

/// An id out of a URL is joined onto the records directory to find its
/// file. One that is not an id this server minted must not be.
#[test]
fn an_id_that_is_not_one_of_ours_never_becomes_a_path() {
    assert!(is_id("SE_0123456789ab"));
    assert!(!is_id("SE_"));
    assert!(!is_id("PA_0123456789ab"));
    for hostile in ["SE_../../etc/passwd", "SE_a/b", "SE_a.json", "../secret"] {
        assert!(
            !is_id(hostile),
            "`{hostile}` would be joined onto the records directory and opened"
        );
    }
}

/// The whole point of the change that took the account out of this crate:
/// a platform embedding mediacore has somewhere of its own to put what a
/// meeting means to it, and this server keeps it and never reads it.
#[test]
fn a_record_keeps_the_room_metadata_and_everybody_s_attributes_untouched() {
    let directory = directory();
    let store = store(&directory);

    store.note(
        "demo",
        &[
            carrying("PA_1", "alice", &[("accountId", "AC_alice")], 1_000),
            carrying("PA_2", "bob", &[("seat", "3"), ("tier", "gold")], 1_000),
            member("PA_3", "guest", 1_000),
        ],
        0,
        0,
        r#"{"sessionPolicy":{"minHumans":1},"tenant":"acme"}"#,
        1_000,
    );

    let session = store.live("demo").expect("a meeting");
    assert_eq!(
        session.metadata.as_deref(),
        Some(r#"{"sessionPolicy":{"minHumans":1},"tenant":"acme"}"#),
        "the room's metadata is the platform above's, and it has to come back byte for byte \
             -- including the part this server does read, because reading a key of it is not a \
             reason to rewrite it"
    );
    assert_eq!(
        session.participants[0].attributes.get("accountId"),
        Some(&"AC_alice".to_owned()),
        "an account is now one attribute among whatever else a platform sends, and this \
             server has no opinion about which of them it is"
    );
    assert_eq!(session.participants[1].attributes.len(), 2);
    assert!(
        session.participants[2].attributes.is_empty(),
        "a token that carried no attributes has to be recorded as carrying none rather than \
             being refused"
    );
}

/// The bound on a door into this server's storage from somebody else's
/// code. Nothing is truncated: half a JSON document is worse than none,
/// because nothing downstream can tell the half from the whole.
#[test]
fn an_oversized_blob_is_dropped_whole_and_the_one_just_under_the_limit_is_kept() {
    let directory = directory();
    let store = SessionStore::files(directory.path(), 64).expect("the session records open");

    let exactly = "m".repeat(64);
    store.note(
        "fits",
        &[carrying("PA_1", "alice", &[("k", "0123456789")], 1_000)],
        0,
        0,
        &exactly,
        1_000,
    );

    let session = store.live("fits").expect("a meeting");
    assert_eq!(
        session.metadata.as_deref(),
        Some(exactly.as_str()),
        "a value the size of the limit is inside it"
    );
    assert_eq!(session.participants[0].attributes.len(), 1);

    store.note(
        "over",
        &[carrying("PA_1", "alice", &[("k", &"v".repeat(64))], 1_000)],
        0,
        0,
        &"m".repeat(65),
        1_000,
    );

    let session = store.live("over").expect("a meeting");
    assert_eq!(
        session.metadata, None,
        "one byte over the limit and the record keeps nothing for that field, rather than \
             keeping a prefix nobody could tell from the whole"
    );
    assert!(
        session.participants[0].attributes.is_empty(),
        "the same rule for attributes, measured as the json the store will hold"
    );
}

/// A directory is not a work queue and does not pretend to be one. The
/// refusal names what it cannot do so that a deployment that wanted
/// recordings finds out at the first recording rather than at the first
/// missing one.
#[test]
fn a_directory_of_files_refuses_to_be_a_work_queue() {
    let directory = directory();
    let store = store(&directory);

    assert!(matches!(
        store.claim(Stage::Upload, 0, 10),
        Err(StoreError::Unsupported(_))
    ));
    assert!(matches!(
        store.recordings("SE_000000000000"),
        Err(StoreError::Unsupported(_))
    ));
}

/// A store that answers slowly, for a test that needs one to be stuck.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    entered: usize,
    open: bool,
}

impl Gate {
    fn hold(&self) {
        let mut state = self.state.lock().expect("gate");
        state.entered += 1;
        self.changed.notify_all();
        while !state.open {
            state = self.changed.wait(state).expect("gate");
        }
    }

    fn wait_until_held(&self) {
        let mut state = self.state.lock().expect("gate");
        while state.entered == 0 {
            state = self.changed.wait(state).expect("gate");
        }
    }

    fn open(&self) {
        let mut state = self.state.lock().expect("gate");
        state.open = true;
        self.changed.notify_all();
    }
}

struct Stuck(Arc<Gate>);

impl Sink for Stuck {
    fn describe(&self) -> String {
        "stuck".to_owned()
    }

    fn healthy(&self) -> bool {
        false
    }

    fn count(&self) -> usize {
        0
    }

    fn persist(&self, _session: &Session, _now_ms: i64) -> Result<(), StoreError> {
        self.0.hold();
        Ok(())
    }

    fn fetch(&self, _id: &str) -> Result<Option<Session>, StoreError> {
        Ok(None)
    }

    fn page(&self, _room: &str, _offset: usize, _limit: usize) -> Result<Listing, StoreError> {
        Ok(Listing {
            entries: Vec::new(),
            total: 0,
        })
    }
}

/// Finding H, stated so that it cannot come back. The record used to be
/// flushed to a disk on the thread that was letting somebody into a room,
/// under a lock every other room's arrivals also needed -- so a busy disk
/// delayed joining, everywhere at once. A database at the end of a network
/// is that failure again with a longer tail.
///
/// The store here is not slow, it is stopped: the first write is inside it
/// and will not come out until this test says so. Two hundred membership
/// changes then have to be as fast as memory, because there is nothing else
/// they could be waiting for.
#[test]
fn recording_a_membership_change_never_waits_for_the_store() {
    let gate = Arc::new(Gate::default());
    let store = SessionStore::around(
        Arc::new(Stuck(Arc::clone(&gate))),
        Records::default(),
        64 * 1024,
    );

    store.noted("demo", &[member("PA_anchor", "anchor", 0)], 0, 90_000, 0);
    gate.wait_until_held();

    let began = Instant::now();
    for index in 0..200i64 {
        let sid = format!("PA_{index}");
        store.noted(
            "demo",
            &[member("PA_anchor", "anchor", 0), member(&sid, "alice", 0)],
            0,
            90_000,
            index,
        );
    }
    let spent = began.elapsed();

    assert!(
        spent < Duration::from_secs(1),
        "200 membership changes took {spent:?} while the store was stopped dead, so somebody \
             joining a room is waiting on the store again -- and waiting under a lock every other \
             room needs"
    );
    assert!(
        store.owed() > 0,
        "the store is stopped and nothing is owed to it, so this test is not testing what it \
             says it is"
    );

    gate.open();
    assert!(store.flush(Duration::from_secs(30)));
}

struct Refuses;

impl Sink for Refuses {
    fn describe(&self) -> String {
        "refuses".to_owned()
    }

    fn healthy(&self) -> bool {
        false
    }

    fn count(&self) -> usize {
        0
    }

    fn persist(&self, _session: &Session, _now_ms: i64) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("on purpose".to_owned()))
    }

    fn fetch(&self, _id: &str) -> Result<Option<Session>, StoreError> {
        Err(StoreError::Unavailable("on purpose".to_owned()))
    }

    fn page(&self, _room: &str, _offset: usize, _limit: usize) -> Result<Listing, StoreError> {
        Err(StoreError::Unavailable("on purpose".to_owned()))
    }
}

/// A store that is away costs the records and never the meeting. This is
/// the whole answer to "what happens when Postgres is down mid-call":
/// people carry on talking, the record is held in memory and retried, and
/// the number of records in that state is the thing on `/healthz` worth an
/// alert.
#[test]
fn a_store_that_is_away_costs_records_and_not_calls() {
    let store = SessionStore::around(Arc::new(Refuses), Records::default(), 64 * 1024);

    let started = store
        .noted("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000)
        .expect("the meeting started even though the store is away");

    let waited = Instant::now();
    while store.unwritten() == 0 {
        assert!(
            waited.elapsed() < Duration::from_secs(5),
            "the writer never even tried to write the record"
        );
        std::thread::yield_now();
    }

    assert_eq!(
        read(&store, &started.id).map(|session| session.id),
        Some(started.id.clone()),
        "a meeting whose record could not be written stopped being answerable, so the API \
             says a meeting that is happening does not exist"
    );
    assert!(
        !store.flush(Duration::from_millis(50)),
        "a flush against a store that is refusing everything claimed to have written it all"
    );
    assert_eq!(
        store.live("demo").map(|session| session.id),
        Some(started.id)
    );
}

/// The strings are the record's public surface: a backoffice reads them out of
/// the JSON and a query filters the `kind` column on them, so every kind has
/// one and reading one back gives the kind it was written from.
#[test]
fn every_participant_kind_survives_the_round_trip_through_a_record() {
    for kind in [Kind::Standard, Kind::Egress, Kind::Sip, Kind::Agent] {
        assert_eq!(
            Kind::parse(kind.as_str()),
            Some(kind),
            "{} did not read back as itself",
            kind.as_str()
        );
    }

    assert!(
        Kind::Sip.is_human(),
        "a meeting held over the telephone would have no attendees at all"
    );
    assert!(!Kind::Egress.is_human());
    assert!(!Kind::Agent.is_human());
}
