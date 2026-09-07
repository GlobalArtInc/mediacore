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
/// Meeting first, because that is the directory a person deletes when a meeting
/// turns out not to have been worth keeping, and because it keeps the number of
/// entries in any one directory to the number of files one meeting produced.
pub fn paths(
    directory: &Path,
    meeting: &str,
    participant: &str,
    segment: u32,
) -> (PathBuf, PathBuf) {
    let folder = directory.join(safe(meeting));
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
/// Nothing here fails the caller. A directory that cannot be read, a sidecar
/// that is not JSON, an audio file that is not there: each costs one recording,
/// is logged, and leaves the rest of them alone.
pub fn scan(directory: &Path, node: &str) -> Vec<Recording> {
    let mut found = Vec::new();

    let meetings = match fs::read_dir(directory) {
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

    for meeting in meetings.flatten() {
        if !meeting.path().is_dir() {
            continue;
        }

        let Ok(files) = fs::read_dir(meeting.path()) else {
            continue;
        };

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

    found
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

/// Removes a recording's two files once the object store has it.
///
/// A node's disk is not a second copy anybody agreed to pay for: an hour of a
/// ten-person meeting is a few tens of megabytes, and a server that keeps every
/// one of them fills a volume in a month and then stops recording. A deployment
/// that wants the local copy kept says so and takes the disk on itself.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn recording(directory: &Path, meeting: &str, participant: &str) -> Recording {
        let (audio, _) = paths(directory, meeting, participant, 0);

        Recording {
            id: mediacore_session::recording_id(),
            meeting: meeting.to_owned(),
            participant: participant.to_owned(),
            account_id: Some("AC_alice".to_owned()),
            segment: 0,
            local_path: audio,
            node: Some("ND_before".to_owned()),
            object_key: None,
            bytes: None,
            sample_rate: 48_000,
            channels: 1,
            started_at_ms: 1_757_155_200_000,
            ended_at_ms: None,
            offset_ms: 0,
            duration_ms: None,
        }
    }

    /// Writes an Ogg stream of `packets` twenty-millisecond packets, and
    /// returns the bytes.
    fn audio(packets: u64, close: bool) -> Vec<u8> {
        let mut writer = ogg::OggWriter::new(Vec::new(), 0x0a0b_0c0d, 1, 48_000).expect("a stream");
        let mut payload = vec![0u8; 80];
        payload[0] = 0b0111_1000;

        for index in 0..packets {
            writer
                .packet(&payload, (index + 1) * 960, false)
                .expect("a packet");
        }
        writer.flush(close).expect("the last page");

        writer.into_inner()
    }

    fn lay_out(directory: &Path, complete: bool, bytes: &[u8]) -> PathBuf {
        let recording = recording(directory, "SE_000000000001", "PA_000000000001");
        let (audio_path, sidecar_path) = paths(directory, &recording.meeting, "PA_000000000001", 0);
        fs::create_dir_all(audio_path.parent().expect("a meeting directory")).expect("the folder");
        fs::write(&audio_path, bytes).expect("the audio");
        write_sidecar(
            &sidecar_path,
            &Sidecar {
                room: "standup".to_owned(),
                identity: "alice".to_owned(),
                complete,
                recording,
            },
        )
        .expect("the sidecar");

        sidecar_path
    }

    #[test]
    fn a_recording_the_process_died_during_comes_back_cut_to_its_last_whole_page() {
        let directory = tempfile::TempDir::new().expect("a directory");
        let mut bytes = audio(100, false);
        let whole = bytes.len();
        bytes.extend_from_slice(b"OggS\x00\x00torn");
        lay_out(directory.path(), false, &bytes);

        let found = scan(directory.path(), "ND_after");

        assert_eq!(
            found.len(),
            1,
            "the recording left on the disk was not found"
        );
        let recovered = &found[0];
        assert_eq!(
            recovered.duration_ms,
            Some(2_000),
            "a hundred twenty-millisecond packets is two seconds, and the length has to come from \
             the granule position rather than from the byte count"
        );
        assert_eq!(
            recovered.node.as_deref(),
            Some("ND_after"),
            "a node that finds a file on its own disk has to adopt it, or the upload stays \
             offered to a process that no longer exists"
        );
        assert!(recovered.is_complete(), "a repaired recording is finished");

        let repaired = fs::read(&recovered.local_path).expect("the repaired audio");
        assert!(
            repaired.len() > whole && repaired.len() < bytes.len() + 27,
            "the torn tail should be gone and a closing page appended in its place"
        );
        let survey = ogg::survey(&repaired[..]).expect("a walk over the repaired file");
        assert_eq!(survey.good_bytes, repaired.len() as u64);
        assert!(
            survey.complete,
            "a repaired file has to say it ended, or every reader treats it as truncated"
        );
    }

    #[test]
    fn a_recording_that_was_closed_and_never_written_down_is_handed_over_again() {
        let directory = tempfile::TempDir::new().expect("a directory");
        lay_out(directory.path(), true, &audio(50, true));

        let found = scan(directory.path(), "ND_after");

        assert_eq!(
            found.len(),
            1,
            "a file that was finished and whose database row never landed -- the process died in \
             between -- is exactly what a scan is for"
        );
        assert_eq!(found[0].duration_ms, Some(1_000));
    }

    #[test]
    fn a_file_with_nothing_but_headers_in_it_is_thrown_away() {
        let directory = tempfile::TempDir::new().expect("a directory");
        let sidecar = lay_out(directory.path(), false, &audio(0, false));

        let found = scan(directory.path(), "ND_after");

        assert!(
            found.is_empty(),
            "a participant who published a track and never spoke leaves a file with no audio in \
             it; uploading that is an object nobody wants and an event nobody can use"
        );
        assert!(
            !sidecar.exists(),
            "and it must not be found again at every start for the rest of the deployment's life"
        );
    }

    /// Audio with no sidecar beside it is invisible to a scan, and that is why
    /// the sidecar is written first.
    ///
    /// The scan walks sidecars, because a sidecar is the only thing that says
    /// which meeting and which participant a file belongs to. So an `.ogg` that
    /// reached the disk before its `.json` -- a crash in the window between the
    /// two, if the writer created them the other way round -- would sit there
    /// for ever: never uploaded, never announced, never cleaned up, and
    /// unidentifiable by hand. `crate::writer` writes the sidecar first for
    /// exactly this reason; the other order is self-healing, because a sidecar
    /// whose audio is missing is dropped on the next start.
    #[test]
    fn audio_with_no_sidecar_beside_it_is_a_file_nothing_can_ever_identify() {
        let directory = tempfile::TempDir::new().expect("a directory");
        let (audio_path, _) = paths(directory.path(), "SE_000000000001", "PA_000000000001", 0);
        fs::create_dir_all(audio_path.parent().expect("a meeting directory")).expect("the folder");
        fs::write(&audio_path, audio(50, true)).expect("the audio");

        assert!(
            scan(directory.path(), "ND_after").is_empty(),
            "there is nothing here to say what this audio is of"
        );
        assert!(
            audio_path.exists(),
            "and nothing removes it either, so it would be on the disk until somebody noticed"
        );
    }

    #[test]
    fn a_sidecar_whose_audio_is_gone_is_dropped_rather_than_retried_for_ever() {
        let directory = tempfile::TempDir::new().expect("a directory");
        let sidecar = lay_out(directory.path(), true, &audio(50, true));
        let recording = read_sidecar(&sidecar).expect("the sidecar").recording;
        fs::remove_file(&recording.local_path).expect("the audio goes");

        assert!(scan(directory.path(), "ND_after").is_empty());
        assert!(!sidecar.exists());
    }

    /// The arithmetic that turns a granule position into a length cannot be a
    /// way into this server.
    ///
    /// A granule position comes from RTP timestamps, and an RTP timestamp is a
    /// number a publisher chooses. `granule * 1_000` in `u64` overflows above
    /// about 1.8e16 -- and str0m's roll-over extension answers with a number
    /// near `u64::MAX` for a step of more than half the 32-bit range, which is
    /// two packets' work for anybody who wants it. In a test or debug build
    /// that multiplication panics; on the writer thread, that used to be every
    /// recording on the node.
    #[test]
    fn a_granule_position_a_publisher_chose_never_overflows_the_length() {
        assert_eq!(duration_ms(0), 0);
        assert_eq!(duration_ms(GRANULE_RATE), 1_000, "one second of samples");
        assert_eq!(
            duration_ms(GRANULE_RATE * 3_600),
            3_600_000,
            "an hour of samples"
        );

        // What a wrap read backwards produces. The answer is nonsense because
        // the input is, and nonsense is fine: what must not happen is a panic
        // on the one thread that writes every recording on this process.
        assert!(
            duration_ms(u64::MAX) > 0,
            "the largest granule position a publisher can produce has to come back as a number, \
             not as a panic on the writer thread"
        );
        assert!(
            duration_ms(0xFFFF_FFFF_8000_0002) > 0,
            "and so does the one str0m's extension answers with when a sender's second packet \
             steps more than half the timestamp range"
        );
    }

    #[test]
    fn an_id_that_is_not_one_of_ours_never_becomes_a_path_of_its_own() {
        let (audio, sidecar) = paths(Path::new("/var/lib/mediacore"), "../../etc", "PA_1", 0);

        assert_eq!(
            audio,
            Path::new("/var/lib/mediacore/______etc/PA_1-0.ogg"),
            "a meeting id is server-minted today, and the day it is not, this must be a bad name \
             inside the directory and not a file outside it"
        );
        assert_eq!(
            sidecar.extension().and_then(|kind| kind.to_str()),
            Some("json")
        );
    }
}
