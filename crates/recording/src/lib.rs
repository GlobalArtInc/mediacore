//! Recording a meeting's audio, uploading it, and saying it is there.
//!
//! Three steps and it stops there. What anybody does with the announcement --
//! transcribe it, bill for it, keep it for seven years -- is another service's
//! business, and this crate has no opinion about any of it.
//!
//! # Nothing is decoded
//!
//! The audio arrives as Opus, in RTP packets, with the sender's own timestamps
//! on them. It is written into an Ogg container as Opus and uploaded as Opus.
//! There is no decoder here, no encoder, no resampler and no mixer: the bytes
//! in the object store are the bytes the participant's browser produced.
//!
//! That is not a shortcut, it is the point. A recorder that sits *outside* an
//! SFU -- a bot that joins the meeting like a person -- cannot do this: what an
//! SDK hands a bot is decoded audio, so the bot has to re-encode it, and every
//! recording it makes has been through a decoder and an encoder for no reason
//! but where the bot was standing. We are the SFU. The packets are already
//! here.
//!
//! # Silence is the thing this gets right
//!
//! An Opus sender stops sending while nobody is talking. Every browser does
//! this and it is most of a meeting. A recorder that measured a recording by
//! the audio it wrote would therefore produce a file shorter than the meeting
//! by however long its participant was quiet -- and every later word in it
//! would be early by that much, compounding across the meeting.
//!
//! So length here comes from the **sender's clock**, not from the byte count:
//! the RTP timestamps drive the Ogg granule positions, and a participant who
//! said nothing for two minutes has a recording two minutes *longer*, not
//! shorter.
//!
//! Moving the granule is not enough on its own, and it is worth knowing why. A
//! demuxer works out where each packet starts by adding up the durations of the
//! packets before it -- so a granule that jumps with no packets to account for
//! it produces a file that *reports* two minutes and hands a decoder two
//! seconds with the words pushed together, which is the same misalignment
//! arriving through the back door. So the silence is written down, as a
//! constant three-byte Opus frame per twenty milliseconds. Nothing is encoded
//! and nothing is analysed: a sender that says nothing is saying "silence", and
//! this writes that down at the length its own clock gives. See
//! [`opus::silence`] and [`ogg`].
//!
//! # Nothing that carries a call waits for any of this
//!
//! The media path's whole share of the work is [`Tap::packet`]: a bounds check,
//! an atomic increment and a queue push, with no lock shared with anything else
//! and no allocation of the payload, which arrives behind an `Arc` and stays
//! there. Everything after that -- reordering, framing, checksums, files,
//! fsync, the object store -- is on a thread of its own and a task of its own.
//!
//! The queue is bounded, and that is deliberate. If the disk stops, packets are
//! dropped and counted rather than piling up until the process is killed: a
//! node that cannot write is a node that loses recording, and it must not
//! become a node that loses meetings. What is lost shows up honestly, as a gap
//! with the right length in it, because the clock is the sender's and not ours.
//!
//! `crates/recording/tests/uploading.rs` is the test that goes red if any of
//! that stops being true, and the one at the bottom of this file is the test for
//! what happens when the queue does fill.
//!
//! # What a node finds on its disk after a crash
//!
//! Everything it was writing, and enough beside it to finish the job: each
//! recording carries a JSON sidecar naming the meeting, the participant and the
//! rest of the row. On the next start the directory is walked, half-written
//! files are cut back to their last whole page and closed, and the lot is handed
//! to the store -- which keys a recording on `(meeting, participant, segment)`,
//! so a recording written down twice is written down once. See [`disk`].

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mediacore_session::Recording;

pub mod disk;
pub mod ogg;
pub mod opus;
mod s3;
mod work;
mod writer;

pub use s3::{S3, S3Settings, UploadError};
pub use work::{Announce, Queue, Running, Worker, WorkerSettings};

