use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

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
pub const DEFAULT_BWE_RECOVERY_STEP_MS: u32 = 1_000;
pub const DEFAULT_BWE_QUEUE_PRESSURE_MS: u32 = 150;
pub const DEFAULT_BWE_MAX_OVERRUN_PERCENT: u32 = 150;

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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomConfig {
    pub auto_create: bool,
    pub empty_timeout: u32,
    pub departure_timeout: u32,
    pub max_participants: u32,
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
        }
    }
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            auto_create: true,
            empty_timeout: 300,
            departure_timeout: 20,
            max_participants: 0,
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
        self.rtc.validate()?;
        self.video.bwe.validate()?;
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
