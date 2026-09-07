//! What ends up on the disk, driven through the recorder itself.
//!
//! Everything here goes in as RTP packets and comes out as files, which is the
//! only way to be sure that the three things that have to line up -- the
//! sender's timestamps, the granule positions and the row that describes the
//! file -- line up.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mediacore_recording::{Recorder, Settings, TrackSpec, disk, ogg};
use mediacore_session::Recording;

/// Twenty milliseconds of fullband Opus, mono. The body is not audio and does
/// not need to be: nothing between here and the object store looks past the
/// first byte.
fn payload() -> Arc<[u8]> {
    let mut bytes = vec![0u8; 80];
    bytes[0] = 0b0111_1000;

    Arc::from(bytes.into_boxed_slice())
}

/// Twenty milliseconds at 48 kHz.
const TICK: u64 = 960;

fn settings(directory: &Path) -> Settings {
    Settings {
        node: "ND_one".to_owned(),
        directory: directory.to_path_buf(),
        max_segment_bytes: 64 * 1024 * 1024,
        // Short, because these tests hand every packet over in order and then
        // close the track, which flushes whatever is still held.
        reorder_hold: Duration::from_millis(5),
        queue: 8_192,
    }
}

fn spec(participant: &str) -> TrackSpec {
    TrackSpec {
        room: "standup".to_owned(),
        participant: participant.to_owned(),
        identity: "alice".to_owned(),
        account_id: Some("AC_alice".to_owned()),
        track: "TR_audio".to_owned(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as i64
}

/// Waits for one recording to be finished, or fails saying so.
fn finished(taken: &mut tokio::sync::mpsc::UnboundedReceiver<Recording>) -> Recording {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if let Ok(recording) = taken.try_recv() {
            return recording;
        }
        assert!(
            Instant::now() < deadline,
            "no recording was finished within ten seconds; the writer thread is not producing \
             files at all"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Every recording a recorder has finished, once nothing more is coming.
fn everything(mut taken: tokio::sync::mpsc::UnboundedReceiver<Recording>) -> Vec<Recording> {
    let mut all = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        match taken.try_recv() {
            Ok(recording) => all.push(recording),
            // The recorder has been dropped, so its writer has closed every
            // file it held and the queue is finished.
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return all,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                assert!(
                    Instant::now() < deadline,
                    "the recorder never let go of its queue; the writer thread has not finished"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Reads a finished recording's file and holds it to the invariant every file
/// this crate writes has to keep.
///
/// **The packets in the file have to add up to exactly what the file's granule
/// position claims**, and the row beside it has to describe the same length. A
/// granule that runs ahead of its packets is a file that *reports* two minutes
/// and hands a decoder two seconds with the words pushed together -- ffmpeg
/// does not read a position off every page, it adds up the durations before
/// each packet -- and a row that disagrees with either is a length somebody
/// aligns a transcript against and is wrong about. It is checked here rather
/// than in each test because a test that forgets it is a test that passes on a
/// file no consumer can use.
fn well_formed(recording: &Recording) -> ogg::Survey {
    let bytes = std::fs::read(&recording.local_path).expect("the audio");
    let survey = ogg::survey(&bytes[..]).expect("a walk over the file");

    assert_eq!(
        survey.good_bytes,
        bytes.len() as u64,
        "{} has a torn tail: every page of a finished recording has to check out",
        recording.local_path.display()
    );
    assert!(
        survey.complete,
        "{} does not say the stream ended, so every reader treats it as cut off",
        recording.local_path.display()
    );
    assert_eq!(
        survey.packet_samples,
        survey.granule,
        "{} claims {} samples and holds {}: a decoder handed this file gets audio of one length \
         out of a file that reports another, with everything after each gap in the wrong place",
        recording.local_path.display(),
        survey.granule,
        survey.packet_samples
    );
    assert_eq!(
        recording.duration_ms,
        Some((u128::from(survey.granule) * 1_000 / 48_000) as i64),
        "the row for {} says {:?} ms and the file holds {} samples; a row that disagrees with its \
         own file is worse than no row",
        recording.local_path.display(),
        recording.duration_ms,
        survey.granule
    );

    survey
}

/// The heart of it: a participant who says nothing for two minutes comes out
/// two minutes longer, not two minutes shorter.
///
/// This is the failure the recorder next door has. It derives a recording's
/// length from the samples it wrote, so every silence vanishes and everything
/// after one is early by however long it lasted -- compounding, so a transcript
/// of an hour's meeting is minutes out by the end. Here the length comes from
/// the sender's own clock, which runs whether or not anybody is talking.
#[test]
fn a_silence_makes_the_recording_longer_and_not_shorter() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));

    // Ten packets, two minutes of nothing -- which is what discontinuous
    // transmission does whenever somebody stops talking -- then ten more.
    for index in 0..10u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    let after = 10 * TICK + 48_000 * 120;
    for index in 0..10u64 {
        tap.packet(
            100 + index,
            after + index * TICK,
            48_000,
            payload(),
            Instant::now(),
        );
    }
    drop(tap);

    let recording = finished(&mut taken);

    assert_eq!(
        recording.duration_ms,
        Some(120_400),
        "two minutes of silence and four hundred milliseconds of speech is a recording of two \
         minutes and four hundred milliseconds; anything shorter means every word after the \
         silence is aligned that much too early"
    );
    assert_eq!(
        recording.ended_at_ms,
        Some(recording.started_at_ms + 120_400),
        "the end is where the sound stops, not when the file happened to be closed"
    );

    let survey = well_formed(&recording);
    assert_eq!(
        survey.granule,
        120_400 * 48,
        "the file's own clock has to agree with the row: two minutes of silence and four hundred \
         milliseconds of speech"
    );
}

/// A publisher whose timestamps claim more time than the recording has existed
/// does not make this node write that much silence, and does not make it write
/// a file that lies either.
///
/// The file is closed where it is and what follows is the next segment, with a
/// clock of its own -- exactly the shape of a rejoin, and for the same reason:
/// the gap belongs in the offsets between two files and not inside one of them.
/// Leaving it as a jump in the granule was the other candidate and is worse, as
/// [`well_formed`] says: it makes the file report a length no decoder can
/// reach.
#[test]
fn a_timestamp_from_the_future_becomes_the_next_segment_and_never_hours_of_silence() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));

    tap.packet(0, 0, 48_000, payload(), Instant::now());
    // A day, out of a sender that has been connected for milliseconds.
    tap.packet(1, 48_000 * 86_400, 48_000, payload(), Instant::now());
    drop(tap);
    drop(recorder);

    let all = everything(taken);
    let written: u64 = all
        .iter()
        .map(|recording| {
            std::fs::metadata(&recording.local_path)
                .expect("the audio")
                .len()
        })
        .sum();

    assert!(
        written < 64 * 1024,
        "a publisher claiming a day of silence made this node write {written} bytes of it; one \
         packet must not be able to spend the disk and the writer thread that every other \
         recording on this node shares"
    );
    assert_eq!(
        all.len(),
        2,
        "the jump has to end one file and begin another, so that each of them is a stretch of \
         audio with a clock that means something"
    );
    for recording in &all {
        well_formed(recording);
    }
    assert_eq!(
        (all[0].segment, all[1].segment),
        (0, 1),
        "and the two files have to be two segments of the same participant, or the second \
         overwrites the first's row"
    );
}

/// One publisher must not be able to spend a node's disk, or the one thread
/// every other recording on it shares.
///
/// The version this replaces bounded a *gap* at four hours and bounded nothing
/// at all: a packet jumping just under the ceiling cost 3.1 MB of disk and
/// 115 ms of writer time, and a publisher sends fifty packets a second. Ten of
/// them wrote 28 MB in under a second; at an ordinary packet rate that is six
/// seconds of writer CPU per second of wall clock, which fills the volume and
/// starves every other participant's audio on the node while it does it. The
/// budget is per track and is not given back, so the whole of what one stream
/// can cost is bounded once rather than bounded per packet.
#[test]
fn one_publisher_cannot_spend_the_disk_and_the_writer_thread_on_silence() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));

    // Twenty packets, each three hours and fifty minutes ahead of the last:
    // under the old four-hour ceiling, so each of them was filled rather than
    // skipped, and repeatable for as long as the publisher cares to keep
    // sending.
    let stride = 48_000 * (3 * 3_600 + 50 * 60);
    let started = Instant::now();
    for index in 0..20u64 {
        tap.packet(index, index * stride, 48_000, payload(), Instant::now());
    }
    drop(tap);
    drop(recorder);

    let all = everything(taken);
    let handing_over = started.elapsed();
    let written: u64 = all
        .iter()
        .map(|recording| {
            std::fs::metadata(&recording.local_path)
                .expect("the audio")
                .len()
        })
        .sum();

    assert!(
        written < 8 * 1024 * 1024,
        "twenty packets from one publisher put {written} bytes of silence on this node's disk. \
         What one stream can cost has to be bounded over its whole life and not per packet, or a \
         single `roomJoin` token fills the volume -- and a node whose volume is full stops \
         recording, which is the failure that costs meetings rather than bytes"
    );
    assert!(
        handing_over < Duration::from_secs(10),
        "twenty packets took {handing_over:?} of the one thread every recording on this node \
         shares, so every other participant's audio was queued behind one publisher's arithmetic"
    );
    for recording in &all {
        well_formed(recording);
    }
}