/// How this node records.
#[derive(Debug, Clone)]
pub struct Settings {
    /// This server's `node_id`, written onto every recording so that the
    /// upload is offered to the process that can see the file.
    pub node: String,
    /// Where the audio goes while a meeting is happening.
    ///
    /// One process per directory. Two sharing one would each finish the other's
    /// live recordings at startup, exactly as two sharing a session directory
    /// would overwrite each other's records.
    pub directory: PathBuf,
    /// How large one file may get before the recorder rolls over into the next
    /// segment.
    ///
    /// It is a ceiling on one object, not a target. Opus at the bitrates a
    /// browser uses is a few megabytes an hour, so an ordinary meeting never
    /// reaches this -- what it bounds is the meeting nobody ever leaves and the
    /// publisher sending far more than it should.
    pub max_segment_bytes: u64,
    /// How long a packet is held so that the ones behind it can catch up.
    pub reorder_hold: Duration,
    /// How many packets may be waiting between the media path and the writer
    /// before new ones are dropped rather than queued.
    pub queue: usize,
}

/// What is being recorded, and whose it is.
#[derive(Debug, Clone)]
pub struct TrackSpec {
    pub room: String,
    /// The participant's sid, which is what the attendance list is keyed by, so
    /// a recording lines up with the stretch it was made during.
    pub participant: String,
    /// What the room calls them, for logs and for the sidecar.
    pub identity: String,
    /// The track's sid, for logs.
    pub track: String,
}

/// What the media path and the control paths send to the writer thread.
pub(crate) enum Message {
    SessionStarted {
        room: String,
        session: String,
        started_at_ms: i64,
    },
    SessionEnded {
        room: String,
        session: String,
    },
    Open {
        key: u64,
        spec: Box<TrackSpec>,
    },
    Packet {
        key: u64,
        sequence: u64,
        time: u64,
        clock_rate: u32,
        payload: Arc<[u8]>,
        at: Instant,
    },
    Close {
        key: u64,
    },
    Stop,
}

impl Message {
    /// Which track this message is about, when it is about one. What the writer
    /// thread blames if handling it goes wrong.
    pub(crate) fn track(&self) -> Option<u64> {
        match self {
            Self::Open { key, .. } | Self::Packet { key, .. } | Self::Close { key } => Some(*key),
            Self::SessionStarted { .. } | Self::SessionEnded { .. } | Self::Stop => None,
        }
    }
}

struct Shared {
    messages: Sender<Message>,
    /// How many packets are in the queue: raised here and lowered by the writer
    /// thread as it takes them. The bound is enforced with a counter rather
    /// than by a bounded channel so that control messages -- a meeting
    /// starting, a track ending -- can never be refused or blocked by a backlog
    /// of audio, and so that the media path never waits on a full queue.
    queued: Arc<AtomicUsize>,
    limit: usize,
    dropped: AtomicU64,
    next_key: AtomicU64,
    /// False once the writer thread has stopped, for any reason at all.
    ///
    /// Without it a thread that died took the node's recording with it in
    /// silence: `/healthz` went on saying `recording=on` with nothing being
    /// written, for the rest of the process's life.
    alive: Arc<AtomicBool>,
    /// Recordings the writer gave up on because something in them panicked.
    abandoned: Arc<AtomicU64>,
}

/// Says the writer thread is gone, however it went.
///
/// A `Drop` and not a store after `run` returns, because the case worth
/// catching is the one where `run` does not return.
struct Gone(Arc<AtomicBool>);

impl Drop for Gone {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// The front door: hand it meetings and tracks, and it writes files.
pub struct Recorder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts the writer thread.
    ///
    /// What comes back beside it is the queue of finished recordings: one per
    /// file, as its last page is written. Whoever takes them is responsible for
    /// telling the store -- see [`Worker`], which does that and the two stages
    /// that follow.
    pub fn start(
        settings: Settings,
    ) -> io::Result<(Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<Recording>)> {
        std::fs::create_dir_all(&settings.directory)?;

        let (messages, inbox) = channel();
        let (finished, taken) = tokio::sync::mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let abandoned = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared {
            messages,
            queued: Arc::clone(&queued),
            limit: settings.queue.max(1),
            dropped: AtomicU64::new(0),
            next_key: AtomicU64::new(1),
            alive: Arc::clone(&alive),
            abandoned: Arc::clone(&abandoned),
        });

