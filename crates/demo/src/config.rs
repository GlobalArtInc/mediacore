use std::env;
use std::fmt;
use std::time::Duration;

use crate::name::{RoomError, clean_room};

pub const API_KEY: &str = "MEDIACORE_API_KEY";
pub const API_SECRET: &str = "MEDIACORE_API_SECRET";
pub const WS_URL: &str = "MEDIACORE_DEMO_WS_URL";
pub const ROOM: &str = "MEDIACORE_DEMO_ROOM";
pub const PORT: &str = "MEDIACORE_DEMO_PORT";

pub const DEFAULT_ROOM: &str = "demo";
pub const DEFAULT_PORT: u16 = 8080;
pub const TOKEN_TTL: Duration = Duration::from_secs(6 * 3600);
const MIN_SECRET_BYTES: usize = 32;
const REDACTED: &str = "<redacted>";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not set")]
    Missing(&'static str),
    #[error("{0} is not valid unicode")]
    NotUnicode(&'static str),
    #[error("{name} is not a valid port number: `{value}`")]
    NotAPort { name: &'static str, value: String },
    #[error("{name} must be a ws:// or wss:// url naming a bare host, got `{value}`")]
    NotAWebSocketUrl { name: &'static str, value: String },
    #[error("{name}: {source}")]
    BadRoom {
        name: &'static str,
        #[source]
        source: RoomError,
    },
    #[error("{name} must be at least {MIN_SECRET_BYTES} bytes long")]
    ShortSecret { name: &'static str },
}

#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    pub api_secret: String,
    pub ws_url: String,
    pub room: String,
    pub port: u16,
    pub token_ttl: Duration,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &self.api_key)
            .field("api_secret", &REDACTED)
            .field("ws_url", &self.ws_url)
            .field("room", &self.room)
            .field("port", &self.port)
            .field("token_ttl", &self.token_ttl)
            .finish()
    }
}

pub trait Environment {
    fn lookup(&self, name: &'static str) -> Result<Option<String>, ConfigError>;
}

impl<F> Environment for F
where
    F: Fn(&'static str) -> Result<Option<String>, ConfigError>,
{
    fn lookup(&self, name: &'static str) -> Result<Option<String>, ConfigError> {
        self(name)
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(&process_env)
    }

    pub fn from_source(source: &dyn Environment) -> Result<Self, ConfigError> {
        let optional = |name| -> Result<Option<String>, ConfigError> {
            Ok(source
                .lookup(name)?
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()))
        };
        let required = |name| match optional(name)? {
            Some(value) => Ok(value),
            None => Err(ConfigError::Missing(name)),
        };

        let api_key = required(API_KEY)?;
        let api_secret = required(API_SECRET)?;
        if api_secret.len() < MIN_SECRET_BYTES {
            return Err(ConfigError::ShortSecret { name: API_SECRET });
        }

        let ws_url = required(WS_URL)?;
        let ws_url = ws_url.trim_end_matches('/').to_owned();
        if !is_websocket_origin(&ws_url) {
            return Err(ConfigError::NotAWebSocketUrl {
                name: WS_URL,
                value: ws_url,
            });
        }

        let room = match optional(ROOM)? {
            Some(room) => room,
            None => DEFAULT_ROOM.to_owned(),
        };
        let room =
            clean_room(&room).map_err(|source| ConfigError::BadRoom { name: ROOM, source })?;

        let port = match optional(PORT)? {
            Some(value) => value
                .parse()
                .map_err(|_| ConfigError::NotAPort { name: PORT, value })?,
            None => DEFAULT_PORT,
        };

        Ok(Self {
            api_key,
            api_secret,
            ws_url,
            room,
            port,
            token_ttl: TOKEN_TTL,
        })
    }
}

fn is_websocket_origin(value: &str) -> bool {
    let Some(authority) = value
        .strip_prefix("wss://")
        .or_else(|| value.strip_prefix("ws://"))
    else {
        return false;
    };

    !authority.is_empty()
        && authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn process_env(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode(name)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Environment {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();

        move |name: &'static str| Ok(map.get(name).cloned())
    }

    fn complete() -> Vec<(&'static str, &'static str)> {
        vec![
            (API_KEY, "devkey"),
            (API_SECRET, "devsecretdevsecretdevsecretdevsecret"),
            (WS_URL, "wss://media.example.test/"),
        ]
    }

    #[test]
    fn reads_a_complete_environment() {
        let config = Config::from_source(&env(&complete())).unwrap();

        assert_eq!(config.api_key, "devkey");
        assert_eq!(config.ws_url, "wss://media.example.test");
        assert_eq!(config.room, DEFAULT_ROOM);
        assert_eq!(config.port, DEFAULT_PORT);
    }

    #[test]
    fn takes_the_room_and_port_when_they_are_given() {
        let mut pairs = complete();
        pairs.push((ROOM, "standup"));
        pairs.push((PORT, "9000"));

        let config = Config::from_source(&env(&pairs)).unwrap();
        assert_eq!(config.room, "standup");
        assert_eq!(config.port, 9000);
    }

    #[test]
    fn a_missing_variable_is_named() {
        for missing in [API_KEY, API_SECRET, WS_URL] {
            let pairs: Vec<_> = complete()
                .into_iter()
                .filter(|(name, _)| *name != missing)
                .collect();

            assert_eq!(
                Config::from_source(&env(&pairs)).unwrap_err(),
                ConfigError::Missing(missing),
                "{missing} was allowed to be absent"
            );
        }
    }

    #[test]
    fn an_empty_variable_counts_as_missing() {
        let mut pairs = complete();
        pairs.retain(|(name, _)| *name != API_SECRET);
        pairs.push((API_SECRET, "   "));

        assert_eq!(
            Config::from_source(&env(&pairs)).unwrap_err(),
            ConfigError::Missing(API_SECRET)
        );
    }

    #[test]
    fn a_short_secret_is_refused() {
        let mut pairs = complete();
        pairs.retain(|(name, _)| *name != API_SECRET);
        pairs.push((API_SECRET, "too-short"));

        assert_eq!(
            Config::from_source(&env(&pairs)).unwrap_err(),
            ConfigError::ShortSecret { name: API_SECRET }
        );
    }

    #[test]
    fn the_debug_rendering_keeps_the_secret_out_of_the_logs() {
        let config = Config::from_source(&env(&complete())).unwrap();
        let rendered = format!("{config:?}");

        assert!(
            !rendered.contains(&config.api_secret),
            "the api secret was printed: {rendered}"
        );
        assert!(rendered.contains(REDACTED));
        assert!(rendered.contains("devkey"));
    }

    #[test]
    fn a_url_that_is_not_a_bare_host_is_refused() {
        for bad in [
            "wss://https://media.example.test",
            "wss://media.example.test/rtc",
            "wss://user@media.example.test",
            "wss://media.example.test?a=b",
            "wss://media example.test",
            "wss://media.example.test;script-src *",
            "wss://media.example.test'",
            "wss://",
        ] {
            let mut pairs = complete();
            pairs.retain(|(name, _)| *name != WS_URL);
            pairs.push((WS_URL, bad));

            assert!(
                matches!(
                    Config::from_source(&env(&pairs)),
                    Err(ConfigError::NotAWebSocketUrl { .. })
                ),
                "`{bad}` was accepted as a websocket url"
            );
        }
    }

    #[test]
    fn a_bracketed_ipv6_host_is_accepted() {
        let mut pairs = complete();
        pairs.retain(|(name, _)| *name != WS_URL);
        pairs.push((WS_URL, "wss://[2001:db8::1]:7880/"));

        let config = Config::from_source(&env(&pairs)).unwrap();
        assert_eq!(config.ws_url, "wss://[2001:db8::1]:7880");
    }

    #[test]
    fn an_http_url_is_refused() {
        let mut pairs = complete();
        pairs.retain(|(name, _)| *name != WS_URL);
        pairs.push((WS_URL, "https://media.example.test"));

        assert!(matches!(
            Config::from_source(&env(&pairs)),
            Err(ConfigError::NotAWebSocketUrl { .. })
        ));
    }

    #[test]
    fn a_nonsense_port_is_refused() {
        let mut pairs = complete();
        pairs.push((PORT, "https"));

        assert!(matches!(
            Config::from_source(&env(&pairs)),
            Err(ConfigError::NotAPort { .. })
        ));
    }

    #[test]
    fn a_nonsense_room_is_refused() {
        let mut pairs = complete();
        pairs.push((ROOM, "not a room"));

        assert!(matches!(
            Config::from_source(&env(&pairs)),
            Err(ConfigError::BadRoom { .. })
        ));
    }
}
