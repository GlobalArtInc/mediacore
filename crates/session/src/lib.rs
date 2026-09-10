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
//! The record is written as the meeting goes on, rather than emitted when it
//! ends. The alternative -- keeping it in memory and announcing the end --
//! makes the event the only copy of the fact, so a consumer that was down at
//! half past six can never learn what it missed. That is the failure this is
//! meant to make impossible, and a record that dies with the process
//! reintroduces it.
//!
//! # Two stores, and which one a deployment wants
//!
//! [`SessionStore::files`] keeps one JSON file per meeting in a directory. It
//! needs nothing installed, an operator reads a meeting with `cat`, and a
//! retention policy is `find -mtime +90 -delete`. It is right for exactly one
//! server process and wrong for more than one: three processes cannot share a
//! directory -- each would hold its own index of it and overwrite the others'
//! records -- and a node that dies takes its unfinished meetings out of reach
//! of the two that are still up.
//!
//! [`SessionStore::postgres`] keeps the same records in a database every
//! process can read, plus the recordings hanging off a meeting and the queue of
//! work that has to happen to them. A deployment that runs more than one server
//! process, or that records anything at all, needs this one.
//!
//! Both are the same store from the outside, and the reason they are is that
//! the thing worth having only once lives above them: **where the boundary of a
//! meeting is**. A meeting begins when a room reaches the number of people it
//! needs and ends when it has been below that number for the whole of its grace
//! period, and both halves of that sentence are decided in
//! [`SessionStore::note`], in memory, against the roster the registry hands
//! over. Whoever else needs the deadline -- the room, so that it can end itself
//! under a session policy -- reads it back with [`SessionStore::ends_at`]
//! rather than keeping a second one. Two deadlines armed by two conditions
//! drift apart, and the shape of that drift is a record closed under people who
//! are still talking.
//!
//! # Nothing waits on a store on the way into a room
//!
//! A membership change takes one in-memory lock, edits a record and wakes a
//! thread. It never opens a file and never speaks to a database, and that is
//! the property everything else here is arranged around: the first version of
//! this crate flushed to disk on the thread that was letting somebody into a
//! room, under a lock every other room's arrivals also needed, so one busy disk
//! delayed joining everywhere at once. A database at the other end of a network
//! is that failure again with a longer tail. See
//! [`SessionStore::flush`] for what the trade costs, and the test called
//! `recording_a_membership_change_never_waits_for_the_store` for the shape of
//! the regression it is meant to catch.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

mod database;
mod files;
mod recording;
mod schema;

pub use database::PostgresSettings;
pub use recording::{
    Claim, RECORDING_ID_PREFIX, Recording, Stage, State as WorkState, Work, recording_id,
};

/// The prefix every session id carries, in the shape of the other ids on this
/// server: `RM_` for a room, `PA_` for a participant, `SE_` for a session.
pub const SESSION_ID_PREFIX: &str = "SE_";

/// How many meetings one page of a room's history holds when the caller does
/// not say.
pub const DEFAULT_PAGE: usize = 100;

/// The most one page may hold however large a page the caller asks for.
///
/// A room a service has been making meetings in for two years has thousands,
/// and the answer to "give me all of them" cannot be a single response that
/// takes a second to build and a megabyte to send.
pub const MAX_PAGE: usize = 1_000;

/// How often the writer wakes when nothing is happening.
///
/// Short, because it is also how long a record that failed to be written waits
/// before anybody notices it is due again. It is not a poll of anything: an
/// arrival wakes the writer immediately.
const WRITER_TICK: Duration = Duration::from_millis(250);

/// The wait before a record that could not be written is tried again, and the
/// cap on that wait as it doubles.
///
/// The cap matters more than the doubling: without one, a database that came
/// back a moment ago would go unwritten for minutes because the last attempt
/// happened to be the eleventh.
const RETRY_AFTER: Duration = Duration::from_secs(1);
const RETRY_AT_MOST: Duration = Duration::from_secs(30);

/// What a participant was, which is what decides whether they count towards a
/// quorum.
///
/// The same kinds `ParticipantInfo.kind` carries. They are spelled out again
/// here rather than borrowed from the room crate because the dependency runs
/// the other way -- the registry writes records, the records know nothing
/// about a registry -- and because these strings are part of the JSON a
/// backoffice parses and of the `kind` column a query filters on, so they
/// should be nailed down where the record is defined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A person.
    #[default]
    Standard,
    /// A recorder.
    Egress,
    /// Somebody on a telephone.
    Sip,
    /// A bot.
    Agent,
}

impl Kind {
    /// Whether this one counts as somebody who came to the meeting.
    ///
    /// A recorder sitting in an empty room is not a meeting, and a room of
    /// bots is not one either: a session opens and closes on people. Somebody
    /// who dialled in is one of those people.
    pub fn is_human(self) -> bool {
        matches!(self, Self::Standard | Self::Sip)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Egress => "egress",
            Self::Sip => "sip",
            Self::Agent => "agent",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(Self::Standard),
            "egress" => Some(Self::Egress),
            "sip" => Some(Self::Sip),
            "agent" => Some(Self::Agent),
            _ => None,
        }
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
    /// Nobody watched this meeting end. The server holding it stopped -- it
    /// was found unfinished on the next start of a single-process deployment,
    /// or it stopped saying it was alive and another node closed it. The end
    /// time is the last thing the record itself knows about, not the moment
    /// the meeting really stopped.
    Restart,
}

