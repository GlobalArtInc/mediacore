use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

pub const DEFAULT_END_GRACE_SECONDS: u32 = 90;
pub const DEFAULT_ICE_DISCONNECTED_TIMEOUT_SECONDS: u32 = 20;
pub const DEFAULT_NEGOTIATION_TIMEOUT_SECONDS: u32 = 15;
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
    pub twirp: TwirpConfig,
    pub sessions: SessionConfig,
    pub recording: RecordingConfig,
    pub events: EventConfig,
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
            twirp: TwirpConfig::default(),
            sessions: SessionConfig::default(),
            recording: RecordingConfig::default(),
            events: EventConfig::default(),
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
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&body).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn parse(body: &str) -> anyhow::Result<Self> {
        let config: Self = serde_yaml_ng::from_str(body)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.keys.is_empty(), "no api keys configured");
        anyhow::ensure!(self.port != 0, "port must be set");
        anyhow::ensure!(
            !self.bind_addresses.is_empty(),
            "at least one bind address is required"
        );
        self.sessions.validate()?;
        self.recording.validate(&self.sessions)?;
        self.rtc.validate()?;
        self.video.bwe.validate()?;
        self.events.validate(&self.keys)?;
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
mod tests {
    use super::*;

    #[test]
    fn fills_defaults_around_supplied_values() {
        let config = Config::parse("port: 8080\nkeys:\n  key1: secret1\n").unwrap();

        assert_eq!(config.port, 8080);
        assert_eq!(config.keys.get("key1").unwrap(), "secret1");
        assert_eq!(config.room.empty_timeout, 300);
        assert!(config.room.auto_create);
        assert!(config.node_id.starts_with("ND_"));
    }

    /// A store of files cannot be an upload queue, and a server that has to
    /// find that out at the first recording has already held a week of
    /// meetings.
    #[test]
    fn recording_without_a_database_is_refused_at_startup() {
        let refused = Config::parse(
            "keys:\n  k: s\nrecording:\n  enabled: true\n  s3:\n    endpoint: \
             http://minio:9000\n    bucket: meetings\n    access_key_id: k\n    \
             secret_access_key: s\n",
        )
        .expect_err("a recorder with nowhere to queue its uploads must not start");

        assert!(
            refused.to_string().contains("sessions.postgres.url"),
            "the refusal has to name the setting that would fix it: {refused}"
        );
    }

    /// The object store's deadline is its own number, and it is related to the
    /// size ceiling it has to carry.
    ///
    /// It used to be `sessions.postgres.timeout_ms` -- how long one SQL
    /// statement may hold a connection -- doing duty as a total deadline for a
    /// whole `PUT`. Ten seconds is a fine statement timeout and asks an
    /// ordinary two-hour participant's thirty megabytes to move at 24 Mbit/s;
    /// past that the upload fails every attempt and the recording is given up
    /// on with the audio still on the node's disk and nothing announced.
    #[test]
    fn an_upload_has_its_own_deadline_and_it_has_to_fit_the_segment_it_carries() {
        let config = Config::parse(
            "keys:\n  k: s\nsessions:\n  postgres:\n    url: postgres://x/y\n    timeout_ms: \
             10000\n    lease_ms: 30000\nrecording:\n  enabled: true\n  s3:\n    endpoint: \
             http://minio:9000\n    bucket: meetings\n    access_key_id: k\n    \
             secret_access_key: s\n",
        )
        .expect("a recording deployment with the defaults");

        assert_eq!(
            config.recording.s3_timeout(),
            Duration::from_millis(u64::from(DEFAULT_RECORDING_S3_TIMEOUT_MS)),
            "an upload's deadline has to come from recording.s3.timeout_ms"
        );
        assert_ne!(
            config.recording.s3_timeout(),
            Duration::from_millis(u64::from(config.sessions.postgres.timeout_ms)),
            "the upload deadline is still the database's statement timeout, which is a number \
             about how long a query may hold a connection and has nothing to say about how long a \
             quarter-gigabyte object takes to reach a bucket"
        );

        let refused = Config::parse(
            "keys:\n  k: s\nsessions:\n  postgres:\n    url: postgres://x/y\nrecording:\n  \
             enabled: true\n  max_segment_mb: 1024\n  s3:\n    endpoint: http://minio:9000\n    \
             bucket: meetings\n    access_key_id: k\n    secret_access_key: s\n    timeout_ms: \
             10000\n",
        )
        .expect_err(
            "a segment ceiling the upload deadline cannot carry at even a megabyte a second has \
             to be refused at startup, not discovered weeks later by somebody asking for a \
             recording",
        );

        assert!(
            refused.to_string().contains("recording.s3.timeout_ms"),
            "the refusal has to name the setting that would fix it: {refused}"
        );
    }

