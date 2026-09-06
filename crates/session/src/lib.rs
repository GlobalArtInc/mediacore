//! What happened in a room, written down so it can be asked about afterwards.
//!
//! A room is a place; a session is a meeting that happened in it. The two are
//! not the same thing and the difference is the whole reason this crate
//! exists: a room somebody created over the API outlives its meetings and
//! holds many of them, one after another, while a room that sprang into being
//! because a person walked in is gone the moment they walk out -- and the
//! meeting they had in it still happened. So the record is kept beside the
//! registry rather than inside a room, and it survives the room it belongs to.
//!
//! Records live on disk, one JSON file per session, and are rewritten in place
//! as the meeting goes on. The alternative -- keeping them in memory and
//! emitting an event when a meeting ends -- makes the event the only copy of
//! the fact, so a consumer that was down at half past six can never learn what
//! it missed. That is the failure this is meant to make impossible, and a
//! record that dies with the process reintroduces it. See [`SessionStore`] for
//! what a restart does and does not cost.
//!
//! This crate also owns one decision that is easy to make twice: **where the
//! boundary of a meeting is**. A meeting begins when a room reaches the number
//! of people it needs and ends when it has been below that number for the
//! whole of its grace period, and both halves of that sentence are decided
//! here, in [`SessionStore::note`], against the roster the registry hands
//! over. Whoever else needs the deadline -- the room, so that it can end
//! itself under a session policy -- reads it back with
//! [`SessionStore::ends_at`] rather than keeping a second one. Two deadlines
//! armed by two conditions drift apart, and the shape of that drift is a
//! record closed under people who are still sitting in the room.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

/// The prefix every session id carries, in the shape of the other ids on this
/// server: `RM_` for a room, `PA_` for a participant, `SE_` for a session.
pub const SESSION_ID_PREFIX: &str = "SE_";

/// The suffix a record wears while it is being written.
///
/// A record is written to this name and then renamed onto its own, so a reader
/// -- the next start of this server included -- never sees half a file. The
/// loader skips anything that is not `<id>.json`, which is what keeps a
/// temporary file left behind by a crash from being read back as a meeting.
const PENDING_SUFFIX: &str = ".writing";

/// How many meetings one page of a room's history holds when the caller does
/// not say.
pub const DEFAULT_PAGE: usize = 100;

/// The most one page may hold however large a page the caller asks for.
///
/// A room a service has been making meetings in for two years has thousands,
/// and the answer to "give me all of them" cannot be a single response that
/// takes a second to build and a megabyte to send.
pub const MAX_PAGE: usize = 1_000;

/// What a participant was, which is what decides whether they count towards a
/// quorum.
///
/// The same three kinds `ParticipantInfo.kind` carries. They are spelled out
/// again here rather than borrowed from the room crate because the dependency
/// runs the other way -- the registry writes records, the records know nothing
/// about a registry -- and because these three strings are part of the JSON a
/// backoffice parses, so they should be nailed down where the JSON is defined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A person.
    #[default]
    Standard,
    /// A recorder.
    Egress,
    /// A bot.
    Agent,
}

impl Kind {
    /// Whether this one counts as somebody who came to the meeting.
    ///
    /// A recorder sitting in an empty room is not a meeting, and a room of
    /// bots is not one either: a session opens and closes on people.
    pub fn is_human(self) -> bool {
        matches!(self, Self::Standard)
    }
}

/// Why a session stopped.
///
/// Worth a field of its own because the third one is not like the other two: a
/// meeting the server was still holding when the process died has an end time
/// that nobody observed, and a reader comparing durations should be able to
/// tell that one from a meeting that ended because it was over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ending {
    /// The room stayed below its floor until the grace period ran out. The
    /// ordinary end of a meeting.
    Grace,
    /// The room was deleted, or the meeting was closed from the outside.
    Closed,
    /// The record was found unfinished when the server started, so the process
    /// that was keeping it is gone. The end time is the last thing the record
    /// itself knows about, not the moment the meeting really stopped.
    Restart,
}

/// One stretch of one connection being in one meeting.
///
/// Per stretch, not per person: somebody who drops out and comes back is two
/// of these, which is what makes "when did they arrive and leave" answerable
/// rather than an average. Two tabs from the same person are two of these as
/// well, and `identity` carries the suffix the room gave the second one.
///
/// One `sid`'s stretches never overlap each other, in one record or across
/// two. That is not decoration: adding up somebody's stretches is the obvious
/// thing to do with this list, and an arrival time that predates the end of
/// the previous meeting makes every such sum too large. See
/// [`Attendance::open`] for how the arrival time is held back to the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendance {
    /// The connection, as the logs and the roster spell it.
    pub sid: String,
    /// The identity the room used, suffix included -- `alice`, then `alice#2`.
    pub identity: String,
    /// What they were called on screen when they arrived, or empty when their
    /// token carried no name. A rename later in the meeting does not reach
    /// back into a record of what already happened.
    #[serde(default)]
    pub name: String,
    pub kind: Kind,
    /// When they arrived, never earlier than the end of this room's previous
    /// meeting. This can still be earlier than the session started: the first
    /// person into a room with a floor of two waits for the second, and the
    /// record says how long they waited.
    pub joined_at_ms: i64,
    /// When they left, or `null` while they are still here.
    #[serde(default)]
    pub left_at_ms: Option<i64>,
}

