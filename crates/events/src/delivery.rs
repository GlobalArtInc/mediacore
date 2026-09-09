//! Getting an event out of this process, and what happens when that fails.
//!
//! Every network destination -- each webhook endpoint, and the broker -- is a
//! [`Target`]: a [`Sink`] that knows how to make one attempt, and a [`Retry`]
//! that says how many attempts it gets. [`Delivery`] is what stands between
//! the emitter and them.
//!
//! ```text
//!   Bus::emit ──▶ (unbounded) ──▶ dispatcher ──▶ outbox.record
//!    no I/O                        one task           │
//!    no waiting                                       ├─▶ (bounded) ─▶ webhook worker ─▶ settle
//!                                                     └─▶ (bounded) ─▶ kafka worker   ─▶ settle
//! ```
//!
//! Three properties come out of that shape, and each of them is a thing that
//! would otherwise go wrong:
//!
//! * **The hot path never waits.** `emit` pushes into an unbounded queue and
//!   returns. Everything below it -- the file, the request, the backoff, the
//!   broker -- is on tasks nobody joining a room is waiting for, and the two
//!   blocking calls in the whole pipeline go through `spawn_blocking` so they
//!   land on the blocking pool instead of a runtime thread that is also
//!   carrying media.
//! * **An event is written down before it is attempted.** So an endpoint that
//!   is away for two minutes costs latency rather than two minutes of
//!   meetings, and a restart in the middle of that costs nothing.
//! * **One slow destination does not hold up another.** A queue and a worker
//!   each, so a webhook endpoint that answers in thirty seconds does not stop
//!   the broker getting the same event now. Within one destination the order
//!   is kept, because a `participant_left` arriving before its
//!   `participant_joined` is worse than arriving late.
//!
//! Where it gives up: after [`Retry::attempts`] failures for one destination,
//! and when a destination's queue is full. Both say so in a log line naming
//! the event and the destination, both are counted for `/healthz`, and neither
//! loses the underlying fact -- the meeting is in the session record either
//! way, which is why an event is allowed to be given up on at all.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::log::quoted;
use crate::outbox::Outbox;
use crate::{Bus, Event, Listener};

/// Why one attempt did not land.
///
/// A string, because every destination fails differently and the only thing
/// done with it is putting it in a log line. It is quoted on the way there:
/// the text can carry a hostname an operator chose and, through a redirect or
/// a broker error, characters that would otherwise write a line of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryError(pub String);

impl DeliveryError {
    pub fn new(reason: impl std::fmt::Display) -> Self {
        Self(reason.to_string())
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Somewhere an event goes, and one attempt at putting it there.
///
/// One attempt: retrying is [`Delivery`]'s job and not the sink's, so that
/// every destination gives up the same way, counts the same way and says so in
/// the same log line.
pub trait Sink: Send + Sync + 'static {
    /// What this destination is called, in the outbox and in the log. It has
    /// to be stable across restarts -- a record on the disk names it -- so it
    /// is the endpoint's URL or the broker's topic, not an index.
    fn name(&self) -> &str;

    /// One attempt. `dropped` is how many events for this destination were
    /// thrown away in front of this one, which goes on the wire so the
    /// consumer knows it has a hole rather than guessing from a gap in the
    /// timestamps.
    fn deliver<'a>(
        &'a self,
        event: &'a Event,
        dropped: u32,
    ) -> BoxFuture<'a, Result<(), DeliveryError>>;
}

/// How many attempts a destination gets, and how long the waits between them
/// are.
///
/// Bounded on purpose. An endpoint that has refused an event five times over
/// half a minute is not going to take it on the sixth, and a queue that
/// retries for ever is a queue that never gets to the next event -- so the
/// event after the one that is wedged is punished for it. Giving up is loud:
/// see [`Delivery`].
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: u32,
    pub backoff: Duration,
    pub max_backoff: Duration,
}

impl Retry {
    /// How long to wait before attempt number `attempt + 1`.
    ///
    /// Doubling, capped. The cap matters more than the doubling: without one,
    /// the fifth wait on a generous configuration is minutes long, and an
    /// endpoint that came back thirty seconds ago waits out the rest of it.
    pub fn wait(&self, attempt: u32) -> Duration {
        let doubled = self
            .backoff
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16)));

        doubled.min(self.max_backoff)
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 5,
            backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// One destination, with the patience it is given.
pub struct Target {
    pub sink: Arc<dyn Sink>,
    pub retry: Retry,
}

impl Target {
    pub fn new(sink: Arc<dyn Sink>, retry: Retry) -> Self {
        Self { sink, retry }
    }
}

#[derive(Default)]
struct Counters {
    delivered: AtomicU64,
    abandoned: AtomicU64,
    dropped: AtomicU64,
}

/// One event on its way to one destination.
struct Job {
    seq: u64,
    event: Arc<Event>,
}

/// A destination's queue, and what has been thrown away in front of it.
struct Queue {
    name: String,
    jobs: mpsc::Sender<Job>,
    dropped: Arc<AtomicU32>,
}

