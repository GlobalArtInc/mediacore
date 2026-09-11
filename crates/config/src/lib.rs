use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

pub mod variables;

/// The shortest secret this server will sign and verify a token with.
///
/// Every token is HS256, whose key must be at least as long as the hash it
/// feeds -- 256 bits, and RFC 7518 section 3.2 says MUST rather than should.
/// `openssl rand -hex 32`, which is what `deploy/README.md` tells people to
/// generate, is twice that in characters.
///
/// The number is the front end's, from `MEDIACORE_API_SECRET` and the same
/// deployment: `crates/front/src/config.rs` refuses to start below it. The two
/// halves mint and verify each other's tokens, so a secret one of them will not
/// start on is not a secret this deployment can use, and a server that accepts
/// what the front end rejects is a disagreement about which tokens are valid
/// rather than a tolerance. The one number lives in both crates because the
/// front end does not depend on this one.
pub const MIN_SECRET_BYTES: usize = 32;

pub const DEFAULT_END_GRACE_SECONDS: u32 = 90;
pub const DEFAULT_ICE_DISCONNECTED_TIMEOUT_SECONDS: u32 = 20;
/// A day, which is not a session length but an outer bound on one.
///
/// A minted TURN credential has to still be valid when the relay is asked for
/// an allocation and every time that allocation is refreshed, which is for as
/// long as the call lasts. A number chosen to be tight -- an hour, say -- ends
/// a meeting that ran long by taking its relay away mid-sentence, and the
/// credential is worth nothing to anybody who steals it beyond bandwidth
/// through one coturn under one identity.
pub const DEFAULT_ICE_CREDENTIAL_TTL_SECONDS: u32 = 24 * 60 * 60;
pub const DEFAULT_NEGOTIATION_TIMEOUT_SECONDS: u32 = 15;
pub const MAX_DATA_PACKET_BYTES: u32 = 64 * 1024;
/// Three beats at ten seconds. Long enough that a node paused by a slow
/// database is not read as dead, short enough that a node that really died
/// stops being counted before anybody notices it in a room list.
pub const DEFAULT_CLUSTER_LEASE_SECONDS: u32 = 30;

pub const DEFAULT_DATA_MAX_PACKET_BYTES: u32 = 64_000;
pub const DEFAULT_DATA_RATE: u32 = 60;
pub const DEFAULT_DATA_BURST: u32 = 120;
pub const DEFAULT_DYNACAST_PAUSE_DELAY_MS: u32 = 5_000;
pub const DEFAULT_LAYER_UPGRADE_HOLD_MS: u32 = 2_000;
pub const DEFAULT_BWE_INITIAL_KBPS: u32 = 600;
pub const DEFAULT_BWE_MIN_BUDGET_KBPS: u32 = 150;
pub const DEFAULT_BWE_MAX_DESIRED_KBPS: u32 = 10_000;
pub const DEFAULT_BWE_AUDIO_RESERVE_KBPS: u32 = 48;
pub const DEFAULT_BWE_OVERHEAD_BYTES: u32 = 40;
pub const DEFAULT_BWE_ALLOCATION_INTERVAL_MS: u32 = 250;
pub const DEFAULT_BWE_RECOVERY_HOLD_MS: u32 = 2_000;
pub const DEFAULT_SWEEP_INTERVAL_MS: u32 = 1_000;
pub const DEFAULT_BWE_RECOVERY_STEP_MS: u32 = 1_000;
pub const DEFAULT_BWE_QUEUE_PRESSURE_MS: u32 = 150;
pub const DEFAULT_BWE_MAX_OVERRUN_PERCENT: u32 = 150;
pub const DEFAULT_EVENT_QUEUE: u32 = 10_000;
pub const DEFAULT_EVENT_ATTEMPTS: u32 = 5;
pub const DEFAULT_EVENT_TIMEOUT_MS: u32 = 5_000;
pub const DEFAULT_EVENT_BACKOFF_MS: u32 = 1_000;
pub const DEFAULT_EVENT_MAX_BACKOFF_MS: u32 = 30_000;
pub const DEFAULT_KAFKA_TOPIC: &str = "mediacore";
pub const DEFAULT_SESSION_CONNECTIONS: u32 = 4;
pub const DEFAULT_SESSION_TIMEOUT_MS: u32 = 10_000;
pub const DEFAULT_SESSION_LEASE_MS: u32 = 30_000;
pub const DEFAULT_SESSION_ATTEMPTS: u32 = 5;
pub const DEFAULT_SESSION_MAX_OPAQUE_BYTES: u32 = 16 * 1024;
pub const DEFAULT_RECORDING_MAX_SEGMENT_MB: u32 = 256;
pub const DEFAULT_RECORDING_REORDER_HOLD_MS: u32 = 100;
pub const DEFAULT_RECORDING_QUEUE: u32 = 8_192;
pub const DEFAULT_RECORDING_POLL_MS: u32 = 2_000;
pub const DEFAULT_RECORDING_BATCH: u32 = 4;
pub const DEFAULT_RECORDING_REGION: &str = "us-east-1";
pub const DEFAULT_RECORDING_S3_TIMEOUT_MS: u32 = 300_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub bind_addresses: Vec<String>,
    pub keys: HashMap<String, String>,
    pub log_level: String,
    pub region: String,
    pub node_id: String,
    pub room: RoomConfig,
    pub rtc: RtcConfig,
    pub video: VideoConfig,
    pub data: DataConfig,
    pub twirp: TwirpConfig,
    pub sessions: SessionConfig,
    pub recording: RecordingConfig,
    pub events: EventConfig,
    pub cluster: ClusterConfig,
}

/// Whether this node is one of several, and how the others reach it.
///
/// **One setting decides, and there is no `enabled` beside it.** A switch and
/// an address can disagree, and a deployment that set one and forgot the other
/// would get a server that quietly ran alone while its operator believed it
/// was clustered -- the same reasoning `sessions.postgres.url` is written down
/// with. An empty `advertise_url` is a node on its own, behaving exactly as
/// every node behaved before any of this existed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    /// How **another node** reaches this one's signalling port, e.g.
    /// `ws://10.251.10.7:7880`. Not what a browser is told: a browser reaches
    /// this server through whatever is in front of it, and the nodes reach
    /// each other on the cluster network.
    ///
    /// Empty means this node is alone.
    pub advertise_url: String,
    /// How long a node may go without saying it is alive before the others
    /// stop counting it. Three beats, so a missed one is not a death.
    pub lease_seconds: u32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            advertise_url: String::new(),
            lease_seconds: DEFAULT_CLUSTER_LEASE_SECONDS,
        }
    }
}

impl ClusterConfig {
    /// A cluster on a store only one process can use is a deployment that will
    /// corrupt its own records: three nodes cannot share a directory -- each
    /// holds its own index of it and they overwrite each other -- and a node
    /// that dies takes its unfinished meetings out of reach of the ones still
    /// up. Refused at startup, named, rather than discovered on the day a
    /// second replica appears.
    fn validate(&self, sessions: &SessionConfig) -> anyhow::Result<()> {
        if !self.joined() {
            return Ok(());
        }

        anyhow::ensure!(
            !sessions.postgres.url.trim().is_empty(),
            "cluster.advertise_url is set, so this node expects company, but sessions.postgres.url              is empty and a directory of files belongs to one process; name a database or leave              the cluster alone"
        );

        anyhow::ensure!(
            self.lease_seconds >= 3,
            "cluster.lease_seconds must be at least 3, or a node is read as dead between two of              its own heartbeats"
        );

        Ok(())
    }

