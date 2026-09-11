//! What happens to a recording after its last page is written.
//!
//! Three things, in order, and each one is a row in a queue several processes
//! share rather than a step in a function: write the recording down, put the
//! file in the object store, say it is there.
//!
//! # Why a queue and not a function call
//!
//! Because the process doing the work can stop. An upload that lives only in
//! the memory of the process that recorded it dies with that process, and a
//! recording nobody uploaded is a meeting that quietly did not happen. The
//! queue is in the database every node shares, so a node that dies before
//! announcing has its announcement picked up by one that is still running, and
//! a node that dies before uploading picks its own upload up when it comes back
//! -- itself, because the file is on its disk and nowhere else.
//!
//! # Where the durability actually comes from
//!
//! Not from this. A finished recording is described by a JSON sidecar written
//! beside the audio *before* anybody tells the database, so a database that is
//! away costs nothing: the file and its description sit on the disk, this
//! worker keeps trying to write the row, and if the process stops first the
//! next start finds them with [`crate::disk::scan`]. Telling the store twice is
//! telling it once -- a recording is keyed on `(meeting, participant, segment)`
//! -- which is what makes retrying safe enough to be the whole strategy.
//!
//! # A claim has an owner and a lifetime, and both are load-bearing
//!
//! Every operation on a piece of work has to respect both, and the three ways
//! of forgetting that are three ways of corrupting the queue. A stage that does
//! not renew has its row handed to somebody else while it is still working. A
//! put-back that does not check who holds the claim undoes whatever the new
//! holder did with it -- a finished announcement returned to the queue and
//! emitted again, a live upload's lease cleared out from under it. And a node
//! that takes back work it is itself still doing does that work twice. So: the
//! claim is renewed under every stage ([`Doing::while_claimed`]), given back
//! only by the node still holding it (`SessionStore::postpone`), and never
//! started twice on one node ([`Doing::begin`]).
//!
//! What is left over is visible rather than silent. An upload names the node
//! whose disk the file is on, and no other node can do it -- so a node that
//! never comes back leaves a row that will wait for ever, and the store counts
//! those and says so on `/healthz` as `stuck_recording_work=` with the remedy
//! in the log line.
//!
//! # An upload that was given up on is offered again to the node holding it
//!
//! `attempts` is spent on the claim and not on the result, so a queue of five
//! attempts can be emptied by an object store that is unreachable for eighteen
//! seconds. Once they are gone the row is `failed` and the queue will never
//! offer it again -- but the file is still on a disk, and the node with that
//! disk knows it, because it finds it there at every start ([`crate::disk::scan`])
//! and writes it down again. That statement is the one piece of information the
//! failed row was missing, so `note` makes it the moment a given-up upload goes
//! back into the queue with its attempts reset. Without that a node walks past
//! its own readable recording for ever, and the only way back is an operator
//! with a `psql` prompt who first has to know that any of this happened.
//!
//! # The announcement is at least once
//!
//! A node that emits the event and stops before marking the row done has the
//! announcement re-emitted by whoever claims it next. That is the right way
//! round: the alternative is marking it done first and losing announcements to
//! a crash, and this crate's neighbour rule -- an event notifies, it is never
//! the only copy of a fact -- means a duplicate costs a consumer one repeated
//! lookup while a loss costs it a recording it never hears about. The
//! recordings stay queryable either way, which is what a consumer that missed
//! one asks.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_session::{
    Claim, Noted, Recording, SessionStore, Stage, StoreError, Work, WorkState,
};

use crate::disk;
use crate::s3::{S3, UploadError};

/// What this worker needs from the record store.
///
/// A trait rather than the store itself so that the ladder below -- claim,
/// fail, come back later, give up -- can be driven in a test without a
/// database. The real implementation is one line per method; see the bottom of
/// this file.
pub trait Queue: Send + Sync + 'static {
    fn note(&self, recording: &Recording, now_ms: i64) -> Result<Noted, StoreError>;
    /// What is happening to one recording: one row per stage.
    fn work(&self, recording: &str) -> Result<Vec<Work>, StoreError>;
    fn claim(&self, stage: Stage, now_ms: i64, limit: usize) -> Result<Vec<Claim>, StoreError>;
    fn renew(&self, recording: &str, stage: Stage, now_ms: i64) -> Result<bool, StoreError>;
    fn uploaded(
        &self,
        recording: &str,
        object_key: &str,
        bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<(), StoreError>;
    fn announced(&self, recording: &str, now_ms: i64) -> Result<(), StoreError>;
    /// Gives a claim back. `false` means it was not this node's to give back
    /// and nothing was changed.
    fn postpone(
        &self,
        recording: &str,
        stage: Stage,
        error: &str,
        due_at_ms: i64,
    ) -> Result<bool, StoreError>;
}