impl Ending {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grace => "grace",
            Self::Closed => "closed",
            Self::Restart => "restart",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "grace" => Some(Self::Grace),
            "closed" => Some(Self::Closed),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }
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
    /// Their attributes, as they stood when this stretch began.
    ///
    /// **Opaque.** Nothing in this server reads a key of it or gives it a
    /// shape: it is the place a platform embedding mediacore puts its own
    /// identifiers -- who this is to it, which contract they are on, whatever
    /// it needs on the record afterwards -- and what that means is the
    /// platform's business and not ours. Empty when nothing set any.
    ///
    /// They arrive in the join token and can be changed while somebody is in a
    /// room; a change after this stretch began does not reach back into it, for
    /// the same reason a rename does not reach back into [`Attendance::name`].
    /// A record is of what was true at the time.
    #[serde(default)]
    pub attributes: HashMap<String, String>,
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
    fn open(member: &Member, not_before_ms: i64, session: &str, opaque_bytes: usize) -> Self {
        Self {
            sid: member.sid.clone(),
            identity: member.identity.clone(),
            name: member.name.clone(),
            kind: member.kind,
            attributes: kept_attributes(&member.attributes, opaque_bytes, session, &member.sid),
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
/// One process that is up, and what it says it is holding.
///
/// Read out of the shared store rather than asked of the process, because the
/// question is usually asked about a node that has stopped answering. A row is
/// rewritten whole by its own node on every beat: nobody writes anybody else's,
/// and a set of rooms is replaced rather than added to, so there is no release
/// step to be missed and no room name left pinned to a node that let it go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    /// Minted at startup. Two processes configured with one `node_id` -- one
    /// value in a shared ConfigMap is all it takes -- would otherwise share
    /// this row and each other's work without anything saying so. A beat that
    /// finds a different boot in its own row is that, proven.
    pub boot: String,
    /// How another node reaches this one. Not how a browser does.
    pub signal_url: String,
    /// The addresses this node puts in its ICE candidates, for reading by
    /// somebody diagnosing why media went nowhere.
    pub advertise: String,
    pub version: String,
    pub region: String,
    pub state: NodeState,
    pub started_at_ms: i64,
    pub kept_alive_at_ms: i64,
    /// Every room this node held when it last beat.
    pub rooms: Vec<String>,
}

impl Node {
    /// Whether this row is still being written.
    ///
    /// Evaluated against a clock the caller supplies, so a test can watch a
    /// lease run out without waiting for one.
    pub fn alive(&self, now_ms: i64, lease_ms: i64) -> bool {
        self.kept_alive_at_ms >= now_ms - lease_ms
    }

    /// Whether this node should be given anything new. A draining node is
    /// still alive and still holds its rooms; what it must not get is more.
    pub fn takes_work(&self, now_ms: i64, lease_ms: i64) -> bool {
        self.alive(now_ms, lease_ms) && self.state == NodeState::Live
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Live,
    Draining,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Draining => "draining",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "live" => Some(Self::Live),
            "draining" => Some(Self::Draining),
            _ => None,
        }
    }
}

/// Enough to answer "was there a meeting in this room, who was in it, how
/// long", and enough to hang a recording off -- a recording belongs to the
/// meeting it recorded and not to the room, which will hold another meeting
/// tomorrow.
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
    /// The room's metadata as it stood when this meeting began, or `null` when
    /// the room had none.
    ///
    /// **Opaque, and the point of it.** mediacore is a core other platforms
    /// build on, and a platform needs somewhere on the record of a meeting to
    /// put what a meeting means to it -- which of its accounts held it, which
    /// of its bookings it was, whatever it is. Without such a place, the first
    /// customer's idea of that ends up as a column of ours and the second
    /// customer has nowhere at all. So this is stored and handed back and never
    /// read: the only thing in a room's metadata this server looks at is
    /// `sessionPolicy`, which the room service reads long before a record
    /// exists.
    ///
    /// As it stood when the meeting began, and not as it stands now. Metadata
    /// changed halfway through belongs to the meeting after this one; a record
    /// of what happened must not be rewritten by something that happened
    /// afterwards.
    #[serde(default)]
    pub metadata: Option<String>,
}

impl Session {
    pub fn is_live(&self) -> bool {
        self.ended_at_ms.is_none()
    }

    fn open(
        room: &str,
        roster: &[Member],
        now_ms: i64,
        not_before_ms: i64,
        metadata: &str,
        opaque_bytes: usize,
    ) -> Self {
        let id = generate_id();

        Self {
            room: room.to_owned(),
            started_at_ms: now_ms,
            ended_at_ms: None,
            ended_by: None,
            participants: roster
                .iter()
                .map(|member| Attendance::open(member, not_before_ms, &id, opaque_bytes))
                .collect(),
            metadata: kept_metadata(metadata, opaque_bytes, &id),
            id,
        }
    }

