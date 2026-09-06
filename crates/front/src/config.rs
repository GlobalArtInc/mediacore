use std::env;
use std::fmt;
use std::time::Duration;

use crate::auth::CALLBACK_PATH;
use crate::name::{RoomError, clean_room};
use crate::oidc::DEFAULT_SCOPES;
use crate::session::DEFAULT_SESSION_TTL;

pub const API_KEY: &str = "MEDIACORE_API_KEY";
pub const API_SECRET: &str = "MEDIACORE_API_SECRET";
pub const WS_URL: &str = "MEDIACORE_FRONT_WS_URL";
pub const ROOM: &str = "MEDIACORE_FRONT_ROOM";
pub const PORT: &str = "MEDIACORE_FRONT_PORT";

/// The one variable that turns signing in on. Everything else about it is
/// required only because this one is set, which is what makes the feature a
/// switch rather than a mandate: a stand that has to stay quick to check keeps
/// working exactly as it did by leaving this unset.
pub const OIDC_ISSUER: &str = "MEDIACORE_FRONT_OIDC_ISSUER";
pub const OIDC_CLIENT_ID: &str = "MEDIACORE_FRONT_OIDC_CLIENT_ID";
pub const OIDC_CLIENT_SECRET: &str = "MEDIACORE_FRONT_OIDC_CLIENT_SECRET";
pub const OIDC_SCOPES: &str = "MEDIACORE_FRONT_OIDC_SCOPES";
pub const PUBLIC_URL: &str = "MEDIACORE_FRONT_PUBLIC_URL";
pub const SESSION_TTL: &str = "MEDIACORE_FRONT_SESSION_TTL";

pub const DEFAULT_ROOM: &str = "demo";
pub const DEFAULT_PORT: u16 = 8080;
pub const TOKEN_TTL: Duration = Duration::from_secs(6 * 3600);
const MIN_SECRET_BYTES: usize = 32;
const REDACTED: &str = "<redacted>";

/// A session shorter than this is a person signing in twice in one meeting;
/// longer than a month is a cookie nobody remembers issuing.
const MIN_SESSION_TTL: Duration = Duration::from_secs(60);
const MAX_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

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
    #[error("{name} is not set, and signing people in needs it because {OIDC_ISSUER} is")]
    NeedsSignIn { name: &'static str },
    #[error("{name} must be an http or https url, got `{value}`")]
    NotAWebUrl { name: &'static str, value: String },
    #[error("{name} must be an origin with no path, query or fragment, got `{value}`")]
    NotAnOrigin { name: &'static str, value: String },
    #[error(
        "{name} must be a number of seconds between {} and {}, got `{value}`",
        MIN_SESSION_TTL.as_secs(),
        MAX_SESSION_TTL.as_secs()
    )]
    NotASessionLifetime { name: &'static str, value: String },
}

/// Everything about signing people in, or nothing at all.
///
/// It is one `Option` rather than six loose fields for a reason worth keeping:
/// with sign-in off there is no half-configured state to reason about, and no
/// handler can find a client id without an issuer beside it.
#[derive(Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// The origin a browser reaches this front end at, and the only origin a
    /// `returnUrl` may name. The redirect URI and the post-logout URI are both
    /// derived from it, so there is one place to get wrong rather than three
    /// that can disagree.
    pub public_url: String,
    pub scopes: String,
    pub session_ttl: Duration,
}

impl OidcConfig {
    /// What the provider has registered, and what it will refuse to redirect
    /// to if it does not match to the character.
    pub fn redirect_uri(&self) -> String {
        format!("{}{CALLBACK_PATH}", self.public_url)
    }

    pub fn post_logout_uri(&self) -> String {
        format!("{}/", self.public_url)
    }
}

/// The client secret is the provider's, and it is redacted here for the same
/// reason `api_secret` is: one `{config:?}` in a log line is enough to hand it
/// to whoever reads logs.
impl fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &REDACTED)
            .field("public_url", &self.public_url)
            .field("redirect_uri", &self.redirect_uri())
            .field("scopes", &self.scopes)
            .field("session_ttl", &self.session_ttl)
            .finish()
    }
}

