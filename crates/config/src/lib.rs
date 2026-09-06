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
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// The directory holding one JSON file per meeting.
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("sessions"),
        }
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
        anyhow::ensure!(
            !self.sessions.directory.as_os_str().is_empty(),
            "sessions.directory must name a directory; there is nowhere else for a meeting record \
             to go"
        );
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

    #[test]
    fn the_session_policy_is_off_until_a_room_asks_for_one() {
        let config = Config::parse("keys:\n  key1: secret1\n").unwrap();

        assert_eq!(
            config.room.min_humans, 0,
            "upgrading must not start ending rooms that nobody asked to be ended"
        );
        assert_eq!(config.room.end_grace, DEFAULT_END_GRACE_SECONDS);
        assert_eq!(config.room.duplicate_identity, DuplicateIdentity::Suffix);
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
                "keys:\n  key1: secret1\nevents:\n  webhooks:\n    urls:\n      - https://example.test/hook\n"
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
