//! Meetings and recordings in Postgres, shared by every server process.
//!
//! **Why a database at all.** One directory of JSON files is right for one
//! process and wrong for three. Three cannot share it -- each would hold its
//! own index of it and overwrite the others' records -- and, worse, a node that
//! dies takes its unfinished meetings out of reach of the two that are still
//! up: nothing else can close them, and nothing else can pick up the recordings
//! they left half-done. A row in a database that all three can read is what
//! makes "the node that was holding this has stopped" a thing the other two can
//! notice and act on.
//!
//! **What is deliberately not in here.** No audio: a row names a file, and the
//! file stays on the node that recorded it until it is uploaded. And nothing on
//! the path a participant joins by -- see [`crate::SessionStore`], where a
//! membership change is recorded in memory and handed to a writer thread. Every
//! statement in this file runs on a thread this module owns.
//!
//! **Threads, and why they are ours.** `postgres::Client` is the blocking
//! client, and it drives its own single-threaded Tokio runtime with `block_on`.
//! Calling it from a thread that is *already* driving a runtime -- an axum
//! handler, say -- panics with "cannot start a runtime from within a runtime".
//! So no caller is ever handed a client: the work is sent to a small pool of
//! plain threads that own the connections, and the caller blocks on a channel
//! until the answer comes back. A caller that is itself on an async runtime
//! wraps that block in `spawn_blocking`, exactly as it would for a file read,
//! and the worst a caller who forgets can do is hold up a runtime thread rather
//! than crash the server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use postgres::{Client, NoTls, Row, Transaction};

use crate::recording::{Claim, Recording, Stage, State, Work};
use crate::schema::{MIGRATION_LOCK, MIGRATIONS, SCHEMA_TABLE};
use crate::{Attendance, Ending, Kind, Listing, Session, Sink, StoreError};

/// How often the cluster-wide meeting count on `/healthz` is refreshed.
///
/// It is a `count(*)`, so it is not run on demand: an operator scraping health
/// every fifteen seconds must not be a reason to walk the table. Stale by up to
/// this long, and a number that is a minute out of date answers the question it
/// is asked ("is this thing recording anything at all") just as well.
const COUNT_EVERY: Duration = Duration::from_secs(60);

/// How often the same failure is allowed to say so.
///
/// A database that is away produces one of these per writer tick, which is
/// several a minute per node. The first is logged, and then one a minute, and
/// coming back is logged as well -- so the log says when it went and when it
/// returned rather than burying both under the outage.
const COMPLAIN_EVERY: Duration = Duration::from_secs(60);

/// Where the database is and how this node is to behave towards it.
#[derive(Debug, Clone)]
pub struct PostgresSettings {
    /// A libpq connection string or URI: `postgres://user:pass@host/db`.
    ///
    /// Never logged. It carries a password in every deployment that has one,
    /// and a connection string in a log line is a password in a log
    /// aggregator; what gets logged is the host and database, pulled out of the
    /// parsed configuration.
    pub url: String,
    /// This server's `node_id`, written onto every meeting it is holding and
    /// onto every piece of work it claims.
    ///
    /// It is what makes "who was holding this when it stopped" answerable, so a
    /// deployment is better off setting it to something stable -- the pod name
    /// -- than taking the random default, which makes every restart look like a
    /// different machine.
    pub node: String,
    /// How many connections this node opens, which is also how many statements
    /// it can have in flight. One thread each.
    pub connections: usize,
    /// The cap on one connection attempt and on one statement. A query that has
    /// wedged holds a connection this long and no longer.
    pub timeout: Duration,
    /// How long a meeting or a claim is believed without being renewed.
    ///
    /// The one number that decides how quickly a dead node's work comes back,
    /// and it cannot be short: a node that is merely busy must not have its
    /// meetings closed under it. Renewal happens three times per lease.
    pub lease: Duration,
    /// How many times one piece of work is handed out before it is left alone.
    ///
    /// Spent on the claim rather than on the result, so a recording whose
    /// upload kills the process that tries it is dropped after this many
    /// nodes have died on it rather than going round for ever.
    pub attempts: u32,
}

impl PostgresSettings {
    /// Turns the url into a connection configuration, or says why it will never
    /// work.
    ///
    /// Called at startup. This is the one Postgres failure that *does* stop the
    /// server: a url that cannot be parsed is a typo in a config file, not an
    /// outage, and starting anyway would mean a server that can never record
    /// anything and says so once in a line at boot.
    fn connection(&self) -> Result<postgres::Config, String> {
        let mut config: postgres::Config = self.url.parse().map_err(|error| {
            format!("sessions.postgres.url is not a connection string: {error}")
        })?;

        // Written the way round that refuses anything but the two modes that
        // are happy without TLS, because `SslMode` is non-exhaustive: a
        // stricter mode added to the client library later would otherwise pass
        // this check and fail obscurely at the first connection instead.
        if !matches!(
            config.get_ssl_mode(),
            postgres::config::SslMode::Disable | postgres::config::SslMode::Prefer
        ) {
            return Err(
                "sessions.postgres.url asks for TLS, and this build has none for Postgres. Put \
                 the database on a trusted network -- in the same cluster, which is where it is \
                 meant to be -- or take sslmode out of the url"
                    .to_owned(),
            );
        }

        config
            .connect_timeout(self.timeout)
            .application_name("mediacore");

        Ok(config)
    }

    fn heartbeat(&self) -> Duration {
        (self.lease / 3).max(Duration::from_secs(1))
    }
}

/// What went wrong inside one job, before the pool decides what it means.
enum Failure {
    /// The database said no, or the connection did.
    Db(postgres::Error),
    /// The database answered, and the answer was not something this crate can
    /// make sense of -- a `kind` column holding a word that is not a kind.
    /// Somebody edited a row by hand, or an older version of this server wrote
    /// it.
    Nonsense(String),
}

impl From<postgres::Error> for Failure {
    fn from(error: postgres::Error) -> Self {
        Self::Db(error)
    }
}

/// Whether a connection is worth keeping after this error.
///
/// A constraint violation says something about the statement; a closed socket
/// says something about the connection. Only the second is a reason to throw
/// the connection away, and throwing it away on every error would mean a busy
/// server reconnecting all afternoon over a duplicate key.
fn ruined(error: &postgres::Error) -> bool {
    error.is_closed() || error.as_db_error().is_none()
}

