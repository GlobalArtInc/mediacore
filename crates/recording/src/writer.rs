//! The one thread that touches a disk, and everything it has to decide.
//!
//! Packets arrive on the media path and are handed here through a queue. This
//! thread does the rest: puts them back in order, decides which meeting they
//! belong to, opens files, writes pages, rolls a file over when it gets large,
//! closes files, writes sidecars. Nothing it does can be waited on by anybody
//! carrying a call -- see [`crate::Tap`] for the half that runs on the media
//! path, which is a bounds check and a queue push.
//!
//! # Putting packets back in order
//!
//! RTP arrives out of order, arrives twice, and does not always arrive. Ogg
//! needs packets in increasing order and gives no way to go back and insert
//! one, so every packet is held for [`crate::Settings::reorder_hold`] before it
//! is written and the ones held are kept sorted by their extended sequence
//! number. A packet that turns up after its place has gone past is counted and
//! dropped -- writing it would mean either a granule position that goes
//! backwards, which is a corrupt file, or rewriting a page that is already on
//! the disk.
//!
//! The hold is not a jitter buffer and does not have to be one: nobody is
//! listening to this in real time. It is a hundred milliseconds because that
//! is comfortably longer than a reordering across a network and short enough
//! that the file on disk is never far behind the meeting.
//!
//! A packet that never arrives leaves a hole, and the hole is *correct*: the
//! next packet carries its own timestamp, so the hole is exactly as long as
//! what was lost and the file stays aligned with the meeting. A recorder that
//! closed the hole by shifting everything after it forward would make every
//! later word early by the length of every earlier loss.
//!
//! Holes are written as silence rather than left as a jump in the granule
//! position -- see [`crate::opus::silence`] for why the difference matters and
//! what it costs, which is three bytes per twenty milliseconds of nothing.
//!
//! # Which meeting a packet belongs to
//!
//! A track is recorded because a meeting is happening in the room it is
//! published to -- not because somebody is publishing. The two are different in
//! a room with a floor of two: the first person to arrive publishes, and there
//! is no meeting until the second arrives. Their audio in between is one person
//! alone in a room, and it belongs to no meeting, so it is not written down.
//!
//! Packets that arrive before a meeting is bound sit in the reorder buffer and
//! are dropped when they age out of it, which is what makes the boundary sharp
//! at the start and cheap in the middle: the meeting is bound a few
//! milliseconds after it begins -- the announcement travels through an
//! in-process queue -- and everything held from before it is discarded rather
//! than backdated.
//!
//! # A timestamp is a number somebody else chose
//!
//! Everything above is arithmetic on the sender's clock, and the sender is on
//! the other side of the internet. The extended timestamp that arrives here is
//! whatever the publisher's encoder said, run through str0m's roll-over
//! extension -- which reads a step of more than half the 32-bit range as a wrap
//! *backwards* and answers with a number near `u64::MAX`. Two packets are
//! enough to produce one. So this thread treats a timestamp the way it treats
//! any other input from a stranger:
//!
//! * A timestamp that goes **backwards** inside an open file is refused. Ogg
//!   granule positions have to increase (RFC 3533); writing one that does not
//!   is a malformed stream, and the row beside it would then describe a file
//!   that does not exist -- a length a transcript would be aligned against and
//!   a decoder would disagree with.
//! * A timestamp that jumps **further forward than the silence this track may
//!   still be charged for** does not become that much silence. The file is
//!   closed where it is and the next packet starts a new segment with a clock
//!   of its own, exactly as a rejoin does, with the gap showing in the offsets
//!   rather than as hours of nothing on the disk. See [`SILENCE_BUDGET`].
//! * A participant may only cost so many files in one meeting. See
//!   [`MAX_SEGMENTS`].
//!
//! And whatever gets past all of that, a panic on this thread costs the
//! recording it happened in and not the node: see [`Writing::shielded`]. This
//! thread is shared by every recording on the process, and one publisher must
//! not be able to stop it.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mediacore_session::{Recording, recording_id};

use crate::disk::{self, GRANULE_RATE, Sidecar};
use crate::ogg::OggWriter;
use crate::{Message, Settings, TrackSpec};

/// How long the thread waits for a packet before looking at the clock.
///
/// It is not a poll of anything -- a packet wakes it at once. It is how often
/// the reorder buffers are drained when a meeting has gone quiet, which is the
/// only case where nothing is arriving to drain them.
const IDLE_TICK: Duration = Duration::from_millis(25);