/// A publisher whose extended timestamp comes back near `u64::MAX` costs its
/// own recording and nothing else.
///
/// **This is the shape that used to stop the node.** str0m extends a 32-bit RTP
/// timestamp against the one before it and reads a step of more than half the
/// range as a wrap *backwards*, answering with a number near `u64::MAX` -- so
/// two packets, `0` and `0x8000_0002`, are enough to produce one. The writer
/// turned that into a granule position of about 1.8e19, `granule * 1_000`
/// overflowed a `u64`, and the one thread every recording on this process
/// shares panicked and died. After that every `Tap::packet` hit a closed
/// channel, `/healthz` went on saying `recording=on dropped_audio_packets=0`,
/// and the node recorded nothing at all for the rest of its life -- silently.
///
/// So this test is mostly about the *second* participant.
#[test]
fn a_timestamp_that_wrapped_backwards_costs_one_recording_and_never_the_node() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());

    let hostile = recorder.track(spec("PA_000000000001"));
    hostile.packet(0, 0, 48_000, payload(), Instant::now());
    // What str0m's extension answers for a second packet of 0x8000_0002 after a
    // first of 0: the step is more than half the range, so it is read as a wrap
    // backwards and comes back just under 2^64. Its own test
    // `extend_u32_with_0_prev` asserts exactly this.
    hostile.packet(1, 0xFFFF_FFFF_8000_0002, 48_000, payload(), Instant::now());
    drop(hostile);

    // A second participant joins afterwards, as one did in the probe that found
    // this: fifty ordinary packets, one second of audio.
    let ordinary = recorder.track(spec("PA_000000000002"));
    for index in 0..50u64 {
        ordinary.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(ordinary);

    // Waited for rather than assumed. The writer takes its queue in order, so a
    // recording from the participant who joined second is proof that everything
    // the first one sent has already been dealt with -- and the assertions below
    // are then about work that has happened rather than about work that has not
    // started. It is also where a writer thread that died shows up: nothing ever
    // arrives, and `finished` says so.
    let mut all = Vec::new();
    let ours = loop {
        let recording = finished(&mut taken);
        if recording.participant == "PA_000000000002" {
            break recording;
        }
        all.push(recording);
    };

    assert!(
        recorder.alive(),
        "the writer thread died on a packet from the network, so this node is holding meetings \
         and recording none of them -- and nothing but this flag would say so"
    );
    assert_eq!(
        recorder.abandoned(),
        0,
        "a timestamp a publisher chose made the writer give up on a recording; whatever it hit \
         has to be refused before it reaches a file, not caught after it"
    );
    assert_eq!(
        ours.duration_ms,
        Some(1_000),
        "the participant who joined after the bad one has to have all of their second of audio"
    );

    all.push(ours);
    drop(recorder);
    all.extend(everything(taken));

    for recording in &all {
        well_formed(recording);
    }
}