type Job = Box<dyn FnOnce(&mut Held<'_>) + Send>;

/// One worker's connection, made when it is first needed and remade when it
/// breaks.
struct Held<'a> {
    client: &'a mut Option<Client>,
    config: &'a postgres::Config,
    timeout: Duration,
}

impl Held<'_> {
    fn client(&mut self) -> Result<&mut Client, StoreError> {
        if self.client.as_ref().is_some_and(Client::is_closed) {
            *self.client = None;
        }

        if self.client.is_none() {
            let mut fresh = self
                .config
                .connect(NoTls)
                .map_err(|error| StoreError::Unavailable(format!("connecting: {error}")))?;

            // Per connection rather than per statement: a query that has wedged
            // -- on a network that is half up, most often -- holds one of this
            // node's few connections until something lets go, and nothing else
            // here would ever let go.
            let statement_timeout = self.timeout.as_millis().max(1);
            fresh
                .batch_execute(&format!("SET statement_timeout = {statement_timeout}"))
                .map_err(|error| {
                    StoreError::Unavailable(format!("setting the statement timeout: {error}"))
                })?;

            *self.client = Some(fresh);
        }

        Ok(self.client.as_mut().expect("just connected"))
    }

    fn discard(&mut self) {
        *self.client = None;
    }
}

/// The threads that own the connections.
struct Pool {
    jobs: Mutex<Option<mpsc::SyncSender<Job>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Pool {
    fn start(config: postgres::Config, connections: usize, timeout: Duration) -> Self {
        // Bounded, and deliberately shallow: the queue's job is to hand work to
        // a free connection, not to hold work for a database that is not
        // keeping up. A caller that finds it full waits, which is the back
        // pressure -- and the one caller that must never wait, the join path,
        // never comes here at all.
        let (sender, receiver) = mpsc::sync_channel::<Job>(connections * 4);
        let receiver = Arc::new(Mutex::new(receiver));
        let config = Arc::new(config);

        let threads = (0..connections.max(1))
            .map(|index| {
                let receiver = Arc::clone(&receiver);
                let config = Arc::clone(&config);

                std::thread::Builder::new()
                    .name(format!("session-sql-{index}"))
                    .spawn(move || {
                        let mut client: Option<Client> = None;
                        loop {
                            let job = {
                                let queue = receiver.lock().expect("session job lock");
                                queue.recv()
                            };
                            let Ok(job) = job else { return };

                            let mut held = Held {
                                client: &mut client,
                                config: &config,
                                timeout,
                            };
                            job(&mut held);
                        }
                    })
                    .expect("a thread for the session database")
            })
            .collect();

        Self {
            jobs: Mutex::new(Some(sender)),
            threads: Mutex::new(threads),
        }
    }

    /// Runs one piece of work on a connection and waits for it.
    ///
    /// The wait is a channel receive and nothing else, so it is safe from any
    /// thread; what is not safe is doing it from a thread that is driving an
    /// async runtime, and that is the caller's business rather than this
    /// function's.
    fn run<T, F>(&self, job: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Client) -> Result<T, Failure> + Send + 'static,
    {
        let (answer, wait) = mpsc::sync_channel(1);

        let boxed: Job = Box::new(move |held: &mut Held<'_>| {
            let mut throw_away = false;

            let outcome = match held.client() {
                Err(error) => Err(error),
                Ok(client) => match job(client) {
                    Ok(value) => Ok(value),
                    Err(Failure::Db(error)) => {
                        throw_away = ruined(&error);
                        Err(StoreError::Unavailable(error.to_string()))
                    }
                    Err(Failure::Nonsense(what)) => Err(StoreError::Nonsense(what)),
                },
            };

            if throw_away {
                held.discard();
            }

            let _ = answer.send(outcome);
        });

        // Cloned out from under the lock rather than sent under it. The queue
        // is bounded, so a send can block until a worker is free, and a
        // blocking send with this lock held would stop the store being closed
        // by whoever is trying to shut the server down.
        let sender = self.jobs.lock().expect("session job lock").clone();
        let Some(sender) = sender else {
            return Err(StoreError::Unavailable(
                "this server is stopping and is not talking to the database any more".to_owned(),
            ));
        };
        sender
            .send(boxed)
            .map_err(|_| StoreError::Unavailable("the database workers have stopped".to_owned()))?;

        wait.recv().map_err(|_| {
            StoreError::Unavailable(
                "a database worker stopped without answering; the statement may or may not have \
                 run"
                .to_owned(),
            )
        })?
    }

    /// Lets the workers go, having let them finish what they were given.
    fn close(&self) {
        self.jobs.lock().expect("session job lock").take();

        for thread in self.threads.lock().expect("session job lock").drain(..) {
            let _ = thread.join();
        }
    }
}

/// Meetings, attendance, recordings and the work queue, in Postgres.
pub(crate) struct Database {
    settings: PostgresSettings,
    pool: Pool,
    /// Where the database is, for logs: host, port and database name, and
    /// never the password.
    address: String,
    /// Whether the schema has been checked and brought up to date on this
    /// connection. Read on every call and written once.
    ready: AtomicBool,
    /// Held while one thread is doing that, so three concurrent calls at
    /// startup do not each go and take the advisory lock.
    preparing: Mutex<()>,
    healthy: AtomicBool,
    meetings: AtomicU64,
    /// Recording work nobody is going to move: given up on, or waiting so far
    /// past its time that whoever it was waiting for is not coming back. On
    /// `/healthz`; refreshed on the same slow clock as the meeting count.
    stuck: AtomicU64,
    counted_at: Mutex<Option<std::time::Instant>>,
    complained_at: Mutex<Option<std::time::Instant>>,
    /// When the stranded-work warning was last said, so that a queue that has
    /// been stuck since Tuesday costs one line a minute and not one a tick.
    stranded_at: Mutex<Option<std::time::Instant>>,
}

impl Database {
    /// Builds the pool. Does **not** connect.
    ///
    /// Connecting here would mean a server that refuses to carry calls because
    /// a database is down, which is a worse failure than one that carries them
    /// and cannot record: the media path does not need this crate at all. So
    /// the first connection happens on the writer thread's first tick, and
    /// until it succeeds the store queues what it is told and says so on
    /// `/healthz`.
    pub(crate) fn open(settings: PostgresSettings) -> Result<Self, String> {
        let config = settings.connection()?;
        let address = describe(&config);
        let pool = Pool::start(config, settings.connections.max(1), settings.timeout);

        tracing::info!(
            database = %address,
            node = %settings.node,
            connections = settings.connections.max(1),
            lease_ms = settings.lease.as_millis(),
            "meeting records are kept in postgres; the schema is checked on the first connection"
        );

        Ok(Self {
            settings,
            pool,
            address,
            ready: AtomicBool::new(false),
            preparing: Mutex::new(()),
            healthy: AtomicBool::new(false),
            meetings: AtomicU64::new(0),
            stuck: AtomicU64::new(0),
            counted_at: Mutex::new(None),
            complained_at: Mutex::new(None),
            stranded_at: Mutex::new(None),
        })
    }

