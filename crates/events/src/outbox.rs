//! Write it down, deliver it, and only then forget it.
//!
//! This is the part that is easy to skip and the reason the rest is worth
//! building. Without it, a broker or an endpoint that is away for two minutes
//! costs every meeting in those two minutes, nothing retries them because
//! nothing remembers them, and no line anywhere says so. With it, an event is
//! on a disk before the first attempt is made and is struck off only once
//! every destination has taken it -- so an outage costs latency instead of
//! events, and a restart in the middle of one costs nothing at all.
//!
//! One JSON file per undelivered event, in the same shape and for the same
//! reasons as a session record: self describing, readable with `cat`, and
//! removable with `rm` by an operator who has decided that an event from last
//! Tuesday is never going to be wanted. A record names the destinations that
//! still owe it, so a broker that was down does not cause a redelivery to the
//! webhook endpoint that already took it.
//!
//! **Nothing here sits on the hot path.** Every method blocks -- it writes and
//! flushes a file -- and the only caller is [`crate::Delivery`], which calls
//! them from `spawn_blocking` so a flush lands on the blocking pool rather
//! than on a runtime thread that is also carrying somebody's media.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::Event;
use crate::log::quoted;

/// The suffix a record wears while it is being written.
///
/// Written to this name and renamed onto its own, so a reader -- the next
/// start of this server included -- never sees half a file. The loader skips
/// anything that is not `<name>.json`, which is what keeps a temporary file
/// left behind by a crash from being read back as an event.
const PENDING_SUFFIX: &str = ".writing";

/// One undelivered event, as it sits on the disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    /// The order it was emitted in. A restart delivers what it finds in this
    /// order, so a `participant_left` cannot arrive in front of the
    /// `participant_joined` it followed.
    seq: u64,
    /// The destinations that have not taken it yet, by name.
    pending: Vec<String>,
    event: Event,
}

/// One event on its way back out after a restart.
pub struct Restored {
    pub seq: u64,
    pub event: Arc<Event>,
    /// The destinations that still owe it, filtered down to the ones this
    /// server still has.
    pub pending: Vec<String>,
}

/// What is still owed for one event, kept beside the file rather than read
/// back out of it.
struct Entry {
    event: Arc<Event>,
    pending: BTreeSet<String>,
}

/// Every event this server has promised to deliver and not yet delivered.
///
/// **What a restart does to what is in it.** Nothing is lost: the records are
/// on a disk, and the next start reads them, puts them back in the order they
/// were emitted in and hands them to the destinations that still owe them. An
/// event may be delivered twice -- once before the process died and again
/// after, if it died between the endpoint answering and the record being
/// struck off -- which is why every event carries an `id` that never changes,
/// LiveKit's own field, for a consumer that wants to throw the second one
/// away. At least once, and never at most once: this crate would rather send
/// a duplicate than promise something it did not send.
///
/// **What happens when it grows.** It cannot grow without bound. A record is
/// written when an event enters a destination's queue and struck off when it
/// leaves, and those queues are bounded, so the directory holds at most
/// `queue depth × destinations` files -- a few megabytes at the depth this
/// server ships with. When a queue is full the event is refused *before* it is
/// recorded and counted as dropped instead, which is a trade made on purpose:
/// a destination ten thousand events behind is not going to catch up, and
/// filling a disk on its behalf would take the whole server down with it. The
/// record of the meeting is still there and still answers, which is the point
/// of it being there.
pub struct Outbox {
    directory: PathBuf,
    entries: Mutex<HashMap<u64, Entry>>,
    /// What the last process left behind, from `open` until `restore` takes
    /// it. Held rather than returned from `open`, because the destinations it
    /// has to be filtered against are not built yet at that point.
    restorable: Mutex<Vec<Record>>,
    next_seq: AtomicU64,
    /// Records that could not be written. Delivery is attempted anyway -- a
    /// full disk must not stop an event reaching a consumer that is up -- so
    /// this counts exactly one thing: events that will not survive a restart.
    unwritten: AtomicU64,
}