impl Attendance {
    /// Opens a stretch for somebody the room says is here.
    ///
    /// `not_before_ms` is the moment this stretch may not start before: the
    /// end of the previous meeting in this room, or the end of this same
    /// connection's previous stretch in this meeting. A recorder that sat
    /// through both halves of a room's evening has one arrival time and two
    /// records to appear in, and taking that one arrival time into the second
    /// record makes its stretch cover the whole of the first -- so anybody who
    /// adds up "time in meetings" counts that hour twice. The clamp is what
    /// keeps the intervals disjoint; what it costs is that the second record
    /// says the recorder arrived when the second meeting became possible
    /// rather than when it plugged itself in, which is the honest answer to
    /// "how long was it at *this* meeting".
    fn open(member: &Member, not_before_ms: i64) -> Self {
        Self {
            sid: member.sid.clone(),
            identity: member.identity.clone(),
            name: member.name.clone(),
            kind: member.kind,
            joined_at_ms: member.joined_at_ms.max(not_before_ms),
            left_at_ms: None,
        }
    }

    fn is_open(&self) -> bool {
        self.left_at_ms.is_none()
    }

    /// The last moment this stretch is known to have been true, for a record
    /// that has to be closed without anybody having watched it end.
    fn last_known_ms(&self) -> i64 {
        self.left_at_ms.unwrap_or(self.joined_at_ms)
    }
}

/// A meeting: when it began, when it ended, and who was in it.
///
/// Enough to answer "was there a meeting in this room, who was in it, how
/// long", and enough to hang a recording off later -- a recording belongs to
/// the meeting it recorded and not to the room, which will hold another
/// meeting tomorrow.
///
/// The room is named, not identified by sid. A meeting that survives a blink
/// spans two `Room` objects with two different sids, because a room nobody
/// created is thrown away when the last person leaves and built again when
/// they come back; the name is the only thing about the room that was true
/// throughout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub room: String,
    pub started_at_ms: i64,
    /// When the meeting ended, or `null` while it is still running.
    #[serde(default)]
    pub ended_at_ms: Option<i64>,
    /// Why it ended, or `null` while it is still running.
    #[serde(default)]
    pub ended_by: Option<Ending>,
    /// Every stretch of attendance, oldest first.
    #[serde(default)]
    pub participants: Vec<Attendance>,
}

impl Session {
    pub fn is_live(&self) -> bool {
        self.ended_at_ms.is_none()
    }

    fn open(room: &str, roster: &[Member], now_ms: i64, not_before_ms: i64) -> Self {
        Self {
            id: generate_id(),
            room: room.to_owned(),
            started_at_ms: now_ms,
            ended_at_ms: None,
            ended_by: None,
            participants: roster
                .iter()
                .map(|member| Attendance::open(member, not_before_ms))
                .collect(),
        }
    }

    /// Writes somebody into this meeting who is in the room and not in it yet.
    ///
    /// The floor is the later of the end of the room's previous meeting and
    /// the end of this connection's own previous stretch here, so that one
    /// `sid` never has two stretches that overlap.
    fn admit(&mut self, member: &Member, not_before_ms: i64) {
        let after_their_own = self
            .participants
            .iter()
            .filter(|attendance| attendance.sid == member.sid)
            .filter_map(|attendance| attendance.left_at_ms)
            .max()
            .unwrap_or(i64::MIN);

        self.participants
            .push(Attendance::open(member, not_before_ms.max(after_their_own)));
    }

    fn close(&mut self, at_ms: i64, ending: Ending) {
        for attendance in &mut self.participants {
            if attendance.is_open() {
                attendance.left_at_ms = Some(at_ms);
            }
        }

        self.ended_at_ms = Some(at_ms);
        self.ended_by = Some(ending);
    }

    /// The latest moment this record knows anything about.
    ///
    /// What an unfinished record is closed at when it is found on disk. It is
    /// not when the meeting stopped -- nobody saw that happen -- it is the
    /// last thing the server managed to write down, and closing at anything
    /// later would be inventing a meeting that went on after the server did.
    fn last_known_ms(&self) -> i64 {
        self.participants
            .iter()
            .map(Attendance::last_known_ms)
            .chain(std::iter::once(self.started_at_ms))
            .max()
            .unwrap_or(self.started_at_ms)
    }
}

/// Somebody in a room, as the registry sees them, on their way into a record.
#[derive(Debug, Clone)]
pub struct Member {
    pub sid: String,
    pub identity: String,
    pub name: String,
    pub kind: Kind,
    pub joined_at_ms: i64,
}

/// One page of a room's history, and how many meetings there were to page
/// through.
///
/// `total` is on the page because a caller that has just been handed a hundred
/// meetings has no way to tell whether that is the room's whole history or the
/// first hundredth of it, and the answer decides whether it asks again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub sessions: Vec<Session>,
    pub total: usize,
}

/// Every session this server has recorded, and the files they are kept in.
///
/// **What a restart costs.** Every meeting this server has held is on disk,
/// and the record of one that was still running when the process died is
/// closed at load with [`Ending::Restart`] and an end time taken from the last
/// thing the record itself knows -- which is a lower bound on when the meeting
/// really stopped and never a guess above it.
///
/// What a crash costs on top of that is a **freshness** window, and it is the
/// price of the write not being on the join path. Membership changes are
/// recorded in memory and handed to a writer thread; that thread has them on
/// disk a moment later, and a crash inside that moment loses the changes it
/// had not reached yet. So after a crash a record may be missing its last few
/// arrivals and departures, and a meeting that started in that moment may be
/// missing altogether. It never says anything that did not happen: a record on
/// disk is always a state the meeting really passed through, because each is
/// written whole, flushed, and renamed into place. A shutdown that runs
/// [`SessionStore::flush`] -- which dropping the store does -- loses nothing
/// at all, so the window belongs to a crash and not to a restart.
///
/// The alternative was the fsync staying where it was, on the thread carrying
/// somebody's media, under a lock every other room's arrivals also need. A
/// busy disk then delays people joining, and delays them everywhere at once.
/// Freshness measured in milliseconds is worth strictly less than that.
///
/// **What it costs while running.** A membership change takes one lock, edits
/// a record in memory and wakes a thread. No caller waits for a disk.
///
/// **What is kept in memory.** An index of every meeting -- its id, its room
/// and when it started -- plus the full record of each meeting that is
/// running, each one waiting to be written, and each one the disk refused.
/// Everything else is read back from its file when somebody asks for it. A
/// server two years into a deployment holds an index of a few hundred
/// thousand short strings rather than a few hundred thousand records, and
/// [`SessionStore::of_room`] hands back a page rather than a history.
///
/// **What it does not do.** Nothing here deletes a record. The directory grows
/// with one small file per meeting and an operator prunes it with `find -mtime
/// +90 -delete` if they want it pruned -- which is a real answer only because
/// the layout is one self-describing file per session and not a database
/// nobody may touch from outside.
pub struct SessionStore {
    inner: Arc<Inner>,
    writer: Option<JoinHandle<()>>,
}

