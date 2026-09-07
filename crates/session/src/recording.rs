//! What was recorded, where it is, and what still has to happen to it.
//!
//! Nothing in this server records anything yet. This is the shape the recorder
//! fills: a row per stretch of one participant's audio, pointing at a file, and
//! beside it the two pieces of work that turn that file into something the rest
//! of the platform can use -- put it in the object store, then say it is there.
//!
//! **Rows point at files. Audio is never in the database.** A minute of Opus is
//! a few hundred kilobytes and an hour of a ten-person meeting is a few hundred
//! megabytes; a database that carried them would be restored, replicated and
//! vacuumed as if that were state, and it is not -- it is a file with a name.
//! What is in the database is the name, the shape and the state of the work.
//!
//! **Why the work is a table and not a channel.** The owner runs three server
//! processes. An upload that lives in the memory of the process that recorded
//! it dies with that process, and a recording nobody uploaded is a meeting that
//! quietly did not happen. So the work is a row several processes poll, one of
//! them claims it, and a claim that stops being renewed comes back -- see
//! [`crate::SessionStore::claim`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The prefix every recording id carries, beside `RM_` for a room, `PA_` for a
/// participant and `SE_` for a meeting.
pub const RECORDING_ID_PREFIX: &str = "RE_";

/// One stretch of one participant's audio, and the file it is in.
///
/// Per stretch, not per participant, for the same reason attendance is per
/// stretch: somebody who drops out and comes back produces two files, and a
/// recorder that rolls a file over at a size limit produces several. `segment`
/// numbers them within one participant's time in one meeting, from zero, so
/// that `(meeting, participant, segment)` names a file exactly once however
/// many times the recorder is asked to write it down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recording {
    pub id: String,
    /// The meeting this belongs to, never the room. A room holds another
    /// meeting tomorrow and this recording is not of that one.
    pub meeting: String,
    /// The participant's `sid`, which is what the attendance list is keyed by
    /// as well, so a recording can be lined up with the stretch it was made
    /// during.
    pub participant: String,
    /// The platform account behind that participant, copied off the attendance
    /// row at the moment the recording started.
    ///
    /// Copied rather than joined because it is the answer to "whose audio is
    /// this" and that answer must survive a meeting record being pruned. `None`
    /// when the token carried no account -- a recorder, an agent, or a guest.
    pub account_id: Option<String>,
    /// Which file of this participant's, from zero.
    pub segment: u32,
    /// Where the file is on the node that recorded it.
    ///
    /// Local to one machine, which is exactly why the upload is a queue: the
    /// node holding the file is the only one that can put it in the object
    /// store, and a claim by anybody else would find nothing there.
    pub local_path: PathBuf,
    /// Which node that is.
    ///
    /// **This is what keeps an upload with the disk that holds the file.**
    /// `local_path` is meaningless anywhere else, so the upload stage of the
    /// queue is offered only to the node named here; the announcement, which
    /// needs no file, is offered to anybody. `None` means nobody has claimed to
    /// hold it -- a row written by hand -- and is offered to anybody, which
    /// ends in an honest "the file is not here" rather than in silence.
    ///
    /// It is whoever last wrote the row down, not whoever first did: a node
    /// that comes back from a crash and finds these files on its disk writes
    /// its own id here, so a volume that outlived the process -- or was
    /// re-attached to a machine with a new name -- is adopted rather than
    /// stranded.
    #[serde(default)]
    pub node: Option<String>,
    /// Where it went in the object store, once it went. `None` until then.
    pub object_key: Option<String>,
    /// How large the file is, `None` while it is still being written.
    pub bytes: Option<u64>,
    pub sample_rate: u32,
    pub channels: u16,
    /// When the recording started and stopped, on the same clock as the
    /// meeting: milliseconds since the epoch. `ended_at_ms` is `None` while the
    /// file is still being written, and an upload is not offered to anybody
    /// until it is set.
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    /// Where this file starts inside the meeting, in milliseconds from the
    /// moment the meeting did.
    ///
    /// The absolute times above answer "when was this said"; this answers
    /// "how far into the recording of that meeting is it", which is what lines
    /// several participants' files up against one another without every reader
    /// having to fetch the meeting to subtract its start.
    pub offset_ms: i64,
    /// How much audio the file holds.
    ///
    /// Not `ended_at_ms - started_at_ms`: a recorder that drops silence writes
    /// a file shorter than the stretch it covers, and the difference is the
    /// thing anybody aligning a transcript needs to know about. `None` until
    /// the file is closed.
    pub duration_ms: Option<i64>,
}