    /// Whether this node registers at all.
    pub fn joined(&self) -> bool {
        !self.advertise_url.trim().is_empty()
    }

    pub fn lease_ms(&self) -> i64 {
        i64::from(self.lease_seconds) * 1_000
    }

    /// How often to say so. A third of the lease, so two beats can be lost
    /// without a live node being read as dead.
    pub fn beat(&self) -> std::time::Duration {
        std::time::Duration::from_millis((self.lease_ms() / 3).max(1_000) as u64)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataConfig {
    pub enabled: bool,
    pub max_packet_bytes: u32,
    pub rate: u32,
    pub burst: u32,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_packet_bytes: DEFAULT_DATA_MAX_PACKET_BYTES,
            rate: DEFAULT_DATA_RATE,
            burst: DEFAULT_DATA_BURST,
        }
    }
}

impl DataConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        anyhow::ensure!(
            self.max_packet_bytes > 0,
            "data.max_packet_bytes must not be zero; a limit of nothing refuses every packet \
             while the server still says it carries data"
        );

        anyhow::ensure!(
            self.max_packet_bytes <= MAX_DATA_PACKET_BYTES,
            "data.max_packet_bytes must not exceed {MAX_DATA_PACKET_BYTES}, which is what a \
             data channel carries in one message"
        );

        anyhow::ensure!(
            self.rate > 0 && self.burst >= self.rate,
            "data.rate must not be zero and data.burst must not be under it; a bucket that \
             refills slower than it drains stops carrying data a few seconds in"
        );

        Ok(())
    }
}

/// Who is told what happened, and how hard this server tries to tell them.
///
/// Nothing in here is on by default: with no webhook endpoint and no broker
/// there are no destinations, no outbox directory is made and no task is
/// started. The record of every meeting is kept either way, which is what
/// makes it safe for this whole block to be absent -- an event is a
/// notification that something changed and never the only copy of it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventConfig {
    /// The outbox: one JSON file per event that has been promised to a
    /// destination and not yet taken by it.
    ///
    /// Created at start, and only when there is a destination to deliver to.
    /// The server refuses to start if it cannot be created, for the same
    /// reason the session directory does: a deployment that believes it has an
    /// outbox and has not got one finds out during the outage it was for.
    ///
    /// Relative paths are relative to the working directory. In a container
    /// name a path on a volume, or a restart during an outage loses exactly
    /// the events the outbox was holding for it.
    pub directory: PathBuf,

    /// How many events one destination may fall behind before the next one is
    /// thrown away rather than queued.
    ///
    /// This is the bound on two things at once: what an unreachable endpoint
    /// costs in memory, and how large the outbox can grow, since a record
    /// exists exactly while an event is in a queue. A destination this far
    /// behind is not going to catch up, and filling a disk on its behalf would
    /// take the server down with it -- so the loss is bounded, counted on
    /// `/healthz`, and named in a log line.
    pub queue: u32,

    /// How many times one event is offered to one destination before this
    /// server stops asking. At least one; the first offer is an attempt.
    pub attempts: u32,

    /// How long a single attempt may take, whether that is an endpoint that
    /// accepts a connection and says nothing or a broker that is not there.
    pub timeout_ms: u32,

    /// The wait before the second attempt. It doubles after that, up to
    /// `max_backoff_ms` -- the cap matters more than the doubling, because
    /// without one an endpoint that came back a moment ago waits out a delay
    /// measured in minutes.
    pub backoff_ms: u32,
    pub max_backoff_ms: u32,

    pub webhooks: WebhookConfig,
    pub kafka: KafkaConfig,
}

/// Events posted to somebody else's HTTP endpoint, in LiveKit's shape and with
/// LiveKit's signing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    /// Every one of these gets every event, each with a queue and a worker of
    /// its own, so a slow endpoint delays only itself.
    pub urls: Vec<String>,

    /// Which of this server's `keys` signs the requests. The receiver reads
    /// the key name out of the token and looks up the matching secret, exactly
    /// as LiveKit's receiver does, so this has to be a key that is actually
    /// configured -- and the server refuses to start if it is not, rather than
    /// signing every webhook with nothing and having them all rejected.
    pub api_key: String,
}

/// Events produced onto a Kafka topic.
///
/// Reading this block needs the `kafka` feature, which is off by default. A
/// broker named on a build without it is refused at startup: a bus that is
/// silently not being fed is the failure this whole subsystem exists to avoid.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KafkaConfig {
    /// `host:port` pairs, comma separated. Empty means no broker, which is the
    /// default and which builds no destination at all.
    pub brokers: String,
    pub topic: String,

    /// Anything else librdkafka takes, passed through unread: `security.protocol`,
    /// `ssl.ca.location`, `sasl.*` and the rest of a real deployment's
    /// connection. Naming each of them here would mean a release of this
    /// server every time a broker wanted a setting nobody had thought of.
    pub properties: HashMap<String, String>,
}

/// Where the record of what happened in a room is kept.
///
/// **Which store is chosen, and how.** Naming a database in `postgres.url` is
/// what selects it; leaving it empty leaves the server on the directory of
/// files it has always used. One setting rather than a `store:` switch beside
/// a url, because a switch and a url can disagree -- a deployment that sets one
/// and forgets the other gets a server that quietly records into the wrong
/// place, and there is no version of that mistake worth being able to make.
///
/// A deployment that runs **more than one server process** has to name a
/// database. Three processes cannot share a directory: each holds its own index
/// of it, they overwrite each other's records, and a node that dies takes its
/// unfinished meetings out of reach of the ones still up. So does a deployment
/// that wants **recordings**, whose upload queue is a thing several processes
/// poll and which a directory therefore refuses to be.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// The directory holding one JSON file per meeting, when no database is
    /// named.
    ///
    /// Created at start if it is not there, and the server refuses to start if
    /// it cannot be created or read. That is deliberate: a server that answers
    /// `/api/rooms/<room>/sessions` with an empty list because it silently
    /// failed to open its own directory is worse than one that will not start,
    /// because nobody finds out until they are asked what happened at a
    /// meeting last week.
    ///
    /// The default is relative, so `cargo run` in a checkout writes into the
    /// checkout. A deployment names an absolute path on a volume that outlives
    /// the container; a path inside the image is a path whose contents go with
    /// the next release.
    pub directory: PathBuf,

    /// The most one meeting's record may carry of the embedding platform's own
    /// data: the room's metadata, and each participant's attributes.
    ///
    /// **Why there is a limit at all.** Both of those arrive from outside --
    /// metadata over the room service, attributes in a join token minted by
    /// somebody we do not control -- and both are written into our database and
    /// handed back on every read of that meeting. Without a number here, a
    /// caller with a bug puts a megabyte on every attendance row of every
    /// meeting it holds, and the first anyone hears of it is the disk.
    ///
    /// **What happens at the limit.** The value is dropped whole and a line in
    /// the log names the meeting. It is never truncated: half a JSON document
    /// is worse than none, because nothing downstream can tell the half from
    /// the whole. Sixteen kilobytes by default, which is far more than an
    /// identifier or two and far less than anything that would be a problem.
    pub max_opaque_bytes: u32,

    pub postgres: PostgresConfig,
}

