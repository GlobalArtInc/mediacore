//! Turning the `recording` block of the config into a recorder, and wiring it
//! to the three things it needs: which meetings are running, where the audio
//! goes, and who is told when it gets there.
//!
//! The recorder itself is `mediacore-recording` and knows nothing about rooms,
//! events or the room service. Everything about *which* meetings are recorded
//! is decided here, in one place, at startup and then per meeting.
//!
//! # How a meeting becomes a recording
//!
//! Three things have to line up, and they arrive from three directions.
//!
//! A **meeting starts**: the registry announces `session_started` on the event
//! bus, and this module -- an in-process listener like the delivery machinery,
//! which is what that bus is for -- asks the room whether it is recorded and
//! tells the recorder. Until that has happened, audio published into the room
//! goes nowhere: somebody alone in a room waiting for it to reach its floor is
//! not in a meeting, and their audio belongs to no record.
//!
//! A **track is published**: `peer.rs` opens a tap on it, if it is a microphone
//! carrying Opus and the room is recorded. From then on every packet that
//! arrives is handed to the recorder, which is a bounds check and a queue push
//! and nothing else -- see [`mediacore_recording::Tap::packet`].
//!
//! A **meeting ends**: the registry announces `session_finished`, every file of
//! that meeting is closed, and each one goes into the queue to be uploaded and
//! announced.
//!
//! # What is deliberately not here
//!
//! A minimum-participants rule of recording's own. Whether a meeting is
//! happening is decided once, by `room.min_humans`, and recording follows it. A
//! room where everybody sits in one place and joins from one account sets that
//! to one and is recorded like any other; there is no second threshold that
//! could refuse it. That is not a simplification -- it is the failure this
//! server's neighbour has, where a recorder that insisted on two people simply
//! did not record the meeting room, and the meetings were missing afterwards
//! with nothing to say why.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::Context as _;
use mediacore_config::Config;
use mediacore_events::{Bus, Event, Kind, Ready};
use mediacore_recording::{
    Announce, Recorder, S3, S3Settings, Settings, Worker, WorkerSettings, disk,
};
use mediacore_room::Registry;
use mediacore_session::SessionStore;

/// Everything recording, held by the server for as long as it runs.
pub struct Recording {
    pub recorder: Arc<Recorder>,
    /// Finished recordings this node has not managed to write into the store.
    /// On `/healthz`.
    pub owed: Arc<AtomicUsize>,
    /// The two tasks. Nothing reads them; dropping the last `AppState` aborts
    /// them, which is how every other task in this server is stopped.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Recording {
    /// How many packets this node has dropped rather than queued.
    pub fn dropped(&self) -> u64 {
        self.recorder.dropped()
    }

    /// How many recordings the writer thread gave up on because something in
    /// them panicked.
    pub fn abandoned(&self) -> u64 {
        self.recorder.abandoned()
    }
}

/// What has to be built before the registry exists, because it listens to the
/// registry's own announcements.
pub struct Pending {
    recorder: Arc<Recorder>,
    listener: mediacore_events::Listener,
    finished: tokio::sync::mpsc::UnboundedReceiver<mediacore_session::Recording>,
    store: Arc<S3>,
    settings: WorkerSettings,
    /// Recordings found on this node's disk at startup: files it was writing
    /// when it last stopped, and files it finished and never managed to write
    /// down.
    found: Vec<mediacore_session::Recording>,
    /// The server-wide default for a room that does not say.
    default_record: bool,
}

/// Builds the recorder, or answers `None` because this deployment records
/// nothing.
///
/// **Called before the registry is built**, so that the queue of events it
/// listens on exists before the first room can announce anything -- exactly as
/// the delivery machinery does. What comes back is finished in [`start`] once
/// there is a registry to ask about rooms.
///
/// Every failure here fails the start. A server that could not open its
/// recording directory or could not build a client for its object store would
/// hold meetings, record nothing and say so once at boot in a line nobody reads
/// again -- and the meetings would be discovered to be missing a week later.
pub fn prepare(config: &Config, events: &Bus) -> anyhow::Result<Option<Pending>> {
    let settings = &config.recording;

    if !settings.enabled && !settings.possible() {
        return Ok(None);
    }

    let store = Arc::new(
        S3::new(
            S3Settings {
                endpoint: settings.s3.endpoint.clone(),
                region: settings.s3.region.clone(),
                bucket: settings.s3.bucket.clone(),
                access_key_id: settings.s3.access_key_id.clone(),
                secret_access_key: settings.s3.secret_access_key.clone(),
                path_style: settings.s3.path_style,
                prefix: settings.s3.prefix.clone(),
            },
            // The object store's own deadline, not the database's. They were
            // one number, and they are two unrelated things: how long a
            // statement may hold a Postgres connection, and how long a whole
            // recording has to reach a bucket. See `recording.s3.timeout_ms`.
            settings.s3_timeout(),
        )
        .map_err(anyhow::Error::msg)
        .context("the object store recordings are uploaded to")?,
    );

    // Before the recorder starts, and it has to be: the scan finishes every
    // half-written file it finds, and a file this process opened a moment ago
    // would look exactly like one the last process was killed in the middle of.
    let found = disk::scan(&settings.directory, &config.node_id);
    if !found.is_empty() {
        tracing::info!(
            recordings = found.len(),
            directory = %settings.directory.display(),
            "recordings were left on this node's disk by an earlier run; they have been finished \
             and will be uploaded"
        );
    }

    // Registered before the registry exists, so there is no window in which a
    // meeting starts and nobody is queued to hear about it.
    let listener = events.listen();

    let (recorder, finished) = Recorder::start(Settings {
        node: config.node_id.clone(),
        directory: settings.directory.clone(),
        max_segment_bytes: settings.max_segment_bytes(),
        reorder_hold: settings.reorder_hold(),
        queue: settings.queue as usize,
    })
    .with_context(|| {
        format!(
            "opening the recording directory {}",
            settings.directory.display()
        )
    })?;

    tracing::info!(
        directory = %settings.directory.display(),
        bucket = %settings.s3.bucket,
        default = settings.enabled,
        "meetings are recorded to disk and uploaded when they end"
    );

    Ok(Some(Pending {
        recorder,
        listener,
        finished,
        store,
        settings: WorkerSettings {
            poll: settings.poll_interval(),
            batch: settings.batch as usize,
            backoff: std::time::Duration::from_millis(config.events.backoff_ms.max(1_000).into()),
            max_backoff: std::time::Duration::from_millis(
                config.events.max_backoff_ms.max(30_000).into(),
            ),
            // Three times per lease, the same rate a meeting's own lease is
            // renewed at: a claim that expires under a node that is merely busy
            // is an upload taken away mid-flight.
            renew: std::time::Duration::from_millis(
                (config.sessions.postgres.lease_ms / 3).max(1_000).into(),
            ),
            keep_local: settings.keep_local,
        },
        found,
        default_record: settings.enabled,
    }))
}