impl Recording {
    /// Whether the file is finished being written.
    ///
    /// The upload waits on this: a half-written file put in the object store is
    /// worse than one that is late, because nothing downstream can tell it is
    /// half a file.
    pub fn is_complete(&self) -> bool {
        self.ended_at_ms.is_some()
    }

    /// Where this file ends inside the meeting, when its length is known.
    pub fn ends_offset_ms(&self) -> Option<i64> {
        self.duration_ms.map(|duration| self.offset_ms + duration)
    }
}

/// A new recording id, in the shape of the other ids on this server.
pub fn recording_id() -> String {
    format!(
        "{RECORDING_ID_PREFIX}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

/// The two things that have to happen to a finished file, in order.
///
/// They are separate rows rather than one column with more states because they
/// fail differently and are retried differently: an object store that is full
/// and a consumer that is not listening are two outages, and a recording that
/// went up but was not announced must not be uploaded again to get the
/// announcement retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Put the file in the object store.
    Upload,
    /// Say that it is there. What "say" means is not this crate's business --
    /// mediacore records, uploads and announces, and whoever is listening
    /// decides what to do about it.
    Announce,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Announce => "announce",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "upload" => Some(Self::Upload),
            "announce" => Some(Self::Announce),
            _ => None,
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where one piece of work has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not yet possible: the file is still being written, or the upload it
    /// follows has not finished. Nobody is offered a blocked row, so a poller
    /// never has to know why it could not do it.
    Blocked,
    /// Due, and the next poller to ask gets it.
    Waiting,
    /// Somebody is doing it. The claim expires; see [`Work::claim_ends_at_ms`].
    Claimed,
    /// Done, at [`Work::done_at_ms`].
    Done,
    /// Tried as often as this deployment allows and never finished. Nothing
    /// picks it up again; `last_error` says what went wrong the last time, and
    /// an operator who has fixed the cause moves it back by hand.
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Waiting => "waiting",
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "blocked" => Some(Self::Blocked),
            "waiting" => Some(Self::Waiting),
            "claimed" => Some(Self::Claimed),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One row of the work queue, as anybody looking at it sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Work {
    pub recording: String,
    pub stage: Stage,
    pub state: State,
    /// How many times this has been handed to somebody. Counted on the claim
    /// and not on the result, which is the whole reason a claimant that dies
    /// cannot be retried for ever: the attempt is spent the moment the work
    /// leaves, whether or not anybody comes back to say what happened.
    pub attempts: u32,
    /// When it may next be claimed.
    pub due_at_ms: i64,
    /// Who holds it, by node id, while it is claimed.
    pub claimed_by: Option<String>,
    /// When that claim stops being believed. A claimant that is still working
    /// renews it; one that has died does not, and the row goes back into the
    /// queue when this passes.
    pub claim_ends_at_ms: Option<i64>,
    /// When it was finished -- when the file went up, or when it was
    /// announced.
    pub done_at_ms: Option<i64>,
    /// What went wrong the last time somebody tried.
    pub last_error: Option<String>,
}

/// One piece of work, handed to the poller that claimed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub recording: Recording,
    pub stage: Stage,
    /// Including this one.
    pub attempts: u32,
    /// When this claim stops being believed. Finish or renew before it, or
    /// somebody else will pick the work up while you are still doing it.
    pub ends_at_ms: i64,
}