/// Timestamps that go backwards do not produce a file that lies about itself.
///
/// The packets are put back in order by *sequence* number, and nothing makes
/// the timestamps follow: a re-based encoder, a wrap read the wrong way round,
/// or a sender simply making them up. A granule position that decreases is a
/// malformed Ogg stream (RFC 3533) and the row beside it claims a length no
/// decoder will agree with -- which is the alignment failure this whole crate
/// is built around, arriving from the other direction.
#[test]
fn timestamps_that_go_backwards_never_reach_the_file() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));

    for index in 0..500u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    // The sequence carries on and the clock starts again at zero.
    for index in 0..500u64 {
        tap.packet(500 + index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);

    let recording = finished(&mut taken);
    let survey = well_formed(&recording);

    assert_eq!(
        recording.duration_ms,
        Some(10_000),
        "five hundred twenty-millisecond packets is ten seconds. The five hundred that went \
         backwards must not have been written: they would have carried the granule back down the \
         file and left a row claiming ten seconds beside a file that hands a decoder twenty"
    );
    assert_eq!(survey.granule, 10_000 * 48);
}

/// A missing packet leaves a hole of exactly its own length, and everything
/// after it stays where it belongs.
#[test]
fn a_packet_that_never_arrives_costs_its_own_twenty_milliseconds_and_nothing_else() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));

    // Fifty packets with the twentieth lost on the way, and the thirtieth and
    // thirty-first swapped -- which is an ordinary minute of the internet.
    for index in 0..50u64 {
        if index == 20 {
            continue;
        }
        let sequence = match index {
            30 => 31,
            31 => 30,
            other => other,
        };
        tap.packet(sequence, sequence * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);

    let recording = finished(&mut taken);

    assert_eq!(
        recording.duration_ms,
        Some(1_000),
        "fifty twenty-millisecond slots is a second, whether or not every packet arrived: the \
         hole belongs where the packet was, and closing it up would move every later word early"
    );

    well_formed(&recording);
}