/// A meeting this node is recording, by room.
struct Meeting {
    id: String,
    started_at_ms: i64,
}

/// One packet on its way to a file.
struct Held {
    time: u64,
    payload: Arc<[u8]>,
    at: Instant,
}

/// One open file.
struct Open {
    recording: Recording,
    sidecar: PathBuf,
    room: String,
    identity: String,
    writer: OggWriter<BufWriter<File>>,
    /// The extended RTP timestamp of the first packet in this file, rescaled to
    /// 48 kHz. Every granule position in the file is measured from it.
    origin: u64,
    /// Where the last packet written ended, in the same units. A packet that
    /// does not start here is a gap, and the page is broken before it so that
    /// the gap is exactly where it says it is.
    end: u64,
}

/// One track being recorded.
struct Track {
    spec: TrackSpec,
    pending: BTreeMap<u64, Held>,
    /// The highest sequence number written. Anything at or below it has had its
    /// turn.
    written_through: Option<u64>,
    open: Option<Open>,
    /// The meeting the open file belongs to, so that a meeting ending closes it
    /// and a new one starts a new file.
    meeting: Option<String>,
    /// How much silence this track may still be charged for, in 48 kHz
    /// samples. See [`SILENCE_BUDGET`].
    silence_left: u64,
    /// Whether this track has ever managed to open a file, so that a track
    /// whose audio was never part of any meeting can say so on the way out.
    made_a_file: bool,
    late: u64,
    unmeeting: u64,
    /// Packets whose timestamp went backwards inside an open file.
    backwards: u64,
    /// Packets dropped because this participant has used up the files one
    /// meeting allows them.
    refused: u64,
    /// Packets that could not start a file because its sidecar would not write.
    unwritable: u64,
}

/// Everything the thread owns.
pub(crate) struct Writing {
    settings: Settings,
    finished: tokio::sync::mpsc::UnboundedSender<Recording>,
    queued: Arc<std::sync::atomic::AtomicUsize>,
    meetings: HashMap<String, Meeting>,
    tracks: HashMap<u64, Track>,
    /// How many files each participant has produced in each meeting, so that
    /// `(meeting, participant, segment)` names one file exactly once even when
    /// somebody publishes two microphones at the same time.
    segments: HashMap<(String, String), u32>,
    /// Recordings this thread gave up on because something in them panicked.
    /// On `/healthz`; see [`Writing::shielded`].
    abandoned: Arc<AtomicU64>,
}

impl Writing {
    pub(crate) fn new(
        settings: Settings,
        finished: tokio::sync::mpsc::UnboundedSender<Recording>,
        queued: Arc<std::sync::atomic::AtomicUsize>,
        abandoned: Arc<AtomicU64>,
    ) -> Self {
        Self {
            settings,
            finished,
            queued,
            meetings: HashMap::new(),
            tracks: HashMap::new(),
            segments: HashMap::new(),
            abandoned,
        }
    }

    /// The thread's whole life.
    pub(crate) fn run(mut self, messages: &Receiver<Message>) {
        loop {
            match messages.recv_timeout(IDLE_TICK) {
                Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(message) => {
                    let blamed = message.track();
                    self.shielded(blamed, move |writing| writing.handle(message));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            self.drain(Instant::now());
        }

        // Every file this thread holds is closed on the way out, so a planned
        // stop leaves nothing for the next start to repair. A kill leaves
        // exactly what a crash leaves, which is what `disk::scan` is for.
        let keys: Vec<u64> = self.tracks.keys().copied().collect();
        for key in keys {
            self.shielded(Some(key), |writing| writing.end_track(key));
        }
    }

    /// Runs one step of this thread's work, and turns a panic inside it into
    /// one lost recording rather than a lost node.
    ///
    /// **This thread is the only one that writes files, and every recording on
    /// this process goes through it.** A panic here used to take the lot: the
    /// thread died, every later `Tap::packet` hit a closed channel, and the
    /// node went on reporting `recording=on dropped_audio_packets=0` for the
    /// rest of its life while recording nothing at all. The input that reaches
    /// this thread comes off the network from somebody else's encoder, so
    /// "nothing here can panic" is not a property anybody can keep -- what can
    /// be kept is that a panic costs the track it happened in, is counted, and
    /// is said out loud.
    ///
    /// The blamed track is dropped rather than repaired, because whatever state
    /// the panic left behind is exactly the state that would panic again on the
    /// next packet. Its half-written file stays on the disk and is finished by
    /// the next start, which is the same path a power cut takes.
    fn shielded(&mut self, blamed: Option<u64>, step: impl FnOnce(&mut Self)) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(self)));
        if outcome.is_ok() {
            return;
        }