/// Starts the two tasks, now that there is a registry and a record store.
pub fn start(
    pending: Pending,
    registry: &Arc<Registry>,
    sessions: &Arc<SessionStore>,
    events: &Arc<Bus>,
) -> Recording {
    let Pending {
        recorder,
        mut listener,
        finished,
        store,
        settings,
        found,
        default_record,
    } = pending;

    let watching = Arc::clone(&recorder);
    let rooms = Arc::clone(registry);
    let meetings = tokio::spawn(async move {
        while let Some(event) = listener.recv().await {
            let Some(session) = &event.session else {
                continue;
            };

            match event.kind {
                Kind::SessionStarted => {
                    // Asked of the room now rather than of the config, because
                    // a room says whether it is recorded and the config only
                    // says what a room that does not say means. Read once, when
                    // the meeting begins: a policy that changed halfway through
                    // would leave a meeting with a recording of part of it and
                    // nothing to say which part.
                    let recorded = rooms
                        .room(&session.room)
                        .map_or(default_record, |room| room.records());

                    if recorded {
                        watching.meeting_started(&session.room, &session.id, session.started_at_ms);
                    }
                }
                // Said whether or not the meeting was being recorded. A
                // recorder that is not holding it does nothing with this, and a
                // recorder that is closes every file of it -- including the
                // files of people who are still in the room and still talking,
                // because what they say next is not part of the meeting that
                // just ended.
                Kind::SessionFinished => watching.meeting_ended(&session.room, &session.id),
                _ => {}
            }
        }
    });

    let running = Worker::start(
        Arc::clone(sessions),
        store,
        Arc::new(Announcer {
            events: Arc::clone(events),
            sessions: Arc::clone(sessions),
        }),
        settings,
        found,
        finished,
    );

    Recording {
        recorder,
        owed: running.owed,
        tasks: vec![meetings, running.task],
    }
}

/// Says a recording is ready, onto the bus every other event goes onto.
///
/// From there it travels the way a `session_finished` does: through the outbox,
/// then to webhooks and to a broker. Nothing here waits for any of that --
/// [`Bus::emit`] takes a lock, pushes into queues and returns.
struct Announcer {
    events: Arc<Bus>,
    sessions: Arc<SessionStore>,
}

impl Announce for Announcer {
    /// **Blocking**, and called on a blocking thread by the worker: it reads
    /// the meeting out of the store.
    ///
    /// It reads it because the event has to carry the room's name and the
    /// participant's identity, and a recording row carries neither -- it names
    /// the meeting and the participant's sid, which is exactly right for a row
    /// and not enough for a consumer that has to route on a room name. A store
    /// that is away turns into a postponed announcement rather than an event
    /// with holes in it.
    fn ready(&self, recording: &mediacore_session::Recording, bucket: &str) -> Result<(), String> {
        let object_key = recording
            .object_key
            .clone()
            .ok_or_else(|| "this recording has no object key yet".to_owned())?;

        let session = self
            .sessions
            .get(&recording.meeting)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!(
                    "the meeting `{}` this recording belongs to is not in the store",
                    recording.meeting
                )
            })?;

        // The stretch this recording was made during, which is what carries the
        // name the room used. Empty when the attendance row has been pruned,
        // which is a recording that outlived its meeting's detail and is still
        // worth announcing.
        let identity = session
            .participants
            .iter()
            .find(|attendance| attendance.sid == recording.participant)
            .map(|attendance| attendance.identity.clone())
            .unwrap_or_default();

        self.events.emit(Event::recording_ready(Ready {
            id: recording.id.clone(),
            session: session.id.clone(),
            room: session.room.clone(),
            participant: recording.participant.clone(),
            identity,
            account_id: recording.account_id.clone(),
            segment: recording.segment,
            bucket: bucket.to_owned(),
            object_key,
            bytes: recording.bytes,
            content_type: "audio/ogg".to_owned(),
            sample_rate: recording.sample_rate,
            channels: recording.channels,
            started_at_ms: recording.started_at_ms,
            ended_at_ms: recording.ended_at_ms,
            offset_ms: recording.offset_ms,
            duration_ms: recording.duration_ms,
        }));

        Ok(())
    }
}