    #[test]
    fn recording_without_a_bucket_is_refused_at_startup() {
        let refused = Config::parse(
            "keys:\n  k: s\nsessions:\n  postgres:\n    url: postgres://x/y\nrecording:\n  \
             enabled: true\n",
        )
        .expect_err("a recorder with nowhere to upload to must not start");

        assert!(
            refused.to_string().contains("recording.s3.endpoint"),
            "{refused}"
        );
    }

    /// A bucket named without `enabled` is a deployment that records the rooms
    /// that ask for it, so it is checked as hard as one that records
    /// everything.
    #[test]
    fn a_bucket_named_without_the_default_switch_is_still_a_recording_deployment() {
        let refused = Config::parse(
            "keys:\n  k: s\nrecording:\n  s3:\n    endpoint: http://minio:9000\n    bucket: \
             meetings\n    access_key_id: k\n    secret_access_key: s\n",
        )
        .expect_err("a bucket with no database behind it must not start either");

        assert!(
            refused.to_string().contains("sessions.postgres.url"),
            "{refused}"
        );
    }

    #[test]
    fn a_server_that_records_nothing_needs_none_of_it() {
        let config = Config::parse("keys:\n  k: s\n").expect("a server with no recording");

        assert!(!config.recording.enabled);
        assert!(
            !config.recording.possible(),
            "no bucket is no recording, and a deployment that never asked for it must not be \
             refused for not configuring it"
        );
    }

    #[test]
    fn an_unknown_recording_setting_is_a_refusal_and_not_a_silent_default() {
        let refused = Config::parse("keys:\n  k: s\nrecording:\n  enabld: true\n")
            .expect_err("a typo in a setting name has to be a refusal");

        assert!(refused.to_string().contains("enabld"), "{refused}");
    }