/// The database the meeting records and the recording queue live in.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostgresConfig {
    /// A libpq connection string or URI, or empty for no database at all.
    ///
    /// It carries a password in every deployment that has one, so it is never
    /// logged: what the server prints is the host, port and database name it
    /// parsed out of it.
    ///
    /// **The database being down does not stop this server starting**, and does
    /// not stop it carrying calls. Nothing on the media path goes near it. What
    /// an outage costs is the records held during it, which are kept in memory
    /// and written when it comes back -- see `unwritten_sessions=` on
    /// `/healthz`.
    ///
    /// **The schema is this server's to create.** It applies whatever
    /// migrations the database has not got on its first connection, under a
    /// lock the cluster shares, so three nodes starting at once is safe and an
    /// upgrade is "start the new binary".
    pub url: String,

    /// How many connections this node opens, which is also how many statements
    /// it can have in flight. One thread each.
    ///
    /// Small on purpose. Every one of them is a backend process on the database
    /// and this server asks very little of it: a few statements per meeting, and
    /// a page of history whenever somebody opens a backoffice.
    pub connections: u32,

    /// The cap on one connection attempt and on one statement. A query that has
    /// wedged holds one of the connections above this long and no longer.
    pub timeout_ms: u32,

    /// How long a meeting or a claim is believed without being renewed.
    ///
    /// The one number that decides how quickly a dead node's work comes back to
    /// the other two, and it is a trade in both directions: too long and a
    /// meeting that ended when a pod was killed sits open until this elapses;
    /// too short and a node that is merely busy has its meetings closed under
    /// it and its uploads taken away mid-flight. It is renewed three times per
    /// lease, so it has to be comfortably more than a statement timeout.
    pub lease_ms: u32,

    /// How many times one recording's upload or announcement is handed out
    /// before it is left alone.
    ///
    /// Spent on the claim rather than on the result, so a file whose upload
    /// kills whatever tries it is dropped after this many nodes have died on it
    /// rather than going round the cluster for ever.
    pub attempts: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("sessions"),
            max_opaque_bytes: DEFAULT_SESSION_MAX_OPAQUE_BYTES,
            postgres: PostgresConfig::default(),
        }
    }
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            connections: DEFAULT_SESSION_CONNECTIONS,
            timeout_ms: DEFAULT_SESSION_TIMEOUT_MS,
            lease_ms: DEFAULT_SESSION_LEASE_MS,
            attempts: DEFAULT_SESSION_ATTEMPTS,
        }
    }
}

impl SessionConfig {
    /// Whether a database was named, which is what decides which store is
    /// built.
    pub fn in_postgres(&self) -> bool {
        !self.postgres.url.trim().is_empty()
    }

    /// The limit as the store wants it.
    pub fn max_opaque_bytes(&self) -> usize {
        self.max_opaque_bytes as usize
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_opaque_bytes > 0,
            "sessions.max_opaque_bytes must not be zero; a limit of nothing is a limit that \
             throws away every room's metadata and says so once a meeting"
        );

        if !self.in_postgres() {
            anyhow::ensure!(
                !self.directory.as_os_str().is_empty(),
                "sessions.directory must name a directory when sessions.postgres.url is empty; \
                 there is nowhere else for a meeting record to go"
            );
            return Ok(());
        }

        anyhow::ensure!(
            self.postgres.connections > 0,
            "sessions.postgres.connections must be at least 1; a database with no connection to \
             it is a database that is never written to"
        );
        anyhow::ensure!(
            self.postgres.timeout_ms > 0,
            "sessions.postgres.timeout_ms must not be zero; a statement with no time to complete \
             in never completes"
        );
        anyhow::ensure!(
            self.postgres.attempts > 0,
            "sessions.postgres.attempts must be at least 1; the first offer is an attempt, and \
             zero of them is a recording that is never uploaded"
        );
        // The lease is renewed three times over its length, so a lease shorter
        // than three statement timeouts can expire while the node holding it is
        // waiting on the very statement that would have renewed it -- and its
        // meetings would then be closed under people who are still talking.
        anyhow::ensure!(
            self.postgres.lease_ms >= self.postgres.timeout_ms.saturating_mul(3),
            "sessions.postgres.lease_ms ({}) must be at least three times \
             sessions.postgres.timeout_ms ({}), or a node that is merely slow will have its \
             meetings closed under it by one that is not",
            self.postgres.lease_ms,
            self.postgres.timeout_ms
        );

        Ok(())
    }
}

/// Whether meetings are recorded, where the audio goes, and where it goes
/// afterwards.
///
/// `enabled` is the **default for rooms, not a master switch**. A room says
/// otherwise in its metadata -- `{"sessionPolicy": {"record": true}}` -- and
/// what it says wins, so a deployment can record nothing by default and one
/// room by name, or the other way round.
///
/// There is deliberately **no minimum-participants setting here**. Whether a
/// meeting is happening is already decided once, by `room.min_humans`, which
/// goes down to one; recording follows the meeting and has no second threshold
/// of its own. That is not a simplification, it is the bug next door: a
/// conference service whose recorder insisted on two people never recorded the
/// meeting room where everybody sits together and joins from one account, and
/// those meetings were simply missing afterwards with nothing to say why.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    /// Record a room's meetings unless the room says otherwise.
    pub enabled: bool,

    /// Where the audio is written while a meeting is happening.
    ///
    /// One process per directory, exactly like `sessions.directory`: two
    /// sharing one would each finish the other's live recordings at startup. It
    /// must be a volume that outlives the container -- a path inside the image
    /// is a path whose contents go with the next release, and what would go
    /// with them is every recording not yet uploaded.
    pub directory: PathBuf,

    /// How large one file may become before the recorder starts the next
    /// segment of the same participant.
    ///
    /// A ceiling, not a target. Opus at the bitrate a browser sends is a few
    /// megabytes an hour, so an ordinary meeting never reaches this; what it
    /// bounds is the meeting nobody ever leaves and the publisher sending far
    /// more than it should. Each segment is a whole file, uploaded as one
    /// object, with its own offset into the meeting.
    pub max_segment_mb: u32,

    /// How long a packet is held so that ones behind it can catch up.
    ///
    /// Not a jitter buffer: nobody is listening to this in real time. It is
    /// long enough to undo an ordinary reordering and short enough that the
    /// file on the disk is never far behind the meeting. A packet that arrives
    /// after its place has gone past is dropped, and the hole it leaves is the
    /// right length, because the clock in the file is the sender's.
    pub reorder_hold_ms: u32,

    /// How many packets may wait between the media path and the writer thread
    /// before new ones are dropped rather than queued.
    ///
    /// The bound exists so that a disk that has stopped costs audio and not
    /// meetings: without it a wedged filesystem would grow this queue until the
    /// process was killed for memory, and a server that is killed is a server
    /// that has dropped every call it was carrying.
    pub queue: u32,

    /// How often this node asks for uploads and announcements to do.
    pub poll_interval_ms: u32,

    /// How many of them it takes at once, which is also how many uploads may be
    /// in flight.
    pub batch: u32,

    /// Keep the local file after the object store has it.
    ///
    /// Off, because a node's disk is not a second copy anybody agreed to pay
    /// for: a server that keeps every recording fills its volume and then stops
    /// recording, which is the failure that costs meetings rather than bytes.
    /// Turn it on when the volume is large and somebody wants a local copy.
    pub keep_local: bool,

    pub s3: S3Config,
}

