//! Signing people in against an OpenID Connect provider.
//!
//! Authorization code with PKCE, and nothing else: no implicit, no hybrid, no
//! `response_type=token`, whatever a provider says it also supports. The code
//! comes back through the browser and is redeemed from this process over TLS,
//! so the thing that is actually worth stealing never touches a URL bar, a
//! history entry or a `Referer` header.
//!
//! Four secrets ride along with the round trip and each one closes a different
//! hole:
//!
//!   `state`     names the sign-in this callback belongs to, and it is a key
//!               into a map in this process rather than anything the browser
//!               can read or edit. It is also the CSRF defence: a callback with
//!               a `state` this process never issued is refused
//!   the login cookie   proves the callback came back to the *same browser*
//!               that started the sign-in. Without it somebody can hand you a
//!               callback URL and sign you into their account
//!   `nonce`     is echoed by the ID token, so a token minted for another
//!               sign-in cannot be replayed into this one
//!   PKCE `S256` makes the code worthless to whoever intercepts it, because
//!               redeeming it needs the verifier, and the verifier never left
//!               this process
//!
//! ## Validating the ID token is the part that gets skipped
//!
//! An ID token is a signed statement about who somebody is, and a validator
//! that omits a check turns it into an unsigned one. Every check here has a
//! test that removes exactly it:
//!
//!   * the **signature**, against a key from the provider's JWKS, chosen by the
//!     token's `kid` and *never* by the algorithm the token names. A token that
//!     says `alg: none` or `alg: HS256` is refused before a key is looked at,
//!     because the classic forgery is to name a symmetric algorithm and get a
//!     validator to verify an RSA public key against itself as an HMAC secret
//!   * `iss`, against the configured issuer
//!   * `aud`, against this client id -- and `azp` against it too whenever the
//!     token carries one, not only when it is addressed to several audiences
//!   * `exp`, with a minute of leeway for clock skew
//!   * `nonce`, against the one *this* sign-in issued, compared whole
//!
//! ## What is deliberately not here
//!
//! No refresh tokens. The session in this process has its own lifetime and
//! there is nothing to refresh: nothing here calls the provider again once
//! somebody is signed in, and a refresh token kept for a call that never
//! happens is a credential lying about.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, errors::ErrorKind};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use url::{Origin, Url};

use crate::log::quoted;
use crate::name::{MAX_NAME_CHARS, clean_name};
use crate::session::{Identity, Login, same_secret};

pub const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";

/// `openid` is what makes this OpenID Connect rather than bare OAuth; `profile`
/// and `email` are what the display name is made of. `offline_access` is
/// deliberately absent -- see the module note on refresh tokens.
pub const DEFAULT_SCOPES: &str = "openid profile email";

pub const RESPONSE_TYPE: &str = "code";

/// The code comes back in the query string, which is the only place this crate
/// reads it from. `fragment` would put it somewhere only the page can see, and
/// the page is exactly who should not have it.
pub const RESPONSE_MODE: &str = "query";

pub const CODE_CHALLENGE_METHOD: &str = "S256";

/// The one algorithm an ID token may be signed with here. It is what the
/// provider advertises, and pinning it is what makes the token's own `alg`
/// field an assertion this code never acts on.
pub const ID_TOKEN_ALGORITHM: Algorithm = Algorithm::RS256;

/// Clock skew between this host and the provider. A minute is generous for two
/// machines that both run NTP and small enough that an expired token is not
/// usable for an interesting length of time.
const LEEWAY: u64 = 60;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
const JWKS_TTL: Duration = Duration::from_secs(3600);

/// How often an unknown `kid` may send us back to the provider. A key really
/// did rotate: fetch it. A stream of tokens carrying invented `kid`s: do not
/// turn one request per token into one request per token at the provider as
/// well.
const JWKS_MIN_REFETCH: Duration = Duration::from_secs(60);

/// A `sub` is an opaque identifier and this is what it is allowed to be: long
/// enough for a UUID or a directory DN, short enough that it cannot be used to
/// stuff something else.
const MAX_SUBJECT_CHARS: usize = 128;