    /// Makes sure the schema is there, at most once however many callers ask.
    fn prepare(&self) -> Result<(), StoreError> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }

        let _one_at_a_time = self.preparing.lock().expect("session schema lock");
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }

        let timeout = self.settings.timeout.as_millis().max(1);
        let applied = self.pool.run(move |client| migrate(client, timeout))?;

        for step in applied {
            tracing::warn!(
                version = step.0,
                name = %step.1,
                "applied a schema migration to the session database"
            );
        }

        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Everything public goes through here: the schema first, then the work.
    fn ready(&self) -> Result<(), StoreError> {
        match self.prepare() {
            Ok(()) => {
                self.recovered();
                Ok(())
            }
            Err(error) => {
                self.complain(&error);
                Err(error)
            }
        }
    }

    /// Says the database is away, at most once a minute, and remembers that it
    /// is so `/healthz` can say so too.
    fn complain(&self, error: &StoreError) {
        self.healthy.store(false, Ordering::Relaxed);

        let mut last = self.complained_at.lock().expect("session complaint lock");
        let now = std::time::Instant::now();
        if last.is_some_and(|when| now.duration_since(when) < COMPLAIN_EVERY) {
            return;
        }
        *last = Some(now);

        tracing::error!(
            database = %self.address,
            %error,
            "the session database is not answering; meetings are still being held and are being \
             kept in memory until it comes back. Watch unwritten_sessions= on /healthz"
        );
    }

    fn recovered(&self) {
        if self.healthy.swap(true, Ordering::Relaxed) {
            return;
        }

        *self.complained_at.lock().expect("session complaint lock") = None;
        tracing::info!(
            database = %self.address,
            "the session database is answering again"
        );
    }

    /// Refreshes the two cluster-wide numbers on `/healthz`, at most once a
    /// minute: how many meetings the database holds, and how much recording
    /// work is stranded in it.
    fn recount(&self, now_ms: i64) {
        {
            let counted = self.counted_at.lock().expect("session count lock");
            if counted.is_some_and(|when| when.elapsed() < COUNT_EVERY) {
                return;
            }
        }

        let overdue = now_ms.saturating_sub(self.stranded_after().as_millis() as i64);
        let counted = self.pool.run(move |client| {
            // Two counts in one round trip, and each of them written so that it
            // can use one of the partial indexes on the work table -- a
            // `state = 'failed' OR (state = 'waiting' AND ...)` in one WHERE
            // walks every row of a table that keeps a recording's history for
            // ever.
            let row = client.query_one(
                "SELECT (SELECT count(*) FROM mediacore_meeting),
                        (SELECT count(*) FROM mediacore_recording_work
                          WHERE state = 'failed')
                      + (SELECT count(*) FROM mediacore_recording_work
                          WHERE state = 'waiting' AND due_at_ms <= $1)",
                &[&overdue],
            )?;
            Ok((row.get::<_, i64>(0), row.get::<_, i64>(1)))
        });

        match counted {
            Ok((meetings, stuck)) => {
                self.meetings
                    .store(meetings.max(0) as u64, Ordering::Relaxed);
                self.stuck.store(stuck.max(0) as u64, Ordering::Relaxed);
                *self.counted_at.lock().expect("session count lock") =
                    Some(std::time::Instant::now());

                if stuck > 0 {
                    self.stranded(stuck);
                }
            }
            Err(error) => self.complain(&error),
        }
    }

    /// How long a piece of work has to be overdue before nobody is coming for
    /// it.
    ///
    /// Twenty leases -- ten minutes with the defaults. It has to be far longer
    /// than a busy node's backlog, because the point is to name work that is
    /// *stranded* and not work that is merely queued behind other work: a node
    /// holding twenty uploads through an object-store outage is doing its job
    /// and must not be reported as broken.
    fn stranded_after(&self) -> Duration {
        (self.settings.lease * 20).max(Duration::from_secs(60))
    }

    /// Says that work is stranded, at most once a minute, with the remedy in
    /// the line.
    ///
    /// The commonest cause is an upload whose node never came back: the file
    /// is on a disk nobody has any more, so no other node can do the work and
    /// the row stays `waiting` for ever. That is deliberate -- handing it round
    /// the cluster would only burn its attempts and end in `failed` with the
    /// audio still where it was -- but it must not be silent, which is what
    /// this is.
    fn stranded(&self, stuck: i64) {
        let mut last = self.stranded_at.lock().expect("session stranded lock");
        let now = std::time::Instant::now();
        if last.is_some_and(|when| now.duration_since(when) < COMPLAIN_EVERY) {
            return;
        }
        *last = Some(now);

        tracing::warn!(
            database = %self.address,
            stuck,
            "recording work in this database is not moving: it has been given up on, or it is \
             waiting for a node that has not come back. `select * from mediacore_recording_work \
             where state = 'failed' or state = 'waiting'` says which; an upload waiting on a node \
             that is gone for good is freed by clearing `node` on its row in mediacore_recording, \
             which offers it to whoever asks -- and that one will honestly report the file is not \
             there. Watch stuck_recording_work= on /healthz"
        );
    }

    /// Says this node is still holding these meetings, and takes away any that
    /// a node has stopped saying that about.
    fn sweep(&self, live: &[String], now_ms: i64) {
        let lease = self.settings.lease.as_millis() as i64;
        let node = self.settings.node.clone();
        let held: Vec<String> = live.to_vec();

        let outcome = self.pool.run(move |client| {
            let mut transaction = client.transaction()?;

            if !held.is_empty() {
                transaction.execute(
                    "UPDATE mediacore_meeting
                        SET kept_alive_at_ms = $1, node = $2
                      WHERE id = ANY($3) AND ended_at_ms IS NULL",
                    &[&now_ms, &node, &held],
                )?;
            }

            let abandoned = reap(&mut transaction, now_ms - lease)?;
            transaction.commit()?;
            Ok(abandoned)
        });

        match outcome {
            Ok(abandoned) => {
                self.recovered();
                for (id, room, ended_at_ms) in abandoned {
                    tracing::warn!(
                        session = %id,
                        %room,
                        ended_at_ms,
                        "the node holding this meeting stopped saying it was alive; the record is \
                         closed at the last moment it knows about"
                    );
                }
            }
            Err(error) => self.complain(&error),
        }
    }

    fn work_of(&self, recording: String) -> Result<Vec<Work>, StoreError> {
        self.ready()?;
        self.pool.run(move |client| {
            let rows = client.query(
                "SELECT recording, stage, state, attempts, due_at_ms, claimed_by,
                        claim_ends_at_ms, done_at_ms, last_error
                   FROM mediacore_recording_work
                  WHERE recording = $1
                  ORDER BY stage",
                &[&recording],
            )?;
            rows.iter().map(read_work).collect()
        })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        self.pool.close();
    }
}