/// The object store finished recordings are uploaded to.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S3Config {
    /// The endpoint, scheme included: `https://s3.eu-central-1.amazonaws.com`,
    /// or whatever a MinIO or Ceph deployment answers on.
    pub endpoint: String,
    /// The region the signature is scoped to. An object store that has no
    /// regions still wants one in the signature; anything consistent will do.
    pub region: String,
    pub bucket: String,
    /// The credentials, in the config file beside the api keys.
    ///
    /// `secret_access_key` is never logged, exactly as an api key and a
    /// database password are not: what a log line says about the object store
    /// is its endpoint and its bucket.
    pub access_key_id: String,
    pub secret_access_key: String,
    /// `<endpoint>/<bucket>/<key>` rather than `<bucket>.<endpoint>/<key>`.
    ///
    /// On, because the deployments this is built for run their own object store
    /// and both MinIO and Ceph want path style. Amazon's own S3 wants the other
    /// one for buckets made since 2020: set this to false there.
    pub path_style: bool,
    /// Put in front of every key, so one bucket can hold this server's
    /// recordings beside something else's.
    pub prefix: String,

    /// How long one `PUT` of a whole recording may take, from the first
    /// connection attempt to the last byte of the answer.
    ///
    /// **A total deadline and not an idle one.** A segment is at most
    /// `max_segment_mb`, and the whole of it has to go up inside this or the
    /// upload fails and is retried -- `sessions.postgres.attempts` times, after
    /// which the recording is `failed` with the audio still on the node's disk
    /// and nothing announced. So it is a number about a bucket and a link, and
    /// it used to be `sessions.postgres.timeout_ms`, which is a number about
    /// how long one SQL statement may hold a connection. Ten seconds is a
    /// perfectly good statement timeout and asks an ordinary two-hour
    /// participant's thirty megabytes to move at 24 Mbit/s.
    ///
    /// The default gives `max_segment_mb` a megabyte a second, which is the
    /// slowest link worth calling a link, and [`RecordingConfig::validate`]
    /// refuses a pairing that does not.
    pub timeout_ms: u32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: PathBuf::from("recordings"),
            max_segment_mb: DEFAULT_RECORDING_MAX_SEGMENT_MB,
            reorder_hold_ms: DEFAULT_RECORDING_REORDER_HOLD_MS,
            queue: DEFAULT_RECORDING_QUEUE,
            poll_interval_ms: DEFAULT_RECORDING_POLL_MS,
            batch: DEFAULT_RECORDING_BATCH,
            keep_local: false,
            s3: S3Config::default(),
        }
    }
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            region: DEFAULT_RECORDING_REGION.to_owned(),
            bucket: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            path_style: true,
            prefix: String::new(),
            timeout_ms: DEFAULT_RECORDING_S3_TIMEOUT_MS,
        }
    }
}

impl RecordingConfig {
    /// Whether this build of the configuration can record anything at all.
    ///
    /// An object store is what makes the difference, not `enabled`: a room
    /// whose metadata asks to be recorded on a server with no bucket has to be
    /// refused loudly at startup rather than silently not recorded, which is
    /// why this and [`RecordingConfig::validate`] are two different questions.
    pub fn possible(&self) -> bool {
        !self.s3.bucket.trim().is_empty()
    }

    pub fn max_segment_bytes(&self) -> u64 {
        u64::from(self.max_segment_mb) * 1024 * 1024
    }

    pub fn reorder_hold(&self) -> Duration {
        Duration::from_millis(self.reorder_hold_ms.into())
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.into())
    }

    /// How long one upload may take, whole.
    pub fn s3_timeout(&self) -> Duration {
        Duration::from_millis(self.s3.timeout_ms.into())
    }

    fn validate(&self, sessions: &SessionConfig) -> anyhow::Result<()> {
        if !self.enabled && !self.possible() {
            return Ok(());
        }

        anyhow::ensure!(
            !self.s3.endpoint.trim().is_empty(),
            "recording.s3.endpoint must name an object store; there is nowhere else for a \
             finished recording to go, and a server that recorded for a day and could not upload \
             any of it would only fill its disk"
        );
        anyhow::ensure!(
            !self.s3.bucket.trim().is_empty(),
            "recording.s3.bucket must be set"
        );
        anyhow::ensure!(
            !self.s3.access_key_id.trim().is_empty()
                && !self.s3.secret_access_key.trim().is_empty(),
            "recording.s3 needs an access_key_id and a secret_access_key; every request to an \
             object store is signed, and an unsigned one is refused"
        );
        anyhow::ensure!(
            !self.directory.as_os_str().is_empty(),
            "recording.directory must name a directory; audio is written there while a meeting is \
             happening and uploaded when it ends"
        );
        anyhow::ensure!(
            self.queue > 0,
            "recording.queue must be at least 1; a queue with no room in it is a recorder that \
             drops every packet"
        );
        anyhow::ensure!(
            self.batch > 0,
            "recording.batch must be at least 1; taking no work is never uploading anything"
        );
        anyhow::ensure!(
            self.max_segment_mb > 0,
            "recording.max_segment_mb must be at least 1"
        );
        // The two numbers are one decision and were never related to each
        // other before: a ceiling on how large one object may be, and a total
        // deadline for putting one object into a bucket. A megabyte a second is
        // the slowest link worth calling a link, and a pairing that does not
        // clear even that is a deployment whose largest recordings will fail
        // every attempt and be given up on with the audio still on the disk --
        // which is discovered weeks later, by somebody asking for a recording.
        anyhow::ensure!(
            u64::from(self.s3.timeout_ms) >= u64::from(self.max_segment_mb) * 1_000,
            "recording.s3.timeout_ms ({}) is not enough time to put a recording.max_segment_mb \
             ({} MB) object into the bucket at even a megabyte a second. It is a total deadline \
             for the whole PUT, so a segment that cannot make it fails every attempt and is given \
             up on with the audio still on this node's disk: raise the timeout or lower the \
             segment ceiling",
            self.s3.timeout_ms,
            self.max_segment_mb
        );
        // The upload queue is a table several processes poll, and a directory
        // of files cannot be one -- see `sessions.postgres.url`. Refused here
        // rather than at the first recording, because "we have been meeting for
        // a week and there are no recordings" is a discovery nobody should make
        // from a log line at boot.
        anyhow::ensure!(
            sessions.in_postgres(),
            "recording needs sessions.postgres.url: a recording's upload is a row several \
             processes claim, and a directory of one JSON file per meeting cannot be a queue"
        );

        Ok(())
    }
}