/// The machinery between the emitter and the network.
///
/// It is itself an ordinary listener on the [`Bus`] -- there is no privileged
/// path, and a consumer inside this process gets its events by the same means
/// the webhooks do.
///
/// Owned by the state it serves: the last clone going takes the tasks with it,
/// rather than leaving them delivering events from a server nobody can reach.
pub struct Delivery {
    outbox: Arc<Outbox>,
    counters: Arc<Counters>,
    tasks: Vec<JoinHandle<()>>,
}

impl Delivery {
    /// Starts the dispatcher and one worker per destination. Must be called
    /// from inside a Tokio runtime.
    ///
    /// `depth` is how many events one destination may be behind before the
    /// next one is thrown away instead of queued. It bounds two things at
    /// once: how much memory an unreachable endpoint costs, and how large the
    /// outbox directory can get, since a record exists exactly while an event
    /// is in a queue.
    pub fn start(bus: &Bus, outbox: Arc<Outbox>, targets: Vec<Target>, depth: usize) -> Arc<Self> {
        let listener = bus.listen();
        let counters = Arc::new(Counters::default());
        let mut queues = Vec::new();
        let mut tasks = Vec::new();

        for target in targets {
            let (tx, rx) = mpsc::channel(depth.max(1));
            let dropped = Arc::new(AtomicU32::new(0));

            queues.push(Queue {
                name: target.sink.name().to_owned(),
                jobs: tx,
                dropped: Arc::clone(&dropped),
            });
            tasks.push(tokio::spawn(work(
                target,
                rx,
                Arc::clone(&outbox),
                Arc::clone(&counters),
                dropped,
            )));
        }

        tasks.push(tokio::spawn(dispatch(
            listener,
            queues,
            Arc::clone(&outbox),
            Arc::clone(&counters),
        )));

        Arc::new(Self {
            outbox,
            counters,
            tasks,
        })
    }

    /// Events every destination has taken.
    pub fn delivered(&self) -> u64 {
        self.counters.delivered.load(Ordering::Relaxed)
    }

    /// Events a destination refused until this server stopped asking. The one
    /// worth an alert: something out there is missing them and nothing else
    /// will say so.
    pub fn abandoned(&self) -> u64 {
        self.counters.abandoned.load(Ordering::Relaxed)
    }