impl Sink for Database {
    fn describe(&self) -> String {
        format!("postgres:{}", self.address)
    }

    fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    fn count(&self) -> usize {
        self.meetings.load(Ordering::Relaxed) as usize
    }

    /// One transaction: the meeting row, then its attendance list replaced
    /// whole.
    ///
    /// Replaced rather than diffed, for the same reason the file store rewrites
    /// the whole file: the record is a small thing that is written a few dozen
    /// times over a meeting, and a diff is a second description of what changed
    /// that can disagree with the first.
    ///
    /// The one thing this will not do is un-end a meeting. If another node has
    /// already decided this meeting was abandoned and closed it, our update
    /// finds `ended_at_ms` set and does nothing but say so: two nodes writing
    /// the same meeting is a mistake somewhere, and the mistake must not be
    /// resolved in favour of whichever wrote last.
    fn persist(&self, session: &Session, now_ms: i64) -> Result<(), StoreError> {
        self.ready()?;

        let named = (session.id.clone(), session.room.clone());
        let session = session.clone();
        let node = self.settings.node.clone();
        // This node's own clock, and never a time out of the record. The
        // record's times are the meeting's -- handed in by whoever is driving
        // the room, and in a test they are whatever that test decided -- while
        // this one is the answer to "when did the node holding this meeting
        // last say it was alive", which is what every other node reads to
        // decide the node is gone.
        let kept_alive = now_ms;

        let outcome = self.pool.run(move |client| {
            let mut transaction = client.transaction()?;

            let written = transaction.query_opt(
                "INSERT INTO mediacore_meeting
                     (id, room, started_at_ms, ended_at_ms, ended_by, node, kept_alive_at_ms)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (id) DO UPDATE
                     SET ended_at_ms = EXCLUDED.ended_at_ms,
                         ended_by = EXCLUDED.ended_by,
                         node = EXCLUDED.node,
                         kept_alive_at_ms = EXCLUDED.kept_alive_at_ms
                   WHERE mediacore_meeting.ended_at_ms IS NULL
                 RETURNING id",
                &[
                    &session.id,
                    &session.room,
                    &session.started_at_ms,
                    &session.ended_at_ms,
                    &session.ended_by.map(|ending| ending.as_str()),
                    &node,
                    &kept_alive,
                ],
            )?;

            if written.is_none() {
                transaction.commit()?;
                return Ok(false);
            }

            transaction.execute(
                "DELETE FROM mediacore_attendance WHERE meeting = $1",
                &[&session.id],
            )?;

            if !session.participants.is_empty() {
                let sids: Vec<&str> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.sid.as_str())
                    .collect();
                let identities: Vec<&str> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.identity.as_str())
                    .collect();
                let names: Vec<&str> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.name.as_str())
                    .collect();
                let kinds: Vec<&str> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.kind.as_str())
                    .collect();
                let accounts: Vec<Option<&str>> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.account_id.as_deref())
                    .collect();
                let joined: Vec<i64> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.joined_at_ms)
                    .collect();
                let left: Vec<Option<i64>> = session
                    .participants
                    .iter()
                    .map(|attendance| attendance.left_at_ms)
                    .collect();

                // One statement rather than one per person: a twenty-person
                // meeting changes membership a few dozen times, and twenty
                // round trips per change is the difference between a write
                // that keeps up and one that does not.
                transaction.execute(
                    "INSERT INTO mediacore_attendance
                         (meeting, ordinal, sid, identity, display_name, kind, account_id,
                          joined_at_ms, left_at_ms)
                     SELECT $1, (person.ordinal - 1)::int, person.sid, person.identity,
                            person.display_name, person.kind, person.account_id,
                            person.joined_at_ms, person.left_at_ms
                       FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::text[],
                                   $7::bigint[], $8::bigint[])
                            WITH ORDINALITY AS person(sid, identity, display_name, kind,
                                                      account_id, joined_at_ms, left_at_ms,
                                                      ordinal)",
                    &[
                        &session.id,
                        &sids,
                        &identities,
                        &names,
                        &kinds,
                        &accounts,
                        &joined,
                        &left,
                    ],
                )?;
            }

            transaction.commit()?;
            Ok(true)
        });

        match outcome {
            Ok(true) => {
                self.recovered();
                Ok(())
            }
            Ok(false) => {
                self.recovered();
                tracing::warn!(
                    session = %named.0,
                    room = %named.1,
                    node = %self.settings.node,
                    "this meeting was already closed by another node, so this node's copy of it \
                     was not written; two nodes were holding one meeting"
                );
                Ok(())
            }
            Err(error) => {
                self.complain(&error);
                Err(error)
            }
        }
    }

    fn fetch(&self, id: &str) -> Result<Option<Session>, StoreError> {
        self.ready()?;

        let id = id.to_owned();
        let found = self.pool.run(move |client| {
            let Some(row) = client.query_opt(
                "SELECT id, room, started_at_ms, ended_at_ms, ended_by
                   FROM mediacore_meeting WHERE id = $1",
                &[&id],
            )?
            else {
                return Ok(None);
            };

            let mut session = read_meeting(&row)?;
            let people = client.query(
                "SELECT sid, identity, display_name, kind, account_id, joined_at_ms, left_at_ms
                   FROM mediacore_attendance WHERE meeting = $1 ORDER BY ordinal",
                &[&id],
            )?;
            session.participants = people
                .iter()
                .map(read_attendance)
                .collect::<Result<Vec<Attendance>, Failure>>()?;

            Ok(Some(session))
        });

        self.report(found)
    }

    fn page(&self, room: &str, offset: usize, limit: usize) -> Result<Listing, StoreError> {
        self.ready()?;

        let room = room.to_owned();
        let offset = offset as i64;
        let limit = limit as i64;

        let page = self.pool.run(move |client| {
            let mut transaction = client.transaction()?;

            let total: i64 = transaction
                .query_one(
                    "SELECT count(*) FROM mediacore_meeting WHERE room = $1",
                    &[&room],
                )?
                .get(0);

            // Newest first, and the id breaks a tie so that two meetings that
            // started in the same millisecond do not swap places between one
            // page and the next.
            let rows = transaction.query(
                "SELECT id, room, started_at_ms, ended_at_ms, ended_by
                   FROM mediacore_meeting
                  WHERE room = $1
                  ORDER BY started_at_ms DESC, id DESC
                 OFFSET $2 LIMIT $3",
                &[&room, &offset, &limit],
            )?;

            let mut sessions: Vec<Session> = rows
                .iter()
                .map(read_meeting)
                .collect::<Result<Vec<Session>, Failure>>()?;
            let ids: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();

            // One query for every attendance list on the page rather than one
            // per meeting: a hundred meetings is a hundred round trips
            // otherwise, with the answer being a few thousand short rows.
            let people = transaction.query(
                "SELECT meeting, sid, identity, display_name, kind, account_id,
                        joined_at_ms, left_at_ms
                   FROM mediacore_attendance
                  WHERE meeting = ANY($1)
                  ORDER BY meeting, ordinal",
                &[&ids],
            )?;
            drop(ids);

            let mut lists: HashMap<String, Vec<Attendance>> = HashMap::new();
            for row in &people {
                lists
                    .entry(row.get::<_, String>("meeting"))
                    .or_default()
                    .push(read_attendance(row)?);
            }
            for session in &mut sessions {
                session.participants = lists.remove(&session.id).unwrap_or_default();
            }

            transaction.commit()?;
            Ok(Listing {
                entries: sessions
                    .into_iter()
                    .map(|session| (session.id.clone(), Some(session)))
                    .collect(),
                total: total.max(0) as usize,
            })
        });

        self.report(page)
    }

    /// Three times per lease. A node that has been quiet for a whole lease has
    /// its meetings closed by somebody else, so the renewal has to be
    /// comfortably more often than that -- twice would leave no room for one
    /// missed statement.
    fn tick_every(&self) -> Duration {
        self.settings.heartbeat()
    }

    /// The writer thread's clock, spent on the two things a shared store needs
    /// and a directory does not: saying this node is still here, and noticing
    /// that another one is not.
    fn tick(&self, live: &[String], now_ms: i64) {
        if self.ready().is_err() {
            return;
        }

        // How often this happens is the writer's business and not this
        // module's: a second gate here, against a clock the caller supplies,
        // would quietly do nothing when a test is the clock -- which is
        // precisely when somebody is trying to watch a lease run out.
        self.sweep(live, now_ms);
        self.recount(now_ms);
    }

    fn note_recording(&self, recording: &Recording, now_ms: i64) -> Result<String, StoreError> {
        self.ready()?;

        let recording = recording.clone();
        let segment = i32::try_from(recording.segment)
            .map_err(|_| StoreError::Nonsense("the segment number does not fit".to_owned()))?;
        let sample_rate = i32::try_from(recording.sample_rate)
            .map_err(|_| StoreError::Nonsense("the sample rate does not fit".to_owned()))?;
        let channels = i16::try_from(recording.channels)
            .map_err(|_| StoreError::Nonsense("the channel count does not fit".to_owned()))?;
        let bytes = recording
            .bytes
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::Nonsense("the file size does not fit".to_owned()))?;

        let outcome = self.pool.run(move |client| {
            let mut transaction = client.transaction()?;
            let path = recording.local_path.to_string_lossy().into_owned();
            let complete = recording.is_complete();

            // On the natural key, not on the id: a recorder that was restarted
            // writes the same segment down again under a new id, and the row it
            // already wrote is the row it means. The id that comes back is the
            // one the store holds, which may not be the one that was asked for.
            let id: String = transaction
                .query_one(
                    "INSERT INTO mediacore_recording
                         (id, meeting, participant, account_id, segment, local_path, node,
                          object_key, bytes, sample_rate, channels, started_at_ms, ended_at_ms,
                          offset_ms, duration_ms)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
                     ON CONFLICT (meeting, participant, segment) DO UPDATE
                         SET local_path = EXCLUDED.local_path,
                             node = EXCLUDED.node,
                             bytes = EXCLUDED.bytes,
                             ended_at_ms = EXCLUDED.ended_at_ms,
                             duration_ms = EXCLUDED.duration_ms
                     RETURNING id",
                    &[
                        &recording.id,
                        &recording.meeting,
                        &recording.participant,
                        &recording.account_id,
                        &segment,
                        &path,
                        // Taken from the row and not from this node's settings:
                        // the file is wherever the caller says it is, and the
                        // caller is the node that has it on its disk -- which
                        // after a crash is a node re-adopting files it wrote in
                        // a previous life under a different id.
                        &recording.node,
                        &recording.object_key,
                        &bytes,
                        &sample_rate,
                        &channels,
                        &recording.started_at_ms,
                        &recording.ended_at_ms,
                        &recording.offset_ms,
                        &recording.duration_ms,
                    ],
                )?
                .get(0);

            // Both stages exist from the moment the recording does, so "what is
            // happening to this file" is always two rows and never a missing
            // one. The announcement is blocked until the upload is done, and
            // the upload is blocked until the file is closed: a half-written
            // file in an object store is worse than a late one, because nothing
            // downstream can tell it is half a file.
            let upload_state = if complete {
                State::Waiting
            } else {
                State::Blocked
            };
            transaction.execute(
                "INSERT INTO mediacore_recording_work (recording, stage, state, due_at_ms)
                 VALUES ($1, 'upload', $2, $3), ($1, 'announce', 'blocked', $3)
                 ON CONFLICT (recording, stage) DO NOTHING",
                &[&id, &upload_state.as_str(), &now_ms],
            )?;

            if complete {
                transaction.execute(
                    "UPDATE mediacore_recording_work
                        SET state = 'waiting', due_at_ms = $2
                      WHERE recording = $1 AND stage = 'upload' AND state = 'blocked'",
                    &[&id, &now_ms],
                )?;
            }

            transaction.commit()?;
            Ok(id)
        });

        self.report(outcome)
    }

    fn recordings(&self, meeting: &str) -> Result<Vec<Recording>, StoreError> {
        self.ready()?;

        let meeting = meeting.to_owned();
        let found = self.pool.run(move |client| {
            let rows = client.query(
                "SELECT id, meeting, participant, account_id, segment, local_path, node,
                        object_key, bytes, sample_rate, channels, started_at_ms, ended_at_ms,
                        offset_ms, duration_ms
                   FROM mediacore_recording
                  WHERE meeting = $1
                  ORDER BY participant, segment",
                &[&meeting],
            )?;
            rows.iter().map(read_recording).collect()
        });

        self.report(found)
    }

    fn work(&self, recording: &str) -> Result<Vec<Work>, StoreError> {
        let outcome = self.work_of(recording.to_owned());
        self.report(outcome)
    }

    fn meeting_work(&self, meeting: &str) -> Result<Vec<Work>, StoreError> {
        self.ready()?;

        let meeting = meeting.to_owned();
        let found = self.pool.run(move |client| {
            let rows = client.query(
                "SELECT work.recording, work.stage, work.state, work.attempts, work.due_at_ms,
                        work.claimed_by, work.claim_ends_at_ms, work.done_at_ms, work.last_error
                   FROM mediacore_recording_work AS work
                   JOIN mediacore_recording AS file ON file.id = work.recording
                  WHERE file.meeting = $1
                  ORDER BY file.participant, file.segment, work.stage",
                &[&meeting],
            )?;
            rows.iter().map(read_work).collect()
        });

        self.report(found)
    }

    /// Takes work out of the queue for this node, and for nobody else.
    ///
    /// **How two pollers do not take the same row.** The rows are picked in a
    /// `SELECT ... FOR UPDATE SKIP LOCKED`, so the second poller does not queue
    /// behind the first's locks and does not see the rows it holds -- it walks
    /// past them and takes the next ones. Both come back with work, neither
    /// comes back with the other's, and neither waits.
    ///
    /// **What happens to a claim whose holder died.** Nothing, for as long as
    /// the lease lasts. After that the row is due again and the next poller to
    /// ask gets it, because the claim is a time and not a flag: there is
    /// nothing to release, so there is nothing that has to be released by the
    /// process that is gone. The attempt it spent is not given back, which is
    /// what stops a recording that kills whatever tries it from going round for
    /// ever -- after `attempts` claimants have died on it, it is left alone
    /// with the last thing anybody said about it.
    fn claim(&self, stage: Stage, now_ms: i64, limit: usize) -> Result<Vec<Claim>, StoreError> {
        self.ready()?;

        let node = self.settings.node.clone();
        let lease = self.settings.lease.as_millis() as i64;
        let attempts = i32::try_from(self.settings.attempts).unwrap_or(i32::MAX);
        let limit = limit as i64;

        // The upload is offered only to the node whose disk holds the file --
        // `local_path` means nothing anywhere else, so a claim by anybody else
        // is an attempt that is spent finding out the file is not there. The
        // announcement needs no file and is offered to everybody, which is what
        // makes a node dying between the upload and the announcement cost
        // nothing. A row whose node is null is offered to everybody too: it was
        // written by hand, and the honest end of that is one node saying the
        // file is missing rather than every node staying silent.
        let on_this_node = matches!(stage, Stage::Upload).then(|| self.settings.node.clone());

        let claimed = self.pool.run(move |client| {
            let mut transaction = client.transaction()?;
            let ends_at = now_ms + lease;
            let name = stage.as_str();

            // A claim that expired with its attempts spent is not offered
            // again. Done before the claim rather than as part of it so that
            // the reason a row stopped moving is written into the row -- an
            // operator reading `failed` beside `last_error` can tell "nobody
            // ever came back" from "it kept saying no".
            transaction.execute(
                "UPDATE mediacore_recording_work
                    SET state = 'failed', claimed_by = NULL, claim_ends_at_ms = NULL,
                        last_error = coalesce(last_error,
                            'the node that claimed this stopped without saying what happened')
                  WHERE stage = $1 AND state = 'claimed'
                    AND claim_ends_at_ms <= $2 AND attempts >= $3",
                &[&name, &now_ms, &attempts],
            )?;

            let rows = transaction.query(
                "WITH picked AS (
                     SELECT candidate.recording
                       FROM mediacore_recording_work AS candidate
                       JOIN mediacore_recording AS file ON file.id = candidate.recording
                      WHERE candidate.stage = $3
                        AND candidate.attempts < $4
                        AND ((candidate.state = 'waiting' AND candidate.due_at_ms <= $5)
                          OR (candidate.state = 'claimed' AND candidate.claim_ends_at_ms <= $5))
                        AND ($7::text IS NULL OR file.node IS NULL OR file.node = $7)
                      ORDER BY candidate.due_at_ms
                        FOR UPDATE OF candidate SKIP LOCKED
                      LIMIT $6
                 )
                 UPDATE mediacore_recording_work AS work
                    SET state = 'claimed', attempts = work.attempts + 1,
                        claimed_by = $1, claim_ends_at_ms = $2
                   FROM picked
                  WHERE work.recording = picked.recording AND work.stage = $3
                 RETURNING work.recording, work.attempts, work.claim_ends_at_ms",
                &[
                    &node,
                    &ends_at,
                    &name,
                    &attempts,
                    &now_ms,
                    &limit,
                    &on_this_node,
                ],
            )?;

            let ids: Vec<String> = rows.iter().map(|row| row.get("recording")).collect();
            // One query for every recording claimed rather than one each: a
            // poller taking ten pieces of work should cost two round trips and
            // not eleven.
            let found = transaction.query(
                "SELECT id, meeting, participant, account_id, segment, local_path, node,
                        object_key, bytes, sample_rate, channels, started_at_ms, ended_at_ms,
                        offset_ms, duration_ms
                   FROM mediacore_recording WHERE id = ANY($1)",
                &[&ids],
            )?;
            let mut recordings: HashMap<String, Recording> = HashMap::new();
            for row in &found {
                let recording = read_recording(row)?;
                recordings.insert(recording.id.clone(), recording);
            }

            let mut claims = Vec::with_capacity(rows.len());
            for row in &rows {
                let id: String = row.get("recording");
                let taken: i32 = row.get("attempts");
                let ends: i64 = row.get("claim_ends_at_ms");

                let Some(recording) = recordings.remove(&id) else {
                    return Err(Failure::Nonsense(format!(
                        "`{id}` has work queued against it and is not a recording any more"
                    )));
                };

                claims.push(Claim {
                    recording,
                    stage,
                    attempts: taken.max(0) as u32,
                    ends_at_ms: ends,
                });
            }

            transaction.commit()?;
            Ok(claims)
        });

        self.report(claimed)
    }

    fn renew(&self, recording: &str, stage: Stage, now_ms: i64) -> Result<bool, StoreError> {
        self.ready()?;

        let recording = recording.to_owned();
        let node = self.settings.node.clone();
        let ends_at = now_ms + self.settings.lease.as_millis() as i64;

        let held = self.pool.run(move |client| {
            let row = client.query_opt(
                "UPDATE mediacore_recording_work
                    SET claim_ends_at_ms = $3
                  WHERE recording = $1 AND stage = $2 AND state = 'claimed' AND claimed_by = $4
                 RETURNING recording",
                &[&recording, &stage.as_str(), &ends_at, &node],
            )?;
            Ok(row.is_some())
        });

        self.report(held)
    }

    fn uploaded(
        &self,
        recording: &str,
        object_key: &str,
        bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        self.ready()?;

        let recording = recording.to_owned();
        let object_key = object_key.to_owned();
        let bytes = bytes
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::Nonsense("the file size does not fit".to_owned()))?;

        let done = self.pool.run(move |client| {
            let mut transaction = client.transaction()?;

            transaction.execute(
                "UPDATE mediacore_recording
                    SET object_key = $2, bytes = coalesce($3, bytes)
                  WHERE id = $1",
                &[&recording, &object_key, &bytes],
            )?;
            transaction.execute(
                "UPDATE mediacore_recording_work
                    SET state = 'done', done_at_ms = $2, claimed_by = NULL,
                        claim_ends_at_ms = NULL, last_error = NULL
                  WHERE recording = $1 AND stage = 'upload'",
                &[&recording, &now_ms],
            )?;
            // The announcement becomes possible in the same transaction that
            // makes the upload finished, so there is no moment in which a file
            // is in the object store and nothing is going to say so.
            transaction.execute(
                "UPDATE mediacore_recording_work
                    SET state = 'waiting', due_at_ms = $2
                  WHERE recording = $1 AND stage = 'announce' AND state = 'blocked'",
                &[&recording, &now_ms],
            )?;

            transaction.commit()?;
            Ok(())
        });

        self.report(done)
    }

    fn announced(&self, recording: &str, now_ms: i64) -> Result<(), StoreError> {
        self.ready()?;

        let recording = recording.to_owned();
        let done = self.pool.run(move |client| {
            client.execute(
                "UPDATE mediacore_recording_work
                    SET state = 'done', done_at_ms = $2, claimed_by = NULL,
                        claim_ends_at_ms = NULL, last_error = NULL
                  WHERE recording = $1 AND stage = 'announce'",
                &[&recording, &now_ms],
            )?;
            Ok(())
        });

        self.report(done)
    }

    /// **Only the node that still holds the claim can give it back**, which is
    /// the same rule [`Database::renew`] enforces and for the same reason.
    ///
    /// Without `state = 'claimed' AND claimed_by = <me>` this is a statement
    /// that undoes other people's work. A claimant whose lease expired while it
    /// was inside a slow upload comes back to say it failed; by then the row
    /// may have been claimed by another node, finished, and marked `done` --
    /// and an unconditional put-back returns a finished announcement to the
    /// queue to be emitted a second time, or clears a live claimant's lease
    /// while it is still uploading and pushes its due time out by the *stale*
    /// node's backoff. Worse, `attempts` keeps climbing, so a recording that
    /// really was announced can end as `failed` with `done_at_ms` set, which
    /// reads as nonsense to whoever has to fix it.
    ///
    /// `false` means the claim had already gone and the row was left exactly as
    /// it was.
    fn postpone(
        &self,
        recording: &str,
        stage: Stage,
        error: &str,
        due_at_ms: i64,
    ) -> Result<bool, StoreError> {
        self.ready()?;

        let recording = recording.to_owned();
        let error = error.to_owned();
        let node = self.settings.node.clone();
        let attempts = i32::try_from(self.settings.attempts).unwrap_or(i32::MAX);

        let put_back = self.pool.run(move |client| {
            let changed = client.execute(
                "UPDATE mediacore_recording_work
                    SET state = CASE WHEN attempts >= $5 THEN 'failed' ELSE 'waiting' END,
                        due_at_ms = $4, claimed_by = NULL, claim_ends_at_ms = NULL,
                        last_error = $3
                  WHERE recording = $1 AND stage = $2
                    AND state = 'claimed' AND claimed_by = $6",
                &[
                    &recording,
                    &stage.as_str(),
                    &error,
                    &due_at_ms,
                    &attempts,
                    &node,
                ],
            )?;
            Ok(changed > 0)
        });

        self.report(put_back)
    }

    fn stuck(&self) -> u64 {
        self.stuck.load(Ordering::Relaxed)
    }
}