/// Somebody who leaves and comes back is two files, and the time they were away
/// is in the offsets rather than inside either of them.
#[test]
fn a_rejoin_is_two_recordings_and_the_gap_between_them_is_in_the_offsets() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    let started = now_ms();
    recorder.meeting_started("standup", "SE_000000000001", started);

    let first = recorder.track(spec("PA_000000000001"));
    for index in 0..25u64 {
        first.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(first);
    let one = finished(&mut taken);

    // The connection comes back. A rejoin inside the grace period is the same
    // meeting, and the RTP stream starts again wherever the new sender's clock
    // happens to be -- which is why the second file's own clock starts at zero
    // and its place in the meeting is `offset_ms`.
    std::thread::sleep(Duration::from_millis(120));
    let again = recorder.track(spec("PA_000000000001"));
    for index in 0..25u64 {
        again.packet(
            index,
            7_000_000 + index * TICK,
            48_000,
            payload(),
            Instant::now(),
        );
    }
    drop(again);
    let two = finished(&mut taken);

    assert_eq!(
        (one.segment, two.segment),
        (0, 1),
        "the two stretches have to be two files of the same participant in the same meeting, or \
         the second overwrites the first's row"
    );
    assert_ne!(one.local_path, two.local_path);
    assert_eq!(one.duration_ms, Some(500));
    assert_eq!(
        two.duration_ms,
        Some(500),
        "the second file holds what was said after the rejoin and not the silence before it: an \
         RTP stream that starts again brings a clock of its own, and treating its first timestamp \
         as time already elapsed would put two hours of nothing at the front of a recording"
    );
    assert!(
        two.offset_ms >= one.offset_ms + 100,
        "where the second stretch sits in the meeting is what says how long they were away, and \
         it is the offset that carries it: {} then {}",
        one.offset_ms,
        two.offset_ms
    );
    well_formed(&one);
    well_formed(&two);
}