/// How the room API answers, as distinct from what it does.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TwirpConfig {
    /// Also answer the room API at `/twirp/livekit.RoomService/<Method>`.
    ///
    /// The protobuf package is `mediacore`, so the service's own path is
    /// `/twirp/mediacore.RoomService/<Method>`. LiveKit's server SDKs do not
    /// take a path: they build it in generated code out of the package their
    /// own `.proto` declares, and so does `lk`. Turning this off makes every
    /// one of them answer `bad_route` against a server that is working
    /// perfectly, and the caller has no setting to change -- they would have
    /// to regenerate their client from our `.proto` or move to raw HTTP.
    ///
    /// So it is on, and it costs nothing to leave on: the alias is the same
    /// handler, the same grant checks and the same error bodies, with the
    /// service name in a `bad_route` message being the only thing that differs.
    /// Turn it off when nothing built against LiveKit calls this server any
    /// more and you would rather the old name stop working loudly than linger.
    pub livekit_alias: bool,
}

impl Default for TwirpConfig {
    fn default() -> Self {
        Self {
            livekit_alias: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomConfig {
    pub auto_create: bool,
    pub empty_timeout: u32,
    pub departure_timeout: u32,
    pub max_participants: u32,
    pub min_humans: u32,
    pub end_grace: u32,
    pub duplicate_identity: DuplicateIdentity,
    pub sweep_interval_ms: u32,
}

impl RoomConfig {
    /// How often the server looks at rooms nobody is doing anything to.
    ///
    /// Everything else about a room happens because somebody joined, left or
    /// called the API. The two things that have to happen when nobody does
    /// anything -- a session policy running out, an empty room being collected
    /// -- need a clock of their own, and this is its tick. Both are counted in
    /// whole seconds, so a second is close enough; it is a setting so that a
    /// test can watch the sweep happen without waiting for one.
    pub fn sweep_interval(&self) -> Duration {
        Duration::from_millis(self.sweep_interval_ms.into())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateIdentity {
    #[default]
    Suffix,
    Displace,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RtcConfig {
    pub udp_port: u16,
    pub port_range_start: u16,
    pub port_range_end: u16,
    pub bind_address: String,
    pub advertise_addresses: Vec<String>,
    pub use_socket_address: bool,
    pub use_external_ip: bool,
    pub node_ip: String,
    pub ice_disconnected_timeout: u32,
    pub negotiation_timeout: u32,

    /// The STUN and TURN servers a client is told to try, in the order they
    /// are written here.
    ///
    /// **This server runs none of them.** It is ice-lite: it answers whoever
    /// asks and never gathers a candidate of its own, so a relay is nothing
    /// more than another address a client's packets arrive from. What lives
    /// here is where somebody else's coturn is and what to say to it, and an
    /// empty list -- the default, and what every deployment had before this
    /// existed -- means a client gathers host candidates only and reaches this
    /// server directly or not at all.
    pub ice_servers: Vec<IceServerConfig>,
}

/// One entry in `rtc.ice_servers`: an address, and the credentials for it.
///
/// **Two ways to hold a credential, and never both at once.** `username` and
/// `credential` are a fixed pair, the same for every client, matching a coturn
/// with a static user in its database. `secret` is the other mode -- coturn's
/// `use-auth-secret` -- where this server mints a fresh, expiring credential
/// for each person as they join, out of a secret it shares with the relay and
/// hands to nobody. An entry that carries both is a configuration whose author
/// did not know which one applies, so it is refused rather than resolved by a
/// precedence rule nobody would remember.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IceServerConfig {
    /// `turn:turn.example.com:3478?transport=udp`,
    /// `turns:turn.example.com:5349?transport=tcp`,
    /// `stun:stun.example.com:3478`. Several of them are one server reachable
    /// several ways, and a client tries them in order.
    pub urls: Vec<String>,

    /// The fixed pair, for a relay with a user in its database.
    pub username: String,
    pub credential: String,

    /// The secret this deployment's coturn was started with, for a relay in
    /// `use-auth-secret` mode. Never sent anywhere: what reaches a client is
    /// one credential derived from it, and it is not logged, exactly as an api
    /// key and a database password are not.
    pub secret: String,

    /// How long a minted credential is good for. Only read when `secret` is
    /// set, and it has to outlive the longest call this deployment expects --
    /// coturn checks the expiry when the allocation is made and again on every
    /// refresh of it.
    pub ttl_seconds: u32,
}

impl Default for IceServerConfig {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            username: String::new(),
            credential: String::new(),
            secret: String::new(),
            ttl_seconds: DEFAULT_ICE_CREDENTIAL_TTL_SECONDS,
        }
    }
}

impl IceServerConfig {
    /// Whether this entry's credential is minted per join rather than fixed.
    pub fn mints_credentials(&self) -> bool {
        !self.secret.trim().is_empty()
    }

    /// The life of a minted credential, as the minting wants it.
    pub fn credential_ttl(&self) -> u64 {
        u64::from(self.ttl_seconds)
    }

    /// Whether anything here is a relay, as opposed to a plain reflexive
    /// lookup. Credentials belong to one and not the other.
    fn relays(&self) -> bool {
        self.urls
            .iter()
            .any(|url| starts_with_scheme(url, "turn:") || starts_with_scheme(url, "turns:"))
    }

    fn names_credentials(&self) -> bool {
        !self.username.is_empty() || !self.credential.is_empty() || self.mints_credentials()
    }

    /// What the refusals name the entry by, since a list has no other handle
    /// on one.
    fn label(&self) -> &str {
        self.urls.first().map(String::as_str).unwrap_or("")
    }

    /// Every shape refused here is a deployment that starts, hands its clients
    /// an ice server, and relays nothing -- which looks from the room exactly
    /// like ordinary silence and is found out by nobody. A url no browser will
    /// parse, refused by [`check_ice_url`], is worse than that again: it takes
    /// every other ice server in the list down with it.
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.urls.is_empty(),
            "an rtc.ice_servers entry has no urls; an ice server is its address, so give the \
             entry a stun:, stuns:, turn: or turns: url or take it out of the list"
        );

        for url in &self.urls {
            check_ice_url(url)?;
        }

        anyhow::ensure!(
            self.relays() || !self.names_credentials(),
            "rtc.ice_servers entry `{}` is a stun server carrying credentials, and stun has \
             nothing to authenticate: drop the username, credential and secret, or give the \
             entry a turn: url if a relay was what was meant",
            self.label()
        );

        if self.relays() {
            anyhow::ensure!(
                !(self.mints_credentials()
                    && (!self.username.is_empty() || !self.credential.is_empty())),
                "rtc.ice_servers entry `{}` carries both a fixed username/credential and a \
                 secret, and only one of them can apply: keep the pair for a coturn with a user \
                 in its database, keep the secret for one started with use-auth-secret",
                self.label()
            );

            anyhow::ensure!(
                self.username.is_empty() == self.credential.is_empty(),
                "rtc.ice_servers entry `{}` names half of a username/credential pair; a relay \
                 checks both, so set the other one or use secret instead",
                self.label()
            );

            anyhow::ensure!(
                self.mints_credentials() || !self.username.is_empty(),
                "rtc.ice_servers entry `{}` is a turn server with no credentials, so every \
                 allocation asked of it would be refused: give it a username and credential \
                 pair, or the secret its coturn was started with",
                self.label()
            );
        }

        anyhow::ensure!(
            !self.mints_credentials() || self.ttl_seconds > 0,
            "rtc.ice_servers entry `{}` mints credentials with ttl_seconds of 0, so every one \
             of them has expired before the client it was minted for receives it; give it the \
             length of the longest call this deployment expects",
            self.label()
        );

        Ok(())
    }
}

/// Case-insensitively, because a url scheme is, and `STUN:` in a config file is
/// a typo nobody should have to see a silent relay over. `get` rather than a
/// slice, because a url whose first bytes are not ascii is exactly the kind of
/// thing this refuses and must not panic on the way to saying so.
fn starts_with_scheme(url: &str, scheme: &str) -> bool {
    url.get(..scheme.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
}

/// The url shapes a browser will take, which is a far smaller set than "begins
/// with a scheme this server knows".
///
/// **A url a client cannot parse costs it the whole list, not one entry.**
/// `new RTCPeerConnection` parses every url of every ice server it was handed
/// before it does anything else, and one it cannot parse throws a `SyntaxError`
/// out of the constructor: no peer connection is built at all, so the perfectly
/// good stun server written above the typo does not save the call, and every
/// client of the deployment fails to join rather than one of them gathering
/// badly. Checked in Chrome, which throws on a list holding
/// `stun:good.example:3478` beside `stun:bad.example:3478?transport=udp` and
/// gathers nothing.
///
/// So the grammar of RFC 7064 (`stun:`, `stuns:`) and RFC 7065 (`turn:`,
/// `turns:`) is checked here, at startup, where the answer is a message naming
/// the line to fix. The shape worth all of this code is
/// `stun:stun.example.com:3478?transport=udp`: the `?transport=udp` belongs to
/// the `turn:` line it was copied from, a stun url takes no parameters at all,
/// and a browser refuses the pair of them rather than ignoring the one it has
/// no use for.
///
/// What is deliberately *not* refused is anything a browser accepts. An
/// uppercase scheme, a host with no port, a trailing dot, an underscore, a
/// non-ascii hostname and a bracketed ipv6 literal are all urls Chrome parses,
/// so a deployment that writes one keeps its relay.
fn check_ice_url(url: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !url.contains(char::is_whitespace),
        "rtc.ice_servers url `{url}` has whitespace in it; a url is one word, and a client that \
         reads a space as part of a hostname looks up a name that does not exist"
    );

    let Some(scheme) = ["stun:", "stuns:", "turn:", "turns:"]
        .into_iter()
        .find(|scheme| starts_with_scheme(url, scheme))
    else {
        anyhow::bail!(
            "rtc.ice_servers url `{url}` is not a stun:, stuns:, turn: or turns: url; a browser \
             refuses the whole ice server list over one url it cannot parse, so this costs the \
             deployment every relay in it and not just this line"
        );
    };

    let relays = matches!(scheme, "turn:" | "turns:");
    let rest = &url[scheme.len()..];
    let address = match rest.split_once('?') {
        None => rest,
        Some((address, query)) => {
            anyhow::ensure!(
                relays,
                "rtc.ice_servers url `{url}` is a stun url carrying `?{query}`; stun is a host \
                 and a port and nothing else -- the transport parameter belongs to the turn: \
                 line it was copied from, and a browser refuses this url and every ice server \
                 beside it"
            );

            let Some(transport) = query.strip_prefix("transport=") else {
                anyhow::bail!(
                    "rtc.ice_servers url `{url}` carries `?{query}`; `transport`, spelled in \
                     lower case, is the only parameter a turn url takes, and a browser refuses a \
                     url with anything else after the `?`"
                );
            };

            anyhow::ensure!(
                transport.eq_ignore_ascii_case("udp") || transport.eq_ignore_ascii_case("tcp"),
                "rtc.ice_servers url `{url}` asks for transport `{transport}`; udp and tcp are \
                 the only two a client can ask a relay for. `tls` is the common way to write this \
                 one wrong: a turns: url is how tls is asked for, and it carries \
                 `?transport=tcp`"
            );

            address
        }
    };

    // Bracketed because the colons of an ipv6 literal are otherwise the colon
    // before the port, which is how `turn:2001:db8::1` becomes a url with a
    // port of `db8::1` and no browser will have it.
    let (host, port) = if let Some(literal) = address.strip_prefix('[') {
        let Some((host, tail)) = literal.split_once(']') else {
            anyhow::bail!(
                "rtc.ice_servers url `{url}` opens a `[` for an ipv6 address and never closes it"
            );
        };

        anyhow::ensure!(
            host.bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':' || byte == b'.'),
            "rtc.ice_servers url `{url}` puts `{host}` in brackets; brackets are how an ipv6 \
             address is written, and a hostname goes without them"
        );

        match tail {
            "" => (host, None),
            tail => {
                let Some(port) = tail.strip_prefix(':') else {
                    anyhow::bail!(
                        "rtc.ice_servers url `{url}` has `{tail}` after the closing bracket; an \
                         ipv6 address is followed by its port or by nothing at all"
                    );
                };

                (host, Some(port))
            }
        }
    } else {
        match address.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        }
    };

    anyhow::ensure!(
        !host.is_empty(),
        "rtc.ice_servers url `{url}` names no host; an ice server is an address, and a scheme on \
         its own resolves to nothing"
    );

    anyhow::ensure!(
        !host.contains('@'),
        "rtc.ice_servers url `{url}` puts a name in front of its host; browsers refused the \
         `user@host` form years ago, and the username and credential of a relay are fields of \
         this entry rather than part of its address"
    );

    anyhow::ensure!(
        !host.contains(['/', '?', '#', '[', ']']),
        "rtc.ice_servers url `{url}` has a path, a query or a stray bracket on it; a stun or turn \
         url is a host and a port, and it is not an http url -- a trailing `/` alone is enough to \
         have it refused"
    );

    if let Some(port) = port {
        anyhow::ensure!(
            port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port > 0),
            "rtc.ice_servers url `{url}` has `{port}` where a port belongs; a port is a number \
             from 1 to 65535, and an ipv6 address needs brackets around it \
             (`turn:[2001:db8::1]:3478`) so that its own colons are not read as the one before \
             the port"
        );
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    pub dynacast_pause_delay_ms: u32,
    pub layer_upgrade_hold_ms: u32,
    pub bwe: BweConfig,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            dynacast_pause_delay_ms: DEFAULT_DYNACAST_PAUSE_DELAY_MS,
            layer_upgrade_hold_ms: DEFAULT_LAYER_UPGRADE_HOLD_MS,
            bwe: BweConfig::default(),
        }
    }
}

