use std::env;
use std::fmt;
use std::time::Duration;

use crate::auth::CALLBACK_PATH;
use crate::brand::{Brand, BrandError};
use crate::name::{RoomError, clean_room};
use crate::oidc::DEFAULT_SCOPES;
use crate::session::DEFAULT_SESSION_TTL;

pub const API_KEY: &str = "MEDIACORE_API_KEY";
pub const API_SECRET: &str = "MEDIACORE_API_SECRET";
pub const WS_URL: &str = "MEDIACORE_FRONT_WS_URL";
pub const ROOM: &str = "MEDIACORE_FRONT_ROOM";
pub const PORT: &str = "MEDIACORE_FRONT_PORT";

/// Whose product this is. The name is the words, and the logo is the picture a
/// header draws instead of them -- inline or from a path, the same two ways in
/// a licence key is offered, because a picture pasted into a Kubernetes Secret
/// and a picture in a file on a VM are both things people do.
pub const BRAND_NAME: &str = "MEDIACORE_FRONT_BRAND_NAME";
pub const BRAND_LOGO: &str = "MEDIACORE_FRONT_BRAND_LOGO";
pub const BRAND_LOGO_FILE: &str = "MEDIACORE_FRONT_BRAND_LOGO_FILE";

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
    #[error("{name}: {source}")]
    BadBrand {
        name: &'static str,
        #[source]
        source: BrandError,
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
    pub brand: Brand,
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
            .field("brand", &self.brand)
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

        let brand = Brand::new(
            optional(BRAND_NAME)?.as_deref(),
            optional(BRAND_LOGO)?.as_deref(),
            optional(BRAND_LOGO_FILE)?.as_deref(),
        )
        .map_err(|source| ConfigError::BadBrand {
            name: brand_variable(&source),
            source,
        })?;

        let oidc = Self::sign_in_from(&optional)?;

        Ok(Self {
            api_key,
            api_secret,
            ws_url,
            room,
            port,
            token_ttl: TOKEN_TTL,
            brand,
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

/// Which variable an operator has to go and fix.
///
/// The message says what is wrong with the value; this says where the value
/// came from, which is the half that turns a refusal to start into something
/// somebody can act on without reading the source.
fn brand_variable(error: &BrandError) -> &'static str {
    match error {
        BrandError::NameTooLong { .. } | BrandError::NameControl => BRAND_NAME,
        BrandError::LogoUnreadable { .. } => BRAND_LOGO_FILE,
        _ => BRAND_LOGO,
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