        let writing = writer::Writing::new(settings, finished, queued, abandoned);
        let thread = std::thread::Builder::new()
            .name("recording-writer".to_owned())
            .spawn(move || {
                let _gone = Gone(alive);
                writing.run(&inbox);
            })?;

        Ok((
            Arc::new(Self {
                shared,
                thread: Some(thread),
            }),
            taken,
        ))
    }

    /// A meeting has begun in this room, and this node is recording it.
    ///
    /// Until this is said, audio published in that room is not written down:
    /// somebody alone in a room waiting for a meeting to reach its floor is not
    /// in a meeting, and their audio belongs to no record.
    pub fn session_started(&self, room: &str, session: &str, started_at_ms: i64) {
        let _ = self.shared.messages.send(Message::SessionStarted {
            room: room.to_owned(),
            session: session.to_owned(),
            started_at_ms,
        });
    }

    /// A meeting has ended. Every file of it is closed, whoever is still
    /// publishing into the room.
    pub fn session_ended(&self, room: &str, session: &str) {
        let _ = self.shared.messages.send(Message::SessionEnded {
            room: room.to_owned(),
            session: session.to_owned(),
        });
    }

    /// Starts recording one track. Dropping what comes back stops it.
    pub fn track(&self, spec: TrackSpec) -> Tap {
        let key = self.shared.next_key.fetch_add(1, Ordering::Relaxed);
        let _ = self.shared.messages.send(Message::Open {
            key,
            spec: Box::new(spec),
        });

        Tap {
            shared: Arc::clone(&self.shared),
            key,
        }
    }

    /// How many packets this node has dropped rather than queued, over its
    /// whole life.
    ///
    /// On `/healthz`, and worth an alert: a number that climbs is a disk that
    /// cannot keep up with the meetings on top of it, and what it costs is
    /// audio.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Whether the one thread that writes files is still running.
    ///
    /// On `/healthz`, and the difference between a node that is recording and a
    /// node that has stopped and not said so. Nothing restarts it: a writer
    /// that has gone has left files for the next start to finish, and a
    /// deployment's answer to this is to restart the process, which is why it is
    /// a number an operator can alert on rather than a retry loop.
    pub fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// How many recordings the writer gave up on because something in them
    /// panicked.
    ///
    /// Worth an alert at any value above zero: what reaches that thread comes
    /// off the network, so this is the number that says somebody's encoder sent
    /// something this server could not handle -- and that it cost one recording
    /// and not the node.
    pub fn abandoned(&self) -> u64 {
        self.shared.abandoned.load(Ordering::Relaxed)
    }
}

