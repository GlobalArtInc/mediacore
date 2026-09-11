use super::*;
use std::path::PathBuf;

/// A node upgrading into this release finds sidecars its previous life
/// wrote, and those spell the session `meeting` and carry an `accountId`
/// this server no longer has.
///
/// A node that could not read them would leave every recording it had not
/// uploaded yet as audio on a disk with nothing to say what it is -- which
/// is exactly the state the sidecar exists to prevent.
#[test]
fn a_sidecar_written_before_the_rename_is_still_read() {
    let body = br#"{
            "room": "standup",
            "identity": "alice",
            "complete": true,
            "recording": {
                "id": "RE_0123456789ab",
                "meeting": "SE_0123456789ab",
                "participant": "PA_0123456789ab",
                "accountId": "AC_9271",
                "segment": 0,
                "localPath": "/var/lib/mediacore/audio/PA_0123456789ab-0.ogg",
                "node": "ND_before",
                "objectKey": null,
                "bytes": 4096,
                "sampleRate": 48000,
                "channels": 1,
                "startedAtMs": 1757155200000,
                "endedAtMs": 1757156740000,
                "offsetMs": 0,
                "durationMs": 1540000
            }
        }"#;

    let sidecar: Sidecar = serde_json::from_slice(body)
        .expect("a sidecar this node wrote in a previous life is unreadable to it now");

    assert_eq!(
        sidecar.recording.session, "SE_0123456789ab",
        "the old spelling of the session has to land on the field that replaced it"
    );
    assert_eq!(sidecar.recording.duration_ms, Some(1_540_000));
}

fn recording(directory: &Path, session: &str, participant: &str) -> Recording {
    let (audio, _) = paths(directory, session, participant, 0);

    Recording {
        id: mediacore_session::recording_id(),
        session: session.to_owned(),
        participant: participant.to_owned(),
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
    let (audio_path, sidecar_path) = paths(directory, &recording.session, "PA_000000000001", 0);
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

// ------------------------------------- the directory a finished recording left

/// Puts one participant's finished pair into a meeting's directory, and hands
/// back the recording that names them.
fn pair(directory: &Path, session: &str, participant: &str) -> Recording {
    let recording = recording(directory, session, participant);
    let folder = recording.local_path.parent().expect("a meeting directory");

    fs::create_dir_all(folder).expect("the folder");
    fs::write(&recording.local_path, audio(10, true)).expect("the audio");
    write_sidecar(
        &recording.local_path.with_extension("json"),
        &Sidecar {
            room: "standup".to_owned(),
            identity: participant.to_owned(),
            complete: true,
            recording: recording.clone(),
        },
    )
    .expect("the sidecar");

    recording
}

/// The whole point: a meeting that has been recorded and uploaded leaves
/// nothing at all behind.
///
/// It was two files and a directory, and only the files went. Bytes are not the
/// cost -- a directory is a few hundred of them -- the count is: one more entry
/// per meeting for ever, on a fixed volume, and inodes run out long before
/// bytes do. The thing that fails when they do is recording.
#[test]
fn a_meeting_that_has_been_uploaded_leaves_no_directory_behind() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let recording = pair(directory.path(), "SE_000000000001", "PA_000000000001");
    let folder = recording
        .local_path
        .parent()
        .expect("a meeting directory")
        .to_path_buf();

    forget(&recording);

    assert!(
        !folder.exists(),
        "the meeting directory outlived the only recording in it, and one of these is left \
         behind by every meeting this node ever records"
    );
    assert!(
        directory.path().exists(),
        "and the volume the operator configured is not a thing this ever removes"
    );
}

/// A participant whose upload finishes first must not take a sibling's audio
/// with it.
///
/// The sibling's file is still on this disk *because* it has not reached the
/// object store yet, so a `remove_dir_all` here would delete the one copy of a
/// recording that exists anywhere. `remove_dir` refusing a directory with
/// anything in it is what makes the ordering between two participants not
/// matter at all.
#[test]
fn the_first_participant_uploaded_does_not_take_the_second_one_s_audio() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let session = "SE_000000000001";
    let first = pair(directory.path(), session, "PA_000000000001");
    let second = pair(directory.path(), session, "PA_000000000002");
    let folder = directory.path().join(session);

    forget(&first);

    assert!(
        folder.exists(),
        "the meeting is not over for everybody just because one upload finished"
    );
    assert!(
        second.local_path.exists() && second.local_path.with_extension("json").exists(),
        "and the audio that has not been uploaded is the only copy there is"
    );

    forget(&second);

    assert!(
        !folder.exists(),
        "the directory goes with the last pair in it and not before"
    );
}