    /// Writes somebody into this meeting who is in the room and not in it yet.
    ///
    /// The floor is the later of the end of the room's previous meeting and
    /// the end of this connection's own previous stretch here, so that one
    /// `sid` never has two stretches that overlap.
    fn admit(&mut self, member: &Member, not_before_ms: i64, opaque_bytes: usize) {
        let after_their_own = self
            .participants
            .iter()
            .filter(|attendance| attendance.sid == member.sid)
            .filter_map(|attendance| attendance.left_at_ms)
            .max()
            .unwrap_or(i64::MIN);

        self.participants.push(Attendance::open(
            member,
            not_before_ms.max(after_their_own),
            &self.id,
            opaque_bytes,
        ));
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
    /// What an unfinished record is closed at when nobody watched it end. It
    /// is not when the meeting stopped -- nobody saw that happen -- it is the
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
    /// Their attributes, as the room has them now.
    ///
    /// Handed through untouched. This crate neither reads a key of it nor
    /// requires one to be there: it is the embedding platform's own data, and
    /// somebody who has none is an ordinary participant.
    pub attributes: HashMap<String, String>,
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

/// One page of a room's meetings on its way out of a store: the ids in the
/// order they belong in, and whatever the store had for each.
struct Listing {
    entries: Vec<(String, Option<Session>)>,
    total: usize,
}

/// What a store can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The store could not be reached, or refused the statement. The meeting
    /// carries on; the caller sees this only on a read.
    Unavailable(String),
    /// This store does not do this at all -- a directory of files asked for a
    /// work queue.
    Unsupported(&'static str),
    /// The store answered and the answer made no sense: a row written by hand,
    /// or by a version of this server that spelled something differently.
    Nonsense(String),
    /// Somebody else holds what this node was writing. There is one of these
    /// and it is worth its own variant rather than an `Unavailable`, because
    /// it is never transient: retrying produces the same answer, and the thing
    /// to do about it is change a configuration.
    Taken(&'static str),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(what) => {
                write!(formatter, "the session store is not answering: {what}")
            }
            Self::Unsupported(what) => write!(formatter, "this session store cannot {what}"),
            Self::Nonsense(what) => write!(formatter, "the session store answered with {what}"),
            Self::Taken(what) => write!(formatter, "{what}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Where records are actually kept.
///
/// Everything about *when* a meeting starts and stops is above this and shared
/// by both implementations; everything below it is about a disk or a database
/// and nothing else. Nothing here is ever called on the path a participant
/// joins by, with one exception that is spelled out on [`Sink::opened`].
trait Sink: Send + Sync + 'static {
    /// One line for a log and for `/healthz`.
    fn describe(&self) -> String;

    /// Whether the store answered the last time anybody asked it anything.
    fn healthy(&self) -> bool;

    /// How many meetings the store holds. Cheap by construction: a store that
    /// would have to ask for this keeps a recent answer instead, because
    /// `/healthz` is scraped and the number is a gauge, not a fact anybody
    /// acts on to the meeting.
    fn count(&self) -> usize;

    /// Says this node is up, and what it is holding.
    ///
    /// Default: nothing. A store that only one process can use has no cluster
    /// to describe, and answering with an empty one would have a caller
    /// believe it had asked and been told there is nobody -- which is a
    /// different thing from not being able to ask.
    fn publish_node(&self, _node: &Node) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("say which nodes are up"))
    }

    /// Every node that has written a row, alive or not. The caller decides
    /// which are still beating, against its own clock.
    fn nodes(&self) -> Result<Vec<Node>, StoreError> {
        Err(StoreError::Unsupported("say which nodes are up"))
    }

    /// Takes this node's row away on a planned stop, so the cluster converges
    /// in a beat rather than in a lease.
    fn forget_node(&self, _id: &str) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("say which nodes are up"))
    }

    /// A meeting has just started.
    ///
    /// **The one thing here that runs on the join path**, so it may take an
    /// in-memory lock and may do nothing else. It exists because a directory
    /// keeps its own index of what it holds, and a room's history that did not
    /// mention the meeting happening in it until the writer caught up would be
    /// wrong in the one case anybody is looking at.
    fn opened(&self, _session: &Session) {}

    /// Writes a record down. `now_ms` is this node's own clock at the moment
    /// of the write, which is what a shared store renews a meeting's lease
    /// against -- deliberately not any of the times inside the record, which
    /// are the meeting's clock and are handed in by whoever is driving it.
    fn persist(&self, session: &Session, now_ms: i64) -> Result<(), StoreError>;
    fn fetch(&self, id: &str) -> Result<Option<Session>, StoreError>;

    /// One page of a room's meetings, newest first: the ids in order, and
    /// whatever the store could produce for each of them.
    ///
    /// The ids come back even when the record behind one does not, because
    /// this node may be carrying a copy of it that has not been written yet --
    /// which is the ordinary case for the meeting happening right now. The
    /// store above decides what to put in the gaps; a gap nobody can fill is
    /// left out of the page rather than failing it.
    fn page(&self, room: &str, offset: usize, limit: usize) -> Result<Listing, StoreError>;

    /// How often [`Sink::tick`] is worth calling.
    fn tick_every(&self) -> Duration {
        Duration::from_secs(3600)
    }

    /// Whatever this store has to do when nothing is happening: say this node
    /// is still holding the meetings it is holding, and notice that another
    /// node has stopped saying so.
    fn tick(&self, _live: &[String], _now_ms: i64) {}

    fn note_recording(&self, _recording: &Recording, _now_ms: i64) -> Result<String, StoreError> {
        Err(StoreError::Unsupported("keep recordings"))
    }

    fn recordings(&self, _session: &str) -> Result<Vec<Recording>, StoreError> {
        Err(StoreError::Unsupported("keep recordings"))
    }

    fn work(&self, _recording: &str) -> Result<Vec<Work>, StoreError> {
        Err(StoreError::Unsupported("keep recordings"))
    }

    fn session_work(&self, _session: &str) -> Result<Vec<Work>, StoreError> {
        Err(StoreError::Unsupported("keep recordings"))
    }

    fn claim(&self, _stage: Stage, _now_ms: i64, _limit: usize) -> Result<Vec<Claim>, StoreError> {
        Err(StoreError::Unsupported("hand out work"))
    }

    fn renew(&self, _recording: &str, _stage: Stage, _now_ms: i64) -> Result<bool, StoreError> {
        Err(StoreError::Unsupported("hand out work"))
    }

    fn uploaded(
        &self,
        _recording: &str,
        _object_key: &str,
        _bytes: Option<u64>,
        _now_ms: i64,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("hand out work"))
    }

    fn announced(&self, _recording: &str, _now_ms: i64) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("hand out work"))
    }

    fn postpone(
        &self,
        _recording: &str,
        _stage: Stage,
        _error: &str,
        _due_at_ms: i64,
    ) -> Result<bool, StoreError> {
        Err(StoreError::Unsupported("hand out work"))
    }

    /// How much work is in a state nobody is going to move: given up on, or
    /// waiting so far past its time that whoever it is waiting for is not
    /// coming.
    ///
    /// A queue that strands work silently is worse than one that fails loudly,
    /// and this is the number that stops it being silent. Zero for a store that
    /// has no queue.
    fn stuck(&self) -> u64 {
        0
    }
}

