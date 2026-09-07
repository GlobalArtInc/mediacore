//! Meetings kept as one JSON file each, in a directory.
//!
//! The store this server started with, and still the right one for a
//! deployment that runs a single process: it needs nothing installed, an
//! operator reads a meeting with `cat`, and a retention policy is
//! `find -mtime +90 -delete`. What it cannot do is be shared. Three processes
//! pointed at one directory would each hold their own index of it, overwrite
//! each other's records and never see each other's meetings, and a node that
//! died would take its unfinished meetings out of reach of the two that were
//! still up -- which is why [`crate::Database`] exists and why a deployment
//! with more than one server process has to use it.
//!
//! It cannot do recordings either. The upload queue is a thing several
//! processes poll, and a queue for one process is a different and much simpler
//! object; implementing a second one here would mean shipping an
//! implementation of the interesting part that nobody has ever run. So the
//! recording methods refuse, by name, and a deployment that wants recordings
//! names a database.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;

use crate::{Ending, Listing, Session, Sink, StoreError, is_id};

/// The suffix a record wears while it is being written.
///
/// A record is written to this name and then renamed onto its own, so a reader
/// -- the next start of this server included -- never sees half a file. The
/// loader skips anything that is not `<id>.json`, which is what keeps a
/// temporary file left behind by a crash from being read back as a meeting.
const PENDING_SUFFIX: &str = ".writing";

/// Where a record's file is and what the index knows about it without opening
/// it.
struct Indexed {
    started_at_ms: i64,
    /// The file this record came out of, when that is not `<id>.json`.
    ///
    /// The two only differ because a record was copied about by hand. The id
    /// inside is the one everything else refers to it by, so the record is
    /// filed under that -- and the name it is actually in has to be kept, or
    /// the record could be listed and never read.
    file: Option<Box<str>>,
}

impl Indexed {
    fn path(&self, directory: &Path, id: &str) -> PathBuf {
        match &self.file {
            Some(file) => directory.join(file.as_ref()),
            None => directory.join(format!("{id}.json")),
        }
    }
}

#[derive(Default)]
struct Index {
    /// Every meeting this server can be asked about, by id.
    by_id: HashMap<String, Indexed>,
    /// Session ids per room, oldest first. A created room holds many meetings
    /// and this is the order they happened in.
    by_room: HashMap<String, Vec<String>>,
}

impl Index {
    /// Files a record under the id inside it, wherever the file happens to be.
    fn remember(&mut self, id: &str, room: &str, started_at_ms: i64, file: Option<Box<str>>) {
        if self.by_id.contains_key(id) {
            return;
        }

        self.by_room
            .entry(room.to_owned())
            .or_default()
            .push(id.to_owned());
        self.by_id.insert(
            id.to_owned(),
            Indexed {
                started_at_ms,
                file,
            },
        );
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
            .by_id
            .iter()
            .map(|(id, indexed)| (id.clone(), indexed.started_at_ms))
            .collect();

        for ids in self.by_room.values_mut() {
            ids.sort_by_key(|id| (started.get(id).copied().unwrap_or(0), id.clone()));
        }
    }
}

/// A directory of meetings.
pub(crate) struct Files {
    directory: PathBuf,
    index: Mutex<Index>,
}

/// What opening a directory found, on its way into the store.
pub(crate) struct Loaded {
    pub files: Files,
    /// Meetings that were still running when the last process stopped, closed
    /// here and owed to the disk.
    pub closed: Vec<Session>,
    /// When the last finished meeting in each room ended.
    pub last_ended: HashMap<String, i64>,
}

impl Files {
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
    ///
    /// **Everything unfinished is closed here**, which is right for a
    /// directory and would be wrong for a shared database: this directory
    /// belongs to one process, so a record left open in it was left open by a
    /// process that is gone. The same assumption applied to a database would
    /// have a server closing the meetings its two colleagues are still holding
    /// -- see [`crate::Database`], where a meeting is only taken away from a
    /// node that has stopped saying it is alive.
    pub(crate) fn open(directory: impl Into<PathBuf>) -> io::Result<Loaded> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;