#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    pub api_secret: String,
    pub ws_url: String,
    pub room: String,
    pub port: u16,
    pub token_ttl: Duration,
    pub oidc: Option<OidcConfig>,
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
            .field("oidc", &self.oidc)
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

        let oidc = Self::sign_in_from(&optional)?;

        Ok(Self {
            api_key,
            api_secret,
            ws_url,
            room,
            port,
            token_ttl: TOKEN_TTL,
            oidc,
        })
    }

    /// Everything the sign-in needs, or nothing, and never half of it. An
    /// issuer with no client id would start a process that serves the page,
    /// redirects nobody and fails at the first press of Join; refusing at
    /// startup with the name of the missing variable is the same bargain the
    /// rest of this file already makes.
    fn sign_in_from(
        optional: &dyn Fn(&'static str) -> Result<Option<String>, ConfigError>,
    ) -> Result<Option<OidcConfig>, ConfigError> {
        let Some(issuer) = optional(OIDC_ISSUER)? else {
            return Ok(None);
        };
        let issuer = web_url(OIDC_ISSUER, &issuer)?;

        let needed = |name| match optional(name)? {
            Some(value) => Ok(value),
            None => Err(ConfigError::NeedsSignIn { name }),
        };

        let client_id = needed(OIDC_CLIENT_ID)?;
        // No minimum length. `MEDIACORE_API_SECRET` has one because this
        // process makes that secret and can insist; a client secret is issued
        // by the provider, and refusing to start because theirs is short would
        // be this process arguing with a system it does not run.
        let client_secret = needed(OIDC_CLIENT_SECRET)?;

        let public_url = needed(PUBLIC_URL)?;
        let public_url = web_url(PUBLIC_URL, &public_url)?;
        if !is_origin(&public_url) {
            return Err(ConfigError::NotAnOrigin {
                name: PUBLIC_URL,
                value: public_url,
            });
        }

        let scopes = optional(OIDC_SCOPES)?.unwrap_or_else(|| DEFAULT_SCOPES.to_owned());
        let session_ttl = match optional(SESSION_TTL)? {
            Some(value) => {
                let seconds = value
                    .parse()
                    .map(Duration::from_secs)
                    .ok()
                    .filter(|ttl| (MIN_SESSION_TTL..=MAX_SESSION_TTL).contains(ttl));

                seconds.ok_or(ConfigError::NotASessionLifetime {
                    name: SESSION_TTL,
                    value,
                })?
            }
            None => DEFAULT_SESSION_TTL,
        };

        Ok(Some(OidcConfig {
            issuer,
            client_id,
            client_secret,
            public_url,
            scopes,
            session_ttl,
        }))
    }
}

/// An http or https URL with a host, trailing slash removed so that everything
/// derived from it is one concatenation rather than two conventions.
fn web_url(name: &'static str, value: &str) -> Result<String, ConfigError> {
    let refuse = || ConfigError::NotAWebUrl {
        name,
        value: value.to_owned(),
    };
    let url = url::Url::parse(value.trim()).map_err(|_| refuse())?;

    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.origin(), url::Origin::Tuple(..))
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(refuse());
    }

    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Whether a URL is a bare origin. The page addresses its own assets from the