/// Every session this server is holding, and the store they are kept in.
///
/// **What it costs while running.** A membership change takes one lock, edits
/// a record in memory and wakes a thread. No caller waits for a disk or a
/// database.
///
/// **What a crash costs** is a freshness window, and it is the price of the
/// write not being on the join path. Membership changes are recorded in memory
/// and handed to a writer thread; that thread has them in the store a moment
/// later, and a crash inside that moment loses the changes it had not reached
/// yet. So after a crash a record may be missing its last few arrivals and
/// departures, and a meeting that started in that moment may be missing
/// altogether. It never says anything that did not happen: what is in the store
/// is always a state the meeting really passed through, because each write is
/// whole. A shutdown that runs [`SessionStore::flush`] loses nothing at all, so
/// the window belongs to a crash and not to a restart.
///
/// **What is kept in memory.** The record of each meeting that is running, and
/// each one waiting to be written. Everything else is asked for when somebody
/// asks for it, so a server two years into a deployment does not hold two years
/// of meetings.
pub struct SessionStore {
    inner: Arc<Inner>,
    writer: Option<JoinHandle<()>>,
}

/// Everything the writer thread and the callers share.
struct Inner {
    sink: Arc<dyn Sink>,
    /// The most an embedding platform may put on one record's opaque fields.
    /// See [`SessionStore::files`].
    opaque_bytes: usize,
    records: Mutex<Records>,
    /// Woken when there is a record to write, or when the store is closing.
    work: Condvar,
    /// Woken when a pass over the queue has finished, which is what
    /// [`SessionStore::flush`] waits on.
    written: Condvar,
}

/// A record owed to the store, and which edit of it is owed.
struct Queued {
    version: u64,
    session: Session,
    /// How many times writing this has failed. A record with any attempts on
    /// it is what `unwritten` counts and what `/healthz` reports.
    attempts: u32,
    /// When it may next be tried.
    due_at_ms: i64,
}

#[derive(Default)]
struct Records {
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
    ///
    /// This node's own memory of it, and that is enough: the invariant it
    /// protects is about one `sid` appearing in two meetings, and a `sid` is
    /// minted by the process a connection is attached to, so the two meetings
    /// it could appear in are both this node's.
    last_ended_ms: HashMap<String, i64>,
    /// Records owed to the store, by id, newest edit only: a meeting that
    /// changes twice before the writer gets to it is written once.
    queued: HashMap<String, Queued>,
    next_version: u64,
    closing: bool,
}