impl Queue for SessionStore {
    fn note(&self, recording: &Recording, now_ms: i64) -> Result<Noted, StoreError> {
        Self::note_recording(self, recording, now_ms)
    }

    fn work(&self, recording: &str) -> Result<Vec<Work>, StoreError> {
        Self::work(self, recording)
    }

    fn claim(&self, stage: Stage, now_ms: i64, limit: usize) -> Result<Vec<Claim>, StoreError> {
        Self::claim(self, stage, now_ms, limit)
    }

    fn renew(&self, recording: &str, stage: Stage, now_ms: i64) -> Result<bool, StoreError> {
        Self::renew(self, recording, stage, now_ms)
    }

    fn uploaded(
        &self,
        recording: &str,
        object_key: &str,
        bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        Self::uploaded(self, recording, object_key, bytes, now_ms)
    }

    fn announced(&self, recording: &str, now_ms: i64) -> Result<(), StoreError> {
        Self::announced(self, recording, now_ms)
    }

    fn postpone(
        &self,
        recording: &str,
        stage: Stage,
        error: &str,
        due_at_ms: i64,
    ) -> Result<bool, StoreError> {
        Self::postpone(self, recording, stage, error, due_at_ms)
    }
}

/// Saying a recording is ready, to whoever is listening.
///
/// Deliberately not this crate's business to know what that means. The server
/// puts an event on its bus, which goes through an outbox to webhooks and to a
/// broker; a test says it happened into a vector. Either way a refusal here
/// postpones the announcement rather than losing it.
pub trait Announce: Send + Sync + 'static {
    fn ready(&self, recording: &Recording, bucket: &str) -> Result<(), String>;
}

/// How hard and how often the worker tries.
#[derive(Debug, Clone)]
pub struct WorkerSettings {
    /// How often the queue is asked for work when nothing is finishing.
    pub poll: Duration,
    /// How many pieces of work one poll takes.
    pub batch: usize,
    /// The first wait after a failure, and the cap as it doubles.
    ///
    /// The cap matters more than the doubling: without one, an object store
    /// that came back a moment ago would go untouched for an hour because the
    /// last attempt happened to be the eighth.
    pub backoff: Duration,
    pub max_backoff: Duration,
    /// How often a claim is renewed while a long upload is running.
    pub renew: Duration,
    /// Keep the local file after it is in the object store.
    ///
    /// Off by default. A node's disk is not a second copy anybody agreed to
    /// pay for, and a server that keeps every recording fills its volume and
    /// then stops recording -- which is the failure that costs meetings rather
    /// than bytes.
    pub keep_local: bool,
}

/// A recording whose row is not in the store yet.
struct Owed {
    recording: Recording,
    due_at_ms: i64,
    attempts: u32,
    /// Whether this came off this node's disk at startup rather than out of the
    /// recorder a moment ago. A recording found on the disk may already be in
    /// the object store -- the process can die between `uploaded` and the
    /// deletion of the local copy -- and that is the one case where writing it
    /// down means cleaning it up rather than queueing an upload.
    from_disk: bool,
}

/// What is left of the worker once it is running: a way to stop it, and the
/// one number worth putting on `/healthz`.
pub struct Running {
    pub task: tokio::task::JoinHandle<()>,
    /// How many recordings this node has finished and not yet managed to write
    /// down. A number that stays above zero is a record store that is away, and
    /// what it costs is the delay before the audio reaches the object store --
    /// not the audio, which is on the disk with its sidecar.
    pub owed: Arc<AtomicUsize>,
}