/// root -- `/app.js`, `/r/<room>` -- so a public URL carrying a path would
/// describe a deployment this binary cannot serve, and would build a redirect
/// URI the provider has never seen.
fn is_origin(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| matches!(url.path(), "" | "/"))
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

    fn signing_in() -> Vec<(&'static str, &'static str)> {
        let mut pairs = complete();
        pairs.push((OIDC_ISSUER, "https://sso.mcn.test"));
        pairs.push((OIDC_CLIENT_ID, "a-client"));
        pairs.push((OIDC_CLIENT_SECRET, "a-client-secret"));
        pairs.push((PUBLIC_URL, "https://rooms.example.test"));
        pairs
    }

    /// A switch, not a mandate: an environment that says nothing about an
    /// identity provider gets the page it always had.
    #[test]
    fn signing_in_is_off_until_an_issuer_says_otherwise() {
        assert!(
            Config::from_source(&env(&complete()))
                .unwrap()
                .oidc
                .is_none()
        );
        assert!(
            Config::from_source(&env(&signing_in()))
                .unwrap()
                .oidc
                .is_some()
        );
    }

    /// Half-configured is the state worth refusing at startup. An issuer with
    /// no client id starts a process that serves the page, redirects nobody
    /// and fails at the first press of Join, with nothing in the log to say
    /// which variable was missing.
    #[test]
    fn an_issuer_with_nothing_beside_it_is_refused_by_name() {
        for missing in [OIDC_CLIENT_ID, OIDC_CLIENT_SECRET, PUBLIC_URL] {
            let pairs: Vec<_> = signing_in()
                .into_iter()
                .filter(|(name, _)| *name != missing)
                .collect();

            assert_eq!(
                Config::from_source(&env(&pairs)).unwrap_err(),
                ConfigError::NeedsSignIn { name: missing },
                "{missing} was allowed to be absent with an issuer set"
            );
        }
    }

    /// The provider has this written down. Deriving it from one variable is
    /// what stops it drifting from the registered value, and the registered
    /// value is what a provider refuses a request over.
    #[test]
    fn the_redirect_uri_is_derived_from_the_one_public_url() {
        for public in [
            "https://rooms.example.test",
            "https://rooms.example.test/",
            "https://10.251.10.201:5400",
        ] {
            let mut pairs = signing_in();
            pairs.retain(|(name, _)| *name != PUBLIC_URL);
            pairs.push((PUBLIC_URL, public));

            let oidc = Config::from_source(&env(&pairs)).unwrap().oidc.unwrap();
            let expected = public.trim_end_matches('/');

            assert_eq!(oidc.redirect_uri(), format!("{expected}/auth/callback"));
            assert_eq!(oidc.post_logout_uri(), format!("{expected}/"));
        }
    }

    /// A public url with a path in it describes a deployment this binary
    /// cannot serve -- the page addresses `/app.js` and `/r/<room>` from the
    /// root -- and would build a redirect uri no provider has ever seen.
    #[test]
    fn a_public_url_that_is_not_an_origin_is_refused() {
        for bad in [
            ("https://rooms.example.test/meet", true),
            ("rooms.example.test", false),
            ("ftp://rooms.example.test", false),
            ("https://rooms.example.test?a=b", false),
            ("https://rooms.example.test#a", false),
        ] {
            let mut pairs = signing_in();
            pairs.retain(|(name, _)| *name != PUBLIC_URL);
            pairs.push((PUBLIC_URL, bad.0));

            let refused = Config::from_source(&env(&pairs)).unwrap_err();
            let expected_shape = match bad.1 {
                true => matches!(refused, ConfigError::NotAnOrigin { .. }),
                false => matches!(refused, ConfigError::NotAWebUrl { .. }),
            };

            assert!(
                expected_shape,
                "`{}` was accepted, and answered {refused}",
                bad.0
            );
        }
    }

    /// An issuer may carry a path -- plenty of providers put a realm there --
    /// so it is checked for being a url and nothing more.
    #[test]
    fn an_issuer_may_name_a_realm_but_has_to_be_a_url() {
        let mut pairs = signing_in();
        pairs.retain(|(name, _)| *name != OIDC_ISSUER);
        pairs.push((OIDC_ISSUER, "https://sso.mcn.test/realms/mediacore/"));

        let oidc = Config::from_source(&env(&pairs)).unwrap().oidc.unwrap();
        assert_eq!(
            oidc.issuer, "https://sso.mcn.test/realms/mediacore",
            "a trailing slash on the issuer would be compared against a token's `iss` and lose"
        );

        pairs.retain(|(name, _)| *name != OIDC_ISSUER);
        pairs.push((OIDC_ISSUER, "sso.mcn.test"));
        assert!(matches!(
            Config::from_source(&env(&pairs)),
            Err(ConfigError::NotAWebUrl { .. })
        ));
    }

    #[test]
    fn a_session_lifetime_that_is_not_one_is_refused() {
        let mut pairs = signing_in();
        assert_eq!(
            Config::from_source(&env(&pairs))
                .unwrap()
                .oidc
                .unwrap()
                .session_ttl,
            DEFAULT_SESSION_TTL
        );

        pairs.push((SESSION_TTL, "3600"));
        assert_eq!(
            Config::from_source(&env(&pairs))
                .unwrap()
                .oidc
                .unwrap()
                .session_ttl,
            Duration::from_secs(3600)
        );

        for bad in ["0", "10", "an hour", "-1", "99999999"] {
            let mut pairs = signing_in();
            pairs.push((SESSION_TTL, bad));

            assert!(
                matches!(
                    Config::from_source(&env(&pairs)),
                    Err(ConfigError::NotASessionLifetime { .. })
                ),
                "`{bad}` was accepted as how long a session lasts"
            );
        }
    }

    /// The provider chose this secret, not us, so its length is not ours to
    /// argue with -- unlike `MEDIACORE_API_SECRET`, which this process makes.
    #[test]
    fn a_short_client_secret_is_the_providers_business() {
        let mut pairs = signing_in();
        pairs.retain(|(name, _)| *name != OIDC_CLIENT_SECRET);
        pairs.push((OIDC_CLIENT_SECRET, "short"));

        assert_eq!(
            Config::from_source(&env(&pairs))
                .unwrap()
                .oidc
                .unwrap()
                .client_secret,
            "short"
        );
    }

    #[test]
    fn the_debug_rendering_keeps_the_client_secret_out_of_the_logs() {
        let config = Config::from_source(&env(&signing_in())).unwrap();
        let rendered = format!("{config:?}");

        assert!(
            !rendered.contains("a-client-secret"),
            "the client secret was printed: {rendered}"
        );
        assert_eq!(
            rendered.matches(REDACTED).count(),
            2,
            "one of the two secrets is being printed: {rendered}"
        );
        assert!(
            rendered.contains("a-client") && rendered.contains("https://sso.mcn.test"),
            "the debug rendering says nothing useful about the provider: {rendered}"
        );
    }
}
