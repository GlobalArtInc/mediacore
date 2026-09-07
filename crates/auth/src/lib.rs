use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

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

/// Where an issuer that cannot mint a claim of its own puts the account.
///
/// LiveKit's server SDKs build the token for you and offer no way to add a
/// top-level claim, but every one of them can set `attributes`. An issuer using
/// one of those has nowhere else to put an account id, so this is read as well
/// as [`Claims::account_id`] -- the same spelling, one level down.
pub const ACCOUNT_ATTRIBUTE: &str = "accountId";

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
    /// Who the platform that issued this token says this is.
    ///
    /// `sub` is what this server calls somebody and is only unique inside one
    /// room -- two people can be `alice` in two rooms, and one person's second
    /// tab becomes `alice#2`. This is the account behind them: the same string
    /// wherever they go, and the thing a recording, a bill or a history hangs
    /// off. Nothing here checks it or gives it a shape; it is the issuer's
    /// identifier and this server carries it.
    #[serde(
        default,
        rename = "accountId",
        skip_serializing_if = "String::is_empty"
    )]
    pub account_id: String,
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

    /// The platform account this token was minted for, if it names one.
    ///
    /// Read here, once, so that everything downstream -- the roster, the
    /// meeting record, a recording -- gets it from one place and cannot
    /// disagree about where it came from. Two places to write it and one place
    /// to read it: the claim first, and the attribute of the same name for an
    /// issuer whose SDK will not mint a claim.
    ///
    /// `None` when the token carries neither, which is not an error: a
    /// recorder, an agent and a guest all legitimately have no account, and a
    /// meeting they were at legitimately happened. What that costs is spelled
    /// out on `mediacore_session::Member::account_id`.
    pub fn account_id(&self) -> Option<&str> {
        [
            self.account_id.as_str(),
            self.attributes
                .get(ACCOUNT_ATTRIBUTE)
                .map(String::as_str)
                .unwrap_or_default(),
        ]
        .into_iter()
        .map(str::trim)
        .find(|account| !account.is_empty())
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
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn keys() -> HashMap<String, String> {
        HashMap::from([("key1".to_owned(), "secret-with-enough-length".to_owned())])
    }

    fn join_claims(exp: u64) -> Claims {
        Claims {
            exp,
            iss: "key1".to_owned(),
            sub: "alice".to_owned(),
            name: "Alice".to_owned(),
            video: VideoGrant {
                room_join: Some(true),
                room: "demo".to_owned(),
                ..VideoGrant::default()
            },
            ..Claims::default()
        }
    }

    #[test]
    fn accepts_token_signed_with_known_key() {
        let token = issue("secret-with-enough-length", &join_claims(now() + 600)).unwrap();
        let claims = verify_join(&token, &keys()).unwrap();

        assert_eq!(claims.identity(), "alice");
        assert_eq!(claims.video.room, "demo");
        assert!(claims.video.can_subscribe());
        assert!(claims.video.can_publish());
    }

    #[test]
    fn rejects_expired_token() {
        let token = issue("secret-with-enough-length", &join_claims(now() - 600)).unwrap();
        assert!(matches!(
            verify(&token, &keys()),
            Err(AuthError::Rejected(_))
        ));
    }

    #[test]
    fn rejects_unknown_api_key() {
        let mut claims = join_claims(now() + 600);
        claims.iss = "key9".to_owned();
        let token = issue("secret-with-enough-length", &claims).unwrap();

        assert!(matches!(
            verify(&token, &keys()),
            Err(AuthError::UnknownKey(_))
        ));
    }

    #[test]
    fn rejects_token_signed_with_wrong_secret() {
        let token = issue("another-secret-entirely", &join_claims(now() + 600)).unwrap();
        assert!(matches!(
            verify(&token, &keys()),
            Err(AuthError::Rejected(_))
        ));
    }

    #[test]
    fn accepts_a_server_api_token_that_names_nobody() {
        let claims = Claims {
            exp: now() + 600,
            iss: "key1".to_owned(),
            video: VideoGrant {
                room_create: Some(true),
                ..VideoGrant::default()
            },
            ..Claims::default()
        };
        let token = issue("secret-with-enough-length", &claims).unwrap();

        let payload = token.split('.').nth(1).unwrap();
        let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert!(
            !payload.contains("\"sub\""),
            "the token under test has an identity after all, so it proves nothing: {payload}"
        );

        let verified = verify(&token, &keys()).expect(
            "LiveKit's server SDKs mint a room service token with no identity on it, so requiring \
             one here refuses every one of them",
        );
        assert!(verified.identity().is_empty());
        assert_eq!(verified.video.room_create, Some(true));

        assert!(
            matches!(verify_join(&token, &keys()), Err(AuthError::NoRoomGrant)),
            "a token with nobody on it must still not be allowed into a room"
        );
    }

    #[test]
    fn an_account_comes_out_of_the_claim_or_out_of_the_attributes() {
        let mut claims = join_claims(now() + 600);
        assert_eq!(
            claims.account_id(),
            None,
            "a token that names no account has to read as naming none, not as naming an empty one"
        );

        claims
            .attributes
            .insert(ACCOUNT_ATTRIBUTE.to_owned(), "  AC_7  ".to_owned());
        assert_eq!(
            claims.account_id(),
            Some("AC_7"),
            "an issuer using a LiveKit server SDK has nowhere but the attributes to put this, so \
             refusing to look there means it can never set it at all"
        );

        claims.account_id = "AC_1".to_owned();
        assert_eq!(
            claims.account_id(),
            Some("AC_1"),
            "the claim is the issuer's own field and the attribute is the fallback; reading the \
             fallback first would let a participant who can set their own attributes overrule it"
        );
    }

    #[test]
    fn an_account_survives_a_round_trip_through_a_token() {
        let mut claims = join_claims(now() + 600);
        claims.account_id = "AC_1".to_owned();

        let token = issue("secret-with-enough-length", &claims).unwrap();
        let verified = verify_join(&token, &keys()).unwrap();

        assert_eq!(verified.account_id(), Some("AC_1"));
    }

    #[test]
    fn rejects_token_without_join_grant() {
        let mut claims = join_claims(now() + 600);
        claims.video.room_join = None;
        let token = issue("secret-with-enough-length", &claims).unwrap();

        assert!(matches!(
            verify_join(&token, &keys()),
            Err(AuthError::NoRoomGrant)
        ));
    }
}
