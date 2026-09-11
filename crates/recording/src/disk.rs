//! Where a recording lives while it is being made, and what a node finds there
//! when it comes back.
//!
//! One directory per meeting, and in it two files per recording: the audio, and
//! a small JSON sidecar saying what the audio is. The sidecar is not a
//! convenience -- it is the reason a crash costs nothing.
//!
//! # Why the file describes itself
//!
//! The obvious design writes a database row when a recording starts, so that
//! nothing is ever on disk unaccounted for. It has two failures. A database
//! that is away -- which is a thing that happens, and which this server is
//! built to carry calls through -- would stop a recording from starting at all;
//! and a row written at the start says the file is *open*, so a node that dies
//! leaves a row nobody can finish without knowing what was in the file anyway.
//!
//! So the file carries its own record. Nothing about recording touches the
//! database while a meeting is happening: the sidecar is written when the file
//! opens and rewritten when it closes, and only then does anybody tell the
//! store. A node that comes back reads its own directory, finishes whatever was
//! open, and hands the lot over -- and because the store keys a recording on
//! `(meeting, participant, segment)`, telling it twice is telling it once.
//!
//! # What "finishing" a torn file means
//!
//! It means reading its pages, finding where they stop being whole, cutting the
//! file there and appending the page that says the stream ended. No decoding:
//! the last good page's granule position is how much audio the file holds,
//! because [`crate::ogg`] wrote that position from the sender's clock. A file
//! that lost its last half-second to a power cut comes back half a second
//! shorter and otherwise exactly right.
//!
//! # The directory belongs to one process
//!
//! Like `sessions.directory`, and for the same reason: two processes sharing it
//! would each finish the other's live recordings on startup. A deployment that
//! runs several nodes gives each one its own volume; what they share is the
//! database and the object store, which is what the node column on a recording
//! is for.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mediacore_session::Recording;
use serde::{Deserialize, Serialize};

use crate::ogg;

/// The 48 kHz clock every Ogg Opus granule position counts in, whatever rate
/// the audio was captured at.
pub const GRANULE_RATE: u64 = 48_000;

/// How long a granule position is, in milliseconds.
///
/// **Through `u128`, and that is not decoration.** A granule position is
/// derived from RTP timestamps, and an RTP timestamp is a number the publisher
/// chooses. str0m extends a 32-bit timestamp against the one before it and
/// reads a step of more than half the range as a wrap *backwards*, so a sender
/// whose second packet is `0x8000_0002` after a first of `0` produces an
/// extended value near `u64::MAX` -- and `granule * 1_000` in `u64` then
/// overflows. In a debug build that is a panic on the one thread every
/// recording on this node shares; in a release build it is a wrapped number
/// that goes into the sidecar, the database row and the `recording_ready`
/// event, and is recomputed identically by the next start, so a restart does
/// not clear it. The writer refuses such a timestamp before it reaches a file
/// -- see `crate::writer` -- and this is the second line of defence, because
/// the arithmetic itself must not be a way in.
pub fn duration_ms(granule: u64) -> i64 {
    let millis = u128::from(granule) * 1_000 / u128::from(GRANULE_RATE);

    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// What is written beside a recording while it is being made.
///
/// The `Recording` in it is exactly the row the store will hold; the three
/// fields around it are what the store does not keep and a person reading the
/// directory wants -- which room, who, and whether the file is finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
    pub room: String,
    /// The identity the room gave this participant, for a human reading the
    /// directory. The recording itself is keyed by the participant's sid,
    /// because two people can be called `alice` and only one of them can be
    /// `PA_1f0c...`.
    pub identity: String,
    /// Whether the audio file is finished. `false` is what a crash leaves
    /// behind and what [`scan`] repairs.
    pub complete: bool,
    pub recording: Recording,
}

/// Where one recording's two files go.
///
/// The session first, because that is the directory a person deletes when a
/// meeting turns out not to have been worth keeping, and because it keeps the
/// number of entries in any one directory to the number of files one meeting
/// produced.
pub fn paths(
    directory: &Path,
    session: &str,
    participant: &str,
    segment: u32,
) -> (PathBuf, PathBuf) {
    let folder = directory.join(safe(session));
    let stem = format!("{}-{segment}", safe(participant));

    (
        folder.join(format!("{stem}.ogg")),
        folder.join(format!("{stem}.json")),
    )
}

/// Everything that is not a letter, a digit, a dash or an underscore becomes an
/// underscore.
///
/// Both ids that reach this are minted by this server and are already `SE_` or
/// `PA_` and hex, so this never changes anything. It is here because a path
/// built out of an identifier is one schema change away from being a path built
/// out of something a caller chose, and the failure then is a file written
/// outside the directory rather than a wrong name.
fn safe(id: &str) -> String {
    id.chars()
        .map(|letter| {
            if letter.is_ascii_alphanumeric() || letter == '-' || letter == '_' {
                letter
            } else {
                '_'
            }
        })
        .collect()
}