/// The endpoints, as the provider itself describes them.
///
/// Discovered rather than configured, so that one variable names the provider
/// and the provider says where its own doors are. The document is cached; the
/// checks on it are in [`Provider::rediscover`].
#[derive(Clone, Debug, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IdTokenError {
    #[error("the id token is not a json web token")]
    Malformed,
    #[error("the id token is signed with `{0}`, and only {ID_TOKEN_ALGORITHM:?} is accepted here")]
    WrongAlgorithm(String),
    #[error("the provider published no signing key named `{0}`")]
    UnknownKey(String),
    #[error("the id token names no signing key and the provider publishes more than one")]
    AmbiguousKey,
    #[error("the provider's signing key is not one this can verify against")]
    UnusableKey,
    #[error("the id token's signature is not the provider's")]
    BadSignature,
    #[error("the id token has expired")]
    Expired,
    #[error("the id token was issued by somebody other than the configured provider")]
    WrongIssuer,
    #[error("the id token was not issued for this client")]
    WrongAudience,
    #[error("the id token was issued to a different client than the one presenting it")]
    WrongParty,
    #[error("the id token has no `{0}`")]
    MissingClaim(String),
    #[error("the id token answers a different sign-in than the one that asked for it")]
    WrongNonce,
    #[error("the id token names nobody this could use as an identity")]
    UnusableSubject,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("the identity provider could not be reached")]
    Unreachable,
    #[error("the identity provider answered {status} for its {what}")]
    Refused { what: &'static str, status: u16 },
    #[error("the identity provider's {0} is not readable")]
    Unreadable(&'static str),
    #[error(
        "the identity provider describes itself as `{found}` and was configured as `{configured}`"
    )]
    NotItself { configured: String, found: String },
    #[error("the identity provider's {what} is not a url: `{value}`")]
    NotAUrl { what: &'static str, value: String },
    #[error("the identity provider handed out no id token, so it said nothing about who signed in")]
    NoIdToken,
    #[error(transparent)]
    IdToken(#[from] IdTokenError),
    #[error("the provider's user info is about somebody other than the id token was")]
    DifferentPerson,
}

/// What a validated ID token said.
#[derive(Debug, Clone, Default)]
pub struct Person {
    pub subject: String,
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    pub email: Option<String>,
    pub roles: Vec<String>,
}

impl Person {
    /// The name to put on the roster, in the order the decision names: `name`,
    /// else `preferred_username`, else `email`.
    ///
    /// The subject is the last resort and not part of that order -- it is a
    /// UUID and nobody would recognise it -- but a roster entry with no text
    /// at all is worse than an ugly one, and a directory that returns none of
    /// the three is a directory this page cannot argue with.
    ///
    /// Whatever comes back goes through the same `clean_name` a typed name
    /// does, and is cut to the same length. A provider is not a trusted source
    /// of short, printable strings: a display name with a newline in it or two
    /// hundred characters of it would otherwise go straight into a token and
    /// onto everybody's screen.
    pub fn display_name(&self) -> String {
        [
            self.name.as_deref(),
            self.preferred_username.as_deref(),
            self.email.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(printable)
        .next()
        .unwrap_or_else(|| self.subject.clone())
    }

    /// What the ID token did not say and the userinfo endpoint did. The ID
    /// token wins wherever it spoke: it is signed and the userinfo response is
    /// merely fetched over TLS from an endpoint the discovery document named.
    fn filled_in_from(mut self, other: Person) -> Self {
        self.name = self.name.or(other.name);
        self.preferred_username = self.preferred_username.or(other.preferred_username);
        self.email = self.email.or(other.email);
        if self.roles.is_empty() {
            self.roles = other.roles;
        }
        self
    }
}

/// A name that can be shown, or nothing. `clean_name` is the same gate a typed
/// name passes; the truncation before it is what stops a long one being
/// refused outright rather than shortened.
fn printable(candidate: &str) -> Option<String> {
    let trimmed: String = candidate.trim().chars().take(MAX_NAME_CHARS).collect();

    clean_name(&trimmed).ok()
}

/// The identity a token is minted for. Bounded and free of anything that could
/// mean something else downstream -- it becomes a LiveKit participant identity,
/// which is a key in the SFU's rooms.
pub fn clean_subject(raw: &str) -> Result<String, IdTokenError> {
    let subject = raw.trim();

    if subject.is_empty()
        || subject.chars().count() > MAX_SUBJECT_CHARS
        || subject.chars().any(char::is_control)
    {
        return Err(IdTokenError::UnusableSubject);
    }

    Ok(subject.to_owned())
}

/// The PKCE challenge for a verifier: `S256`, which is the only method offered
/// here. `plain` is in the RFC and puts the verifier in the redirect, which is
/// the one place the whole exchange is trying to keep it out of.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What goes in the query string of the redirect to the provider.
#[derive(Debug, Clone)]
pub struct Authorize<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub scopes: &'a str,
    pub state: &'a str,
    pub nonce: &'a str,
    pub challenge: &'a str,
}

/// The URL a browser is sent to, built with a URL library rather than with
/// `format!`: a redirect URI with a query of its own, or a room name with a
/// `&` in it, has to come out the other side as one parameter rather than
/// several.
pub fn authorize_url(endpoint: &str, request: &Authorize<'_>) -> Result<String, ProviderError> {
    let mut url = Url::parse(endpoint).map_err(|_| ProviderError::NotAUrl {
        what: "authorization endpoint",
        value: endpoint.to_owned(),
    })?;

    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", RESPONSE_TYPE);
        query.append_pair("response_mode", RESPONSE_MODE);
        query.append_pair("client_id", request.client_id);
        query.append_pair("redirect_uri", request.redirect_uri);
        query.append_pair("scope", request.scopes);
        query.append_pair("state", request.state);
        query.append_pair("nonce", request.nonce);
        query.append_pair("code_challenge", request.challenge);
        query.append_pair("code_challenge_method", CODE_CHALLENGE_METHOD);
    }

    Ok(url.to_string())
}