impl VideoConfig {
    pub fn dynacast_pause_delay(&self) -> Duration {
        Duration::from_millis(self.dynacast_pause_delay_ms.into())
    }

    pub fn layer_upgrade_hold(&self) -> Duration {
        Duration::from_millis(self.layer_upgrade_hold_ms.into())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BweConfig {
    pub enabled: bool,
    pub initial_kbps: u32,
    pub min_budget_kbps: u32,
    pub max_desired_kbps: u32,
    pub audio_reserve_kbps: u32,
    pub allow_pause: bool,
    pub overhead_bytes: u32,
    pub allocation_interval_ms: u32,
    pub recovery_hold_ms: u32,
    pub recovery_step_ms: u32,
    pub queue_pressure_ms: u32,
    pub max_overrun_percent: u32,
}

impl Default for BweConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            initial_kbps: DEFAULT_BWE_INITIAL_KBPS,
            min_budget_kbps: DEFAULT_BWE_MIN_BUDGET_KBPS,
            max_desired_kbps: DEFAULT_BWE_MAX_DESIRED_KBPS,
            audio_reserve_kbps: DEFAULT_BWE_AUDIO_RESERVE_KBPS,
            allow_pause: false,
            overhead_bytes: DEFAULT_BWE_OVERHEAD_BYTES,
            allocation_interval_ms: DEFAULT_BWE_ALLOCATION_INTERVAL_MS,
            recovery_hold_ms: DEFAULT_BWE_RECOVERY_HOLD_MS,
            recovery_step_ms: DEFAULT_BWE_RECOVERY_STEP_MS,
            queue_pressure_ms: DEFAULT_BWE_QUEUE_PRESSURE_MS,
            max_overrun_percent: DEFAULT_BWE_MAX_OVERRUN_PERCENT,
        }
    }
}