/// Everything the writer thread and the callers share.
struct Inner {
    directory: PathBuf,
    records: Mutex<Records>,
    /// Woken when there is a record to write, or when the store is closing.
    work: Condvar,
    /// Woken when a pass over the queue has finished, which is what
    /// [`SessionStore::flush`] waits on.
    written: Condvar,
}

/// Where a record's file is and what the index knows about it without opening
/// it.
struct Indexed {
    /// Shared with every other record of the same room: a room with a thousand
    /// meetings holds its name once.
    room: Arc<str>,
    started_at_ms: i64,
    /// The file this record came out of, when that is not `<id>.json`.
    ///
    /// The two only differ because a record was copied about by hand. The id
    /// inside is the one everything else refers to it by, so the record is
    /// filed under that -- and the name it is actually in has to be kept, or
    /// the record could be listed and never read.
    file: Option<Box<str>>,
}

/// A record owed to the disk, and which edit of it is owed.
struct Queued {
    version: u64,
    session: Session,
}

#[derive(Default)]
struct Records {
    /// Every meeting this server can be asked about, by id.
    index: HashMap<String, Indexed>,
    /// Session ids per room, oldest first. A created room holds many meetings
    /// and this is the order they happened in.
    by_room: HashMap<String, Vec<String>>,
    /// The meeting currently running in a room, in full, by room name.
    live: HashMap<String, Session>,
    /// When the meeting running in a room ends if nobody comes back, by room
    /// name. Armed the moment the room drops below its floor and cleared by
    /// the arrival that restores it. This is the only such deadline on the
    /// server; a room reads it rather than keeping one of its own.
    ends_at: HashMap<String, i64>,
    /// When the last finished meeting in a room ended, by room name. The floor
    /// under every arrival time in the next one, so that one connection's
    /// stretches never overlap across two records.
    last_ended_ms: HashMap<String, i64>,
    /// Records owed to the disk, by id, newest edit only: a meeting that
    /// changes twice before the writer gets to it is written once.
    queued: HashMap<String, Queued>,
    /// Records the disk refused. A meeting is never failed because a disk was:
    /// the record stays here and serves reads until the process ends, and the
    /// size of this is how an operator finds out it is going to be lost.
    unwritten: HashMap<String, Session>,
    next_version: u64,
    closing: bool,
}

impl SessionStore {
    /// Opens the directory, indexing every meeting it holds.
    ///
    /// Creating the directory is part of opening it: a server told to record
    /// meetings somewhere that does not exist yet should make the place rather
    /// than refuse to start, and a server that cannot make it should refuse to
    /// start rather than run for a month and be asked for records it never
    /// kept.
    ///
    /// Only the head of each file is parsed -- the id, the room and the two
    /// times -- and only the records that were still running when the last
    /// process stopped are read in full. Reading every attendance list of
    /// every meeting ever held would put the whole history in memory for the
    /// life of the process to answer questions about one afternoon of it.
    pub fn open(directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;

        let mut records = Records::default();
        let mut interrupted = Vec::new();

        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if !is_record(&path) {
                continue;
            }

            match read_header(&path) {
                Ok(header) => {
                    // The id names the file every later read and write of this
                    // record goes to, so a record whose id is not one this
                    // server could have minted is left alone rather than
                    // followed out of the directory it was found in.
                    if !is_id(&header.id) {
                        tracing::error!(
                            path = %path.display(),
                            id = %header.id,
                            "a session record carries an id this server did not mint; it is left \
                             alone and that meeting cannot be looked up"
                        );
                        continue;
                    }

                    match header.ended_at_ms {
                        Some(ended) => records.remember_ending(&header.room, ended),
                        None => interrupted.push(header.id.clone()),
                    }
                    records.index_record(&header, &path);
                }
                // Kept, not deleted, and not fatal. One unreadable file is one
                // meeting nobody can look up; refusing to start over it takes
                // the server down with it, and deleting it destroys the only
                // copy of whatever is still legible in there.
                Err(error) => tracing::error!(
                    path = %path.display(),
                    %error,
                    "a session record could not be read and was left alone; that meeting cannot \
                     be looked up"
                ),
            }
        }

        records.sort();