impl Outbox {
    /// Opens the directory, reading back everything the last process still
    /// owed.
    ///
    /// Creating the directory is part of opening it, and a server that cannot
    /// create it refuses to start: the alternative is a deployment that
    /// believes it has an outbox, loses its broker for an afternoon, and finds
    /// out afterwards that there never was one.
    pub fn open(directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;

        let mut records = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if !is_record(&path) {
                continue;
            }

            match read_record(&path) {
                Ok(record) => records.push(record),
                // Kept, not deleted, and not fatal. One unreadable file is one
                // event nobody will receive; refusing to start over it takes
                // every other event down with it, and deleting it destroys
                // whatever is still legible in there.
                Err(error) => tracing::error!(
                    path = %path.display(),
                    %error,
                    "an outbox record could not be read and was left alone; that event will not \
                     be delivered"
                ),
            }
        }

        records.sort_by_key(|record| record.seq);
        let next_seq = records.last().map_or(0, |record| record.seq + 1);

        tracing::info!(
            directory = %directory.display(),
            owed = records.len(),
            "the event outbox is open"
        );

        Ok(Self {
            directory,
            entries: Mutex::new(HashMap::new()),
            restorable: Mutex::new(records),
            next_seq: AtomicU64::new(next_seq),
            unwritten: AtomicU64::new(0),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// How many events are owed to somebody right now.
    pub fn owed(&self) -> usize {
        self.entries.lock().expect("outbox lock").len()
    }

    /// How many records this server has failed to write.
    pub fn unwritten(&self) -> u64 {
        self.unwritten.load(Ordering::Relaxed)
    }

    /// Promises an event to a set of destinations, and returns the sequence
    /// number it was filed under.
    ///
    /// The write happens before the event is handed to anybody, which is the
    /// ordering this module exists to enforce: an event being attempted is an
    /// event already written down.
    ///
    /// Destinations are named, and two of them may not share a name. The set
    /// below is what a record owes; two destinations under one name are one
    /// entry in it, so the first of the two to settle empties the set and
    /// deletes the record while the second has not been offered the event at
    /// all -- and a crash there loses it for that one with no counter and no
    /// line. `EventConfig::validate` refuses a duplicated webhook url, which
    /// is the only way two destinations here could ever be called the same
    /// thing; a broker's name carries a `kafka:` prefix a url cannot have.
    pub fn record(&self, event: &Arc<Event>, destinations: &[String]) -> u64 {
        debug_assert_eq!(
            destinations.len(),
            destinations.iter().collect::<BTreeSet<&String>>().len(),
            "two destinations share a name, so one of them will be handed events the other has \
             already struck off"
        );

        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let pending: BTreeSet<String> = destinations.iter().cloned().collect();

        self.entries.lock().expect("outbox lock").insert(
            seq,
            Entry {
                event: Arc::clone(event),
                pending: pending.clone(),
            },
        );
        self.write(seq, event, &pending);

        seq
    }

    /// One destination has finished with an event: delivered, or given up on.
    ///
    /// Given up on counts, and it has to. A record kept for a destination that
    /// has already abandoned it would be handed back to that destination by
    /// every restart from now until somebody deleted the file by hand.
    pub fn settle(&self, seq: u64, destination: &str) {
        let (event, pending) = {
            let mut entries = self.entries.lock().expect("outbox lock");
            let Some(entry) = entries.get_mut(&seq) else {
                return;
            };
            entry.pending.remove(destination);

            if entry.pending.is_empty() {
                let entry = entries.remove(&seq).expect("it was there a line ago");
                (entry.event, BTreeSet::new())
            } else {
                (Arc::clone(&entry.event), entry.pending.clone())
            }
        };

        if pending.is_empty() {
            self.forget(seq, &event.id);
        } else {
            self.write(seq, &event, &pending);
        }
    }

    /// Everything the last process promised and did not deliver, oldest first.
    ///
    /// `known` is the destinations this server has now. A record naming one it
    /// no longer has -- an endpoint an operator removed from the config -- has
    /// that name struck off rather than kept, because nothing will ever settle
    /// it and it would otherwise be read back on every start for ever.
    pub fn restore(&self, known: &[String]) -> Vec<Restored> {
        let records = std::mem::take(&mut *self.restorable.lock().expect("outbox lock"));
        let mut restored = Vec::new();

        for record in records {
            let event = Arc::new(record.event);
            let pending: Vec<String> = record
                .pending
                .into_iter()
                .filter(|destination| known.iter().any(|name| name == destination))
                .collect();

            if pending.is_empty() {
                tracing::info!(
                    seq = record.seq,
                    event = %event.id,
                    "an outbox record was owed only to destinations this server no longer has; \
                     it is struck off rather than read back on every start from now on"
                );
                self.forget(record.seq, &event.id);
                continue;
            }

            self.entries.lock().expect("outbox lock").insert(
                record.seq,
                Entry {
                    event: Arc::clone(&event),
                    pending: pending.iter().cloned().collect(),
                },
            );
            restored.push(Restored {
                seq: record.seq,
                event,
                pending,
            });
        }

        if !restored.is_empty() {
            tracing::warn!(
                events = restored.len(),
                "events the last process promised and did not deliver are going out now; a \
                 consumer that took one of them before the process died will see it twice, and \
                 the event id is the same both times"
            );
        }

        restored
    }

    fn path(&self, seq: u64, id: &str) -> PathBuf {
        // Zero padded, so `ls` lists the directory in the order the events
        // happened rather than putting event 10 in front of event 9.
        self.directory.join(format!("{seq:012}-{id}.json"))
    }

    /// Writes a record to its own file, replacing whatever was there.
    ///
    /// A failure is logged and counted, and the delivery carries on. Nobody's
    /// call is failed because a disk was full; the consequence is that this
    /// event will not survive the process, which is what the counter on
    /// `/healthz` is for.
    fn write(&self, seq: u64, event: &Arc<Event>, pending: &BTreeSet<String>) {
        let record = Record {
            seq,
            pending: pending.iter().cloned().collect(),
            event: (**event).clone(),
        };

        if let Err(error) = write_record(&self.path(seq, &event.id), &record) {
            self.unwritten.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                event = %event.id,
                kind = %event.kind,
                room = %quoted(event.room_name()),
                directory = %self.directory.display(),
                %error,
                "an outbox record could not be written; this event will not survive a restart"
            );
        }
    }

    fn forget(&self, seq: u64, id: &str) {
        let path = self.path(seq, id);

        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::error!(
                path = %path.display(),
                %error,
                "an event that everybody has taken could not be struck off the outbox; it will \
                 be delivered again after a restart"
            );
        }
    }
}