impl BweConfig {
    pub fn allocation_interval(&self) -> Duration {
        Duration::from_millis(self.allocation_interval_ms.into())
    }

    pub fn recovery_hold(&self) -> Duration {
        Duration::from_millis(self.recovery_hold_ms.into())
    }

    pub fn recovery_step(&self) -> Duration {
        Duration::from_millis(self.recovery_step_ms.into())
    }

    pub fn queue_pressure(&self) -> Duration {
        Duration::from_millis(self.queue_pressure_ms.into())
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allocation_interval_ms > 0,
            "video.bwe.allocation_interval_ms must not be zero"
        );
        anyhow::ensure!(
            self.initial_kbps > 0,
            "video.bwe.initial_kbps must not be zero"
        );
        anyhow::ensure!(
            self.max_desired_kbps >= self.initial_kbps,
            "video.bwe.max_desired_kbps must not be below video.bwe.initial_kbps"
        );
        anyhow::ensure!(
            self.overhead_bytes > 0,
            "video.bwe.overhead_bytes must not be zero"
        );
        anyhow::ensure!(
            self.max_overrun_percent >= 100,
            "video.bwe.max_overrun_percent must not be below 100"
        );

        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7880,
            bind_addresses: vec!["0.0.0.0".to_owned()],
            keys: HashMap::new(),
            log_level: "info".to_owned(),
            region: String::new(),
            node_id: format!("ND_{}", short_id()),
            room: RoomConfig::default(),
            rtc: RtcConfig::default(),
            video: VideoConfig::default(),
            data: DataConfig::default(),
            twirp: TwirpConfig::default(),
            sessions: SessionConfig::default(),
            recording: RecordingConfig::default(),
            events: EventConfig::default(),
            cluster: ClusterConfig::default(),
        }
    }
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("events"),
            queue: DEFAULT_EVENT_QUEUE,
            attempts: DEFAULT_EVENT_ATTEMPTS,
            timeout_ms: DEFAULT_EVENT_TIMEOUT_MS,
            backoff_ms: DEFAULT_EVENT_BACKOFF_MS,
            max_backoff_ms: DEFAULT_EVENT_MAX_BACKOFF_MS,
            webhooks: WebhookConfig::default(),
            kafka: KafkaConfig::default(),
        }
    }
}

impl Default for KafkaConfig {
    fn default() -> Self {
        Self {
            brokers: String::new(),
            topic: DEFAULT_KAFKA_TOPIC.to_owned(),
            properties: HashMap::new(),
        }
    }
}

impl EventConfig {
    /// Whether anything outside this process is waiting to be told.
    ///
    /// Nothing is started and no directory is made when this is false, so a
    /// deployment that has not asked for events does not grow one.
    pub fn has_destinations(&self) -> bool {
        !self.webhooks.urls.is_empty() || !self.kafka.brokers.trim().is_empty()
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.into())
    }

    pub fn backoff(&self) -> Duration {
        Duration::from_millis(self.backoff_ms.into())
    }

    pub fn max_backoff(&self) -> Duration {
        Duration::from_millis(self.max_backoff_ms.into())
    }

    fn validate(&self, keys: &HashMap<String, String>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.attempts > 0,
            "events.attempts must be at least 1; the first offer is an attempt, and zero of them \
             is a destination that is configured and never written to"
        );
        anyhow::ensure!(self.queue > 0, "events.queue must be at least 1");
        anyhow::ensure!(
            self.timeout_ms > 0,
            "events.timeout_ms must not be zero; an attempt with no time to complete in never \
             completes"
        );
        anyhow::ensure!(
            self.max_backoff_ms >= self.backoff_ms,
            "events.max_backoff_ms must not be below events.backoff_ms"
        );

        if self.has_destinations() {
            anyhow::ensure!(
                !self.directory.as_os_str().is_empty(),
                "events.directory must name a directory; without an outbox an endpoint that is \
                 away for two minutes costs two minutes of events and nothing says so"
            );
        }

        let mut seen: HashSet<&str> = HashSet::new();
        for url in &self.webhooks.urls {
            anyhow::ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "events.webhooks.urls entry `{url}` is not an http or https url"
            );
            // A destination is named by its url, and the outbox strikes a
            // record off when every name that owed it has settled. Two
            // entries with one name are one name: the first worker to finish
            // deletes the record out from under the second, so a crash after
            // that loses the event for an endpoint that never saw it, with no
            // counter and no line. Refused here because there is no shape of
            // this that a deployment wanted -- it is a copied line in a config
            // file, and the second copy delivers nothing the first does not.
            anyhow::ensure!(
                seen.insert(url.as_str()),
                "events.webhooks.urls names `{url}` twice; one url is one destination, so the \
                 second copy would deliver nothing and would cost the first its outbox record"
            );
        }

        if !self.webhooks.urls.is_empty() {
            anyhow::ensure!(
                !self.webhooks.api_key.is_empty(),
                "events.webhooks.api_key must name one of the configured keys; a webhook signed \
                 with nothing is rejected by every receiver"
            );
            anyhow::ensure!(
                keys.contains_key(&self.webhooks.api_key),
                "events.webhooks.api_key `{}` is not one of the keys this server has, so every \
                 webhook would be signed with a secret nobody can check",
                self.webhooks.api_key
            );
        }

        if !self.kafka.brokers.trim().is_empty() {
            anyhow::ensure!(
                !self.kafka.topic.trim().is_empty(),
                "events.kafka.topic must be set when events.kafka.brokers is"
            );
        }

        Ok(())
    }
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            auto_create: true,
            empty_timeout: 300,
            departure_timeout: 20,
            max_participants: 0,
            min_humans: 0,
            end_grace: DEFAULT_END_GRACE_SECONDS,
            duplicate_identity: DuplicateIdentity::default(),
            sweep_interval_ms: DEFAULT_SWEEP_INTERVAL_MS,
        }
    }
}