/// Two uploads finishing at the same moment are two threads in one directory,
/// and the loser must not be an error anybody sees.
///
/// There are exactly two ways to lose. Remove the pair, look, and find a
/// sibling's files still there -- the other thread has not got to its own
/// deletion yet. Or remove the pair, and find the directory already gone --
/// the other thread emptied it and swept it between the two. Both are right,
/// and neither is a failure; this asserts on `tidy` directly because the
/// answer, not the log line, is what the rest of the code reads.
#[test]
fn neither_way_of_losing_the_race_to_remove_a_directory_is_a_failure() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let folder = directory.path().join("SE_000000000001");
    let sibling = folder.join("PA_000000000002-0.ogg");
    fs::create_dir_all(&folder).expect("the folder");
    fs::write(&sibling, b"a recording that is not in the object store yet").expect("the audio");

    assert!(
        !tidy(&folder),
        "a directory with somebody else's audio in it is not this call's to remove"
    );
    assert!(sibling.exists(), "and the audio in it is untouched");

    fs::remove_file(&sibling).expect("the sibling's upload finishes too");

    assert!(tidy(&folder), "an emptied directory goes");
    assert!(
        !tidy(&folder),
        "and a thread that finds it already gone has nothing to report either"
    );
}

/// The same thing for real, on two threads, because the argument above is about
/// an interleaving and interleavings are what a test can actually run.
#[test]
fn two_participants_uploads_finishing_at_once_leave_nothing_behind() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let session = "SE_000000000001";
    let first = pair(directory.path(), session, "PA_000000000001");
    let second = pair(directory.path(), session, "PA_000000000002");
    let folder = directory.path().join(session);

    std::thread::scope(|threads| {
        threads.spawn(|| forget(&first));
        threads.spawn(|| forget(&second));
    });

    assert!(
        !folder.exists(),
        "whichever of them was last had to remove it"
    );
    assert!(
        directory.path().exists(),
        "and neither of them may remove the volume itself"
    );
}

/// A sidecar from before meetings had a directory of their own names a file
/// straight in the recordings root -- and the parent of that is the volume.
///
/// The node that uploads the last such recording would remove the directory the
/// operator configured, and a `create_dir_all` would quietly make it again
/// somewhere else if the mount had gone. So the removal is not "the parent of
/// the audio": it is "a directory this layout would have made", which is one
/// named for the meeting.
#[test]
fn a_recording_from_before_meetings_had_directories_never_removes_the_volume() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let flat = directory.path().join("PA_000000000001-0.ogg");
    fs::write(&flat, audio(10, true)).expect("the audio");
    fs::write(flat.with_extension("json"), b"{}").expect("the sidecar");

    let mut recording = recording(directory.path(), "SE_000000000001", "PA_000000000001");
    recording.local_path = flat.clone();

    forget(&recording);

    assert!(!flat.exists(), "the pair still goes");
    assert!(
        directory.path().exists(),
        "and the recordings directory itself survives the last recording in it"
    );
}