/// The task that empties the queue.
///
/// **One slow piece of work must not stop the others.** An object store that
/// accepts connections and never answers is the ordinary shape of an outage,
/// and a worker that awaited each upload in turn would spend its whole life in
/// the first one -- not uploading anything else, not writing new recordings
/// down and not announcing the ones already up. So each claim is handled on a
/// task of its own, and the number of them at once is exactly the number of
/// pieces of work one poll takes.
pub struct Worker<Q: Queue, A: Announce> {
    doing: Arc<Doing<Q, A>>,
    owed: Vec<Owed>,
    gauge: Arc<AtomicUsize>,
    /// One permit per piece of work that may be in flight. Nothing is claimed
    /// while they are all out, because a claim nobody is going to start on is
    /// an attempt spent on waiting.
    permits: Arc<tokio::sync::Semaphore>,
}

/// What one piece of work needs, shared by every task doing one.
struct Doing<Q: Queue, A: Announce> {
    queue: Arc<Q>,
    store: Arc<S3>,
    announce: Arc<A>,
    settings: WorkerSettings,
    /// What this node is doing right now, so that it never starts a second copy
    /// of one piece of work.
    ///
    /// A claim can be re-offered to the node that already holds it: the queue
    /// re-offers rows whose lease has run out to whoever asks, and "whoever
    /// asks" includes the node that is still inside the work. That is right for
    /// a node that crashed and came back under the same name -- its uploads are
    /// on its own disk and nobody else can do them -- and wrong for a node that
    /// is merely slow, where it means the same recording announced twice. The
    /// database cannot tell those apart; this can, because it is the one that
    /// knows.
    busy: Mutex<HashSet<(String, Stage)>>,
}

impl<Q: Queue, A: Announce> Worker<Q, A> {
    /// Starts the task.
    ///
    /// `initial` is whatever [`crate::disk::scan`] found on this node's disk at
    /// startup: recordings this node made in a previous life whose rows may
    /// never have been written. They go through the same path as one that
    /// finished a moment ago, because after the sidecar is written there is no
    /// difference between them.
    pub fn start(
        queue: Arc<Q>,
        store: Arc<S3>,
        announce: Arc<A>,
        settings: WorkerSettings,
        initial: Vec<Recording>,
        mut finished: tokio::sync::mpsc::UnboundedReceiver<Recording>,
    ) -> Running {
        let gauge = Arc::new(AtomicUsize::new(0));
        let batch = settings.batch.max(1);
        let mut worker = Self {
            doing: Arc::new(Doing {
                queue,
                store,
                announce,
                settings,
                busy: Mutex::new(HashSet::new()),
            }),
            owed: Vec::new(),
            gauge: Arc::clone(&gauge),
            permits: Arc::new(tokio::sync::Semaphore::new(batch)),
        };

        for recording in initial {
            worker.owe_from_disk(recording);
        }

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    arrived = finished.recv() => match arrived {
                        Some(recording) => worker.owe(recording),
                        // The recorder is gone, which happens when the server
                        // is shutting down. Whatever is still owed is on the
                        // disk with its sidecar, and the next start finds it.
                        None => break,
                    },
                    () = tokio::time::sleep(worker.doing.settings.poll) => {}
                }