        // Nothing that was running is running now: this process has no rooms
        // and no participants yet. Closing them here rather than leaving them
        // open is what stops a meeting from last March being reported as the
        // one happening right now. They are closed one by one by id rather
        // than through a per-room map, because two unfinished records for one
        // room is not supposed to be possible but becomes possible the moment
        // a close fails to reach the disk -- and a map keyed by room would
        // keep whichever of the two it saw last and leave the other open for
        // ever, reported as the meeting happening now.
        for id in &interrupted {
            let Some(path) = records
                .index
                .get(id)
                .map(|indexed| indexed.path(&directory, id))
            else {
                continue;
            };
            let mut session = match read_record(&path) {
                Ok(session) => session,
                Err(error) => {
                    tracing::error!(
                        path = %path.display(),
                        %error,
                        "a meeting that was still running could not be read back and so cannot be \
                         closed; it stays open on the disk"
                    );
                    continue;
                }
            };

            let at_ms = session.last_known_ms();
            session.close(at_ms, Ending::Restart);

            tracing::warn!(
                session = %session.id,
                room = %session.room,
                ended_at_ms = at_ms,
                "a meeting was still running when this server last stopped; its record is closed \
                 at the last moment it knows about"
            );

            records.remember_ending(&session.room, at_ms);
            records.queue(session);
        }

        tracing::info!(
            directory = %directory.display(),
            sessions = records.index.len(),
            "session records loaded"
        );

        let inner = Arc::new(Inner {
            directory,
            records: Mutex::new(records),
            work: Condvar::new(),
            written: Condvar::new(),
        });
        let writing = Arc::clone(&inner);
        let writer = std::thread::Builder::new()
            .name("session-records".to_owned())
            .spawn(move || write_queued(&writing))?;