/// Audio published into a room where no meeting is running is not written down.
#[test]
fn somebody_alone_in_a_room_is_not_a_meeting_and_is_not_recorded() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    // No `meeting_started`: this is the first person into a room with a floor
    // of two, publishing while they wait for the second.
    let tap = recorder.track(spec("PA_000000000001"));
    for index in 0..50u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);
    drop(recorder);

    assert!(
        taken.try_recv().is_err(),
        "a meeting had not begun, so there was nothing for this audio to be part of"
    );
    assert!(
        disk::scan(directory.path(), "ND_one").is_empty(),
        "and nothing was left on the disk for a later start to find and upload"
    );
}

/// A meeting ending closes every file of it, even for somebody still
/// publishing into the room.
#[test]
fn a_meeting_ending_closes_the_recording_of_whoever_is_still_talking() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));
    for index in 0..25u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }

    // The room drops below its floor and the grace period runs out. One person
    // is still there and still sending.
    recorder.meeting_ended("standup", "SE_000000000001");
    let recording = finished(&mut taken);

    assert_eq!(recording.meeting, "SE_000000000001");
    assert_eq!(recording.duration_ms, Some(500));
    well_formed(&recording);

    for index in 25..50u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);
    drop(recorder);

    assert!(
        taken.try_recv().is_err(),
        "what somebody says after a meeting has ended belongs to no meeting, so there is nothing \
         to hang a second recording off"
    );
}

/// The whole crash story, on a file the real writer produced.
#[test]
fn a_recording_the_process_died_during_is_finished_by_the_next_start() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, mut taken) = Recorder::start(settings(directory.path())).expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));
    for index in 0..100u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);
    let whole = finished(&mut taken);
    drop(recorder);

    // What a power cut leaves: the sidecar still says the file is open, the
    // audio has a page that never finished being written, and nothing has told
    // the database anything.
    let sidecar_path = whole.local_path.with_extension("json");
    let mut sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar_path).expect("the sidecar")).expect("json");
    sidecar["complete"] = serde_json::Value::Bool(false);
    std::fs::write(
        &sidecar_path,
        serde_json::to_vec(&sidecar).expect("the sidecar"),
    )
    .expect("the sidecar goes back");

    let mut audio = std::fs::read(&whole.local_path).expect("the audio");
    audio.truncate(audio.len() - 40);
    audio.extend_from_slice(b"OggS\x00\x00half a page");
    std::fs::write(&whole.local_path, &audio).expect("the torn audio");

    // The node comes back.
    let found = disk::scan(directory.path(), "ND_two");

    assert_eq!(
        found.len(),
        1,
        "the recording on the disk was not recovered"
    );
    let recovered = &found[0];
    assert_eq!(recovered.id, whole.id, "and it is the same recording");
    assert_eq!(
        recovered.node.as_deref(),
        Some("ND_two"),
        "adopted by the node that can see the file, so its upload is offered to a process that \
         exists"
    );
    let duration = recovered.duration_ms.expect("a length");
    assert!(
        duration > 0 && duration <= 2_000,
        "what survived the tear is whole and what did not is gone; {duration} ms is neither"
    );

    let survey = well_formed(recovered);
    assert_eq!(
        survey.granule,
        duration as u64 * 48,
        "with a clock that agrees with the row"
    );
}