/// The thirty directories already on the volume: a start clears what earlier
/// runs left, because an empty meeting directory is the same class of leftover
/// as a recording nobody finished.
///
/// A scan is the one place this is safe. It runs before the recorder exists, so
/// an empty directory here cannot be a meeting that is about to be written
/// into -- which is what it would be at any other moment.
#[test]
fn empty_meeting_directories_from_an_earlier_run_are_cleared_at_startup() {
    let directory = tempfile::TempDir::new().expect("a directory");
    for old in ["SE_00000000ab01", "SE_00000000ab02", "SE_00000000ab03"] {
        fs::create_dir_all(directory.path().join(old)).expect("what an earlier run left");
    }
    lay_out(directory.path(), true, &audio(50, true));

    let found = scan(directory.path(), "ND_after");

    assert_eq!(found.len(), 1, "the recording on the disk is still found");
    for old in ["SE_00000000ab01", "SE_00000000ab02", "SE_00000000ab03"] {
        assert!(
            !directory.path().join(old).exists(),
            "{old} is a meeting that ended, uploaded and was cleaned up except for this; a \
             volume that has recorded for a year carries one per meeting"
        );
    }
    assert!(
        directory.path().join("SE_000000000001").is_dir(),
        "and a directory with a recording in it stays, because the recording has not been \
         uploaded yet"
    );
}

/// Audio with nothing beside it to say what it is keeps its directory.
///
/// A scan walks sidecars, so this file is invisible to it and is never
/// uploaded -- which makes it the one thing on the volume a person has to look
/// at by hand. Removing the directory around it would need `remove_dir_all`,
/// and that would take the audio; `remove_dir` refuses and the file stays
/// findable.
#[test]
fn a_startup_scan_leaves_the_directory_of_a_file_it_cannot_identify() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (orphan, _) = paths(directory.path(), "SE_000000000001", "PA_000000000001", 0);
    fs::create_dir_all(orphan.parent().expect("a meeting directory")).expect("the folder");
    fs::write(&orphan, audio(50, true)).expect("the audio");

    assert!(scan(directory.path(), "ND_after").is_empty());
    assert!(
        orphan.exists(),
        "a scan that removed this would be deleting the only copy of a recording"
    );
    assert!(
        orphan.parent().expect("a meeting directory").is_dir(),
        "and the directory has to stay for the file to stay in"
    );
}

/// A recording the scan itself throws away takes its directory with it.
///
/// A participant who published a track and never spoke leaves a file with
/// nothing in it, and the scan drops both of its files -- which before this
/// left an empty directory behind at every start, rather than only once.
#[test]
fn the_directory_of_a_recording_the_scan_discards_goes_with_it() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let sidecar = lay_out(directory.path(), false, &audio(0, false));
    let folder = sidecar.parent().expect("a meeting directory").to_path_buf();

    assert!(scan(directory.path(), "ND_after").is_empty());

    assert!(
        !folder.exists(),
        "the scan emptied this directory itself, so leaving it would be litter of its own \
         making"
    );
}

/// The recordings root is a filesystem an operator mounted, and it has things
/// in it that are not ours.
///
/// The volume in production is ext4, so its root holds a `lost+found` -- and
/// one on a filesystem that has never been repaired is empty, so a sweep that
/// took every empty directory would take the one place `fsck` puts an orphaned
/// inode, on the volume holding recordings that exist nowhere else. Only a
/// directory named the way this server names one is a directory this server
/// made.
#[test]
fn a_startup_scan_only_removes_directories_this_server_named() {
    let directory = tempfile::TempDir::new().expect("a directory");
    for theirs in ["lost+found", "incoming", ".snapshot"] {
        fs::create_dir_all(directory.path().join(theirs)).expect("what the volume came with");
    }
    fs::create_dir_all(directory.path().join("SE_00000000ab01")).expect("what an earlier run left");

    assert!(scan(directory.path(), "ND_after").is_empty());

    for theirs in ["lost+found", "incoming", ".snapshot"] {
        assert!(
            directory.path().join(theirs).is_dir(),
            "{theirs} is empty and is still not this code's to remove"
        );
    }
    assert!(
        !directory.path().join("SE_00000000ab01").exists(),
        "and the one that is ours still goes"
    );
    assert!(
        is_session_directory("SE_794e5eb27dda") && !is_session_directory("SE_"),
        "a bare prefix is not an id, and a directory called one is not ours either"
    );
}