impl Database {
    /// Marks the database up or down on the way out of every call, so that
    /// `/healthz` and the log say the same thing as the last statement did.
    fn report<T>(&self, outcome: Result<T, StoreError>) -> Result<T, StoreError> {
        match outcome {
            Ok(value) => {
                self.recovered();
                Ok(value)
            }
            Err(error) => {
                self.complain(&error);
                Err(error)
            }
        }
    }
}

/// Closes every meeting whose node has stopped saying it is alive.
///
/// The end time is the last moment the record itself knows about -- somebody's
/// departure, or the meeting's start if nobody ever left -- and never the
/// moment the sweep noticed, because nobody watched this meeting end and a
/// record that claims otherwise is a meeting invented to have gone on after the
/// server did.
fn reap(
    transaction: &mut Transaction<'_>,
    older_than_ms: i64,
) -> Result<Vec<(String, String, i64)>, Failure> {
    let rows = transaction.query(
        "WITH abandoned AS (
             SELECT meeting.id,
                    greatest(
                        meeting.started_at_ms,
                        coalesce((SELECT max(greatest(person.joined_at_ms,
                                                      coalesce(person.left_at_ms,
                                                               person.joined_at_ms)))
                                    FROM mediacore_attendance AS person
                                   WHERE person.meeting = meeting.id),
                                 meeting.started_at_ms)
                    ) AS last_known
               FROM mediacore_meeting AS meeting
              WHERE meeting.ended_at_ms IS NULL
                AND meeting.kept_alive_at_ms < $1
              LIMIT 200
         )
         UPDATE mediacore_meeting AS meeting
            SET ended_at_ms = abandoned.last_known, ended_by = 'restart'
           FROM abandoned
          WHERE meeting.id = abandoned.id AND meeting.ended_at_ms IS NULL
         RETURNING meeting.id, meeting.room, meeting.ended_at_ms",
        &[&older_than_ms],
    )?;

    let closed: Vec<(String, String, i64)> = rows
        .iter()
        .map(|row| {
            (
                row.get::<_, String>(0),
                row.get::<_, String>(1),
                row.get::<_, i64>(2),
            )
        })
        .collect();

    if !closed.is_empty() {
        let ids: Vec<&str> = closed.iter().map(|(id, _, _)| id.as_str()).collect();
        // Everybody in a meeting that ended left when it did. Without this
        // their stretches stay open for ever and nobody's attendance can be
        // measured.
        transaction.execute(
            "UPDATE mediacore_attendance AS person
                SET left_at_ms = meeting.ended_at_ms
               FROM mediacore_meeting AS meeting
              WHERE person.meeting = meeting.id
                AND person.meeting = ANY($1)
                AND person.left_at_ms IS NULL",
            &[&ids],
        )?;
    }

    Ok(closed)
}