/// A path on this server, or nothing.
///
/// This is what stands between a `state` map and an open redirect. Everything
/// that reaches it is a path out of a request to this process, so it starts
/// with `/` already; what it is looking for is the shapes a browser reads as
/// *another origin*:
///
///   `//evil.example`   protocol-relative: a `Location` of this goes to
///                      `https://evil.example`, not to a path here
///   `/\evil.example`   the same thing, because browsers fold `\` into `/`
///                      inside a URL and one of the two forms would otherwise
///                      slip through
///   `https://…`        an absolute URL, in case one ever reaches here
///
/// Control characters are refused as well: a `\r\n` in a `Location` is a
/// response splitting attempt, and `HeaderValue` would reject it later anyway
/// -- refusing it here means the answer is a redirect to `/` rather than a 500.
pub fn local_path(candidate: &str) -> Option<String> {
    let path = candidate.trim();

    if !path.starts_with('/')
        || path.starts_with("//")
        || path.starts_with("/\\")
        || path.chars().any(char::is_control)
        || path.chars().count() > 2048
    {
        return None;
    }

    Some(path.to_owned())
}

/// Where a `returnUrl` may actually send somebody: itself if it is on this
/// front end's own origin, a path on this front end if that is what it is, and
/// otherwise the front page.
///
/// Logout is the one route that takes a destination from the query string, and
/// a redirect that will forward to anywhere is a phishing primitive: the link
/// somebody clicks is genuinely this host, and it lands them on a copy of the
/// login page somewhere else.
pub fn safe_return_url(candidate: &str, public_url: &str) -> String {
    let root = format!("{}/", public_url.trim().trim_end_matches('/'));
    let Some(public) = web_url(&root) else {
        return root;
    };

    if let Some(path) = local_path(candidate) {
        return match public.join(&path) {
            Ok(joined) if joined.origin() == public.origin() => joined.to_string(),
            _ => root,
        };
    }

    match web_url(candidate) {
        Some(url) if url.origin() == public.origin() => url.to_string(),
        _ => root,
    }
}

fn web_url(value: &str) -> Option<Url> {
    let url = Url::parse(value.trim()).ok()?;

    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }

    match url.origin() {
        Origin::Tuple(..) => Some(url),
        Origin::Opaque(_) => None,
    }
}

/// What an ID token has to agree with before anybody is signed in.
#[derive(Debug, Clone)]
pub struct Expected<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub nonce: &'a str,
}

#[derive(Debug, Deserialize)]
struct RawIdToken {
    #[serde(default)]
    sub: String,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    aud: serde_json::Value,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    roles: serde_json::Value,
}

impl From<RawIdToken> for Person {
    fn from(raw: RawIdToken) -> Self {
        Self {
            subject: raw.sub,
            name: raw.name,
            preferred_username: raw.preferred_username,
            email: raw.email,
            roles: strings_in(&raw.roles),
        }
    }
}