/// Writes the sidecar, whole or not at all.
///
/// Through a temporary file and a rename, because the alternative -- a sidecar
/// truncated by the same power cut that tore the audio -- is a recording nobody
/// can identify afterwards, which is worse than one that is merely unfinished.
pub fn write_sidecar(path: &Path, sidecar: &Sidecar) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }

    let body = serde_json::to_vec_pretty(sidecar)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let staging = path.with_extension("json.part");
    let mut file = fs::File::create(&staging)?;
    file.write_all(&body)?;
    file.sync_all()?;
    fs::rename(&staging, path)
}

fn read_sidecar(path: &Path) -> io::Result<Sidecar> {
    let body = fs::read(path)?;

    serde_json::from_slice(&body).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a recording sidecar: {error}", path.display()),
        )
    })
}

/// Reads the directory and hands back every recording in it, finished.
///
/// Run once, at startup, before anything is recorded into the same directory.
/// What comes back is ready to be given to the store: files that were closed
/// properly and never made it into the database, and files that were open when
/// the process died, cut back to their last whole page and closed.
///
/// `node` is written onto every one of them. That is the adoption: a volume
/// that outlived the process -- or was re-attached to a machine that generates
/// a new node id every start, which is the default -- becomes this node's, so
/// its uploads are offered to the process that can actually see the files.
///
/// It also throws away the meeting directories an earlier run left empty, which
/// is the same class of leftover arriving from the other direction: not a
/// recording nobody finished, but the hole one left behind. See the comment at
/// the end of the loop for why a startup scan is the only place that may.
///
/// Nothing here fails the caller. A directory that cannot be read, a sidecar
/// that is not JSON, an audio file that is not there: each costs one recording,
/// is logged, and leaves the rest of them alone.
pub fn scan(directory: &Path, node: &str) -> Vec<Recording> {
    let mut found = Vec::new();
    let mut litter = 0usize;

    let sessions = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return found,
        Err(error) => {
            tracing::error!(
                directory = %directory.display(),
                %error,
                "the recording directory could not be read, so whatever is in it will not be \
                 uploaded"
            );
            return found;
        }
    };

    for session in sessions.flatten() {
        let folder = session.path();
        if !folder.is_dir() {
            continue;
        }

        if let Ok(files) = fs::read_dir(&folder) {
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|kind| kind != "json") {
                    continue;
                }

                match recover(&path, node) {
                    Ok(Some(recording)) => found.push(recording),
                    Ok(None) => {}
                    Err(error) => tracing::error!(
                        sidecar = %path.display(),
                        %error,
                        "a recording left on this disk could not be finished, so it will not be \
                         uploaded and the audio in it is only here"
                    ),
                }
            }
        }

        // What an earlier run left is not only files. Before the directory went
        // with its last pair, every uploaded recording left the folder that had
        // held it, so a volume that has recorded for a year carries one empty
        // directory per meeting it has ever held -- nothing in bytes, an inode
        // each, on a volume whose own answer to running out of either is that
        // this node stops recording.
        //
        // **Here and nowhere else**, because an empty meeting directory is
        // otherwise indistinguishable from one a meeting is about to be written
        // into. A scan runs once, at startup, before the recorder exists: no
        // file in this directory can be opened yet, and the directory belongs
        // to one process, so there is no second writer to lose a race with.
        // `remove_dir` refusing anything that is not empty is the rest of it --
        // including under `keep_local`, where the copies a deployment asked to
        // keep are what makes these directories not empty.
        if folder
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_session_directory)
            && tidy(&folder)
        {
            litter += 1;
        }
    }

    if litter > 0 {
        tracing::info!(
            directories = litter,
            directory = %directory.display(),
            "empty meeting directories were left on this node's disk by an earlier run; they \
             have been removed"
        );
    }

    found
}