/// Brings the schema up to date, under a lock the whole cluster shares.
fn migrate(client: &mut Client, timeout_ms: u128) -> Result<Vec<(i32, &'static str)>, Failure> {
    // A migration that adds an index to a table with a year of meetings in it
    // takes longer than any statement timeout worth having on ordinary
    // queries, and being killed halfway is the one thing a migration must not
    // be.
    client.batch_execute("SET statement_timeout = 0")?;
    client.execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK])?;

    let applied = apply(client);
    let released = client.execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK]);
    client.batch_execute(&format!("SET statement_timeout = {timeout_ms}"))?;

    // The unlock matters more than the migration did. A lock left held by a
    // connection this pool keeps would stop every other node in the cluster
    // from ever checking its schema, so a failure to release is reported as
    // the connection being ruined, which throws it away and takes the lock
    // with it.
    released?;
    applied
}

fn apply(client: &mut Client) -> Result<Vec<(i32, &'static str)>, Failure> {
    client.batch_execute(SCHEMA_TABLE)?;

    let rows = client.query("SELECT version FROM mediacore_schema", &[])?;
    let already: Vec<i32> = rows.iter().map(|row| row.get::<_, i32>(0)).collect();

    let mut applied = Vec::new();
    for migration in MIGRATIONS {
        if already.contains(&migration.version) {
            continue;
        }

        let mut transaction = client.transaction()?;
        transaction.batch_execute(migration.sql)?;
        transaction.execute(
            "INSERT INTO mediacore_schema (version, name) VALUES ($1, $2)",
            &[&migration.version, &migration.name],
        )?;
        transaction.commit()?;

        applied.push((migration.version, migration.name));
    }

    Ok(applied)
}