        self.abandoned.fetch_add(1, Ordering::Relaxed);

        let dropped = blamed.and_then(|key| self.tracks.remove(&key));
        match dropped {
            Some(track) => tracing::error!(
                identity = %track.spec.identity,
                room = %track.spec.room,
                track = %track.spec.track,
                "the recording writer hit something it could not handle on this track; the track \
                 is dropped and whatever is on the disk is left for the next start to finish. \
                 Every other recording on this node carries on. Watch abandoned_recordings= on \
                 /healthz"
            ),
            None => tracing::error!(
                "the recording writer hit something it could not handle; the work it was doing is \
                 lost and every recording on this node carries on. Watch abandoned_recordings= on \
                 /healthz"
            ),
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Meeting {
                room,
                meeting,
                started_at_ms,
            } => {
                tracing::info!(%room, %meeting, "recording this meeting");
                self.meetings.insert(
                    room,
                    Meeting {
                        id: meeting,
                        started_at_ms,
                    },
                );
            }
            Message::MeetingOver { room, meeting } => {
                // Every track of that room, not only the ones with a file open:
                // a track whose packets are all still in its reorder buffer has
                // no file yet, and what is in that buffer was said during the
                // meeting and belongs in its record.
                let keys: Vec<u64> = self
                    .tracks
                    .iter()
                    .filter(|(_, track)| {
                        track.spec.room == room || track.meeting.as_deref() == Some(&meeting)
                    })
                    .map(|(key, _)| *key)
                    .collect();

                // Drained while the meeting is still bound, so those packets
                // land in it rather than in nothing.
                for key in &keys {
                    self.drain_track(*key, Instant::now(), true);
                }

                if self
                    .meetings
                    .get(&room)
                    .is_some_and(|held| held.id == meeting)
                {
                    self.meetings.remove(&room);
                }

                // Then every file of that meeting closes with it, whoever is
                // still publishing. A room with a floor of two goes on carrying
                // the one person left in it, and what they say next is not part
                // of the meeting that just ended.
                //
                // By the meeting and not by the room, unlike the drain above: a
                // track that has just been drained into this meeting names it,
                // and one that names another meeting is not this one's to
                // close. The two can only differ if these announcements arrive
                // out of order, which one queue with one consumer does not do --
                // and a rule that is right whether or not that holds is worth
                // the extra line.
                for key in keys {
                    if self
                        .tracks
                        .get(&key)
                        .is_some_and(|track| track.meeting.as_deref() == Some(&meeting))
                    {
                        self.close(key);
                    }
                }

                self.segments.retain(|(held, _), _| held != &meeting);
            }
            Message::Open { key, spec } => {
                self.tracks.insert(
                    key,
                    Track {
                        spec: *spec,
                        pending: BTreeMap::new(),
                        written_through: None,
                        open: None,
                        meeting: None,
                        silence_left: SILENCE_BUDGET,
                        made_a_file: false,
                        late: 0,
                        unmeeting: 0,
                        backwards: 0,
                        refused: 0,
                        unwritable: 0,
                    },
                );
            }
            Message::Packet {
                key,
                sequence,
                time,
                clock_rate,
                payload,
                at,
            } => {
                self.queued
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

                let Some(track) = self.tracks.get_mut(&key) else {
                    return;
                };

                if track
                    .written_through
                    .is_some_and(|through| sequence <= through)
                {
                    track.late += 1;
                    return;
                }

                track.pending.entry(sequence).or_insert(Held {
                    time: rescale(time, clock_rate),
                    payload,
                    at,
                });
            }
            Message::Close { key } => self.end_track(key),
            Message::Stop => {}
        }
    }

    /// Writes out every packet that has waited long enough, on every track.
    ///
    /// One track at a time behind the shield, so that a track that cannot be
    /// written does not stop the ones that can -- and does not come back on the
    /// next tick to fail again, because the shield drops it.
    fn drain(&mut self, now: Instant) {
        let keys: Vec<u64> = self.tracks.keys().copied().collect();

        for key in keys {
            self.shielded(Some(key), |writing| writing.drain_track(key, now, false));
        }
    }

    fn drain_track(&mut self, key: u64, now: Instant, everything: bool) {
        let hold = self.settings.reorder_hold;

        loop {
            let Some(track) = self.tracks.get_mut(&key) else {
                return;
            };
            let Some((sequence, held)) = track.pending.first_key_value() else {
                return;
            };
            if !everything && now.duration_since(held.at) < hold {
                return;
            }

            let sequence = *sequence;
            let Some(held) = track.pending.remove(&sequence) else {
                return;
            };
            track.written_through = Some(sequence);

            self.write(key, held);
        }
    }

    /// Puts one packet into its file, opening the file if this is the first.
    fn write(&mut self, key: u64, held: Held) {
        let Some(track) = self.tracks.get(&key) else {
            return;
        };

        let Some(meeting) = self.meetings.get(&track.spec.room) else {
            // No meeting in this room: this is somebody alone in a room, or
            // somebody still publishing after a meeting ended. Neither is part
            // of a meeting and neither is recorded.
            if let Some(track) = self.tracks.get_mut(&key) {
                track.unmeeting += 1;
            }
            return;
        };
        let meeting_id = meeting.id.clone();
        let meeting_started = meeting.started_at_ms;

        // A file that belongs to a meeting that has been replaced -- which only
        // happens if a meeting ended and another began before this packet was
        // drained -- is closed rather than continued into the wrong record.
        if track
            .meeting
            .as_deref()
            .is_some_and(|held| held != meeting_id)
        {
            self.close(key);
        }

        // What the open file can be asked to do with this packet's timestamp.
        // Both answers are about a number the publisher chose, so both are
        // refusals rather than arithmetic.
        let mut jumped = None;
        if let Some(track) = self.tracks.get_mut(&key)
            && let Some(open) = track.open.as_ref()
        {
            let start = held.time.saturating_sub(open.origin);

            if start < open.end {
                // A granule position that goes backwards is a malformed Ogg
                // stream, and the row beside it would claim a length no decoder
                // agrees with. The packets are ordered by sequence and nothing
                // makes the timestamps follow -- a re-based encoder, a wrap
                // read the wrong way round, or a sender simply making them up.
                track.backwards += 1;
                if track.backwards == 1 {
                    tracing::warn!(
                        recording = %open.recording.id,
                        identity = %track.spec.identity,
                        "this publisher's timestamps went backwards inside an open file; those \
                         packets are dropped, because writing them would leave a file whose \
                         granule positions decrease and a row claiming a length the audio does \
                         not have"
                    );
                }
                return;
            }

            if start - open.end > track.silence_left {
                jumped = Some((open.recording.id.clone(), start - open.end));
            }
        }

        // Further ahead than this track may still be charged for. The file is
        // closed where it is and this packet starts the next segment with a
        // clock of its own -- the same shape as a rejoin, with the gap in the
        // offsets rather than as hours of silence on the disk.
        if let Some((recording, gap)) = jumped {
            tracing::info!(
                %recording,
                gap_ms = disk::duration_ms(gap),
                "this publisher's timestamps jumped further ahead than the silence this track may \
                 still be charged for; the file is closed here and what comes next is a new \
                 segment, so the gap is in the offsets and not on the disk"
            );
            self.close(key);
        }

        let Some(track) = self.tracks.get_mut(&key) else {
            return;
        };

        if track.open.is_none() {
            let Some(toc) = crate::opus::read(&held.payload) else {
                return;
            };

            let started_at_ms = now_ms();
            // Read and not yet spent: the number is what names the file, and a
            // number spent on a file that did not open is a name nothing ever
            // wrote and a hole in a sequence whose whole job is to name one
            // file exactly once. It goes up below, once there is a file.
            let number = self
                .segments
                .get(&(meeting_id.clone(), track.spec.participant.clone()))
                .copied()
                .unwrap_or(0);
            if number >= MAX_SEGMENTS {
                track.refused += 1;
                if track.refused == 1 {
                    tracing::warn!(
                        meeting = %meeting_id,
                        identity = %track.spec.identity,
                        participant = %track.spec.participant,
                        segments = number,
                        "this participant has produced every file one meeting allows them; the \
                         rest of their audio is not recorded. A meeting reaches this only through \
                         a publisher whose clock is nonsense or a connection that rejoined \
                         hundreds of times -- see MAX_SEGMENTS"
                    );
                }
                return;
            }

            let (audio, sidecar) = disk::paths(
                &self.settings.directory,
                &meeting_id,
                &track.spec.participant,
                number,
            );

            let recording = Recording {
                id: recording_id(),
                meeting: meeting_id.clone(),
                participant: track.spec.participant.clone(),
                account_id: track.spec.account_id.clone(),
                segment: number,
                local_path: audio.clone(),
                node: Some(self.settings.node.clone()),
                object_key: None,
                bytes: None,
                sample_rate: GRANULE_RATE as u32,
                channels: if toc.stereo { 2 } else { 1 },
                started_at_ms,
                ended_at_ms: None,
                // Where this file starts inside the meeting. A file opened by
                // the first packet after a rejoin starts where the rejoin did,
                // which is what lines it up against everybody else's.
                offset_ms: (started_at_ms - meeting_started).max(0),
                duration_ms: None,
            };

            // The sidecar goes down **before** the audio it describes, and the
            // audio is not opened at all if it would not go down. An audio file
            // with no sidecar beside it is one nothing can ever identify: the
            // startup scan walks sidecars, so a file that reached the disk
            // without one would sit there for ever, never uploaded, never
            // announced and never cleaned up. The other way round is
            // self-healing -- a sidecar whose audio is missing is dropped on
            // the next start -- which is why the order is this way and why a
            // sidecar that will not write stops the recording rather than being
            // logged and stepped over.
            if let Err(error) = write_sidecar(
                &sidecar,
                &track.spec.room,
                &track.spec.identity,
                &recording,
                false,
            ) {
                track.unwritable += 1;
                if track.unwritable == 1 {
                    tracing::error!(
                        identity = %track.spec.identity,
                        path = %sidecar.display(),
                        %error,
                        "this participant's audio cannot be recorded: its sidecar would not \
                         write, and audio with nothing beside it to say what it is would sit on \
                         this disk for ever"
                    );
                }
                return;
            }

            match open_file(&audio, toc.stereo) {
                Ok(writer) => {
                    tracing::info!(
                        recording = %recording.id,
                        meeting = %meeting_id,
                        identity = %track.spec.identity,
                        track = %track.spec.track,
                        segment = number,
                        path = %recording.local_path.display(),
                        "recording a participant's audio"
                    );

                    self.segments.insert(
                        (meeting_id.clone(), track.spec.participant.clone()),
                        number + 1,
                    );
                    track.meeting = Some(meeting_id);
                    track.made_a_file = true;
                    track.open = Some(Open {
                        sidecar,
                        room: track.spec.room.clone(),
                        identity: track.spec.identity.clone(),
                        recording,
                        writer,
                        origin: held.time,
                        end: 0,
                    });
                }
                Err(error) => {
                    tracing::error!(
                        identity = %track.spec.identity,
                        path = %audio.display(),
                        %error,
                        "this participant's audio cannot be recorded: the file would not open"
                    );
                    let _ = fs::remove_file(&sidecar);
                    return;
                }
            }
        }

        // Split so that the file and this track's silence budget can be held at
        // once: the fill spends the budget, and the budget is what bounds the
        // fill.
        let Track {
            open, silence_left, ..
        } = track;
        let Some(open) = open.as_mut() else {
            return;
        };
        let Some(toc) = crate::opus::read(&held.payload) else {
            return;
        };

        let start = held.time.saturating_sub(open.origin);

        // A gap: the sender stopped sending, which is what every browser does
        // while nobody is talking. It is written down as silence rather than
        // left as a jump in the granule -- a jump makes the file *report* the
        // right length and hand a decoder the wrong one, with every word after
        // the gap pushed forward into it.
        if start > open.end {
            // The channel count the file's header declared, not this packet's.
            // A sender that switches between mono and stereo mid-stream is odd
            // but allowed, and filling its silence with the other one would ask
            // a decoder to change channel count in the middle of nothing.
            //
            // The gap was measured against `silence_left` above and the file
            // was closed if it did not fit, so what is filled here is always
            // within the budget; the subtraction is what makes it a budget and
            // not a per-gap ceiling.
            *silence_left = silence_left.saturating_sub(fill_silence(
                open,
                start,
                open.recording.channels == 2,
            ));
        }

        let end = start + u64::from(toc.samples);
        // Whatever the fill could not express: less than one silence frame,
        // and nothing at all for a sender whose frames are twenty milliseconds
        // like everybody's.
        let gap = start != open.end;

        if let Err(error) = open.writer.packet(&held.payload, end, gap) {
            tracing::error!(
                recording = %open.recording.id,
                %error,
                "a page could not be written; this recording stops here and what is on the disk \
                 is kept"
            );
            self.close(key);
            return;
        }
        open.end = end;

        if open.writer.written() >= self.settings.max_segment_bytes {
            tracing::info!(
                recording = %open.recording.id,
                bytes = open.writer.written(),
                "this recording has reached the size a single object may be; rolling over into \
                 the next segment"
            );
            self.close(key);
        }
    }

    /// Closes whatever file a track has open and hands the recording on.
    fn close(&mut self, key: u64) {
        let Some(track) = self.tracks.get_mut(&key) else {
            return;
        };
        let Some(open) = track.open.take() else {
            track.meeting = None;
            return;
        };
        track.meeting = None;

        let Open {
            mut recording,
            sidecar,
            room,
            identity,
            mut writer,
            ..
        } = open;

        if let Err(error) = writer.flush(true) {
            tracing::error!(
                recording = %recording.id,
                %error,
                "the last page of this recording could not be written"
            );
        }

        let granule = writer.granule();
        // The one place this crate waits for a disk to say yes. It is once per
        // file, on this thread, with this participant's part of the meeting
        // already over -- and it is the difference between a crash costing the
        // last second of a recording and costing the buffer's worth of it.
        let landed = writer
            .into_inner()
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)
            .and_then(|file| file.sync_all());
        if let Err(error) = landed {
            tracing::error!(
                recording = %recording.id,
                %error,
                "this recording could not be flushed to the disk; what reached it is kept"
            );
        }

        let duration_ms = disk::duration_ms(granule);
        recording.duration_ms = Some(duration_ms);
        // The end of the audio rather than the moment this ran. They differ
        // when a meeting ends under somebody who stopped sending long before --
        // a muted participant with discontinuous transmission on -- and the
        // honest answer is where the sound stops.
        recording.ended_at_ms = Some(recording.started_at_ms + duration_ms);
        recording.bytes = fs::metadata(&recording.local_path)
            .map(|metadata| metadata.len())
            .ok();

        if granule == 0 {
            tracing::info!(
                recording = %recording.id,
                %identity,
                "this participant published a track and never sent any audio; nothing to upload"
            );
            let _ = fs::remove_file(&recording.local_path);
            let _ = fs::remove_file(&sidecar);
            return;
        }

        // A closing sidecar that will not write leaves the file marked
        // unfinished, which the next start repairs from the file itself -- so
        // this one is logged and stepped over, unlike the sidecar that opens a
        // recording.
        if let Err(error) = write_sidecar(&sidecar, &room, &identity, &recording, true) {
            tracing::error!(
                recording = %recording.id,
                path = %sidecar.display(),
                %error,
                "a finished recording's sidecar could not be rewritten; the file is whole and the \
                 next start will finish the record from it"
            );
        }

        tracing::info!(
            recording = %recording.id,
            meeting = %recording.meeting,
            %identity,
            duration_ms,
            bytes = recording.bytes,
            late = track.late,
            "a recording is finished"
        );

        // Unbounded, and it has to be: the thing at the other end talks to a
        // database, and a full queue here would stop this thread and with it
        // every recording on this node. What is queued is a handful of small
        // structs, and the same facts are on the disk beside the audio -- so
        // the worst a consumer that never drains can do is waste a few
        // kilobytes and let the next start find the files instead.
        let _ = self.finished.send(recording);
    }

    /// A track has stopped publishing: everything held is written, and the file
    /// closed.
    fn end_track(&mut self, key: u64) {
        if !self.tracks.contains_key(&key) {
            return;
        }

        // Everything, not just what has waited long enough: nothing more is
        // coming, so holding the tail back for a reordering that cannot happen
        // would only throw it away.
        self.drain_track(key, Instant::now(), true);
        self.close(key);

        let Some(track) = self.tracks.remove(&key) else {
            return;
        };

        // A track that never produced a file at all and had packets to produce
        // one with is worth a line somebody will actually see. It is the shape
        // of two different things: somebody alone in a room that never reached
        // its floor, which is ordinary, and a room switched to recorded while a
        // meeting was already running, which looks like recording being on and
        // is not. Neither has anywhere else to show up.
        if !track.made_a_file && track.unmeeting > 0 {
            tracing::info!(
                room = %track.spec.room,
                identity = %track.spec.identity,
                track = %track.spec.track,
                packets = track.unmeeting,
                "this track's audio was never part of a meeting, so none of it was recorded: \
                 either no meeting was running in the room, or the room was asked to be recorded \
                 while one already was -- recording follows the meeting, and begins with the next \
                 one"
            );
        }

        if track.late > 0
            || track.unmeeting > 0
            || track.backwards > 0
            || track.refused > 0
            || track.unwritable > 0
        {
            tracing::debug!(
                identity = %track.spec.identity,
                track = %track.spec.track,
                late = track.late,
                outside_a_meeting = track.unmeeting,
                backwards = track.backwards,
                past_the_segment_limit = track.refused,
                sidecar_would_not_write = track.unwritable,
                "packets this track did not put in a file"
            );
        }
    }
}