                worker.write_down().await;
                worker.hand_out(Stage::Upload).await;
                worker.hand_out(Stage::Announce).await;
            }
        });

        Running { task, owed: gauge }
    }

    fn owe(&mut self, recording: Recording) {
        self.push(recording, false);
    }

    fn owe_from_disk(&mut self, recording: Recording) {
        self.push(recording, true);
    }

    fn push(&mut self, recording: Recording, from_disk: bool) {
        self.owed.push(Owed {
            recording,
            due_at_ms: 0,
            attempts: 0,
            from_disk,
        });
        self.gauge.store(self.owed.len(), Ordering::Relaxed);
    }

    /// Tells the store about every recording it has not been told about.
    ///
    /// Retried for as long as the process lives, because there is no attempt
    /// count that would make sense here: the file exists, so giving up would
    /// mean deciding it should never be uploaded. A store that is away for an
    /// hour costs an hour's delay and nothing else -- the audio and everything
    /// that describes it are on the disk.
    async fn write_down(&mut self) {
        let now = now_ms();
        let mut still_owed = Vec::new();

        for owed in std::mem::take(&mut self.owed) {
            if owed.due_at_ms > now {
                still_owed.push(owed);
                continue;
            }

            let queue = Arc::clone(&self.doing.queue);
            let recording = owed.recording.clone();
            let written =
                tokio::task::spawn_blocking(move || queue.note(&recording, now_ms())).await;

            match written {
                Ok(Ok(noted)) => {
                    // Said apart from the ordinary line, and said loudly,
                    // because it is the only moment at which a recording that
                    // had been given up on starts moving again -- and because
                    // one line saying "its upload is queued" for both cases is
                    // what let a node walk past a readable file fourteen
                    // restarts running while claiming each time that it had
                    // queued the upload.
                    if noted.upload_offered_again {
                        tracing::warn!(
                            recording = %noted.id,
                            session = %owed.recording.session,
                            path = %owed.recording.local_path.display(),
                            "this recording's upload had been given up on; the file is on this \
                             node's disk, so it has been put back in the queue with its attempts \
                             reset"
                        );
                    } else {
                        tracing::info!(
                            recording = %noted.id,
                            session = %owed.recording.session,
                            "a recording is in the store and its upload is queued"
                        );
                    }

                    if owed.from_disk {
                        self.sweep_up(&noted.id, &owed.recording).await;
                    }
                }
                Ok(Err(error)) => {
                    let attempts = owed.attempts.saturating_add(1);
                    let wait = self.doing.wait_for(attempts);
                    if attempts == 1 || attempts % 20 == 0 {
                        tracing::error!(
                            recording = %owed.recording.id,
                            attempts,
                            %error,
                            "this recording could not be written down; the audio and its sidecar \
                             are on this node's disk and it will be tried again"
                        );
                    }
                    still_owed.push(Owed {
                        recording: owed.recording,
                        due_at_ms: now + wait.as_millis() as i64,
                        attempts,
                        from_disk: owed.from_disk,
                    });
                }
                Err(error) => {
                    tracing::error!(%error, "writing a recording down did not finish");
                    still_owed.push(owed);
                }
            }
        }

        self.owed = still_owed;
        self.gauge.store(self.owed.len(), Ordering::Relaxed);
    }

    /// Removes a recording that was found on this node's disk and is already in
    /// the object store.
    ///
    /// **The one path that would otherwise never clean up.** `disk::forget`
    /// runs in the successful-upload arm and nowhere else, so a process that
    /// died between `uploaded` and that deletion left the `.ogg` and its
    /// sidecar on the volume for good: both work rows are `done`, so the queue
    /// never offers the recording to anybody again, and every later start finds
    /// the same two files, writes the same row down and leaves them exactly
    /// where they are. The disk fills slowly and nothing says why -- on a server
    /// whose own answer to a full volume is that it stops recording, which is
    /// the failure that costs meetings rather than bytes.
    ///
    /// One query per recording found on the disk at startup, which is a handful
    /// per restart and nothing at all in steady state.
    async fn sweep_up(&self, id: &str, recording: &Recording) {
        if self.doing.settings.keep_local {
            return;
        }

        let queue = Arc::clone(&self.doing.queue);
        let asked = id.to_owned();
        let Ok(Ok(work)) = tokio::task::spawn_blocking(move || queue.work(&asked)).await else {
            // A store that could not answer means the files stay where they
            // are and the next start asks again, which is exactly the right
            // outcome: nothing is deleted on a guess.
            return;
        };

        let uploaded = work
            .iter()
            .any(|row| row.stage == Stage::Upload && row.state == WorkState::Done);
        if !uploaded {
            return;
        }

        tracing::info!(
            recording = %id,
            path = %recording.local_path.display(),
            "this recording was already in the object store when this node found it on its disk; \
             the local copy is removed"
        );

        let recording = recording.clone();
        let _ = tokio::task::spawn_blocking(move || disk::forget(&recording)).await;
    }

    /// Takes as much work as there is room to start, and starts it.
    async fn hand_out(&self, stage: Stage) {
        let room = self.permits.available_permits();
        if room == 0 {
            return;
        }

        let queue = Arc::clone(&self.doing.queue);
        let claimed = tokio::task::spawn_blocking(move || queue.claim(stage, now_ms(), room)).await;

        let claims = match claimed {
            Ok(Ok(claims)) => claims,
            // A store that cannot hand out work at all -- a directory of files
            // -- is a deployment that is not recording, and saying so once a
            // poll would fill a log with it.
            Ok(Err(StoreError::Unsupported(_))) => return,
            Ok(Err(error)) => {
                tracing::debug!(%stage, %error, "the recording queue could not be asked for work");
                return;
            }
            Err(error) => {
                tracing::error!(%stage, %error, "asking the recording queue for work did not finish");
                return;
            }
        };

        for claim in claims {
            // Work this node is already doing. The row's lease ran out under a
            // task that is still inside it, and the queue offered it back to
            // whoever asked -- which this time was us. Starting a second copy
            // would upload the same object twice and, worse, emit the same
            // announcement twice; and because announcing reads the store, a
            // store that is slow enough to expire a lease is a store that makes
            // this happen again on the next poll, and again. The claim the
            // queue just handed over is not given back: it belongs to the task
            // that is still working, and it has pushed that task's lease out,
            // which is what the task wanted. The attempt it spent is the price,
            // and it is one attempt rather than one duplicate event.
            let Some(held) = self.doing.begin(&claim) else {
                tracing::debug!(
                    recording = %claim.recording.id,
                    %stage,
                    "this node was offered work it is already doing; the second copy is dropped"
                );
                continue;
            };

            let Ok(permit) = Arc::clone(&self.permits).acquire_owned().await else {
                return;
            };
            let doing = Arc::clone(&self.doing);

            tokio::spawn(async move {
                match stage {
                    Stage::Upload => doing.upload(claim).await,
                    Stage::Announce => doing.announce(claim).await,
                }
                drop(held);
                drop(permit);
            });
        }
    }
}