impl Default for RtcConfig {
    fn default() -> Self {
        Self {
            udp_port: 7882,
            port_range_start: 50000,
            port_range_end: 60000,
            bind_address: "0.0.0.0".to_owned(),
            advertise_addresses: Vec::new(),
            use_socket_address: true,
            use_external_ip: false,
            node_ip: String::new(),
            ice_disconnected_timeout: DEFAULT_ICE_DISCONNECTED_TIMEOUT_SECONDS,
            negotiation_timeout: DEFAULT_NEGOTIATION_TIMEOUT_SECONDS,
            ice_servers: Vec::new(),
        }
    }
}

impl Config {
    /// Read the config, fill in the variables it asks the environment for, and
    /// check the result.
    ///
    /// **The variables are filled in here rather than by something in front of
    /// this process.** `envsubst` used to do it -- in `deploy/start-sfu.sh`, and
    /// in a chart's init container writing into an `emptyDir` -- which meant a
    /// rendered copy of this file, api secret and database password and bucket
    /// credentials in cleartext, existed on a filesystem for the life of the
    /// container. What the server reads now is the file the deployment wrote,
    /// `${MEDIACORE_API_SECRET}` still in it, and the secret stays an
    /// environment variable of one process. It also means an unset name is a
    /// refusal that names it rather than an empty string nobody notices until
    /// every token is rejected: [`variables`] has the whole of the reasoning.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let body = variables::expand(&body)
            .with_context(|| format!("rendering config {}", path.display()))?;
        Self::parse(&body).with_context(|| format!("parsing config {}", path.display()))
    }

    /// The text exactly as it stands, with nothing filled in.
    ///
    /// [`Config::load`] is the one that renders `${MEDIACORE_...}` first, and
    /// the two are separate so that each can be tested for what it does: this
    /// one is a function of its argument alone, and does not read the
    /// environment.
    pub fn parse(body: &str) -> anyhow::Result<Self> {
        let config: Self = serde_yaml_ng::from_str(body)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.keys.is_empty(), "no api keys configured");
        // A non-empty map was the whole of this check, and an entry in it can be
        // as good as no key at all. HS256 signs and verifies with an empty key
        // without complaining, so a deployment whose secret went missing --
        // a Secret whose api-secret key exists and holds nothing, a Helm value
        // left as `""` -- starts, answers `/healthz`, and accepts a token minted
        // by anybody who knows the key's *name*. The name is not a secret:
        // `mediacore` is what the shipped template calls it. That makes this the
        // one config mistake that hands a stranger a join token for every room,
        // and it has to be refused here rather than in `deploy/start-sfu.sh`,
        // which only the compose stack runs and which the published image, with
        // no shell in it, cannot run at all.
        for (name, secret) in &self.keys {
            anyhow::ensure!(
                !name.trim().is_empty(),
                "an entry in `keys` has no name. The name is the `iss` of every token and what the \
                 secret to check it with is looked up by, so a nameless key is one whose issuer is \
                 the empty string -- and a token claiming that issuer verifies. It is what this \
                 config renders when MEDIACORE_API_KEY is set to an empty value"
            );
            anyhow::ensure!(
                !secret.is_empty(),
                "the api key `{name}` has no secret, and a token signed with nothing verifies \
                 against nothing: this server would start and then accept a token minted by \
                 anybody who knows that the key is called `{name}`. Set it where this server reads \
                 its config -- MEDIACORE_API_SECRET if the config asks the environment for it -- \
                 and generate it with `openssl rand -hex 32`. An empty value is honoured elsewhere \
                 in this file, and this is not one of the places where nothing is an answer"
            );
            anyhow::ensure!(
                secret.len() >= MIN_SECRET_BYTES,
                "the api key `{name}` has a secret of {} bytes, and every token this server issues \
                 or is shown is signed with it, so it has to be at least {MIN_SECRET_BYTES}: that \
                 is what RFC 7518 section 3.2 requires of an HS256 key, and the length the front \
                 end already refuses to start below on the same secret. `openssl rand -hex 32`",
                secret.len()
            );
        }
        anyhow::ensure!(self.port != 0, "port must be set");
        anyhow::ensure!(
            !self.bind_addresses.is_empty(),
            "at least one bind address is required"
        );
        self.sessions.validate()?;
        self.recording.validate(&self.sessions)?;
        self.rtc.validate()?;
        self.video.bwe.validate()?;
        self.data.validate()?;
        self.events.validate(&self.keys)?;
        self.cluster.validate(&self.sessions)?;
        Ok(())
    }
}

impl RtcConfig {
    fn validate(&self) -> anyhow::Result<()> {
        self.bind_address.parse::<IpAddr>().with_context(|| {
            format!(
                "rtc.bind_address `{}` is not an ip address",
                self.bind_address
            )
        })?;

        if !self.node_ip.is_empty() {
            self.node_ip
                .parse::<IpAddr>()
                .with_context(|| format!("rtc.node_ip `{}` is not an ip address", self.node_ip))?;
        }

        for address in &self.advertise_addresses {
            anyhow::ensure!(
                parse_advertise(address).is_some(),
                "rtc.advertise_addresses entry `{address}` is neither an ip nor an ip:port"
            );
        }

        if self.udp_port == 0 && self.port_range_start != 0 {
            anyhow::ensure!(
                self.port_range_start <= self.port_range_end,
                "rtc.port_range_start must not be above rtc.port_range_end"
            );
        }

        for server in &self.ice_servers {
            server.validate()?;
        }

        Ok(())
    }

    pub fn candidate_ports(&self) -> Vec<u16> {
        if self.udp_port != 0 {
            return vec![self.udp_port];
        }
        if self.port_range_start == 0 {
            return vec![0];
        }
        (self.port_range_start..=self.port_range_end).collect()
    }

    pub fn ice_disconnected_grace(&self) -> Duration {
        Duration::from_secs(self.ice_disconnected_timeout.into())
    }

    pub fn negotiation_grace(&self) -> Duration {
        Duration::from_secs(self.negotiation_timeout.into())
    }

    pub fn advertise(&self, port: u16) -> Vec<SocketAddr> {
        self.advertise_addresses
            .iter()
            .filter_map(|address| parse_advertise(address))
            .map(|(ip, explicit)| SocketAddr::new(ip, explicit.unwrap_or(port)))
            .collect()
    }
}

fn parse_advertise(address: &str) -> Option<(IpAddr, Option<u16>)> {
    if let Ok(addr) = address.parse::<SocketAddr>() {
        return Some((addr.ip(), Some(addr.port())));
    }
    address.parse::<IpAddr>().ok().map(|ip| (ip, None))
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