/// How much silence **one track** may be charged for over its whole life: four
/// hours of it.
///
/// A budget and not a per-gap ceiling, and the difference is the whole point. A
/// ceiling bounds one packet; a publisher sends fifty a second, and each of them
/// can jump just under the ceiling, so a ceiling alone bounds nothing. Measured
/// on the version this replaces: one packet with a three-hour-fifty-minute jump
/// cost 115 ms of writer time and 3.1 MB on the disk, repeatable per packet --
/// six seconds of writer CPU per second of wall clock at an ordinary packet
/// rate, which fills the disk and starves every other recording on the node
/// while it does it.
///
/// Four hours because a participant muted for four hours in an all-day room is
/// ordinary and this is not here to refuse them. Silence costs about 225 bytes
/// a second, so the whole budget is a little over three megabytes and a tenth
/// of a second of writing, once per track. Past it the file is closed and the
/// next packet starts a new segment with a clock of its own -- not left as a
/// jump in the granule, which would make the file *report* a length no decoder
/// can reach and break the one invariant every recording test checks.
const SILENCE_BUDGET: u64 = GRANULE_RATE * 3_600 * 4;

/// How many files one participant may produce in one meeting.
///
/// The segment number is what makes `(meeting, participant, segment)` name a
/// file exactly once, and every file is a row, an object and an event. Files
/// come from three places: the size ceiling, a rejoin, and a clock that jumped
/// past its silence budget. Only the third can be driven from the far end of a
/// network, and without a cap on it a publisher alternating two timestamps
/// turns fifty packets a second into fifty rows, fifty uploads and fifty
/// announcements.
///
/// Two hundred and fifty-six is far above anything an honest meeting reaches:
/// at `max_segment_mb` it is tens of gigabytes of one person's audio, and as
/// rejoins it is a connection that came back every two minutes for eight hours.
/// Past it that participant's audio stops being recorded, with a line in the
/// log naming them.
const MAX_SEGMENTS: u32 = 256;