fn read_meeting(row: &Row) -> Result<Session, Failure> {
    let ended_by: Option<String> = row.get("ended_by");
    let ended_by = match ended_by.as_deref() {
        None => None,
        Some(word) => Some(Ending::parse(word).ok_or_else(|| {
            Failure::Nonsense(format!("`{word}` is not a way for a meeting to end"))
        })?),
    };

    Ok(Session {
        id: row.get("id"),
        room: row.get("room"),
        started_at_ms: row.get("started_at_ms"),
        ended_at_ms: row.get("ended_at_ms"),
        ended_by,
        participants: Vec::new(),
    })
}

fn read_attendance(row: &Row) -> Result<Attendance, Failure> {
    let kind: String = row.get("kind");

    Ok(Attendance {
        sid: row.get("sid"),
        identity: row.get("identity"),
        name: row.get("display_name"),
        kind: Kind::parse(&kind)
            .ok_or_else(|| Failure::Nonsense(format!("`{kind}` is not a kind of participant")))?,
        account_id: row.get("account_id"),
        joined_at_ms: row.get("joined_at_ms"),
        left_at_ms: row.get("left_at_ms"),
    })
}

fn read_recording(row: &Row) -> Result<Recording, Failure> {
    let path: String = row.get("local_path");
    let segment: i32 = row.get("segment");
    let sample_rate: i32 = row.get("sample_rate");
    let channels: i16 = row.get("channels");
    let bytes: Option<i64> = row.get("bytes");

    Ok(Recording {
        id: row.get("id"),
        meeting: row.get("meeting"),
        participant: row.get("participant"),
        account_id: row.get("account_id"),
        segment: segment.max(0) as u32,
        local_path: path.into(),
        node: row.get("node"),
        object_key: row.get("object_key"),
        bytes: bytes.map(|size| size.max(0) as u64),
        sample_rate: sample_rate.max(0) as u32,
        channels: channels.max(0) as u16,
        started_at_ms: row.get("started_at_ms"),
        ended_at_ms: row.get("ended_at_ms"),
        offset_ms: row.get("offset_ms"),
        duration_ms: row.get("duration_ms"),
    })
}