/// A claim that is a string, or a list of them, or a list with things in it
/// that are neither. `aud` is a string or an array by specification; `roles` is
/// whatever the directory decided, and a role that is not a string is not a
/// role this page has any use for.
fn strings_in(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(one) => vec![one.clone()],
        serde_json::Value::Array(many) => many
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Checks an ID token and answers with what it says, or with which check it
/// failed.
///
/// The key comes from `kid`. When a token carries none and the provider
/// publishes exactly one key there is no ambiguity and that key is used; with
/// two published and no `kid` the token is refused rather than tried against
/// each in turn, because "it verified against one of them" is a weaker
/// statement than it looks.
pub fn validate_id_token(
    token: &str,
    keys: &JwkSet,
    expected: &Expected<'_>,
) -> Result<Person, IdTokenError> {
    let header = decode_header(token).map_err(|_| IdTokenError::Malformed)?;

    // Before a key is chosen, and deliberately so. The forgery this refuses is
    // a token that names a symmetric algorithm so that the provider's *public*
    // key -- which anybody can fetch -- is used as an HMAC secret.
    if header.alg != ID_TOKEN_ALGORITHM {
        return Err(IdTokenError::WrongAlgorithm(format!("{:?}", header.alg)));
    }

    let jwk = match header.kid.as_deref() {
        Some(kid) => keys
            .find(kid)
            .ok_or_else(|| IdTokenError::UnknownKey(kid.to_owned()))?,
        None => match keys.keys.as_slice() {
            [only] => only,
            _ => return Err(IdTokenError::AmbiguousKey),
        },
    };

    let AlgorithmParameters::RSA(rsa) = &jwk.algorithm else {
        return Err(IdTokenError::UnusableKey);
    };
    let key =
        DecodingKey::from_rsa_components(&rsa.n, &rsa.e).map_err(|_| IdTokenError::UnusableKey)?;

    let mut validation = Validation::new(ID_TOKEN_ALGORITHM);
    validation.leeway = LEEWAY;
    validation.validate_exp = true;
    validation.set_issuer(&[expected.issuer]);
    validation.set_audience(&[expected.audience]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

    let raw = decode::<RawIdToken>(token, &key, &validation)
        .map_err(|error| match error.kind() {
            ErrorKind::InvalidSignature => IdTokenError::BadSignature,
            ErrorKind::ExpiredSignature => IdTokenError::Expired,
            ErrorKind::InvalidIssuer => IdTokenError::WrongIssuer,
            ErrorKind::InvalidAudience => IdTokenError::WrongAudience,
            ErrorKind::InvalidAlgorithm => {
                IdTokenError::WrongAlgorithm(format!("{:?}", header.alg))
            }
            ErrorKind::MissingRequiredClaim(claim) => IdTokenError::MissingClaim(claim.clone()),
            _ => IdTokenError::Malformed,
        })?
        .claims;

    // OpenID Connect core 3.1.3.7, both halves of it. A token addressed to
    // several audiences has to say which of them asked for it (step 4), and an
    // `azp` that is there at all has to be us (step 5) -- whatever `aud` says.
    // The second half used to be conditional on the first, which left a token
    // carrying `aud: <us>` and `azp: <somebody else>` accepted: it is a token
    // the provider minted for another client and merely addressed to us, and
    // whether that is possible is the provider's business rather than a thing
    // to assume.
    let party = raw.azp.as_deref();
    if (party.is_some() || strings_in(&raw.aud).len() > 1) && party != Some(expected.audience) {
        return Err(IdTokenError::WrongParty);
    }

    let Some(nonce) = raw.nonce.as_deref() else {
        return Err(IdTokenError::MissingClaim("nonce".to_owned()));
    };
    if !same_secret(nonce, expected.nonce) {
        return Err(IdTokenError::WrongNonce);
    }

    let subject = clean_subject(&raw.sub)?;
    let mut person = Person::from(raw);
    person.subject = subject;

    Ok(person)
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

struct Cached<T> {
    value: T,
    fetched: Instant,
}

/// The provider, and the two documents of its own that this process keeps: the
/// discovery document that says where its doors are, and the key set its
/// signatures are checked against.
pub struct Provider {
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    scopes: String,
    http: reqwest::Client,
    discovery: RwLock<Option<Cached<Arc<Discovery>>>>,
    keys: RwLock<Option<Cached<Arc<JwkSet>>>>,
}

/// Written by hand for one reason: the client secret. `MEDIACORE_API_SECRET`
/// is kept out of a log the same way, and a stray `{provider:?}` in a `dbg!` or
/// a `tracing::debug` is exactly how a secret ends up in a log aggregator that
/// keeps it for a year.
impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl Provider {
    pub fn new(
        issuer: &str,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
        scopes: &str,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();

        Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            redirect_uri: redirect_uri.to_owned(),
            scopes: scopes.to_owned(),
            http,
            discovery: RwLock::new(None),
            keys: RwLock::new(None),
        }
    }

    /// The provider's own description of itself, cached for an hour.
    ///
    /// **A refetch that fails keeps the copy we have.** Endpoints move about
    /// once a decade; providers have bad minutes rather more often than that,
    /// and a stand that stops signing anybody in because a refresh timed out is
    /// a worse failure than one running an hour-old copy of a document that did
    /// not change.
    pub async fn discovery(&self) -> Result<Arc<Discovery>, ProviderError> {
        if let Some(cached) = self.discovery.read().await.as_ref()
            && cached.fetched.elapsed() < DISCOVERY_TTL
        {
            return Ok(Arc::clone(&cached.value));
        }

        match self.rediscover().await {
            Ok(found) => Ok(found),
            Err(error) => match self.discovery.read().await.as_ref() {
                Some(stale) => {
                    tracing::warn!(
                        error = %quoted(&error),
                        "the identity provider's configuration could not be refreshed, carrying \
                         on with the copy this process already has"
                    );

                    Ok(Arc::clone(&stale.value))
                }
                None => Err(error),
            },
        }
    }

    async fn rediscover(&self) -> Result<Arc<Discovery>, ProviderError> {
        let found: Discovery = self
            .fetch(
                &format!("{}{DISCOVERY_PATH}", self.issuer),
                None,
                "configuration",
            )
            .await?;

        // RFC 8414 section 3.3, and not a formality: without it, pointing this
        // at a URL that serves somebody else's document would have it accept
        // tokens issued by that somebody. It is also the check that catches a
        // trailing slash or an http/https mix-up in configuration, which is the
        // way this actually goes wrong.
        if found.issuer.trim_end_matches('/') != self.issuer {
            return Err(ProviderError::NotItself {
                configured: self.issuer.clone(),
                found: found.issuer,
            });
        }

        // Every door in the document, including the two that are optional.
        // Those two are the ones that most wanted checking: `end_session` is
        // put in a `Location` a browser follows, and `userinfo` is fetched with
        // an access token in the header. A document that describes one of its
        // own doors as `javascript:` or as `not a url` is either broken or
        // hostile, and neither is a document to take the other four values
        // from -- so one bad endpoint refuses the whole thing rather than being
        // quietly dropped.
        for (what, endpoint) in [
            (
                "authorization endpoint",
                Some(&found.authorization_endpoint),
            ),
            ("token endpoint", Some(&found.token_endpoint)),
            ("jwks uri", Some(&found.jwks_uri)),
            ("userinfo endpoint", found.userinfo_endpoint.as_ref()),
            ("end session endpoint", found.end_session_endpoint.as_ref()),
        ] {
            let Some(endpoint) = endpoint else {
                continue;
            };

            if web_url(endpoint).is_none() {
                return Err(ProviderError::NotAUrl {
                    what,
                    value: endpoint.clone(),
                });
            }
        }

        let found = Arc::new(found);
        *self.discovery.write().await = Some(Cached {
            value: Arc::clone(&found),
            fetched: Instant::now(),
        });

        Ok(found)
    }

    /// The provider's signing keys. `force` is what an unknown `kid` asks for,
    /// and it is rate limited: a rotation is worth one fetch, and a stream of
    /// tokens naming keys that never existed is worth none.
    async fn signing_keys(&self, force: bool) -> Result<Arc<JwkSet>, ProviderError> {
        if let Some(cached) = self.keys.read().await.as_ref() {
            let fresh = cached.fetched.elapsed() < JWKS_TTL;
            let too_soon = cached.fetched.elapsed() < JWKS_MIN_REFETCH;

            if (fresh && !force) || (force && too_soon) {
                return Ok(Arc::clone(&cached.value));
            }
        }

        let uri = self.discovery().await?.jwks_uri.clone();
        match self.fetch::<JwkSet>(&uri, None, "signing keys").await {
            Ok(found) => {
                let found = Arc::new(found);
                *self.keys.write().await = Some(Cached {
                    value: Arc::clone(&found),
                    fetched: Instant::now(),
                });

                Ok(found)
            }
            Err(error) => match self.keys.read().await.as_ref() {
                Some(stale) => {
                    tracing::warn!(
                        error = %quoted(&error),
                        "the identity provider's signing keys could not be refreshed, carrying on \
                         with the ones this process already has"
                    );

                    Ok(Arc::clone(&stale.value))
                }
                None => Err(error),
            },
        }
    }

    /// The URL a browser is sent to to sign in.
    pub async fn authorize(&self, state: &str, login: &Login) -> Result<String, ProviderError> {
        let discovery = self.discovery().await?;

        authorize_url(
            &discovery.authorization_endpoint,
            &Authorize {
                client_id: &self.client_id,
                redirect_uri: &self.redirect_uri,
                scopes: &self.scopes,
                state,
                nonce: &login.nonce,
                challenge: &pkce_challenge(&login.verifier),
            },
        )
    }

    /// The whole of the second half: redeem the code, check what comes back,
    /// and answer with somebody to open a session for.
    pub async fn complete(&self, code: &str, login: &Login) -> Result<Identity, ProviderError> {
        let tokens = self.exchange(code, &login.verifier).await?;
        let Some(id_token) = tokens.id_token else {
            return Err(ProviderError::NoIdToken);
        };

        let expected = Expected {
            issuer: &self.issuer,
            audience: &self.client_id,
            nonce: &login.nonce,
        };

        let keys = self.signing_keys(false).await?;
        let person = match validate_id_token(&id_token, &keys, &expected) {
            Err(IdTokenError::UnknownKey(kid)) => {
                tracing::info!(
                    kid = %quoted(&kid),
                    "the id token names a signing key this process has not seen"
                );
                let keys = self.signing_keys(true).await?;

                validate_id_token(&id_token, &keys, &expected)?
            }
            other => other?,
        };

        let person = match tokens.access_token.as_deref() {
            Some(access_token) => self.fill_in(person, access_token).await?,
            None => person,
        };

        Ok(Identity {
            display_name: person.display_name(),
            subject: person.subject,
            roles: person.roles,
            id_token,
        })
    }

    /// What the userinfo endpoint adds, if it can be reached.
    ///
    /// It is asked because an ID token that carries only `sub` is a shape
    /// plenty of providers ship, and a roster of UUIDs is not a room. It is not
    /// *required* to answer: an endpoint having a bad minute should not stop
    /// somebody signing in when the signed token already said who they are.
    ///
    /// What it may not do is disagree. OpenID Connect 5.3.2 requires the `sub`
    /// in a userinfo response to be compared with the one in the ID token, and
    /// a mismatch is the whole sign-in refused -- it means one of the two
    /// responses is about somebody else.
    async fn fill_in(&self, person: Person, access_token: &str) -> Result<Person, ProviderError> {
        let Some(endpoint) = self.discovery().await?.userinfo_endpoint.clone() else {
            return Ok(person);
        };

        let asked = self.fetch(&endpoint, Some(access_token), "user info").await;
        let fetched: RawIdToken = match asked {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!(
                    error = %quoted(&error),
                    "the provider would not say more about who signed in, going with what the id \
                     token carried"
                );

                return Ok(person);
            }
        };

        let found = Person::from(fetched);
        if found.subject.trim() != person.subject {
            return Err(ProviderError::DifferentPerson);
        }

        Ok(person.filled_in_from(found))
    }

    /// `client_secret_post`, form encoded, which is what RFC 6749 section 2.3.1
    /// describes and what the provider advertises. The secret is in the body
    /// and never in the URL: a URL is logged by every proxy on the way.
    async fn exchange(&self, code: &str, verifier: &str) -> Result<TokenResponse, ProviderError> {
        let endpoint = self.discovery().await?.token_endpoint.clone();
        let response = self
            .http
            .post(&endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("code_verifier", verifier),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(unreachable_provider)?;

        read(response, "token response").await
    }

    /// One `GET` at the provider. `as_whom` is the access token for the one
    /// endpoint that needs it -- the discovery document and the key set are
    /// public by specification and are asked for as nobody.
    async fn fetch<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        as_whom: Option<&str>,
        what: &'static str,
    ) -> Result<T, ProviderError> {
        let request = self.http.get(url);
        let request = match as_whom {
            Some(access_token) => request.bearer_auth(access_token),
            None => request,
        };

        let response = request.send().await.map_err(unreachable_provider)?;

        read(response, what).await
    }

    /// Where to send somebody who pressed Sign out, so that the provider ends
    /// its own session too. Without this, signing out here and pressing Join
    /// signs the same person straight back in without a prompt, which reads as
    /// the button not working.
    ///
    /// `None` when the provider publishes no `end_session_endpoint`, or cannot
    /// be reached to say -- the local session is already gone by then, and the
    /// caller sends them to the front page.
    pub async fn end_session(&self, id_token: &str, post_logout: &str) -> Option<String> {
        let endpoint = self.discovery().await.ok()?.end_session_endpoint.clone()?;
        let mut url = Url::parse(&endpoint).ok()?;

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("id_token_hint", id_token);
            query.append_pair("post_logout_redirect_uri", post_logout);
            query.append_pair("client_id", &self.client_id);
        }

        Some(url.to_string())
    }
}