impl Drop for Recorder {
    /// Stops the writer, having let it close every file it holds.
    ///
    /// A recorder that went away with files open would leave every recording of
    /// the shutdown's last meeting to be repaired by the next start, which
    /// works and is slower and noisier than closing them.
    fn drop(&mut self) {
        let _ = self.shared.messages.send(Message::Stop);

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One track's way into the recorder, held by whatever owns the publisher.
///
/// **This is the only thing here that the media path touches.** Everything it
/// does is bounded and none of it can wait: no lock that anything else holds,
/// no allocation of the payload, no syscall.
pub struct Tap {
    shared: Arc<Shared>,
    key: u64,
}

impl Tap {
    /// Hands one RTP packet to the recorder.
    ///
    /// `sequence` and `time` are the *extended* sequence number and timestamp
    /// -- the ones that do not wrap -- because a recording an hour into a
    /// meeting has been through both wraps and a writer that had to undo them
    /// itself would be a second place to get roll-over wrong.
    pub fn packet(
        &self,
        sequence: u64,
        time: u64,
        clock_rate: u32,
        payload: Arc<[u8]>,
        at: Instant,
    ) {
        if self.shared.queued.fetch_add(1, Ordering::Relaxed) >= self.shared.limit {
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }

        if self
            .shared
            .messages
            .send(Message::Packet {
                key: self.key,
                sequence,
                time,
                clock_rate,
                payload,
                at,
            })
            .is_err()
        {
            // The place in the queue goes back, and the packet is counted as
            // dropped -- because it *was* dropped. Giving the place back
            // without counting it is how a writer thread that had stopped went
            // on being reported as `dropped_audio_packets=0` while nothing at
            // all was being written.
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        let _ = self.shared.messages.send(Message::Close { key: self.key });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorder's queue with nothing at the other end of it.
    ///
    /// The receiver is held here and never read, which is a writer thread that
    /// has stopped dead -- a disk that has wedged, a filesystem that has gone
    /// read-only and is taking its time about saying so. It is the state the
    /// bound exists for, and the one a bounded channel would turn into a media
    /// path that blocks.
    fn wedged(limit: usize) -> (Tap, std::sync::mpsc::Receiver<Message>) {
        let (messages, inbox) = channel();
        let shared = Arc::new(Shared {
            messages,
            queued: Arc::new(AtomicUsize::new(0)),
            limit,
            dropped: AtomicU64::new(0),
            next_key: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
            abandoned: Arc::new(AtomicU64::new(0)),
        });

        (Tap { shared, key: 1 }, inbox)
    }

    #[test]
    fn a_wedged_writer_costs_audio_and_never_a_moment_of_anybody_carrying_a_call() {
        let (tap, held) = wedged(64);
        let payload: Arc<[u8]> = Arc::from(vec![0b0111_1000u8; 80].into_boxed_slice());

        // Every one of these has to return. The test finishing at all is the
        // assertion: a bounded channel here, or a lock shared with the writer,
        // would stop on the sixty-fifth and never come back.
        for sequence in 0..1_064u64 {
            tap.packet(
                sequence,
                sequence * 960,
                48_000,
                Arc::clone(&payload),
                Instant::now(),
            );
        }

        assert_eq!(
            tap.shared.dropped.load(Ordering::Relaxed),
            1_000,
            "everything past the queue's depth has to be dropped and counted; what is lost shows \
             up in the file as a gap of the right length, because the clock is the sender's"
        );
        assert_eq!(
            tap.shared.queued.load(Ordering::Relaxed),
            64,
            "and the queue must not grow past its bound, or a disk that has stopped becomes a \
             process that is killed for memory"
        );
        drop(held);
    }

    /// A writer that has gone costs audio, and the audio it costs is counted.
    ///
    /// The counting is the half worth guarding. A recorder whose thread has
    /// stopped goes on accepting packets -- it must, because the media path
    /// hands them over and nothing there may wait or fail -- so the only thing
    /// that says the node has stopped recording is this number and
    /// `Recorder::alive`. Without the count, `/healthz` reports
    /// `dropped_audio_packets=0` for the rest of the process's life while not
    /// one byte is being written.
    #[test]
    fn a_recorder_whose_thread_is_gone_counts_every_packet_it_can_no_longer_write() {
        let (tap, held) = wedged(8);
        drop(held);

        for sequence in 0..5u64 {
            tap.packet(
                sequence,
                sequence * 960,
                48_000,
                Arc::from(vec![0u8; 4].into_boxed_slice()),
                Instant::now(),
            );
        }

        assert_eq!(
            tap.shared.queued.load(Ordering::Relaxed),
            0,
            "a send that failed has to give its place in the queue back, or a writer that has \
             stopped leaves the queue permanently full and every later packet counted as dropped \
             twice"
        );
        assert_eq!(
            tap.shared.dropped.load(Ordering::Relaxed),
            5,
            "audio that reached a writer which is no longer there was thrown away and not \
             counted, so a node that has silently stopped recording reports \
             dropped_audio_packets=0 and looks exactly like one that is working"
        );
    }
}