/// Whether a directory sitting in the recordings root is one this server made.
///
/// **Read off a production volume rather than reasoned about.** The volume
/// mounted there is ext4, so there is a `lost+found` in the root -- and
/// `lost+found` on a filesystem that has never been repaired is *empty*, which
/// makes a sweep that removed every empty directory remove the one place
/// `fsck` has to put an orphaned inode, on the volume whose whole job is
/// holding recordings that exist nowhere else. Nothing else an operator left in
/// the root is this code's to delete either.
///
/// A session id is minted by this server and is `SE_` and alphanumerics, which
/// is the same shape `mediacore_session` insists on before it joins an id onto
/// a path, and for the same reason: a directory name is not a thing to trust
/// just because it is on our own disk.
fn is_session_directory(name: &str) -> bool {
    let Some(tail) = name.strip_prefix(mediacore_session::SESSION_ID_PREFIX) else {
        return false;
    };

    !tail.is_empty()
        && tail
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

/// Finishes one recording found on disk, or says there is nothing to finish.
fn recover(sidecar_path: &Path, node: &str) -> io::Result<Option<Recording>> {
    let mut sidecar = read_sidecar(sidecar_path)?;
    let audio = sidecar.recording.local_path.clone();

    let length = match fs::metadata(&audio) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // The audio was uploaded and deleted and the sidecar outlived it by
            // one crash, or somebody removed it by hand. Either way there is
            // nothing to upload, and leaving the sidecar would mean finding it
            // again at every start for ever.
            tracing::warn!(
                sidecar = %sidecar_path.display(),
                audio = %audio.display(),
                "a recording's sidecar has no audio beside it; dropping the sidecar"
            );
            let _ = fs::remove_file(sidecar_path);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    let survey = ogg::survey(io::BufReader::new(fs::File::open(&audio)?))?;

    if survey.granule == 0 {
        tracing::warn!(
            audio = %audio.display(),
            length,
            "a recording on this disk holds no audio -- the meeting ended before anybody spoke, \
             or the file never got past its headers; dropping it"
        );
        let _ = fs::remove_file(&audio);
        let _ = fs::remove_file(sidecar_path);
        return Ok(None);
    }

    if survey.good_bytes < length || !survey.complete {
        let file = fs::OpenOptions::new().append(true).open(&audio)?;
        file.set_len(survey.good_bytes)?;
        let mut file = file;
        file.write_all(&ogg::closing_page(
            survey.serial,
            survey.next_page,
            survey.granule,
        ))?;
        file.sync_all()?;

        tracing::warn!(
            audio = %audio.display(),
            torn = length - survey.good_bytes,
            "this recording was still being written when the process stopped; it has been cut \
             back to its last whole page and closed"
        );
    }

    let duration_ms = duration_ms(survey.granule);
    sidecar.complete = true;
    sidecar.recording.node = Some(node.to_owned());
    sidecar.recording.bytes = Some(fs::metadata(&audio)?.len());
    sidecar.recording.duration_ms = Some(duration_ms);
    // The end of the audio, not the moment this was noticed. The granule
    // position counts the sender's own clock and therefore includes every
    // silence, so the last sample really is this far after the first one --
    // whereas "now" would claim as recording however long the node was down.
    sidecar.recording.ended_at_ms = Some(sidecar.recording.started_at_ms + duration_ms);

    write_sidecar(sidecar_path, &sidecar)?;

    Ok(Some(sidecar.recording))
}

/// Removes a recording's two files once the object store has it, and the
/// meeting directory with them if that pair was the last one in it.
///
/// A node's disk is not a second copy anybody agreed to pay for: an hour of a
/// ten-person meeting is a few tens of megabytes, and a server that keeps every
/// one of them fills a volume in a month and then stops recording. A deployment
/// that wants the local copy kept says so and takes the disk on itself -- and
/// gets the directory with it, because this is not reached at all when the
/// files stay.
pub fn forget(recording: &Recording) {
    let audio = &recording.local_path;
    let sidecar = audio.with_extension("json");

    for path in [audio.as_path(), sidecar.as_path()] {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                %error,
                "an uploaded recording could not be removed from this node's disk; the object \
                 store has it, so this costs space and nothing else"
            );
        }
    }

    // Only a directory this layout would itself have created, which is to say
    // one named for this recording's meeting. A sidecar written before meetings
    // had a directory of their own -- see the test that reads one -- names a
    // file straight in the recordings root, and the parent of *that* is the
    // volume the operator configured, which a node whose last upload finished
    // would otherwise delete out from under itself.
    let expected = safe(&recording.session);
    if let Some(folder) = audio.parent()
        && folder.file_name().and_then(|name| name.to_str()) == Some(expected.as_str())
    {
        tidy(folder);
    }
}

/// Removes a meeting's directory if there is nothing left in it, and says
/// whether it did.
///
/// **`remove_dir` and never `remove_dir_all`.** A meeting directory holds one
/// pair of files per participant, and each pair leaves on its own upload; the
/// participant whose upload finishes first must not take a sibling's audio with
/// it, which is on this disk precisely because it has *not* reached the object
/// store yet. A directory with anything in it is not this call's to remove, and
/// the kernel refusing it is that check -- not a condition worth asking about
/// separately, because between the asking and the removing is the race.
///
/// Two participants' uploads finishing at the same moment are two threads in
/// one directory, and the loser of that race finds either a sibling's files
/// still there or the directory already gone. Both are the outcome this wants,
/// so neither is said out loud. What is left is a genuine failure -- a
/// permission, a volume that has gone read-only -- and it is reported exactly
/// as loudly as failing to remove the files was: this costs space, and the
/// object store has the audio.
fn tidy(folder: &Path) -> bool {
    match fs::remove_dir(folder) {
        Ok(()) => true,
        // `AlreadyExists` is `EEXIST`, which POSIX allows a `rmdir` to answer
        // instead of `ENOTEMPTY` for exactly this case and which some network
        // filesystems do. There is no other thing it could mean here, and
        // taking it for what it is costs one arm rather than a warning per
        // finished meeting on such a mount.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::DirectoryNotEmpty
                    | io::ErrorKind::AlreadyExists
            ) =>
        {
            false
        }
        Err(error) => {
            tracing::warn!(
                path = %folder.display(),
                %error,
                "an emptied meeting directory could not be removed from this node's disk; it \
                 holds nothing, so this costs one inode and nothing else"
            );
            false
        }
    }
}

#[cfg(test)]
#[path = "disk_tests.rs"]
mod tests;