/// Writes silence up to `start`, in whole frames, and says how much it wrote.
///
/// The caller has already checked the gap against the track's remaining budget
/// and closed the file if it did not fit, so this never loops longer than the
/// budget allows -- and what it returns is what the budget is charged.
fn fill_silence(open: &mut Open, start: u64, stereo: bool) -> u64 {
    let from = open.end;
    let frame = crate::opus::silence(stereo);

    while start - open.end >= crate::opus::SILENCE_SAMPLES {
        open.end += crate::opus::SILENCE_SAMPLES;
        if let Err(error) = open.writer.packet(&frame, open.end, false) {
            tracing::error!(
                recording = %open.recording.id,
                %error,
                "a silence frame could not be written"
            );
            break;
        }
    }

    open.end - from
}

fn open_file(path: &PathBuf, stereo: bool) -> std::io::Result<OggWriter<BufWriter<File>>> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }

    let file = File::create(path)?;

    OggWriter::new(
        BufWriter::new(file),
        // A random serial, so that two of these files concatenated by a backup
        // tool are two streams a player can tell apart rather than one it
        // mangles silently.
        u32::from_le_bytes(
            uuid::Uuid::new_v4().as_bytes()[..4]
                .try_into()
                .unwrap_or([0, 0, 0, 1]),
        ),
        if stereo { 2 } else { 1 },
        GRANULE_RATE as u32,
    )
}