        Ok(Self {
            inner,
            writer: Some(writer),
        })
    }

    /// Where the records are kept.
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    /// How many meetings this server can be asked about.
    pub fn count(&self) -> usize {
        self.inner.records.lock().expect("session lock").index.len()
    }

    /// How many meetings are running right now, across every room.
    pub fn live_count(&self) -> usize {
        self.inner.records.lock().expect("session lock").live.len()
    }

    /// How many records are held in memory in full, rather than only indexed.
    ///
    /// The bound this server is meant to keep: the meetings running, the ones
    /// waiting to be written and the ones the disk refused, and nothing else
    /// however long the history is.
    pub fn resident(&self) -> usize {
        let records = self.inner.records.lock().expect("session lock");
        let mut ids: HashSet<&str> = HashSet::new();

        ids.extend(records.queued.keys().map(String::as_str));
        ids.extend(records.unwritten.keys().map(String::as_str));
        ids.extend(records.live.values().map(|session| session.id.as_str()));

        ids.len()
    }

    /// How many records are waiting to be written.
    ///
    /// Zero almost always: it rises while a burst of arrivals outruns the
    /// disk and falls again. Staying up is a disk that cannot keep pace with
    /// the meetings being held on top of it.
    pub fn owed(&self) -> usize {
        self.inner
            .records
            .lock()
            .expect("session lock")
            .queued
            .len()
    }

    /// How many records this server has failed to write and still holds.
    ///
    /// A gauge rather than a tally: a record whose next write succeeds comes
    /// back off it, so a disk that hiccuped once is not reported for ever as a
    /// meeting that is going to be lost.
    pub fn unwritten(&self) -> u64 {
        self.inner
            .records
            .lock()
            .expect("session lock")
            .unwritten
            .len() as u64
    }

    /// Waits until everything recorded so far is on the disk.
    ///
    /// What makes a planned stop lose nothing. It is not a promise about a
    /// crash: a crash is exactly the stop that does not get to call this.
    pub fn flush(&self) {
        let mut records = self.inner.records.lock().expect("session lock");
        while !records.queued.is_empty() {
            records = self.inner.written.wait(records).expect("session lock");
        }
    }

    /// Takes in who is in a room, and works out what that means for the
    /// meeting.
    ///
    /// The whole roster every time, rather than an arrival or a departure,
    /// because the caller has the roster and a diff cannot be missed the way
    /// an event can: a path that forgot to report one departure would leave a
    /// person in the record for ever, and there are several such paths --
    /// leaving, being removed, being displaced by the same person's second
    /// tab. Called again with a roster that has not changed, this does nothing
    /// and writes nothing.
    ///
    /// `floor` is how many people the room needs, never less than one: a
    /// meeting with nobody at it is not a meeting, so a room whose operator
    /// asked for no policy at all still opens a session when the first person
    /// arrives and closes it when the last one goes. `grace_ms` is how long
    /// the meeting is held open after the room falls below that floor, and it
    /// is what makes somebody rejoining one meeting with a gap in it rather
    /// than two meetings.
    ///
    /// This is where the boundary of a meeting is decided, and the only place.
    /// The caller reads the deadline back out of [`SessionStore::ends_at`]
    /// rather than working it out again: a room that armed a countdown on its
    /// own condition would end itself on one clock while its record ended on
    /// another.
    ///
    /// Returns the meeting if this roster started one, so that the caller can
    /// announce it. A meeting only ever *starts* here -- it ends in
    /// [`SessionStore::expire`] or [`SessionStore::close`], both of which
    /// hand back what they ended for the same reason.
    pub fn note(
        &self,
        room: &str,
        roster: &[Member],
        floor: u32,
        grace_ms: i64,
        now_ms: i64,
    ) -> Option<Session> {
        let floor = floor.max(1) as usize;
        let humans = roster
            .iter()
            .filter(|member| member.kind.is_human())
            .count();

        let started = {
            let mut records = self.inner.records.lock().expect("session lock");

            if !records.live.contains_key(room) {
                if humans < floor {
                    return None;
                }

                let not_before = records.floor_of(room);
                let session = Session::open(room, roster, now_ms, not_before);
                tracing::info!(
                    session = %session.id,
                    %room,
                    humans,
                    floor,
                    "a meeting started"
                );
                records.start(session.clone());
                records.queue(session.clone());
                Some(session)
            } else {
                let not_before = records.floor_of(room);
                let session = records.live.get_mut(room).expect("checked a line above");
                let mut changed = false;

                for member in roster {
                    if !session
                        .participants
                        .iter()
                        .any(|attendance| attendance.is_open() && attendance.sid == member.sid)
                    {
                        session.admit(member, not_before);
                        changed = true;
                    }
                }

                for attendance in &mut session.participants {
                    if attendance.is_open()
                        && !roster.iter().any(|member| member.sid == attendance.sid)
                    {
                        attendance.left_at_ms = Some(now_ms);
                        changed = true;
                    }
                }

                let session = session.clone();

                if humans >= floor {
                    if records.ends_at.remove(room).is_some() {
                        tracing::info!(
                            session = %session.id,
                            %room,
                            humans,
                            "the room filled up again; the meeting is no longer ending"
                        );
                    }
                } else if !records.ends_at.contains_key(room) {
                    // The first departure sets the deadline and the ones after
                    // it leave it where it is. A second person leaving is not a
                    // reason for the meeting to run longer than the first
                    // departure promised.
                    records.ends_at.insert(room.to_owned(), now_ms + grace_ms);
                    tracing::info!(
                        session = %session.id,
                        %room,
                        humans,
                        floor,
                        grace_ms,
                        "too few people left in the room; the meeting ends when the grace period \
                         runs out"
                    );
                }

                if changed {
                    records.queue(session);
                }

                None
            }
        };

        self.inner.work.notify_one();
        started
    }

    /// Ends the meeting running in a room, now, because something ended it.
    ///
    /// The room was deleted or otherwise taken away: there is no grace period
    /// to serve, because there is nothing left to come back to.
    pub fn close(&self, room: &str, now_ms: i64) -> Option<Session> {
        let session = {
            let mut records = self.inner.records.lock().expect("session lock");
            records.finish(room, now_ms, Ending::Closed)?
        };

        tracing::info!(
            session = %session.id,
            %room,
            "a meeting ended because its room did"
        );
        self.inner.work.notify_one();

        Some(session)
    }

    /// Ends every meeting whose grace period has run out.
    ///
    /// This is the session half of the registry's one clock, and it takes the
    /// time rather than reading it for the same reason the other half does: a
    /// test has to be able to be the clock.
    ///
    /// A meeting ends at the deadline it was given, not at the moment the
    /// sweep noticed. The sweep runs on a tick and a record that says a
    /// meeting ran ninety-one seconds past its last departure because the tick
    /// landed there is a record nobody can reconcile with the promise the
    /// grace period made.
    ///
    /// The caller has to hold whatever lock an arrival takes while it runs
    /// this. A deadline that expired in the same instant somebody walked back
    /// in must end nothing, and that is only true if the arrival cannot be
    /// halfway between changing the room and saying so.
    pub fn expire(&self, now_ms: i64) -> Vec<Session> {
        let ended = {
            let mut records = self.inner.records.lock().expect("session lock");

            let due: Vec<(String, i64)> = records
                .ends_at
                .iter()
                .filter(|(_, ends_at)| now_ms >= **ends_at)
                .map(|(room, ends_at)| (room.clone(), *ends_at))
                .collect();

            let mut ended = Vec::new();
            for (room, ends_at) in due {
                let Some(session) = records.finish(&room, ends_at, Ending::Grace) else {
                    continue;
                };

                tracing::info!(
                    session = %session.id,
                    %room,
                    "a meeting ended: nobody came back before the grace period ran out"
                );
                ended.push(session);
            }

            ended
        };

        self.inner.work.notify_one();
        ended
    }

    /// The meeting running in a room, if one is.
    pub fn live(&self, room: &str) -> Option<Session> {
        self.inner
            .records
            .lock()
            .expect("session lock")
            .live
            .get(room)
            .cloned()
    }

    /// When the meeting running in a room ends if nobody comes back.
    ///
    /// The one deadline. A room that has to end itself when its session policy
    /// runs out reads this rather than arming a second one, because two
    /// deadlines armed on two conditions disagree, and the disagreement ends a
    /// meeting under people who are still in the room.
    pub fn ends_at(&self, room: &str) -> Option<i64> {
        self.inner
            .records
            .lock()
            .expect("session lock")
            .ends_at
            .get(room)
            .copied()
    }

    /// One page of a room's meetings, newest first.
    ///
    /// Newest first because the question is almost always about the last one:
    /// a backoffice opening a room's history wants what happened today above
    /// what happened in March. A page rather than the history because a room a
    /// service has been meeting in for two years has thousands, and building
    /// all of them into one answer would read thousands of files with a lock
    /// held that every arrival on the server also needs.
    ///
    /// The files are read outside that lock. A record that has gone missing
    /// under the server -- deleted by the retention policy between the index
    /// being consulted and the file being opened -- is left out of the page
    /// rather than failing it.
    pub fn of_room(&self, room: &str, offset: usize, limit: usize) -> Page {
        let limit = limit.min(MAX_PAGE);

        let (total, wanted) = {
            let records = self.inner.records.lock().expect("session lock");
            let Some(ids) = records.by_room.get(room) else {
                return Page::default();
            };

            let wanted: Vec<Located> = ids
                .iter()
                .rev()
                .skip(offset)
                .take(limit)
                .filter_map(|id| records.locate(id, &self.inner.directory))
                .collect();

            (ids.len(), wanted)
        };

        Page {
            sessions: wanted.into_iter().filter_map(Located::read).collect(),
            total,
        }
    }

    /// One meeting by id.
    pub fn get(&self, id: &str) -> Option<Session> {
        let located = {
            let records = self.inner.records.lock().expect("session lock");
            records.locate(id, &self.inner.directory)?
        };

        located.read()
    }
}