/// One piece of work this node has started, struck off when it finishes.
///
/// A guard rather than a pair of calls, so that a task that returns early -- or
/// panics -- does not leave the node believing for ever that it is still
/// uploading something.
struct Started {
    busy: Arc<dyn Busy>,
    key: (String, Stage),
}

impl Drop for Started {
    fn drop(&mut self) {
        self.busy.finished(&self.key);
    }
}

/// What [`Started`] needs from the worker, without carrying its type parameters
/// into the guard.
trait Busy: Send + Sync + 'static {
    fn finished(&self, key: &(String, Stage));
}

impl<Q: Queue, A: Announce> Busy for Doing<Q, A> {
    fn finished(&self, key: &(String, Stage)) {
        self.busy.lock().expect("the work in flight").remove(key);
    }
}

impl<Q: Queue, A: Announce> Doing<Q, A> {
    /// Marks a piece of work as this node's, or says it already was.
    fn begin(self: &Arc<Self>, claim: &Claim) -> Option<Started> {
        let key = (claim.recording.id.clone(), claim.stage);

        if !self
            .busy
            .lock()
            .expect("the work in flight")
            .insert(key.clone())
        {
            return None;
        }

        Some(Started {
            busy: Arc::clone(self) as Arc<dyn Busy>,
            key,
        })
    }

    /// Runs one piece of work with its claim kept alive underneath it.
    ///
    /// **Every stage needs this, not only the slow-looking one.** A claim is a
    /// lease: `Claim::ends_at_ms` says finish or renew before it, and a stage
    /// that does neither is a stage whose row goes back into the queue while it
    /// is still working. The upload is the obvious case. The announcement looks
    /// like a function call and is not: it reads the meeting out of the record
    /// store to find the room's name and the participant's identity, and that
    /// read waits for a connection out of a small pool -- so exactly when the
    /// store is slow, the announcement outlives its lease, the row is offered
    /// again, and the event is emitted twice. Which makes the store slower.
    async fn while_claimed<T>(
        &self,
        recording: &str,
        stage: Stage,
        work: impl Future<Output = T>,
    ) -> T {
        tokio::pin!(work);

        loop {
            tokio::select! {
                outcome = &mut work => break outcome,
                () = tokio::time::sleep(self.settings.renew) => {
                    // A claim that has gone to somebody else does not stop the
                    // work. Writing the same object twice is writing it once,
                    // and `uploaded` deliberately does not check who holds the
                    // claim, so finishing is better than abandoning; an
                    // announcement that has already been claimed elsewhere is
                    // the at-least-once case this crate is built around. What
                    // it does mean is that `postpone` will refuse the put-back,
                    // and the log line below is what says why.
                    let queue = Arc::clone(&self.queue);
                    let held = recording.to_owned();
                    let renewed = tokio::task::spawn_blocking(
                        move || queue.renew(&held, stage, now_ms()),
                    ).await;

                    if matches!(renewed, Ok(Ok(false))) {
                        tracing::warn!(
                            recording = %recording,
                            %stage,
                            "this claim expired while the work was still running; finishing it \
                             anyway, and whoever holds it now may do it a second time"
                        );
                    }
                }
            }
        }
    }

