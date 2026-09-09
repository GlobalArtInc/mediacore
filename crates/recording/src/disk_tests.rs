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