impl Drop for SessionStore {
    /// Stops the writer, having let it finish what it was owed.
    ///
    /// A store that went away with records still queued would lose exactly the
    /// meetings that ended last, which in a test is every meeting it held.
    fn drop(&mut self) {
        self.inner.records.lock().expect("session lock").closing = true;
        self.inner.work.notify_all();

        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// A record and where to get it: already in memory, or in a file.
enum Located {
    Resident(Session),
    OnDisk(PathBuf),
}

impl Located {
    fn read(self) -> Option<Session> {
        match self {
            Self::Resident(session) => Some(session),
            Self::OnDisk(path) => match read_record(&path) {
                Ok(session) => Some(session),
                Err(error) => {
                    tracing::error!(
                        path = %path.display(),
                        %error,
                        "a session record this server has indexed could not be read back"
                    );
                    None
                }
            },
        }
    }
}

impl Indexed {
    fn path(&self, directory: &Path, id: &str) -> PathBuf {
        match &self.file {
            Some(file) => directory.join(file.as_ref()),
            None => directory.join(format!("{id}.json")),
        }
    }
}

impl Records {
    /// Files a record found on disk under the id inside it.
    fn index_record(&mut self, header: &Header, path: &Path) {
        let expected = format!("{}.json", header.id);
        let file = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| *name != expected)
            .map(|name| {
                tracing::warn!(
                    path = %path.display(),
                    id = %header.id,
                    "a session record is in a file that is not named after it; it is filed under \
                     the id inside it and read back out of the file it is in"
                );
                Box::from(name.as_str())
            });

        self.by_room
            .entry(header.room.clone())
            .or_default()
            .push(header.id.clone());
        self.index.insert(
            header.id.clone(),
            Indexed {
                room: Arc::from(header.room.as_str()),
                started_at_ms: header.started_at_ms,
                file,
            },
        );
    }

    /// Files a meeting that has just started.
    fn start(&mut self, session: Session) {
        self.by_room
            .entry(session.room.clone())
            .or_default()
            .push(session.id.clone());
        self.index.insert(
            session.id.clone(),
            Indexed {
                room: Arc::from(session.room.as_str()),
                started_at_ms: session.started_at_ms,
                file: None,
            },
        );
        self.live.insert(session.room.clone(), session);
    }

    /// The moment no arrival time in this room's next meeting may precede.
    fn floor_of(&self, room: &str) -> i64 {
        self.last_ended_ms.get(room).copied().unwrap_or(i64::MIN)
    }

    fn remember_ending(&mut self, room: &str, ended_at_ms: i64) {
        let entry = self
            .last_ended_ms
            .entry(room.to_owned())
            .or_insert(ended_at_ms);
        *entry = (*entry).max(ended_at_ms);
    }

    /// Where to get a record from, if this server has one.
    fn locate(&self, id: &str, directory: &Path) -> Option<Located> {
        if let Some(queued) = self.queued.get(id) {
            return Some(Located::Resident(queued.session.clone()));
        }
        if let Some(session) = self.unwritten.get(id) {
            return Some(Located::Resident(session.clone()));
        }

        let indexed = self.index.get(id)?;
        if let Some(session) = self.live.get(indexed.room.as_ref())
            && session.id == id
        {
            return Some(Located::Resident(session.clone()));
        }

        Some(Located::OnDisk(indexed.path(directory, id)))
    }

    /// Puts every room's meetings back into the order they happened in.
    ///
    /// Insertion order is already that order while the server runs, because a
    /// room holds one meeting at a time and each is inserted as it starts. It
    /// is not that order after a load, where the records arrive in whatever
    /// order the directory listed them -- so this runs once, at the end of a
    /// load, rather than on every insert.
    fn sort(&mut self) {
        let started: HashMap<String, i64> = self
            .index
            .iter()
            .map(|(id, indexed)| (id.clone(), indexed.started_at_ms))
            .collect();

        for ids in self.by_room.values_mut() {
            ids.sort_by_key(|id| (started.get(id).copied().unwrap_or(0), id.clone()));
        }
    }

    fn finish(&mut self, room: &str, at_ms: i64, ending: Ending) -> Option<Session> {
        let mut session = self.live.remove(room)?;
        self.ends_at.remove(room);

        // Never backwards. A meeting closed at its deadline while the sweep
        // was late must not be given an end time before somebody's departure
        // was written into it.
        session.close(at_ms.max(session.last_known_ms()), ending);
        let ended_at = session.ended_at_ms.expect("just closed");
        self.remember_ending(room, ended_at);
        self.queue(session.clone());

        Some(session)
    }

    /// Owes this record to the disk, replacing whatever edit of it was owed.
    fn queue(&mut self, session: Session) {
        self.next_version += 1;
        let version = self.next_version;

        self.queued
            .insert(session.id.clone(), Queued { version, session });
    }
}

/// The writer: the one place in this crate that waits for a disk.
///
/// Whole-file rewrite rather than an appended line, so that the file on disk is
/// the record and not a log that has to be replayed to become one: whoever
/// finds this directory in two years reads a meeting with `cat`. A record is a
/// few hundred bytes and a meeting produces a few dozen writes, so the cost of
/// rewriting it is not worth an append format nobody but this crate could read.
///
/// Nothing here holds the records lock across a write. It takes the lock to
/// pick up what is owed, drops it, writes, and takes it again to strike off
/// what it wrote -- which is what keeps a busy disk out of the way of somebody
/// joining a room. A record is left in the queue until it is written, so a
/// reader is never told a meeting has no record because it is mid-flight.
fn write_queued(inner: &Arc<Inner>) {
    loop {
        let batch: Vec<(u64, Session)> = {
            let mut records = inner.records.lock().expect("session lock");
            while records.queued.is_empty() && !records.closing {
                records = inner.work.wait(records).expect("session lock");
            }
            if records.queued.is_empty() {
                return;
            }

            records
                .queued
                .values()
                .map(|queued| (queued.version, queued.session.clone()))
                .collect()
        };

        let written: Vec<(u64, Session, io::Result<()>)> = batch
            .into_iter()
            .map(|(version, session)| {
                let outcome = write_record(&inner.directory, &session);
                (version, session, outcome)
            })
            .collect();

        {
            let mut records = inner.records.lock().expect("session lock");
            for (version, session, outcome) in written {
                // Only if nothing has edited the record since: a meeting that
                // moved on while its older self was being written is still
                // owed to the disk.
                if records
                    .queued
                    .get(&session.id)
                    .is_some_and(|queued| queued.version == version)
                {
                    records.queued.remove(&session.id);
                }

                match outcome {
                    Ok(()) => {
                        records.unwritten.remove(&session.id);
                    }
                    // Logged, counted, and the meeting carries on. Nobody's
                    // call is failed because the disk was full; the
                    // consequence is that this record will not survive the
                    // process, which is what the counter on `/healthz` is for.
                    Err(error) => {
                        tracing::error!(
                            session = %session.id,
                            room = %session.room,
                            directory = %inner.directory.display(),
                            %error,
                            "a session record could not be written; this meeting will not survive \
                             a restart"
                        );
                        records.unwritten.insert(session.id.clone(), session);
                    }
                }
            }
        }

        inner.written.notify_all();
    }
}

/// What the loader reads out of a record without reading the record.
///
/// Everything the index needs and nothing else: the attendance list of every
/// meeting ever held is what makes a load expensive, and it is not looked at
/// until somebody asks for that meeting.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    id: String,
    room: String,
    started_at_ms: i64,
    #[serde(default)]
    ended_at_ms: Option<i64>,
}