/// Every way of not reaching the provider collapses to one answer on purpose.
/// A timeout, a refused connection, a certificate this host does not trust and
/// a DNS name that does not resolve are the same thing to whoever is looking at
/// the page; the distinction is in the log, where `error` carries the original.
fn unreachable_provider(error: reqwest::Error) -> ProviderError {
    tracing::warn!(error = %quoted(&error), "the identity provider could not be reached");

    ProviderError::Unreachable
}

async fn read<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    what: &'static str,
) -> Result<T, ProviderError> {
    let status = response.status();
    if !status.is_success() {
        // The body is not logged. A token endpoint that refuses answers with
        // an error document, and the request that produced it carried the
        // client secret; a provider that echoes any part of a request back is
        // one echo away from that secret being in this process's log.
        return Err(ProviderError::Refused {
            what,
            status: status.as_u16(),
        });
    }

    response.json().await.map_err(|error| {
        tracing::warn!(error = %quoted(&error), "the identity provider's {what} could not be read");

        ProviderError::Unreadable(what)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_everything_the_flow_needs() {
        let url = authorize_url(
            "https://sso.example.test/authorize",
            &Authorize {
                client_id: "a-client",
                redirect_uri: "https://rooms.example.test/auth/callback",
                scopes: DEFAULT_SCOPES,
                state: "a-state",
                nonce: "a-nonce",
                challenge: "a-challenge",
            },
        )
        .unwrap();

        let parsed = Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(
            parsed.origin(),
            Url::parse("https://sso.example.test").unwrap().origin()
        );
        assert_eq!(parsed.path(), "/authorize");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["response_mode"], "query");
        assert_eq!(query["client_id"], "a-client");
        assert_eq!(
            query["redirect_uri"],
            "https://rooms.example.test/auth/callback"
        );
        assert_eq!(query["scope"], DEFAULT_SCOPES);
        assert_eq!(query["state"], "a-state");
        assert_eq!(query["nonce"], "a-nonce");
        assert_eq!(query["code_challenge"], "a-challenge");
        assert_eq!(query["code_challenge_method"], "S256");
    }

    /// Anything but `code` is a flow where a token comes back through the
    /// browser, which is the flow this deliberately does not implement.
    #[test]
    fn nothing_but_the_code_flow_is_ever_asked_for() {
        assert_eq!(RESPONSE_TYPE, "code");
        assert_eq!(RESPONSE_MODE, "query");
        assert_eq!(CODE_CHALLENGE_METHOD, "S256");
        assert!(
            !DEFAULT_SCOPES.contains("offline_access"),
            "a refresh token is asked for that nothing here ever spends"
        );
        assert!(DEFAULT_SCOPES.starts_with("openid"));
    }

    /// A `&` or a `?` in any of these has to come out as one parameter. The
    /// redirect URI is the one that really carries a query in the wild.
    #[test]
    fn a_parameter_with_punctuation_in_it_stays_one_parameter() {
        let url = authorize_url(
            "https://sso.example.test/authorize?tenant=one",
            &Authorize {
                client_id: "a&b",
                redirect_uri: "https://rooms.example.test/auth/callback?next=/r/a b",
                scopes: "openid profile",
                state: "a-state",
                nonce: "a-nonce",
                challenge: "a-challenge",
            },
        )
        .unwrap();

        let parsed = Url::parse(&url).unwrap();
        let query: Vec<_> = parsed.query_pairs().into_owned().collect();

        assert!(query.contains(&("client_id".to_owned(), "a&b".to_owned())));
        assert!(query.contains(&(
            "redirect_uri".to_owned(),
            "https://rooms.example.test/auth/callback?next=/r/a b".to_owned()
        )));
        assert!(
            query.contains(&("tenant".to_owned(), "one".to_owned())),
            "the endpoint's own query was thrown away: {url}"
        );
        assert_eq!(
            query.iter().filter(|(name, _)| name == "scope").count(),
            1,
            "a parameter was split in two: {url}"
        );
    }

    #[test]
    fn an_authorization_endpoint_that_is_not_a_url_is_refused() {
        assert!(matches!(
            authorize_url(
                "not a url",
                &Authorize {
                    client_id: "a-client",
                    redirect_uri: "https://rooms.example.test/auth/callback",
                    scopes: DEFAULT_SCOPES,
                    state: "a-state",
                    nonce: "a-nonce",
                    challenge: "a-challenge",
                }
            ),
            Err(ProviderError::NotAUrl { .. })
        ));
    }

    /// RFC 7636 appendix B, verbatim. It is here because a challenge that is
    /// nearly right fails at the provider with an error that says nothing.
    #[test]
    fn the_pkce_challenge_is_the_one_in_the_rfc() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn the_challenge_is_not_the_verifier() {
        let verifier = "a-verifier-nobody-else-holds-abcdefghijklmnop";
        let challenge = pkce_challenge(verifier);

        assert!(
            !challenge.contains(verifier),
            "the redirect would carry the verifier, which is the one thing pkce keeps back"
        );
        assert!(
            !challenge.contains('=') && !challenge.contains('+') && !challenge.contains('/'),
            "the challenge is not base64url, so it would need escaping in the redirect: {challenge}"
        );
        assert_ne!(challenge, pkce_challenge("a-different-verifier"));
    }

    #[test]
    fn a_path_on_this_server_is_the_only_thing_that_comes_back_as_one() {
        for path in ["/", "/r/standup", "/r/standup?video=1", "/r/a.b-c_d"] {
            assert_eq!(
                local_path(path).as_deref(),
                Some(path),
                "`{path}` was refused"
            );
        }

        for elsewhere in [
            "//evil.example",
            "//evil.example/r/standup",
            "/\\evil.example",
            "https://evil.example/r/standup",
            "http://evil.example",
            "r/standup",
            "",
            "javascript:alert(1)",
            "/r/standup\r\nSet-Cookie: a=b",
        ] {
            assert_eq!(
                local_path(elsewhere),
                None,
                "`{elsewhere}` was accepted as a path on this server, so a redirect built from it \
                 leaves this origin"
            );
        }
    }

    #[test]
    fn a_return_url_that_is_not_ours_becomes_the_front_page() {
        let public = "https://rooms.example.test";

        assert_eq!(
            safe_return_url("/r/standup", public),
            "https://rooms.example.test/r/standup"
        );
        assert_eq!(
            safe_return_url("https://rooms.example.test/r/standup", public),
            "https://rooms.example.test/r/standup"
        );

        for elsewhere in [
            "https://evil.example/",
            "http://rooms.example.test/r/standup",
            "https://rooms.example.test.evil.example/",
            "https://rooms.example.test:8443/",
            "//evil.example",
            "javascript:alert(1)",
            "",
        ] {
            assert_eq!(
                safe_return_url(elsewhere, public),
                "https://rooms.example.test/",
                "`{elsewhere}` was honoured as somewhere to send a person after signing out"
            );
        }
    }

    #[test]
    fn a_trailing_slash_on_the_public_url_changes_nothing() {
        for public in [
            "https://rooms.example.test",
            "https://rooms.example.test/",
            " https://rooms.example.test/ ",
        ] {
            assert_eq!(
                safe_return_url("/r/standup", public),
                "https://rooms.example.test/r/standup"
            );
        }
    }

    #[test]
    fn the_name_shown_is_the_first_of_the_three_the_provider_gave() {
        let full = Person {
            subject: "a-sub".to_owned(),
            name: Some("Роман Морозов".to_owned()),
            preferred_username: Some("roman".to_owned()),
            email: Some("roman@example.test".to_owned()),
            roles: vec![],
        };
        assert_eq!(full.display_name(), "Роман Морозов");

        let no_name = Person {
            name: None,
            ..full.clone()
        };
        assert_eq!(no_name.display_name(), "roman");

        let only_email = Person {
            name: None,
            preferred_username: None,
            ..full.clone()
        };
        assert_eq!(only_email.display_name(), "roman@example.test");

        let nothing = Person {
            subject: "a-sub".to_owned(),
            ..Person::default()
        };
        assert_eq!(
            nothing.display_name(),
            "a-sub",
            "a roster entry with no text at all is worse than an ugly one"
        );
    }

    /// A directory is not a source of short printable strings, and whatever it
    /// returns goes into a token and onto everybody's screen.
    #[test]
    fn a_name_the_directory_returned_is_cleaned_like_a_typed_one() {
        let long = Person {
            subject: "a-sub".to_owned(),
            name: Some("Ро".repeat(60)),
            ..Person::default()
        };
        assert_eq!(long.display_name().chars().count(), MAX_NAME_CHARS);

        let ragged = Person {
            subject: "a-sub".to_owned(),
            name: Some("  Роман  ".to_owned()),
            ..Person::default()
        };
        assert_eq!(ragged.display_name(), "Роман");

        let control = Person {
            subject: "a-sub".to_owned(),
            name: Some("Ро\u{7}ман".to_owned()),
            preferred_username: Some("roman".to_owned()),
            ..Person::default()
        };
        assert_eq!(
            control.display_name(),
            "roman",
            "a name with a control character in it was shown rather than passed over"
        );

        let blank = Person {
            subject: "a-sub".to_owned(),
            name: Some("   ".to_owned()),
            email: Some("roman@example.test".to_owned()),
            ..Person::default()
        };
        assert_eq!(blank.display_name(), "roman@example.test");
    }

    #[test]
    fn a_subject_that_could_not_be_an_identity_is_refused() {
        assert_eq!(clean_subject(" a-sub ").unwrap(), "a-sub");
        assert_eq!(
            clean_subject("3f6b1c88-0000-4000-8000-abcdefabcdef").unwrap(),
            "3f6b1c88-0000-4000-8000-abcdefabcdef"
        );

        for bad in ["", "   ", "a\u{0}sub", "a\nsub"] {
            assert!(
                matches!(clean_subject(bad), Err(IdTokenError::UnusableSubject)),
                "`{bad}` was accepted as somebody's identity"
            );
        }
        assert!(clean_subject(&"a".repeat(MAX_SUBJECT_CHARS + 1)).is_err());
    }

    #[test]
    fn a_claim_is_a_string_or_a_list_of_them_and_nothing_else() {
        assert_eq!(strings_in(&serde_json::json!("one")), vec!["one"]);
        assert_eq!(
            strings_in(&serde_json::json!(["one", "two"])),
            vec!["one", "two"]
        );
        assert_eq!(
            strings_in(&serde_json::json!(["one", 2, null, { "three": true }])),
            vec!["one"],
            "a role that is not a string was carried into the session as one"
        );
        assert!(strings_in(&serde_json::json!(null)).is_empty());
        assert!(strings_in(&serde_json::json!(7)).is_empty());
    }

    #[test]
    fn the_id_token_the_provider_signs_with_is_the_one_we_check_against() {
        assert_eq!(ID_TOKEN_ALGORITHM, Algorithm::RS256);
    }
}