    async fn upload(&self, claim: Claim) {
        let key = self.store.key_for(&claim.recording);
        let path = claim.recording.local_path.clone();
        let id = claim.recording.id.clone();

        let outcome = self
            .while_claimed(&id, Stage::Upload, self.store.put(&key, &path))
            .await;

        match outcome {
            Ok(bytes) => {
                let queue = Arc::clone(&self.queue);
                let done = {
                    let key = key.clone();
                    let id = id.clone();
                    tokio::task::spawn_blocking(move || {
                        queue.uploaded(&id, &key, Some(bytes), now_ms())
                    })
                    .await
                };

                match done {
                    Ok(Ok(())) => {
                        tracing::info!(
                            recording = %id,
                            bucket = self.store.bucket(),
                            object = %key,
                            bytes,
                            "a recording is in the object store"
                        );

                        if !self.settings.keep_local {
                            let recording = claim.recording.clone();
                            let _ =
                                tokio::task::spawn_blocking(move || disk::forget(&recording)).await;
                        }
                    }
                    // The object is up and the row does not say so. The upload
                    // is claimed again when the lease runs out and the same
                    // object is written again, which costs one upload and no
                    // correctness: the key is a function of the recording.
                    Ok(Err(error)) => tracing::error!(
                        recording = %id,
                        object = %key,
                        %error,
                        "this recording is in the object store and the store could not be told; \
                         it will be uploaded again"
                    ),
                    Err(error) => tracing::error!(%error, "recording the upload did not finish"),
                }
            }
            Err(error) => {
                let missing = matches!(error, UploadError::Missing(_));
                self.put_back(&id, Stage::Upload, claim.attempts, &error.to_string())
                    .await;

                if missing {
                    tracing::error!(
                        recording = %id,
                        path = %path.display(),
                        "the file this recording names is not on this node; either its volume did \
                         not come back or the row belongs to another node's disk"
                    );
                } else {
                    tracing::warn!(recording = %id, %error, "an upload failed and will be retried");
                }
            }
        }
    }

    async fn announce(&self, claim: Claim) {
        let id = claim.recording.id.clone();
        let announce = Arc::clone(&self.announce);
        let bucket = self.store.bucket().to_owned();
        let recording = claim.recording.clone();
        let said = self
            .while_claimed(
                &id,
                Stage::Announce,
                tokio::task::spawn_blocking(move || announce.ready(&recording, &bucket)),
            )
            .await;

        match said {
            Ok(Ok(())) => {
                let queue = Arc::clone(&self.queue);
                let marked = {
                    let id = id.clone();
                    tokio::task::spawn_blocking(move || queue.announced(&id, now_ms())).await
                };

                if let Ok(Err(error)) = marked {
                    tracing::error!(
                        recording = %id,
                        %error,
                        "a recording was announced and the store could not be told; it will be \
                         announced again"
                    );
                }
            }
            Ok(Err(reason)) => {
                self.put_back(&id, Stage::Announce, claim.attempts, &reason)
                    .await;
                tracing::warn!(recording = %id, %reason, "a recording could not be announced");
            }
            Err(error) => tracing::error!(%error, "announcing a recording did not finish"),
        }
    }

    /// Gives a claim back with a reason and a time to try again.
    ///
    /// The store refuses the put-back if the claim has already gone to somebody
    /// else, and that refusal is worth a line: it means this node spent a lease
    /// inside one piece of work, and whatever it was doing has been -- or is
    /// being -- done again elsewhere.
    async fn put_back(&self, recording: &str, stage: Stage, attempts: u32, reason: &str) {
        let queue = Arc::clone(&self.queue);
        let due = now_ms() + self.wait_for(attempts).as_millis() as i64;
        let id = recording.to_owned();
        let reason = reason.to_owned();

        let given =
            tokio::task::spawn_blocking(move || queue.postpone(&id, stage, &reason, due)).await;

        if matches!(given, Ok(Ok(false))) {
            tracing::warn!(
                %recording,
                %stage,
                "this work failed and the claim was no longer this node's to give back, so the \
                 row was left exactly as whoever holds it now has it"
            );
        }
    }

    fn wait_for(&self, attempts: u32) -> Duration {
        let doubling = attempts.saturating_sub(1).min(10);

        self.settings
            .backoff
            .saturating_mul(1u32 << doubling)
            .min(self.settings.max_backoff)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