fn is_record(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == "json")
}

/// Whether this is an id this server could have minted.
///
/// Checked before an id is ever joined onto the records directory. Ids reach
/// this server from a URL and out of files an operator may have put in the
/// directory by hand, and neither is a reason to open a path of somebody
/// else's choosing.
fn is_id(id: &str) -> bool {
    let Some(tail) = id.strip_prefix(SESSION_ID_PREFIX) else {
        return false;
    };

    !tail.is_empty()
        && tail
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn read_header(path: &Path) -> io::Result<Header> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn read_record(path: &Path) -> io::Result<Session> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn write_record(directory: &Path, session: &Session) -> io::Result<()> {
    use std::io::Write as _;

    let body = serde_json::to_vec_pretty(session).map_err(io::Error::other)?;
    let final_path = directory.join(format!("{}.json", session.id));
    let pending_path = directory.join(format!("{}.json{PENDING_SUFFIX}", session.id));

    let mut file = std::fs::File::create(&pending_path)?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    // Flushed before the rename, so the record is either the old one or the
    // new one and never the first half of the new one. What is not done here
    // is flushing the directory itself: a power cut can still lose the rename,
    // and the cost of not losing it is an fsync of the directory on every
    // arrival.
    file.sync_all()?;
    drop(file);

    std::fs::rename(&pending_path, &final_path)
}

fn generate_id() -> String {
    format!(
        "{SESSION_ID_PREFIX}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn member(sid: &str, identity: &str, joined_at_ms: i64) -> Member {
        Member {
            sid: sid.to_owned(),
            identity: identity.to_owned(),
            name: identity.to_owned(),
            kind: Kind::Standard,
            joined_at_ms,
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

    fn store(directory: &tempfile::TempDir) -> SessionStore {
        SessionStore::open(directory.path()).expect("the session records open")
    }

    /// Everything a room has held, which is what `of_room` used to hand back
    /// in one piece.
    fn history(store: &SessionStore, room: &str) -> Vec<Session> {
        store.of_room(room, 0, MAX_PAGE).sessions
    }

    /// The reason these records are on a disk at all. A consumer that was down
    /// when a meeting ended has to be able to come back and ask what it
    /// missed, and it cannot ask a process that has been restarted since.
    #[test]
    fn a_meeting_outlives_the_process_that_recorded_it() {
        let directory = directory();

        let id = {
            let store = store(&directory);
            store.note("demo", &[member("PA_1", "alice", 1_000)], 0, 90_000, 1_000);
            let id = store.live("demo").expect("a meeting started").id;

            store.note("demo", &[], 0, 90_000, 5_000);
            assert_eq!(store.expire(95_000).len(), 1);
            id
        };

        let store = store(&directory);
        let session = store
            .get(&id)
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
            store.note("demo", &[member("PA_1", "alice", 1_000)], 0, 90_000, 1_000);
            store.note(
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
        let session = store.get(&id).expect("the record is kept");
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
                store.note("standup", &[member(&sid, "alice", start)], 0, 0, start);
                store.note("standup", &[], 0, 0, start + 1_000);
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
            store.note("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
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

        let store = SessionStore::open(directory.path())
            .expect("one unreadable file took every other meeting on this server with it");

        assert_eq!(
            store.count(),
            1,
            "a file that is not a record was read back as a meeting"
        );
        assert!(store.get(&id).is_some());
    }

    /// The store is told the whole roster on every change, so it is told the
    /// same roster many times. Writing a record each time would turn a quiet
    /// room into a stream of identical files.
    #[test]
    fn a_roster_that_has_not_moved_changes_nothing() {
        let directory = directory();
        let store = store(&directory);
        let roster = [member("PA_1", "alice", 1_000)];

        store.note("demo", &roster, 0, 90_000, 1_000);
        let first = store.live("demo").expect("a meeting");

        store.note("demo", &roster, 0, 90_000, 2_000);
        store.note("demo", &roster, 0, 90_000, 3_000);

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
                )],
            };
            write_record(directory.path(), &session).unwrap();
        }

        let store = store(&directory);

        assert!(
            store.live("demo").is_none(),
            "a meeting nobody has been at since the last restart is being reported as the one \
             happening now"
        );
        for id in ["SE_first0000000", "SE_second000000"] {
            let session = store.get(id).expect("the record is kept");
            assert_eq!(
                session.ended_by,
                Some(Ending::Restart),
                "`{id}` is still open"
            );
        }

        store.note("demo", &[member("PA_2", "bob", 20_000)], 0, 0, 20_000);
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

        store.note("standup", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
        store.note("payroll", &[member("PA_2", "bob", 1_000)], 0, 0, 1_000);

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

        store.note("demo", std::slice::from_ref(&scribe), 0, 90_000, 1_000);
        store.note("demo", &[scribe.clone(), alice], 0, 90_000, 2_000);
        store.note("demo", std::slice::from_ref(&scribe), 0, 90_000, 10_000);
        assert_eq!(store.expire(100_000).len(), 1, "the first meeting ended");

        store.note("demo", &[scribe.clone(), bob], 0, 90_000, 200_000);
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

        store.note("demo", &[alice.clone(), bob.clone()], 2, 90_000, 1_000);
        store.note("demo", std::slice::from_ref(&alice), 2, 90_000, 5_000);
        // The same connection comes back: the room reports the sid it always
        // had, with the arrival time it always had.
        store.note("demo", &[alice, bob.clone()], 2, 90_000, 9_000);

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
                store.note("standup", &[member(&sid, "alice", at)], 0, 0, at);
                store.note("standup", &[], 0, 0, at + 1_000);
                store.expire(at + 1_000);
            }
        }

        let store = store(&directory);

        assert_eq!(store.count(), held, "the meetings were not all indexed");
        assert_eq!(
            store.resident(),
            0,
            "{} finished meetings are being held in memory; nothing is running, nothing is owed \
             to the disk, and this is the number that grows for every meeting the server ever \
             holds",
            store.resident()
        );

        let newest = store
            .of_room("standup", 0, 1)
            .sessions
            .pop()
            .expect("the newest meeting");
        assert!(
            store.get(&newest.id).is_some(),
            "a meeting that is only on disk cannot be read back by id"
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
            store.note("standup", &[member(&sid, "alice", at)], 0, 0, at);
            store.note("standup", &[], 0, 0, at + 1_000);
            store.expire(at + 1_000);
        }

        let first = store.of_room("standup", 0, 10);
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

        let second = store.of_room("standup", 10, 10);
        assert_eq!(second.sessions.len(), 10);
        assert_eq!(second.sessions[0].started_at_ms, 141_000);

        let last = store.of_room("standup", 20, 10);
        assert_eq!(
            last.sessions.len(),
            5,
            "the last page is short and that is the end"
        );

        assert_eq!(
            store.of_room("standup", 0, MAX_PAGE + 500).sessions.len(),
            25,
            "a caller asking for more than the cap got more than the cap"
        );
    }

    /// Finding H. The record used to be flushed to the disk on the thread that
    /// was letting somebody into a room, under a lock every other room's
    /// arrivals also needed -- so a busy disk delayed joining, everywhere at
    /// once.
    ///
    /// Measured against this machine's own `sync_all` rather than a fixed
    /// number of milliseconds, because the whole question is "how many disk
    /// flushes did the caller wait for" and the answer has to mean the same
    /// thing on an SSD and on a loaded CI box. On a filesystem where a flush
    /// is free this proves nothing -- and on one where it is free there was
    /// nothing to prove.
    #[test]
    fn recording_a_membership_change_does_not_wait_for_a_disk() {
        let directory = directory();
        let store = store(&directory);
        let changes = 200;

        let one_flush = {
            let probe = directory.path().join("probe");
            let began = Instant::now();
            for round in 0..10 {
                let session = Session {
                    id: format!("SE_probe{round:07}"),
                    room: "probe".to_owned(),
                    started_at_ms: 0,
                    ended_at_ms: Some(0),
                    ended_by: Some(Ending::Closed),
                    participants: Vec::new(),
                };
                std::fs::create_dir_all(&probe).unwrap();
                write_record(&probe, &session).unwrap();
            }
            began.elapsed() / 10
        };

        let began = Instant::now();
        for index in 0..changes {
            let sid = format!("PA_{index}");
            store.note(
                "demo",
                &[member("PA_anchor", "anchor", 0), member(&sid, "alice", 0)],
                0,
                90_000,
                index as i64,
            );
        }
        let spent = began.elapsed();

        assert!(
            spent < one_flush * 20,
            "{changes} membership changes took {spent:?}, which is more than twenty flushes of \
             this filesystem ({one_flush:?} each): somebody joining a room is waiting for a disk \
             again, and waiting for it under a lock every other room needs"
        );
    }

    /// The freshness the writer trades away has a bottom: a store that is
    /// dropped has written everything it was owed, so a planned stop loses
    /// nothing.
    #[test]
    fn a_planned_stop_leaves_nothing_owed_to_the_disk() {
        let directory = directory();
        let store = store(&directory);

        store.note("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
        let id = store.live("demo").expect("a meeting").id;
        store.close("demo", 2_000);

        store.flush();
        assert_eq!(store.owed(), 0, "a flush returned with records still owed");

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

    /// A record that is owed to the disk still answers, or a backoffice
    /// polling for the end of a meeting would see it un-end for a moment.
    #[test]
    fn a_record_answers_before_it_reaches_the_disk() {
        let directory = directory();
        let store = store(&directory);

        store.note("demo", &[member("PA_1", "alice", 1_000)], 0, 0, 1_000);
        let session = store.close("demo", 2_000).expect("the meeting ended");

        assert_eq!(
            store.get(&session.id).map(|record| record.ended_at_ms),
            Some(Some(2_000)),
            "a meeting that has ended reads as still running until its file lands"
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
}