    /// Events never attempted, because a destination was too far behind to
    /// queue another. Also worth an alert, and for a different reason: this
    /// one means a consumer is slower than the server, not that it is broken.
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }

    /// Events nobody at some destination will ever hear about: given up on
    /// after every attempt, or never attempted because that destination was
    /// too far behind to take another. One number rather than two because
    /// there is one thing to do about either of them, and the log line says
    /// which happened.
    pub fn undelivered(&self) -> u64 {
        self.abandoned() + self.dropped()
    }

    /// Events written down and not yet taken by everybody.
    pub fn owed(&self) -> usize {
        self.outbox.owed()
    }

    /// Outbox records that could not be written: events that will not survive
    /// a restart.
    pub fn unwritten(&self) -> u64 {
        self.outbox.unwritten()
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The one task that turns an emitted event into a promise on the disk and a
/// job for each destination.
///
/// One task rather than one per destination, because the outbox record has to
/// be written once and written before anybody is handed the event.
async fn dispatch(
    mut listener: Listener,
    queues: Vec<Queue>,
    outbox: Arc<Outbox>,
    counters: Arc<Counters>,
) {
    let names: Vec<String> = queues.iter().map(|queue| queue.name.clone()).collect();

    // What the last process promised and did not deliver, before anything new.
    // Their sequence numbers are lower than anything this process will hand
    // out, so a consumer sees yesterday's events before today's.
    let restoring = {
        let outbox = Arc::clone(&outbox);
        let names = names.clone();
        tokio::task::spawn_blocking(move || outbox.restore(&names)).await
    };
    for owed in restoring.unwrap_or_default() {
        for queue in &queues {
            if owed.pending.contains(&queue.name) {
                hand(queue, &outbox, &counters, owed.seq, &owed.event).await;
            }
        }
    }

    while let Some(event) = listener.recv().await {
        announce(&queues, &outbox, &counters, &event).await;
    }
}

/// Writes one event down and hands it to every destination that has room for
/// it.
///
/// The room is taken **before** the record is written, with `try_reserve`
/// rather than a later `try_send`. Two things come out of that order. A
/// destination that is too far behind never appears in the record at all, so
/// an event nobody can take costs no disk write and not a write followed
/// immediately by a delete. And the record is still on the disk before any
/// worker can see the event, which is the invariant the whole module rests on:
/// an event being attempted is an event already written down.
///
/// Reserving never waits. Waiting here would stop the dispatcher, and the
/// queue in front of the dispatcher is the unbounded one -- so waiting on a
/// full queue would turn a slow consumer into a growing heap rather than into a
/// bounded, counted, logged loss.
async fn announce(
    queues: &[Queue],
    outbox: &Arc<Outbox>,
    counters: &Arc<Counters>,
    event: &Arc<Event>,
) {
    let mut taking = Vec::new();
    let mut names = Vec::new();

    for queue in queues {
        match queue.jobs.try_reserve() {
            Ok(permit) => {
                names.push(queue.name.clone());
                taking.push(permit);
            }
            Err(_) => refuse(queue, counters, event),
        }
    }

    if taking.is_empty() {
        return;
    }

    // On the blocking pool: this is a file being flushed, and a flush on a
    // runtime thread is a flush somebody's audio is queued behind.
    let seq = {
        let outbox = Arc::clone(outbox);
        let event = Arc::clone(event);
        match tokio::task::spawn_blocking(move || outbox.record(&event, &names)).await {
            Ok(seq) => seq,
            // The blocking pool is gone, which happens when the runtime is
            // shutting down and nothing else. Nothing was written, so there is
            // nothing to settle and nothing to hand on.
            Err(_) => return,
        }
    };

    for permit in taking {
        permit.send(Job {
            seq,
            event: Arc::clone(event),
        });
    }
}

/// Hands one already-recorded event to one destination.
///
/// The restart path only: the record was written by the process that promised
/// it, so there is nothing to write here and the queue may already be full if
/// `queue` was lowered since. A destination that cannot take it is struck off
/// the record, because nothing else ever would be.
async fn hand(
    queue: &Queue,
    outbox: &Arc<Outbox>,
    counters: &Arc<Counters>,
    seq: u64,
    event: &Arc<Event>,
) {
    let job = Job {
        seq,
        event: Arc::clone(event),
    };
    if queue.jobs.try_send(job).is_ok() {
        return;
    }

    refuse(queue, counters, event);
    settle(outbox, seq, &queue.name).await;
}

/// Counts and says out loud that one destination did not get one event,
/// because it is too far behind to be given another.
fn refuse(queue: &Queue, counters: &Arc<Counters>, event: &Arc<Event>) {
    queue.dropped.fetch_add(1, Ordering::Relaxed);
    counters.dropped.fetch_add(1, Ordering::Relaxed);

    tracing::error!(
        event = %event.id,
        kind = %event.kind,
        room = %quoted(event.room_name()),
        destination = %quoted(&queue.name),
        "this destination is too far behind to be given another event, so this one was thrown \
         away; the next event it does take will say how many it missed, and the meeting itself is \
         still in the session record"
    );
}

/// One destination's worker: takes its jobs in order, gives each one the
/// attempts it is allowed, and strikes it off the outbox either way.
async fn work(
    target: Target,
    mut jobs: mpsc::Receiver<Job>,
    outbox: Arc<Outbox>,
    counters: Arc<Counters>,
    dropped: Arc<AtomicU32>,
) {
    let name = target.sink.name().to_owned();

    while let Some(job) = jobs.recv().await {
        // Read rather than taken: if this event is abandoned, the count stays
        // for the next one, so a hole is reported to whoever eventually hears
        // about it rather than being lost with the event that was carrying it.
        let missed = dropped.load(Ordering::Relaxed);

        for attempt in 1..=target.retry.attempts.max(1) {
            match target.sink.deliver(&job.event, missed).await {
                Ok(()) => {
                    dropped.fetch_sub(missed, Ordering::Relaxed);
                    counters.delivered.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        event = %job.event.id,
                        kind = %job.event.kind,
                        destination = %quoted(&name),
                        attempt,
                        "event delivered"
                    );
                    break;
                }
                Err(error) if attempt < target.retry.attempts.max(1) => {
                    let wait = target.retry.wait(attempt);
                    tracing::warn!(
                        event = %job.event.id,
                        kind = %job.event.kind,
                        destination = %quoted(&name),
                        attempt,
                        of = target.retry.attempts,
                        wait_ms = wait.as_millis() as u64,
                        reason = %quoted(&error),
                        "an event could not be delivered; trying again"
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(error) => {
                    counters.abandoned.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        event = %job.event.id,
                        kind = %job.event.kind,
                        room = %quoted(job.event.room_name()),
                        destination = %quoted(&name),
                        attempts = target.retry.attempts.max(1),
                        reason = %quoted(&error),
                        "an event was abandoned: nobody at this destination will hear about it. \
                         What it was announcing is still on this server -- a meeting under \
                         /api/sessions/<id>, a room under ListRooms -- and this line is the only \
                         notice that the announcement did not arrive"
                    );
                    break;
                }
            }
        }

        settle(&outbox, job.seq, &name).await;
    }
}

/// Strikes one destination off an outbox record, on the blocking pool.
async fn settle(outbox: &Arc<Outbox>, seq: u64, name: &str) {
    let outbox = Arc::clone(outbox);
    let name = name.to_owned();

    let _ = tokio::task::spawn_blocking(move || outbox.settle(seq, &name)).await;
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