    #[test]
    fn the_session_policy_is_off_until_a_room_asks_for_one() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert_eq!(
            config.room.min_humans, 0,
            "upgrading must not start ending rooms that nobody asked to be ended"
        );
        assert_eq!(config.room.end_grace, DEFAULT_END_GRACE_SECONDS);
        assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Suffix);
        assert!(
            !config.sessions.in_postgres(),
            "the sample names a database, so copying it points somebody's meetings at a server \
             they did not choose -- and the single process the sample describes does not need one"
        );
    }

    #[test]
    fn the_livekit_twirp_alias_is_on_until_a_deployment_retires_it() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert!(
            config.twirp.livekit_alias,
            "a deployment that upgrades without touching its config keeps answering \
             /twirp/livekit.RoomService/ -- every LiveKit server SDK builds that path in \
             generated code and cannot be pointed at another one"
        );

        let retired =
            Config::parse("keys:\n  key1: secret1\ntwirp:\n  livekit_alias: false\n").unwrap();
        assert!(!retired.twirp.livekit_alias);
    }

    #[test]
    fn the_sweep_has_a_tick_of_its_own() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
        assert_eq!(
            config.room.sweep_interval(),
            Duration::from_secs(1),
            "the session policy and both room timeouts are counted in whole seconds, and nothing \
             else on the server enforces them"
        );

        let quick =
            Config::parse("keys:\n  key1: secret1\nroom:\n  sweep_interval_ms: 50\n").unwrap();
        assert_eq!(quick.room.sweep_interval(), Duration::from_millis(50));
    }

    #[test]
    fn the_session_policy_and_the_duplicate_rule_are_configurable() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nroom:\n  min_humans: 2\n  end_grace: 90\n  duplicate_identity: displace\n",
        )
        .unwrap();

        assert_eq!(config.room.min_humans, 2);
        assert_eq!(config.room.end_grace, 90);
        assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Displace);
        assert!(
            config.room.auto_create,
            "the rest of the room block kept its defaults"
        );

        assert!(
            Config::parse("keys:\n  key1: secret1\nroom:\n  duplicate_identity: rename\n").is_err(),
            "a rule the server does not have must not be read as one it does"
        );
    }

    #[test]
    fn meetings_are_recorded_somewhere_whether_or_not_a_config_says_so() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert_eq!(
            config.sessions.directory,
            PathBuf::from("sessions"),
            "a deployment that upgrades without touching its config still has to keep a record of \
             its meetings; there is no configuration that turns recording off"
        );

        let chosen =
            Config::parse("keys:\n  key1: secret1\nsessions:\n  directory: /srv/meetings\n")
                .unwrap();
        assert_eq!(chosen.sessions.directory, PathBuf::from("/srv/meetings"));

        assert!(
            Config::parse("keys:\n  key1: secret1\nsessions:\n  directory: \"\"\n").is_err(),
            "an empty directory is a server that starts and quietly records nothing"
        );
    }

    #[test]
    fn nobody_is_told_anything_until_a_destination_is_configured() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert!(
            !config.events.has_destinations(),
            "an upgrade started posting webhooks nobody asked for, at urls nobody configured"
        );
        assert_eq!(config.events.queue, DEFAULT_EVENT_QUEUE);
        assert_eq!(config.events.attempts, DEFAULT_EVENT_ATTEMPTS);
        assert_eq!(config.events.directory, PathBuf::from("events"));
        assert_eq!(config.events.kafka.topic, DEFAULT_KAFKA_TOPIC);
        assert!(config.events.kafka.brokers.is_empty());
    }

    #[test]
    fn the_webhook_block_is_configurable() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nevents:\n  directory: /var/lib/mediacore/events\n  attempts: 3\n  \
             webhooks:\n    api_key: key1\n    urls:\n      - https://example.test/hook\n",
        )
        .unwrap();

        assert!(config.events.has_destinations());
        assert_eq!(
            config.events.webhooks.urls,
            vec!["https://example.test/hook"]
        );
        assert_eq!(config.events.attempts, 3);
        assert_eq!(
            config.events.directory,
            PathBuf::from("/var/lib/mediacore/events")
        );
        assert_eq!(
            config.events.timeout(),
            Duration::from_millis(DEFAULT_EVENT_TIMEOUT_MS.into()),
            "the rest of the events block kept its defaults"
        );
    }

    /// A webhook is signed with the secret belonging to the key named here,
    /// and a receiver looks that key up to know which secret to check. Naming
    /// a key this server does not have produces a token nobody can verify --
    /// on every event, silently, until somebody reads the receiver's 401s.
    #[test]
    fn a_webhook_signed_by_a_key_this_server_does_not_have_is_refused_at_startup() {
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nevents:\n  webhooks:\n    api_key: someone-elses-key\n    \
                 urls:\n      - https://example.test/hook\n"
            )
            .is_err(),
            "every webhook this server sent would be signed with an empty secret and rejected"
        );
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nevents:\n  webhooks:\n    urls:\n \
                 - https://example.test/hook\n"
            )
            .is_err(),
            "endpoints with no signing key named at all were accepted"
        );
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nevents:\n  webhooks:\n    api_key: key1\n    urls:\n      \
                 - example.test/hook\n"
            )
            .is_err(),
            "a url with no scheme is a url nothing will ever be posted to"
        );
    }

    /// Finding I. A destination is named by its url, and the outbox strikes a
    /// record off once every name that owed it has settled -- so two entries
    /// with one name are one name, the first worker to finish deletes the
    /// record while the second has not been tried, and a crash then loses that
    /// event with nothing to say so.
    #[test]
    fn the_same_endpoint_cannot_be_named_twice() {
        let duplicated = Config::parse(
            "keys:\n  key1: secret1\nevents:\n  directory: /tmp/mediacore-events\n  webhooks:\n    \
             api_key: key1\n    urls:\n      - https://example.test/hook\n      - \
             https://example.test/hook\n",
        );

        assert!(
            duplicated.is_err(),
            "one url named twice is one destination that owes twice and settles once: the first \
             worker to finish deletes the outbox record while the second endpoint has not been \
             tried, and a crash there loses the event with no counter and no line"
        );
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nevents:\n  directory: /tmp/mediacore-events\n  webhooks:\n    \
                 api_key: key1\n    urls:\n      - https://example.test/hook\n      - \
                 https://example.test/other\n"
            )
            .is_ok(),
            "two different endpoints are two destinations and always were"
        );
    }

    #[test]
    fn rejects_a_delivery_policy_that_delivers_nothing() {
        assert!(
            Config::parse("keys:\n  key1: secret1\nevents:\n  attempts: 0\n").is_err(),
            "a destination configured to be offered an event zero times is a destination that is \
             configured and never written to"
        );
        assert!(Config::parse("keys:\n  key1: secret1\nevents:\n  queue: 0\n").is_err());
        assert!(Config::parse("keys:\n  key1: secret1\nevents:\n  timeout_ms: 0\n").is_err());
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nevents:\n  backoff_ms: 5000\n  max_backoff_ms: 1000\n"
            )
            .is_err()
        );
        assert!(
            Config::parse("keys:\n  key1: secret1\nevents:\n  nope: 1\n").is_err(),
            "a delivery setting this server does not have must not be read as one it does"
        );
    }

    #[test]
    fn a_broker_can_be_named_with_settings_of_its_own() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nevents:\n  kafka:\n    brokers: kafka1:9093,kafka2:9093\n    \
             topic: mediacore.events\n    properties:\n      security.protocol: ssl\n",
        )
        .unwrap();

        assert!(config.events.has_destinations());
        assert_eq!(config.events.kafka.topic, "mediacore.events");
        assert_eq!(
            config
                .events
                .kafka
                .properties
                .get("security.protocol")
                .map(String::as_str),
            Some("ssl"),
            "a librdkafka setting was dropped, so a broker that needs tls is unreachable and the \
             config that says otherwise parses"
        );

        assert!(
            Config::parse("keys:\n  key1: secret1\nevents:\n  kafka:\n    brokers: kafka1:9093\n    topic: \"\"\n")
                .is_err(),
            "a broker with no topic is a producer with nowhere to produce to"
        );
    }

    #[test]
    fn rejects_config_without_keys() {
        assert!(Config::parse("port: 8080\n").is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::parse("keys:\n  key1: secret1\nnope: 1\n").is_err());
    }

    #[test]
    fn the_ice_socket_defaults_to_a_single_udp_port() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert_eq!(config.rtc.udp_port, 7882);
        assert_eq!(config.rtc.bind_address, "0.0.0.0");
        assert!(config.rtc.use_socket_address);
        assert_eq!(config.rtc.candidate_ports(), vec![7882]);
    }

    #[test]
    fn a_port_range_is_used_when_no_single_port_is_set() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nrtc:\n  udp_port: 0\n  port_range_start: 50000\n  port_range_end: 50002\n",
        )
        .unwrap();

        assert_eq!(config.rtc.candidate_ports(), vec![50000, 50001, 50002]);
    }

    #[test]
    fn an_empty_port_range_leaves_the_port_to_the_operating_system() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nrtc:\n  udp_port: 0\n  port_range_start: 0\n  port_range_end: 0\n",
        )
        .unwrap();

        assert_eq!(config.rtc.candidate_ports(), vec![0]);
    }

    #[test]
    fn advertised_addresses_take_the_bound_port_unless_they_carry_one() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nrtc:\n  advertise_addresses:\n    - 203.0.113.7\n    - 203.0.113.8:1234\n",
        )
        .unwrap();

        let advertised = config.rtc.advertise(7882);
        assert_eq!(advertised[0].to_string(), "203.0.113.7:7882");
        assert_eq!(advertised[1].to_string(), "203.0.113.8:1234");
    }

    #[test]
    fn the_ice_disconnected_grace_period_is_configurable() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
        assert_eq!(
            config.rtc.ice_disconnected_grace(),
            Duration::from_secs(DEFAULT_ICE_DISCONNECTED_TIMEOUT_SECONDS.into())
        );

        let config =
            Config::parse("keys:\n  key1: secret1\nrtc:\n  ice_disconnected_timeout: 3\n").unwrap();
        assert_eq!(config.rtc.ice_disconnected_grace(), Duration::from_secs(3));
    }

    #[test]
    fn the_negotiation_timeout_is_configurable() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
        assert_eq!(
            config.rtc.negotiation_grace(),
            Duration::from_secs(DEFAULT_NEGOTIATION_TIMEOUT_SECONDS.into())
        );

        let config =
            Config::parse("keys:\n  key1: secret1\nrtc:\n  negotiation_timeout: 2\n").unwrap();
        assert_eq!(config.rtc.negotiation_grace(), Duration::from_secs(2));
    }

    #[test]
    fn the_simulcast_timings_are_configurable() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();
        assert_eq!(
            config.video.dynacast_pause_delay(),
            Duration::from_millis(DEFAULT_DYNACAST_PAUSE_DELAY_MS.into())
        );
        assert_eq!(
            config.video.layer_upgrade_hold(),
            Duration::from_millis(DEFAULT_LAYER_UPGRADE_HOLD_MS.into())
        );

        let config = Config::parse(
            "keys:\n  key1: secret1\nvideo:\n  dynacast_pause_delay_ms: 250\n  layer_upgrade_hold_ms: 100\n",
        )
        .unwrap();
        assert_eq!(
            config.video.dynacast_pause_delay(),
            Duration::from_millis(250)
        );
        assert_eq!(
            config.video.layer_upgrade_hold(),
            Duration::from_millis(100)
        );
    }

    /// The url is the switch. There is nothing else to set and nothing that
    /// can disagree with it.
    #[test]
    fn naming_a_database_is_what_chooses_the_database() {
        let files = Config::parse("keys:\n  devkey: devsecret\n").expect("a config with no store");
        assert!(!files.sessions.in_postgres());

        let database = Config::parse(
            "keys:\n  devkey: devsecret\nsessions:\n  postgres:\n    url: \
             postgres://mediacore@db/mediacore\n",
        )
        .expect("a config that names a database");
        assert!(database.sessions.in_postgres());
        assert_eq!(
            database.sessions.postgres.connections, DEFAULT_SESSION_CONNECTIONS,
            "naming a database made a deployment spell out everything else about it as well"
        );
    }

    /// A lease shorter than three statement timeouts can run out while the node
    /// holding it is waiting on the very statement that would have renewed it,
    /// and its meetings would then be closed under people who are still
    /// talking. Refused at startup, where it is a number somebody chose, rather
    /// than at three in the morning, where it is an outage.
    #[test]
    fn a_lease_that_cannot_outlive_a_statement_is_refused() {
        let refusal = Config::parse(
            "keys:\n  devkey: devsecret\nsessions:\n  postgres:\n    url: \
             postgres://mediacore@db/mediacore\n    timeout_ms: 10000\n    lease_ms: 15000\n",
        )
        .expect_err("a lease shorter than three statement timeouts was accepted");

        assert!(
            refusal.to_string().contains("lease_ms"),
            "the refusal has to name the setting somebody has to change, and it says: {refusal}"
        );
    }

    #[test]
    fn a_directory_is_still_required_when_no_database_is_named() {
        let refusal = Config::parse("keys:\n  devkey: devsecret\nsessions:\n  directory: \"\"\n")
            .expect_err("a server with nowhere at all to put a meeting record started");

        assert!(refusal.to_string().contains("sessions.directory"));
    }

    #[test]
    fn the_shipped_sample_is_a_config_this_server_parses() {
        let config = Config::parse(include_str!("../../../config-sample.yaml"))
            .expect("config-sample.yaml has drifted away from the config this server reads");

        assert_eq!(config.port, 7880);
        assert_eq!(
            config.room.min_humans, 0,
            "the sample must not ship a policy that ends rooms nobody asked it to end"
        );
        assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Suffix);
        assert!(
            !config.sessions.in_postgres(),
            "the sample names a database, so copying it points somebody's meetings at a server \
             they did not choose -- and the single process the sample describes does not need one"
        );
        assert!(
            !config.events.has_destinations(),
            "the sample ships a destination, so copying it starts posting somebody's meetings to \
             an address they did not choose"
        );
        assert!(
            !config.video.bwe.enabled,
            "the sample must not turn bandwidth estimation on before the bench has passed"
        );
        assert_eq!(config.video.bwe.initial_kbps, DEFAULT_BWE_INITIAL_KBPS);
        assert_eq!(
            config.video.bwe.allocation_interval_ms,
            DEFAULT_BWE_ALLOCATION_INTERVAL_MS
        );
    }

    #[test]
    fn bandwidth_estimation_is_off_until_it_is_asked_for() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert!(!config.video.bwe.enabled);
        assert_eq!(config.video.bwe.initial_kbps, DEFAULT_BWE_INITIAL_KBPS);
        assert_eq!(
            config.video.bwe.min_budget_kbps,
            DEFAULT_BWE_MIN_BUDGET_KBPS
        );
        assert_eq!(
            config.video.bwe.audio_reserve_kbps,
            DEFAULT_BWE_AUDIO_RESERVE_KBPS
        );
        assert!(!config.video.bwe.allow_pause);
        assert_eq!(
            config.video.bwe.allocation_interval(),
            Duration::from_millis(DEFAULT_BWE_ALLOCATION_INTERVAL_MS.into())
        );
        assert_eq!(
            config.video.bwe.recovery_hold(),
            Duration::from_millis(DEFAULT_BWE_RECOVERY_HOLD_MS.into())
        );
        assert_eq!(
            config.video.bwe.recovery_step(),
            Duration::from_millis(DEFAULT_BWE_RECOVERY_STEP_MS.into())
        );
        assert_eq!(
            config.video.bwe.queue_pressure(),
            Duration::from_millis(DEFAULT_BWE_QUEUE_PRESSURE_MS.into())
        );
    }

    #[test]
    fn the_bandwidth_block_is_configurable() {
        let config = Config::parse(
            "keys:\n  key1: secret1\nvideo:\n  bwe:\n    enabled: true\n    initial_kbps: 1200\n    \
             allow_pause: true\n    audio_reserve_kbps: 64\n    allocation_interval_ms: 100\n",
        )
        .unwrap();

        assert!(config.video.bwe.enabled);
        assert_eq!(config.video.bwe.initial_kbps, 1_200);
        assert!(config.video.bwe.allow_pause);
        assert_eq!(config.video.bwe.audio_reserve_kbps, 64);
        assert_eq!(
            config.video.bwe.allocation_interval(),
            Duration::from_millis(100)
        );
        assert_eq!(
            config.video.dynacast_pause_delay(),
            Duration::from_millis(DEFAULT_DYNACAST_PAUSE_DELAY_MS.into()),
            "the rest of the video block kept its defaults"
        );
    }

    #[test]
    fn rejects_a_bandwidth_block_that_cannot_run() {
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nvideo:\n  bwe:\n    allocation_interval_ms: 0\n"
            )
            .is_err(),
            "an allocator that never ticks is a silently dead allocator"
        );
        assert!(
            Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    initial_kbps: 0\n").is_err()
        );
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nvideo:\n  bwe:\n    initial_kbps: 4000\n    max_desired_kbps: 1000\n"
            )
            .is_err()
        );
        assert!(
            Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    overhead_bytes: 0\n")
                .is_err()
        );
        assert!(Config::parse("keys:\n  key1: secret1\nvideo:\n  bwe:\n    nope: 1\n").is_err());
    }

    #[test]
    fn rejects_addresses_that_are_not_addresses() {
        assert!(Config::parse("keys:\n  key1: secret1\nrtc:\n  bind_address: nowhere\n").is_err());
        assert!(
            Config::parse("keys:\n  key1: secret1\nrtc:\n  advertise_addresses:\n    - nowhere\n")
                .is_err()
        );
        assert!(
            Config::parse(
                "keys:\n  key1: secret1\nrtc:\n  udp_port: 0\n  port_range_start: 60000\n  port_range_end: 50000\n"
            )
            .is_err()
        );
    }
}