impl SessionStore {
    /// One JSON file per meeting, in a directory.
    ///
    /// Creating the directory is part of opening it: a server told to record
    /// meetings somewhere that does not exist yet should make the place rather
    /// than refuse to start, and a server that cannot make it should refuse to
    /// start rather than run for a month and be asked for records it never
    /// kept.
    ///
    /// `opaque_bytes` is the most the platform above may put on one record: the
    /// room metadata a meeting is opened with, and each participant's
    /// attributes. It is a bound on a door into this server's storage from
    /// somebody else's code, which is why it exists at all and why it is an
    /// operator's number rather than a constant -- `sessions.max_opaque_bytes`
    /// in the configuration file. A value over it is not truncated: nothing at
    /// all is kept for that field, because half a document is worse than none.
    pub fn files(
        directory: impl Into<std::path::PathBuf>,
        opaque_bytes: usize,
    ) -> io::Result<Self> {
        let loaded = files::Files::open(directory)?;
        let mut records = Records {
            last_ended_ms: loaded.last_ended,
            ..Records::default()
        };

        for session in loaded.closed {
            records.queue(session);
        }

        Ok(Self::around(Arc::new(loaded.files), records, opaque_bytes))
    }

    /// Meetings, recordings and the work queue in Postgres, shared with every
    /// other process pointed at the same database.
    ///
    /// **This does not connect.** A media server that refuses to carry calls
    /// because a database is down is a worse failure than one that carries them
    /// and cannot record: nothing on the media path goes anywhere near this
    /// crate. So a database that is away at startup costs the meetings held
    /// while it is away and not the calls -- the records queue in memory, one
    /// line a minute says the store is not answering, `/healthz` carries
    /// `session_store=postgres:host:5432/database:down` with
    /// `unwritten_sessions=` climbing beside it, and everything queued is
    /// written when it comes back.
    ///
    /// The one thing that does fail here is a url that is not a url. That is a
    /// typo in a config file rather than an outage, and a server that started
    /// anyway would be one that can never record anything and said so once, at
    /// boot, in a line nobody reads again.
    ///
    /// `opaque_bytes` means what it means on [`SessionStore::files`].
    pub fn postgres(settings: PostgresSettings, opaque_bytes: usize) -> Result<Self, String> {
        let database = database::Database::open(settings)?;
        Ok(Self::around(
            Arc::new(database),
            Records::default(),
            opaque_bytes,
        ))
    }

    fn around(sink: Arc<dyn Sink>, records: Records, opaque_bytes: usize) -> Self {
        let inner = Arc::new(Inner {
            sink,
            opaque_bytes,
            records: Mutex::new(records),
            work: Condvar::new(),
            written: Condvar::new(),
        });

        let writing = Arc::clone(&inner);
        let writer = std::thread::Builder::new()
            .name("session-records".to_owned())
            .spawn(move || write_queued(&writing))
            .expect("a thread for the session records");

        Self {
            inner,
            writer: Some(writer),
        }
    }

    /// Where the records are kept, in one line: `files:/var/lib/...` or
    /// `postgres:host:5432/database`, never a password.
    pub fn describe(&self) -> String {
        self.inner.sink.describe()
    }

    /// Whether the store answered the last time anybody asked it anything.
    pub fn healthy(&self) -> bool {
        self.inner.sink.healthy()
    }

    /// How many meetings the store holds.
    pub fn count(&self) -> usize {
        self.inner.sink.count()
    }

    /// How many meetings are running right now on this node.
    pub fn live_count(&self) -> usize {
        self.inner.records.lock().expect("session lock").live.len()
    }

    /// How many records are held in memory in full, rather than being asked
    /// for.
    ///
    /// The bound this server is meant to keep: the meetings running and the
    /// ones waiting to be written, and nothing else however long the history
    /// is.
    pub fn resident(&self) -> usize {
        let records = self.inner.records.lock().expect("session lock");
        let mut ids: HashSet<&str> = HashSet::new();

        ids.extend(records.queued.keys().map(String::as_str));
        ids.extend(records.live.values().map(|session| session.id.as_str()));

        ids.len()
    }

    /// How many records are waiting to be written.
    ///
    /// Zero almost always: it rises while a burst of arrivals outruns the
    /// store and falls again. Staying up is a store that cannot keep pace with
    /// the meetings being held on top of it.
    pub fn owed(&self) -> usize {
        self.inner
            .records
            .lock()
            .expect("session lock")
            .queued
            .len()
    }

    /// How many records this server has failed to write and is still carrying.
    ///
    /// A gauge rather than a tally: a record whose next write succeeds comes
    /// back off it, so a store that hiccuped once is not reported for ever as
    /// a meeting that is going to be lost. It is the alertable number here --
    /// every other one goes up when the server is working.
    pub fn unwritten(&self) -> u64 {
        self.inner
            .records
            .lock()
            .expect("session lock")
            .queued
            .values()
            .filter(|queued| queued.attempts > 0)
            .count() as u64
    }

