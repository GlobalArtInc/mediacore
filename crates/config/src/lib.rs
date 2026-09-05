use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

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
    pub port_range_start: u16,
    pub port_range_end: u16,
    pub use_external_ip: bool,
    pub node_ip: String,
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
            port_range_start: 50000,
            port_range_end: 60000,
            use_external_ip: false,
            node_ip: String::new(),
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
        Ok(())
    }
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
}