fn is_record(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == "json")
}

fn read_record(path: &Path) -> io::Result<Record> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn write_record(path: &Path, record: &Record) -> io::Result<()> {
    use std::io::Write as _;

    let body = serde_json::to_vec(record).map_err(io::Error::other)?;
    let pending_path = path.with_extension(format!("json{PENDING_SUFFIX}"));

    let mut file = std::fs::File::create(&pending_path)?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    // Flushed before the rename, so the record is either the old one or the
    // new one and never the first half of the new one. What is not done here
    // is flushing the directory itself: a power cut can still lose the rename,
    // and the cost of not losing it is a second flush on every event.
    file.sync_all()?;
    drop(file);

    std::fs::rename(&pending_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediacore_proto::mediacore;

    fn directory() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("a directory for the outbox")
    }

    fn event(name: &str) -> Arc<Event> {
        Arc::new(Event::room_started(mediacore::Room {
            sid: "RM_000000000000".to_owned(),
            name: name.to_owned(),
            ..Default::default()
        }))
    }

    fn names(of: &[&str]) -> Vec<String> {
        of.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The failure this module exists to prevent, as a test: a destination
    /// that was away when the event happened and a process that stopped while
    /// it was still away.
    #[test]
    fn an_event_nobody_took_is_still_owed_after_a_restart() {
        let directory = directory();
        let promised = event("standup");

        {
            let outbox = Outbox::open(directory.path()).expect("an empty outbox opens");
            outbox.record(&promised, &names(&["webhook"]));
            assert_eq!(outbox.owed(), 1);
        }

        let outbox = Outbox::open(directory.path()).expect("the outbox reopens");
        let restored = outbox.restore(&names(&["webhook"]));

        assert_eq!(
            restored.len(),
            1,
            "an event promised to a destination that was down was forgotten when the process \
             stopped, which is the outage nobody ever finds out about"
        );
        assert_eq!(restored[0].event.id, promised.id);
        assert_eq!(restored[0].event.room_name(), "standup");
        assert_eq!(restored[0].pending, names(&["webhook"]));
    }

    #[test]
    fn an_event_everybody_took_is_gone() {
        let directory = directory();
        let outbox = Outbox::open(directory.path()).unwrap();

        let seq = outbox.record(&event("standup"), &names(&["webhook", "kafka"]));
        outbox.settle(seq, "webhook");
        assert_eq!(
            outbox.owed(),
            1,
            "an event was struck off while a destination still owed it"
        );

        outbox.settle(seq, "kafka");
        assert_eq!(outbox.owed(), 0);

        let reopened = Outbox::open(directory.path()).unwrap();
        assert!(
            reopened.restore(&names(&["webhook", "kafka"])).is_empty(),
            "a delivered event was left on the disk, so every restart sends it again for ever"
        );
    }

    /// The reason a record names its destinations one by one. A broker that
    /// was down for an hour must not cause an hour of redelivery to the
    /// webhook endpoint that took everything the first time.
    #[test]
    fn a_restart_owes_an_event_only_to_whoever_had_not_taken_it() {
        let directory = directory();

        {
            let outbox = Outbox::open(directory.path()).unwrap();
            let seq = outbox.record(&event("standup"), &names(&["webhook", "kafka"]));
            outbox.settle(seq, "webhook");
        }

        let restored = Outbox::open(directory.path())
            .unwrap()
            .restore(&names(&["webhook", "kafka"]));

        assert_eq!(restored.len(), 1);
        assert_eq!(
            restored[0].pending,
            names(&["kafka"]),
            "the endpoint that already answered 200 is being sent the event a second time"
        );
    }

    #[test]
    fn what_is_owed_comes_back_in_the_order_it_happened() {
        let directory = directory();

        {
            let outbox = Outbox::open(directory.path()).unwrap();
            for index in 0..12 {
                outbox.record(&event(&format!("room-{index}")), &names(&["webhook"]));
            }
        }

        let restored = Outbox::open(directory.path())
            .unwrap()
            .restore(&names(&["webhook"]));

        assert_eq!(
            restored
                .iter()
                .map(|owed| owed.event.room_name().to_owned())
                .collect::<Vec<String>>(),
            (0..12)
                .map(|index| format!("room-{index}"))
                .collect::<Vec<String>>(),
            "the directory was read back in whatever order the filesystem listed it, so a \
             participant_left can arrive before the participant_joined it followed"
        );
    }

    /// A restart must not start numbering again from zero on top of records
    /// that are still there, or two events share a file and one of them is
    /// quietly overwritten.
    #[test]
    fn a_restart_carries_on_numbering_where_the_last_one_stopped() {
        let directory = directory();

        {
            let outbox = Outbox::open(directory.path()).unwrap();
            outbox.record(&event("standup"), &names(&["webhook"]));
            outbox.record(&event("payroll"), &names(&["webhook"]));
        }

        let outbox = Outbox::open(directory.path()).unwrap();
        outbox.restore(&names(&["webhook"]));
        let seq = outbox.record(&event("retro"), &names(&["webhook"]));

        assert_eq!(seq, 2, "the new event was filed on top of an old one");
        assert_eq!(outbox.owed(), 3);
    }

    /// An operator who removes an endpoint from the config has removed it. A
    /// record still naming it would be read back on every start, for ever,
    /// and nothing would ever settle it.
    #[test]
    fn a_destination_that_no_longer_exists_is_struck_off() {
        let directory = directory();

        {
            let outbox = Outbox::open(directory.path()).unwrap();
            outbox.record(&event("standup"), &names(&["retired-endpoint"]));
        }

        {
            let outbox = Outbox::open(directory.path()).unwrap();
            assert!(outbox.restore(&names(&["webhook"])).is_empty());
        }

        let outbox = Outbox::open(directory.path()).unwrap();
        assert!(
            outbox.restore(&names(&["webhook"])).is_empty(),
            "a record owed to nobody survived a start, so it is read back on every start from now on"
        );
        assert_eq!(outbox.owed(), 0);
    }

    /// One unreadable file is one event nobody receives. Refusing to open the
    /// outbox over it would take every other event down with it, and the
    /// server with them.
    #[test]
    fn a_file_that_is_not_a_record_costs_one_event_and_not_the_server() {
        let directory = directory();

        {
            let outbox = Outbox::open(directory.path()).unwrap();
            outbox.record(&event("standup"), &names(&["webhook"]));
        }
        std::fs::write(
            directory.path().join("000000000009-EV_x.json"),
            b"{ not json",
        )
        .unwrap();
        std::fs::write(directory.path().join("notes.txt"), b"an operator's note").unwrap();
        std::fs::write(
            directory.path().join("000000000010-EV_y.json.writing"),
            b"{ half",
        )
        .unwrap();

        let outbox = Outbox::open(directory.path())
            .expect("one unreadable file took every other event on this server with it");

        assert_eq!(
            outbox.restore(&names(&["webhook"])).len(),
            1,
            "a file that is not a record was read back as an event"
        );
    }
}