/// Writes the small JSON record that says what the audio beside it is.
///
/// The caller decides what a failure means, and the two callers decide
/// differently: one opening a recording refuses to open the audio at all, and
/// one closing it carries on. See both.
fn write_sidecar(
    path: &std::path::Path,
    room: &str,
    identity: &str,
    recording: &Recording,
    complete: bool,
) -> std::io::Result<()> {
    let sidecar = Sidecar {
        room: room.to_owned(),
        identity: identity.to_owned(),
        complete,
        recording: recording.clone(),
    };

    disk::write_sidecar(path, &sidecar)
}

/// An extended RTP timestamp in the codec's own clock, in 48 kHz samples.
///
/// Opus is negotiated at 48 kHz by every client there is, so this is almost
/// always the identity. It is here because a granule position is defined in 48
/// kHz samples whatever the payload's clock is (RFC 7845 section 4), and a
/// recorder that assumed the two were the same would produce a file whose
/// length was wrong by the ratio between them.
fn rescale(time: u64, clock_rate: u32) -> u64 {
    if clock_rate == 0 || u64::from(clock_rate) == GRANULE_RATE {
        return time;
    }

    (u128::from(time) * u128::from(GRANULE_RATE) / u128::from(clock_rate)) as u64
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writing(directory: &std::path::Path) -> (Writing, Arc<AtomicU64>) {
        let abandoned = Arc::new(AtomicU64::new(0));
        let (finished, _taken) = tokio::sync::mpsc::unbounded_channel();

        let writing = Writing::new(
            Settings {
                node: "ND_one".to_owned(),
                directory: directory.to_path_buf(),
                max_segment_bytes: 64 * 1024 * 1024,
                reorder_hold: Duration::from_millis(5),
                queue: 64,
            },
            finished,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::clone(&abandoned),
        );

        (writing, abandoned)
    }

    /// A panic on this thread costs the track it happened in and nothing else.
    ///
    /// **This thread is the one every recording on the process shares**, and
    /// what reaches it came off the network from somebody else's encoder. It
    /// used to be that a panic here killed the thread, every later
    /// `Tap::packet` hit a closed channel, and the node reported
    /// `recording=on dropped_audio_packets=0` while recording nothing at all
    /// for the rest of its life. The rule now is that one packet costs at most
    /// the recording it belongs to, and says so.
    ///
    /// This deliberately panics inside the shield, so the test prints a panic
    /// message and a note about `RUST_BACKTRACE` and then passes. That output
    /// is the default panic hook doing its job -- the point of the shield is
    /// that the process carries on afterwards, not that the panic goes unsaid.
    #[test]
    fn a_panic_in_the_writer_costs_one_track_and_leaves_the_thread_running() {
        let directory = tempfile::TempDir::new().expect("a directory");
        let (mut writing, abandoned) = writing(directory.path());

        for key in [1u64, 2] {
            writing.handle(Message::Open {
                key,
                spec: Box::new(TrackSpec {
                    room: "standup".to_owned(),
                    participant: format!("PA_0000000000{key}"),
                    identity: "alice".to_owned(),
                    account_id: None,
                    track: "TR_audio".to_owned(),
                }),
            });
        }

        writing.shielded(Some(1), |_| panic!("a packet nobody expected"));

        assert_eq!(
            abandoned.load(Ordering::Relaxed),
            1,
            "a recording the writer gave up on has to be counted, or a node that has stopped \
             recording somebody's audio looks exactly like one that never had any to record"
        );
        assert!(
            !writing.tracks.contains_key(&1),
            "the track that panicked has to be dropped: whatever state it was left in is the \
             state that panics again on its next packet, once per tick, for ever"
        );
        assert!(
            writing.tracks.contains_key(&2),
            "and every other recording on the node has to survive it -- which is the whole \
             difference between one lost recording and a node that silently stops recording"
        );

        // And the thread goes on working: another track opens as if nothing had
        // happened.
        writing.handle(Message::Open {
            key: 3,
            spec: Box::new(TrackSpec {
                room: "standup".to_owned(),
                participant: "PA_0000000000003".to_owned(),
                identity: "carol".to_owned(),
                account_id: None,
                track: "TR_audio".to_owned(),
            }),
        });
        assert!(writing.tracks.contains_key(&3));
    }
}
