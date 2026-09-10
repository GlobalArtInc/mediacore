use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

pub mod turn;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("token is malformed")]
    Malformed,
    #[error("unknown api key `{0}`")]
    UnknownKey(String),
    #[error("token rejected: {0}")]
    Rejected(#[from] jsonwebtoken::errors::Error),
    #[error("token does not grant access to a room")]
    NoRoomGrant,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoGrant {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_create: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_list: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_record: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_admin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_join: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub room: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_publish: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_subscribe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_publish_data: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub can_publish_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_update_own_metadata: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorder: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
}

impl VideoGrant {
    pub fn room_join(&self) -> bool {
        self.room_join.unwrap_or(false)
    }

    pub fn can_publish(&self) -> bool {
        self.can_publish.unwrap_or(true)
    }

    pub fn can_subscribe(&self) -> bool {
        self.can_subscribe.unwrap_or(true)
    }

    pub fn can_publish_data(&self) -> bool {
        self.can_publish_data.unwrap_or(true)
    }

    pub fn can_update_own_metadata(&self) -> bool {
        self.can_update_own_metadata.unwrap_or(false)
    }

    pub fn hidden(&self) -> bool {
        self.hidden.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Claims {
    pub exp: u64,
    pub iss: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nbf: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metadata: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub attributes: HashMap<String, String>,
    #[serde(default)]
    pub video: VideoGrant,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
}

impl Claims {
    pub fn identity(&self) -> &str {
        &self.sub
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.sub
        } else {
            &self.name
        }
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Checks that a token is ours and still valid, and nothing else.
///
/// `sub` is deliberately not required. A token for the room service carries no
/// identity at all -- LiveKit's own server SDKs refuse to set one unless the
/// grant is `roomJoin`, so their `CreateRoom` token has no `sub` field to
/// find. Insisting on one here would reject every server SDK on the first
/// call. The identity is required where it is actually needed, by
/// [`verify_join`].
pub fn verify(token: &str, keys: &HashMap<String, String>) -> Result<Claims, AuthError> {
    let issuer = peek_issuer(token)?;
    let secret = keys
        .get(&issuer)
        .ok_or_else(|| AuthError::UnknownKey(issuer.clone()))?;

    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_nbf = true;
    validation.leeway = 5;
    validation.set_required_spec_claims(&["exp", "iss"]);

    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;
    Ok(data.claims)
}

pub fn verify_join(token: &str, keys: &HashMap<String, String>) -> Result<Claims, AuthError> {
    let claims = verify(token, keys)?;
    if !claims.video.room_join() || claims.video.room.is_empty() || claims.sub.is_empty() {
        return Err(AuthError::NoRoomGrant);
    }
    Ok(claims)
}

pub fn issue(secret: &str, claims: &Claims) -> Result<String, AuthError> {
    let token = encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok(token)
}

fn peek_issuer(token: &str) -> Result<String, AuthError> {
    let payload = token.split('.').nth(1).ok_or(AuthError::Malformed)?;
    let raw = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AuthError::Malformed)?;
    let value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|_| AuthError::Malformed)?;
    value
        .get("iss")
        .and_then(|iss| iss.as_str())
        .map(str::to_owned)
        .ok_or(AuthError::Malformed)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