    /// Waits until everything recorded so far is in the store, or until the
    /// deadline, and says which happened.
    ///
    /// What makes a planned stop lose nothing. It is not a promise about a
    /// crash: a crash is exactly the stop that does not get to call this. The
    /// deadline is not decoration either -- "wait until it lands" against a
    /// database that is down is "wait for ever", and a server that will not
    /// shut down is a worse problem than a few meetings that end a minute
    /// early in the record.
    pub fn flush(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut records = self.inner.records.lock().expect("session lock");

        while !records.queued.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, _) = self
                .inner
                .written
                .wait_timeout(records, left)
                .expect("session lock");
            records = guard;
        }

        true
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
    /// `metadata` is the room's, and it is written onto the record of a meeting
    /// this roster starts and ignored otherwise. It is the platform above's
    /// own data and nothing here reads it -- see [`Session::metadata`] -- and a
    /// meeting already running keeps the metadata it opened with, because a
    /// record of what happened is not rewritten by something that happened
    /// after it.
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
    /// **Nothing here waits on the store.** It takes one lock, edits memory,
    /// and wakes the writer.
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
        metadata: &str,
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
                let session = Session::open(
                    room,
                    roster,
                    now_ms,
                    not_before,
                    metadata,
                    self.inner.opaque_bytes,
                );
                tracing::info!(
                    session = %session.id,
                    %room,
                    humans,
                    floor,
                    "a meeting started"
                );
                records.live.insert(room.to_owned(), session.clone());
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
                        session.admit(member, not_before, self.inner.opaque_bytes);
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

        // Outside the lock, and it only takes an in-memory one of its own: a
        // directory has to know about a meeting before the writer gets to it,
        // or the room's history would not mention the meeting happening in it.
        if let Some(session) = &started {
            self.inner.sink.opened(session);
        }

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

    /// What the store has to do when nothing is happening: say this node is
    /// still holding the meetings it is holding, and take away the meetings of
    /// a node that has stopped saying so.
    ///
    /// The writer thread calls this on its own clock. It is public and takes
    /// the time for the same reason [`SessionStore::expire`] does -- a test has
    /// to be able to be the clock, and a lease that can only be tested by
    /// waiting for one is a lease nobody tests.
    pub fn tick(&self, now_ms: i64) {
        let live: Vec<String> = self
            .inner
            .records
            .lock()
            .expect("session lock")
            .live
            .values()
            .map(|session| session.id.clone())
            .collect();

        self.inner.sink.tick(&live, now_ms);
    }

    /// Says this node is up and holding these rooms.
    ///
    /// Fails on a store that cannot answer for more than one process, which is
    /// the honest answer: a directory has no cluster in it. The caller turns
    /// that into a refusal to start rather than into a cluster of one, because
    /// a deployment that thinks it is clustered and is not corrupts its own
    /// records.
    pub fn publish_node(&self, node: &Node) -> Result<(), StoreError> {
        self.inner.sink.publish_node(node)
    }

    /// Every node in the store, whether or not it is still beating.
    pub fn nodes(&self) -> Result<Vec<Node>, StoreError> {
        self.inner.sink.nodes()
    }

    pub fn forget_node(&self, id: &str) -> Result<(), StoreError> {
        self.inner.sink.forget_node(id)
    }

    /// The meeting running in a room on this node, if one is.
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
    /// service has been meeting in for two years has thousands.
    ///
    /// The meeting happening right now is put at the top of the first page even
    /// if the store has not caught up with it yet, and any meeting on the page
    /// that this node holds a fresher copy of is answered from that copy --
    /// otherwise a backoffice polling for the end of a meeting would watch it
    /// un-end for a moment.
    ///
    /// **Blocking.** It asks the store. A caller on an async runtime has to run
    /// it on a blocking thread, exactly as it would a file read.
    pub fn of_room(&self, room: &str, offset: usize, limit: usize) -> Result<Page, StoreError> {
        let limit = limit.min(MAX_PAGE);
        let listing = self.inner.sink.page(room, offset, limit)?;

        let records = self.inner.records.lock().expect("session lock");
        let mut page = Page {
            sessions: listing
                .entries
                .into_iter()
                .filter_map(|(id, stored)| records.resident(&id).or(stored))
                .collect(),
            total: listing.total,
        };

        if offset == 0
            && let Some(live) = records.live.get(room)
            && !page.sessions.iter().any(|session| session.id == live.id)
        {
            page.sessions.insert(0, live.clone());
            page.sessions.truncate(limit.max(1));
        }

        Ok(page)
    }

    /// One meeting by id, whichever node held it.
    ///
    /// **Blocking**, for the same reason [`SessionStore::of_room`] is.
    pub fn get(&self, id: &str) -> Result<Option<Session>, StoreError> {
        if let Some(session) = self
            .inner
            .records
            .lock()
            .expect("session lock")
            .resident(id)
        {
            return Ok(Some(session));
        }

        self.inner.sink.fetch(id)
    }

    /// Writes down a stretch of recorded audio, and the work that follows it.
    ///
    /// Idempotent on `(meeting, participant, segment)`: a recorder that was
    /// restarted and writes the same segment down again under a new id lands on
    /// the row it already wrote. The id that comes back is the one the store
    /// holds, which is the one to use from then on.
    ///
    /// **Blocking**, and only Postgres does it: see [`SessionStore::files`].
    pub fn note_recording(&self, recording: &Recording, now_ms: i64) -> Result<String, StoreError> {
        self.inner.sink.note_recording(recording, now_ms)
    }

    /// Everything recorded during one meeting, by participant and then by
    /// segment.
    pub fn recordings(&self, session: &str) -> Result<Vec<Recording>, StoreError> {
        self.inner.sink.recordings(session)
    }

    /// What is happening to one recording: two rows, one per stage.
    pub fn work(&self, recording: &str) -> Result<Vec<Work>, StoreError> {
        self.inner.sink.work(recording)
    }

    /// The same for every recording of one meeting, in one query rather than
    /// one per recording -- a ten-person meeting has at least ten recordings,
    /// and a backoffice opening it should cost two round trips and not eleven.
    pub fn session_work(&self, session: &str) -> Result<Vec<Work>, StoreError> {
        self.inner.sink.session_work(session)
    }

    /// Takes work out of the queue for this node.
    ///
    /// Several processes poll this and no two of them get the same row; a claim
    /// that stops being renewed goes back into the queue. See the
    /// implementation on the database for how, and the tests in
    /// `tests/postgres.rs` for the proof against a real one.
    pub fn claim(&self, stage: Stage, now_ms: i64, limit: usize) -> Result<Vec<Claim>, StoreError> {
        self.inner.sink.claim(stage, now_ms, limit)
    }

    /// Says this node is still working on a claim it holds. `false` means the
    /// claim has already gone to somebody else, and whatever is being done
    /// should stop.
    pub fn renew(&self, recording: &str, stage: Stage, now_ms: i64) -> Result<bool, StoreError> {
        self.inner.sink.renew(recording, stage, now_ms)
    }

    /// The file is in the object store. This finishes the upload and makes the
    /// announcement possible, in one transaction, so there is no moment in
    /// which a file is up and nothing is going to say so.
    ///
    /// It does not check that the caller still holds the claim, and that is
    /// deliberate: what it records is that the file is in the object store,
    /// which is true whoever put it there. A claimant whose lease ran out while
    /// it was finishing writes down something correct rather than losing the
    /// upload it just did.
    pub fn uploaded(
        &self,
        recording: &str,
        object_key: &str,
        bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.inner
            .sink
            .uploaded(recording, object_key, bytes, now_ms)
    }

    /// Somebody has been told the recording is ready.
    pub fn announced(&self, recording: &str, now_ms: i64) -> Result<(), StoreError> {
        self.inner.sink.announced(recording, now_ms)
    }

    /// Gives a claim back with a reason and a time to try again. Work that has
    /// used up its attempts is left alone instead.
    ///
    /// **Only the node still holding the claim can give it back**, exactly as
    /// only the holder can renew it. A claimant whose lease ran out while it
    /// was working is not holding anything any more: by the time it comes back
    /// to say it failed, the row may have been claimed, finished and marked
    /// done by somebody else, and an unconditional put-back would undo that --
    /// a `done` announcement returned to the queue and emitted a second time,
    /// or a live claimant's lease cleared out from under it while it is still
    /// uploading. `false` means the claim had already gone and nothing was
    /// changed.
    pub fn postpone(
        &self,
        recording: &str,
        stage: Stage,
        error: &str,
        due_at_ms: i64,
    ) -> Result<bool, StoreError> {
        self.inner.sink.postpone(recording, stage, error, due_at_ms)
    }

    /// Recording work nobody is going to move: given up on, or overdue by so
    /// far that whoever it is waiting for is not coming.
    ///
    /// On `/healthz`, and the one number that makes a stranded upload visible.
    /// It is refreshed on the store's own slow clock, not on demand.
    pub fn stuck(&self) -> u64 {
        self.inner.sink.stuck()
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

impl Records {
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

    /// The copy of a record this node is carrying, if it is carrying one.
    ///
    /// A record that is owed to the store still answers, or a backoffice
    /// polling for the end of a meeting would see it un-end for a moment.
    fn resident(&self, id: &str) -> Option<Session> {
        if let Some(queued) = self.queued.get(id) {
            return Some(queued.session.clone());
        }

        self.live.values().find(|session| session.id == id).cloned()
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

    /// Owes this record to the store, replacing whatever edit of it was owed.
    ///
    /// The attempts are reset with the edit: this is a different record from
    /// the one that failed, and making it wait out the old one's backoff would
    /// mean a meeting that ends during an outage takes a further half minute to
    /// be written down after the store comes back.
    fn queue(&mut self, session: Session) {
        self.next_version += 1;
        let version = self.next_version;

        self.queued.insert(
            session.id.clone(),
            Queued {
                version,
                session,
                attempts: 0,
                due_at_ms: i64::MIN,
            },
        );
    }

    /// Whether anything is due to be written now.
    fn has_due(&self, now_ms: i64) -> bool {
        self.queued
            .values()
            .any(|queued| now_ms >= queued.due_at_ms)
    }
}

/// The writer: the one place in this crate that waits for a store.
///
/// Nothing here holds the records lock across a write. It takes the lock to
/// pick up what is owed, drops it, writes, and takes it again to strike off
/// what it wrote -- which is what keeps a busy store out of the way of somebody
/// joining a room. A record is left in the queue until it is written, so a
/// reader is never told a meeting has no record because it is mid-flight.
fn write_queued(inner: &Arc<Inner>) {
    let tick_every = inner.sink.tick_every();
    // Due immediately, so a store that has to connect to say it is healthy does
    // so at boot rather than a heartbeat later -- `/healthz` saying `down` for
    // the first ten seconds of every process is a graph nobody can read.
    let mut ticked = Instant::now()
        .checked_sub(tick_every)
        .unwrap_or_else(Instant::now);

    loop {
        let closing = {
            let mut records = inner.records.lock().expect("session lock");
            if !records.closing && !records.has_due(now_ms()) {
                let (guard, _) = inner
                    .work
                    .wait_timeout(records, WRITER_TICK)
                    .expect("session lock");
                records = guard;
            }
            records.closing
        };

        let batch: Vec<(u64, Session)> = {
            let records = inner.records.lock().expect("session lock");
            let now = now_ms();
            records
                .queued
                .values()
                // On the way out, everything goes, whatever it is waiting for.
                // This is the last chance any of it has.
                .filter(|queued| closing || now >= queued.due_at_ms)
                .map(|queued| (queued.version, queued.session.clone()))
                .collect()
        };

        if !batch.is_empty() {
            let written: Vec<(u64, Session, Result<(), StoreError>)> = batch
                .into_iter()
                .map(|(version, session)| {
                    let outcome = inner.sink.persist(&session, now_ms());
                    (version, session, outcome)
                })
                .collect();

            settle(inner, written);
            inner.written.notify_all();
        }

        if closing {
            let left = inner.records.lock().expect("session lock").queued.len();
            if left > 0 {
                tracing::error!(
                    sessions = left,
                    store = %inner.sink.describe(),
                    "this server is stopping and could not write these meetings down; they are \
                     lost"
                );
            }
            return;
        }

        if ticked.elapsed() >= tick_every {
            ticked = Instant::now();
            let live: Vec<String> = inner
                .records
                .lock()
                .expect("session lock")
                .live
                .values()
                .map(|session| session.id.clone())
                .collect();
            inner.sink.tick(&live, now_ms());
        }
    }
}

/// Strikes off what was written and puts back what was not.
fn settle(inner: &Arc<Inner>, written: Vec<(u64, Session, Result<(), StoreError>)>) {
    let mut records = inner.records.lock().expect("session lock");
    let now = now_ms();

    for (version, session, outcome) in written {
        // Only if nothing has edited the record since: a meeting that moved on
        // while its older self was being written is still owed to the store.
        let Some(queued) = records.queued.get_mut(&session.id) else {
            continue;
        };
        if queued.version != version {
            continue;
        }

        match outcome {
            Ok(()) => {
                records.queued.remove(&session.id);
            }
            // Logged, counted, and the meeting carries on. Nobody's call is
            // failed because a store was: the record stays here, keeps
            // answering reads, and is tried again -- and the number of records
            // in this state is on `/healthz` as the thing to alert on.
            Err(error) => {
                let wait = RETRY_AFTER
                    .saturating_mul(1 << queued.attempts.min(5))
                    .min(RETRY_AT_MOST);
                queued.attempts = queued.attempts.saturating_add(1);
                queued.due_at_ms = now + wait.as_millis() as i64;

                let attempts = queued.attempts;
                if attempts == 1 {
                    tracing::error!(
                        session = %session.id,
                        room = %session.room,
                        store = %inner.sink.describe(),
                        %error,
                        "a meeting record could not be written; it is kept in memory and tried \
                         again, and it will not survive a restart until it lands"
                    );
                }
            }
        }
    }
}

/// The room metadata a record keeps, or nothing at all.
///
/// **Never truncated.** Half a document is worse than none: a reader cannot
/// tell the half from the whole, so everything downstream would have to treat
/// every value as possibly cut. An oversized one is dropped instead and said
/// so, once, as the meeting opens -- which is the moment an operator can still
/// connect the warning to the platform that sent it.
fn kept_metadata(metadata: &str, opaque_bytes: usize, session: &str) -> Option<String> {
    if metadata.is_empty() {
        return None;
    }

    if metadata.len() > opaque_bytes {
        tracing::warn!(
            %session,
            bytes = metadata.len(),
            limit = opaque_bytes,
            "the room's metadata is larger than sessions.max_opaque_bytes, so none of it is kept \
             on this meeting's record; raise the limit or put less in it"
        );
        return None;
    }

    Some(metadata.to_owned())
}

/// The same rule for one participant's attributes.
///
/// Measured as the JSON the store will hold rather than as the map, so that the
/// number an operator sets is the number of bytes that can reach a column.
fn kept_attributes(
    attributes: &HashMap<String, String>,
    opaque_bytes: usize,
    session: &str,
    sid: &str,
) -> HashMap<String, String> {
    if attributes.is_empty() {
        return HashMap::new();
    }

    let size = serde_json::to_string(attributes).map_or(usize::MAX, |json| json.len());
    if size > opaque_bytes {
        tracing::warn!(
            %session,
            participant = %sid,
            bytes = size,
            limit = opaque_bytes,
            "this participant's attributes are larger than sessions.max_opaque_bytes, so none of \
             them are kept on this meeting's record; raise the limit or put less in them"
        );
        return HashMap::new();
    }

    attributes.clone()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

fn generate_id() -> String {
    format!(
        "{SESSION_ID_PREFIX}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