fn read_work(row: &Row) -> Result<Work, Failure> {
    let stage: String = row.get("stage");
    let state: String = row.get("state");
    let attempts: i32 = row.get("attempts");

    Ok(Work {
        recording: row.get("recording"),
        stage: Stage::parse(&stage)
            .ok_or_else(|| Failure::Nonsense(format!("`{stage}` is not a stage")))?,
        state: State::parse(&state)
            .ok_or_else(|| Failure::Nonsense(format!("`{state}` is not a state")))?,
        attempts: attempts.max(0) as u32,
        due_at_ms: row.get("due_at_ms"),
        claimed_by: row.get("claimed_by"),
        claim_ends_at_ms: row.get("claim_ends_at_ms"),
        done_at_ms: row.get("done_at_ms"),
        last_error: row.get("last_error"),
    })
}

/// Where the database is, in a form that is safe to put in a log line.
///
/// The password is in the connection string and never in here.
fn describe(config: &postgres::Config) -> String {
    let host = config
        .get_hosts()
        .first()
        .map(|host| match host {
            postgres::config::Host::Tcp(name) => name.clone(),
            #[cfg(unix)]
            postgres::config::Host::Unix(path) => path.display().to_string(),
        })
        .unwrap_or_else(|| "?".to_owned());
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let database = config.get_dbname().unwrap_or("?");

    format!("{host}:{port}/{database}")
}
