use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use mediacore_auth::{Claims, VideoGrant};

/// Where a bot's token comes from.
///
/// Minting needs the api secret, which is the one thing a bench is not always
/// allowed to have: pointing this at somebody else's staging cluster means
/// asking their token endpoint for a handful of tokens and passing them in.
/// A run over given tokens can grow no further than the tokens it was given,
/// and says so at the start rather than half way up a ladder.
///
/// No `Debug`, on purpose: the minting arm holds the api secret, and a type
/// that can be printed is a type that ends up in a log line eventually.
pub enum Tokens {
    Mint {
        key: String,
        secret: String,
        ttl: Duration,
    },
    Given(Vec<String>),
}

impl Tokens {
    pub fn mint(key: String, secret: String, ttl: Duration) -> Result<Self> {
        if secret.is_empty() {
            bail!(
                "no api secret: pass --api-secret (or MEDIACORE_API_SECRET) to mint tokens, or \
                 --token once per bot to bring your own"
            );
        }

        Ok(Self::Mint { key, secret, ttl })
    }

    /// How many bots this can dress, when there is a limit.
    pub fn capacity(&self) -> Option<usize> {
        match self {
            Self::Mint { .. } => None,
            Self::Given(tokens) => Some(tokens.len()),
        }
    }

    pub fn issue(&self, index: usize, identity: &str, room: &str) -> Result<String> {
        match self {
            Self::Given(tokens) => tokens.get(index).cloned().with_context(|| {
                format!(
                    "bot {index} has no token: {} were given with --token",
                    tokens.len()
                )
            }),
            Self::Mint { key, secret, ttl } => {
                let exp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("this machine's clock is before 1970")?
                    + *ttl;

                let claims = Claims {
                    exp: exp.as_secs(),
                    iss: key.clone(),
                    sub: identity.to_owned(),
                    name: identity.to_owned(),
                    video: VideoGrant {
                        room_join: Some(true),
                        room: room.to_owned(),
                        can_publish: Some(true),
                        can_subscribe: Some(true),
                        ..VideoGrant::default()
                    },
                    ..Claims::default()
                };

                mediacore_auth::issue(secret, &claims)
                    .with_context(|| format!("cannot mint a token for {identity}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "token_tests.rs"]
mod tests;