        let mut index = Index::default();
        let mut last_ended: HashMap<String, i64> = HashMap::new();
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
                        Some(ended) => remember_ending(&mut last_ended, &header.room, ended),
                        None => interrupted.push(header.id.clone()),
                    }

                    let expected = format!("{}.json", header.id);
                    let file = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .filter(|name| *name != expected)
                        .map(|name| {
                            tracing::warn!(
                                path = %path.display(),
                                id = %header.id,
                                "a session record is in a file that is not named after it; it is \
                                 filed under the id inside it and read back out of the file it is \
                                 in"
                            );
                            Box::from(name.as_str())
                        });

                    index.remember(&header.id, &header.room, header.started_at_ms, file);
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

        index.sort();

        // Nothing that was running is running now: this process has no rooms
        // and no participants yet. Closing them here rather than leaving them
        // open is what stops a meeting from last March being reported as the
        // one happening right now. They are closed one by one by id rather
        // than through a per-room map, because two unfinished records for one
        // room is not supposed to be possible but becomes possible the moment
        // a close fails to reach the disk -- and a map keyed by room would
        // keep whichever of the two it saw last and leave the other open for
        // ever, reported as the meeting happening now.
        let mut closed = Vec::new();
        for id in &interrupted {
            let Some(path) = index
                .by_id
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

            remember_ending(&mut last_ended, &session.room, at_ms);
            closed.push(session);
        }

        tracing::info!(
            directory = %directory.display(),
            sessions = index.by_id.len(),
            "session records loaded"
        );

        Ok(Loaded {
            files: Self {
                directory,
                index: Mutex::new(index),
            },
            closed,
            last_ended,
        })
    }
}

impl Sink for Files {
    fn describe(&self) -> String {
        format!("files:{}", self.directory.display())
    }

    /// Always. A disk that refuses a write is reported by the store's
    /// `unwritten` gauge rather than by taking the whole store out of service:
    /// the directory is still there and every record already in it still
    /// answers.
    fn healthy(&self) -> bool {
        true
    }

    fn count(&self) -> usize {
        self.index.lock().expect("session index lock").by_id.len()
    }

    /// Puts a meeting into the index the moment it starts, before it has been
    /// written.
    ///
    /// Without this a room's history would not mention the meeting happening
    /// in it until the writer thread caught up. It takes an in-memory lock and
    /// nothing else, which is what makes it safe to call from the join path.
    fn opened(&self, session: &Session) {
        self.index.lock().expect("session index lock").remember(
            &session.id,
            &session.room,
            session.started_at_ms,
            None,
        );
    }

    /// Whole-file rewrite rather than an appended line, so that the file on
    /// disk is the record and not a log that has to be replayed to become one:
    /// whoever finds this directory in two years reads a meeting with `cat`. A
    /// record is a few hundred bytes and a meeting produces a few dozen
    /// writes, so the cost of rewriting it is not worth an append format
    /// nobody but this crate could read.
    fn persist(&self, session: &Session, _now_ms: i64) -> Result<(), StoreError> {
        write_record(&self.directory, session).map_err(|error| {
            StoreError::Unavailable(format!(
                "writing {} into {}: {error}",
                session.id,
                self.directory.display()
            ))
        })?;

        self.opened(session);
        Ok(())
    }

    fn fetch(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let path = {
            let index = self.index.lock().expect("session index lock");
            let Some(indexed) = index.by_id.get(id) else {
                return Ok(None);
            };
            indexed.path(&self.directory, id)
        };

        match read_record(&path) {
            Ok(session) => Ok(Some(session)),
            Err(error) => Err(StoreError::Unavailable(format!(
                "reading {}: {error}",
                path.display()
            ))),
        }
    }

    /// The files are read outside the index lock. A record that has gone
    /// missing under the server -- deleted by a retention policy between the
    /// index being consulted and the file being opened -- is left out of the
    /// page rather than failing it.
    fn page(&self, room: &str, offset: usize, limit: usize) -> Result<Listing, StoreError> {
        let (total, wanted) = {
            let index = self.index.lock().expect("session index lock");
            let Some(ids) = index.by_room.get(room) else {
                return Ok(Listing {
                    entries: Vec::new(),
                    total: 0,
                });
            };

            let wanted: Vec<(String, PathBuf)> = ids
                .iter()
                .rev()
                .skip(offset)
                .take(limit)
                .filter_map(|id| {
                    index
                        .by_id
                        .get(id)
                        .map(|indexed| (id.clone(), indexed.path(&self.directory, id)))
                })
                .collect();

            (ids.len(), wanted)
        };

        // A record that has gone missing under the server -- deleted by a
        // retention policy between the index being consulted and the file
        // being opened -- comes back as a gap rather than as a failure, and
        // the store fills it from the copy it is carrying if it has one.
        let entries = wanted
            .into_iter()
            .map(|(id, path)| {
                let session = match read_record(&path) {
                    Ok(session) => Some(session),
                    Err(error) => {
                        tracing::debug!(
                            path = %path.display(),
                            %error,
                            "a session record this server has indexed could not be read back"
                        );
                        None
                    }
                };
                (id, session)
            })
            .collect();

        Ok(Listing { entries, total })
    }
}

fn remember_ending(last_ended: &mut HashMap<String, i64>, room: &str, ended_at_ms: i64) {
    let entry = last_ended.entry(room.to_owned()).or_insert(ended_at_ms);
    *entry = (*entry).max(ended_at_ms);
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

fn read_header(path: &Path) -> io::Result<Header> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn read_record(path: &Path) -> io::Result<Session> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

pub(crate) fn write_record(directory: &Path, session: &Session) -> io::Result<()> {
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