/// A recording that outgrows one object rolls over into the next segment, and
/// the segments do not overlap.
#[test]
fn a_recording_that_grows_very_large_becomes_several_objects_in_order() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (recorder, taken) = Recorder::start(Settings {
        // Small enough that a couple of seconds of audio crosses it. The
        // real ceiling is hundreds of megabytes and no ordinary meeting
        // reaches it; what it bounds is the meeting nobody leaves.
        max_segment_bytes: 4_000,
        ..settings(directory.path())
    })
    .expect("a recorder");

    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));
    for index in 0..300u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);
    drop(recorder);

    let all = everything(taken);
    assert!(
        all.len() > 1,
        "three hundred packets past a four-kilobyte ceiling should have rolled over at least once"
    );

    let numbers: Vec<u32> = all.iter().map(|recording| recording.segment).collect();
    assert_eq!(
        numbers,
        (0..all.len() as u32).collect::<Vec<u32>>(),
        "the segments have to be numbered from zero without gaps, because that is what makes \
         `(meeting, participant, segment)` name one file exactly once"
    );
    assert!(
        all[0].bytes.unwrap_or_default() >= 4_000,
        "the first file should have been rolled over once it reached the ceiling"
    );

    // The real question about a rollover is whether it loses or repeats a
    // packet at the seam, and neither an offset nor a byte count answers it:
    // the offsets come off the wall clock, and this test hands six seconds of
    // audio over in a few milliseconds. What answers it is the audio. Three
    // hundred twenty-millisecond packets is six seconds, whatever it is split
    // into.
    let held: i64 = all
        .iter()
        .map(|recording| {
            recording
                .duration_ms
                .expect("a finished recording has a length")
        })
        .sum();
    assert_eq!(
        held, 6_000,
        "the segments hold {held} ms between them out of six thousand handed over: a rollover \
         either dropped a packet at the seam or wrote one into both files, and either way the \
         segments no longer line up end to end"
    );

    for (before, after) in all.iter().zip(all.iter().skip(1)) {
        assert!(
            after.offset_ms >= before.offset_ms,
            "segment {} starts before segment {} does",
            after.segment,
            before.segment
        );
    }
    for recording in &all {
        well_formed(recording);
    }
}

/// The sidecar goes down before the audio, and if it cannot go down the audio
/// is not written at all.
///
/// **The ordering is the whole of how a crash costs nothing**, and until this
/// test it was pinned by nothing: `disk`'s own test lays a directory out by
/// hand and watches `scan` walk it, which says what a stray `.ogg` costs but
/// says nothing about which of the two files this writer creates first. Moving
/// `write_sidecar` after `open_file` left every recording test green.
///
/// It is observable because the two answers differ on disk. The scan walks
/// sidecars -- a sidecar is the only thing that says which meeting and which
/// participant a file belongs to -- so an `.ogg` that reached the disk without
/// one would sit on the volume for ever: never uploaded, never announced, never
/// cleaned up, and unidentifiable by hand. Here the sidecar cannot be written,
/// because the name it stages itself under is a directory; the audio must
/// therefore not exist either.
#[test]
fn a_recording_whose_sidecar_will_not_write_leaves_no_audio_behind() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (audio, sidecar) = disk::paths(directory.path(), "SE_000000000001", "PA_000000000001", 0);

    // `disk::write_sidecar` writes to `<name>.json.part` and renames it into
    // place, so a directory sitting on that name is a sidecar that cannot be
    // written and an audio file that perfectly well could be.
    std::fs::create_dir_all(sidecar.with_extension("json.part")).expect("the blocked name");

    let (recorder, taken) = Recorder::start(settings(directory.path())).expect("a recorder");
    recorder.meeting_started("standup", "SE_000000000001", now_ms());
    let tap = recorder.track(spec("PA_000000000001"));
    for index in 0..50u64 {
        tap.packet(index, index * TICK, 48_000, payload(), Instant::now());
    }
    drop(tap);
    drop(recorder);

    assert!(
        everything(taken).is_empty(),
        "a recording nothing on the disk describes was handed on to be uploaded"
    );
    assert!(
        !audio.exists(),
        "the audio was written without the sidecar that says what it is. The startup scan walks \
         sidecars, so this file would sit on the volume for ever -- never uploaded, never \
         announced, never cleaned up, and impossible to identify by hand. The sidecar goes first \
         and the audio does not open without it"
    );
    assert!(
        disk::scan(directory.path(), "ND_two").is_empty(),
        "and there is nothing here a later start could pick up either"
    );
}
