//! Signing in, against a provider that only exists inside this test.
//!
//! Nothing here reaches the network. The "identity provider" is an axum router
//! on a loopback port that serves a discovery document, a JWKS, a token
//! endpoint and a userinfo endpoint, and the test drives the whole round trip
//! the way a browser would: follow the `302`, read `state` and `nonce` out of
//! the URL the browser was sent to, carry the cookies back, and see where it
//! lands.
//!
//! That shape is what makes the interesting failures reachable. Half of what
//! this file is for is the ID token, and the way to be sure a check is being
//! made is to break exactly that check and watch the sign-in be refused --
//! which is what most of the first half does, one check at a time.
//!
//! The RSA key below is a throwaway generated for this file and is a test
//! fixture in the same sense a sample WAV is. Nothing else is: no client
//! secret, no session cookie and no token from any real provider appears here,
//! and `nothing_of_the_client_secret_escapes` is the guard that says so.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mediacore_front::config::{Config, OidcConfig, TOKEN_TTL};
use mediacore_front::http;
use mediacore_front::oidc::{Expected, IdTokenError, pkce_challenge, validate_id_token};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";
const ROOM: &str = "demo";
const WS_URL: &str = "wss://media.example.test";
const CLIENT_ID: &str = "8b704c73-0000-4000-8000-000000000000";

/// Not a secret and not anybody's: a string this test invents so that it can
/// check it never comes out anywhere.
const CLIENT_SECRET: &str = "a-client-secret-only-this-test-knows";

const KID: &str = "test-key-1";
const SUBJECT: &str = "3f6b1c88-0000-4000-8000-abcdefabcdef";

/// A 2048-bit RSA key generated for this file, PKCS#1 DER, base64. It signs the
/// ID tokens the fake provider hands out and its public half is what the fake
/// JWKS publishes.
const PRIVATE_KEY_PKCS1_DER: &str = "MIIEpAIBAAKCAQEA0dl/NCPMEndGMIWJiPJQjiYM6lA8/D7QnGnktPMTFADXzn6OLO6jO9D78PPb+u4P0P4l83uqNArEmFdrzIphCF/DXslzyaW4NaQiv2LM/WLrrbwnSQrAvBLgFQDZBbqP0Tk8BJwDH95dL8o2p2z05bBeZJ3twMXhxvbyzL8Pkh1RkaAABUDhNczjG8U1hDBqQphAnGt6A00GDusxe+bGe8GhR7ChZ1gdHycheoVfV7Dt0e4OTPYnQpdryW723izEf0Q17ESF0yH0h8UoZRr35cSSIhR0JCDvn4gQYZ1cmlb9A30mm6fEQSOkwM7PPNyioeB1VBzjJowF4mksSq5UcQIDAQABAoIBAGcbj8XFpPqZwlIK+vXf4bbGeWgTvrNOBeAko6xJFuGoPGEIi8EGDAxS9ceBel/D4t+UUH9fDtT9SXV01tGsWFrt+qezIgI9O9KXcD9LQ19o4FX/8emQr/x45cNMJFYNQabSkWvOYDajYnQdzlbbUUUeMHMNqgki8Xl6UvhJFFR80UReNA/7uQNY8piIdckRd1hDcieiw/gThFJZPodDdR0gyqaQkuXXvQdtlM9FqIqBGG/I3sc9dfAD8ASMVDaQA7u9StEwDZ5+BapdWs7rdwatR22QZ/7pcrg3GTGVRxEHdtzDf9rRHlU96NlVHRXJokOiMrHLoC3RYx5ZY8IfNQUCgYEA9IAJDQhFtEw8uU4z1om3k2tixz8h1h7hkkmhqOUw0JJ2qPJpCGVDGDRGfPlUfbFWRHZdlhkpMihEUvUi6LBUNqSMK0K53KUVqqgrZtIOWuQX9Idjo7NTXvpMo2uSZVea3J+eaz83gnvQ96+bVQUXsNRUiLw5NFPvcaqn7AIhs68CgYEA27g+OyIwbMxrI+qHjNaPQU9gMcP2nu7EUJvMn4VGwpK1nzXHnSeeBAaYMyFFC1ayiLk+1XSpzQPLjFLuT2TyLMEWc8If0EQYrGtayEYtjQSQ0D1Ndz0HVUbsBmx4n7ZPBU1slHeEQhHthtNRbhAf5JJnAmqSTF2XE/o3pIF64d8CgYEA3Wz2xW6wcdB5TficybZ/ImsmUfvC0P+2mZ1FYoft5XY/MFIY/Rl34fu1uoizFAH+dpIbv0uaG2qMNfT7YHAQVExBBQyw9jornRof9EZX6ciYSUQz4JUO8/0KWxWYx17HSPblqYre4rTvUuEec6jhA6vHwu1zgQhi2TYMTZjAnNECgYEA1k1InUKGeseSxHAuj4aCKRAzYbahp3h4j6q2FOzYDFUtwkChFj4a2BBaiJwtjJaeAc60Y1urdiP6dz4348YwksgtjvBLOUYBzsHSxL32FbHbUNBJ6z/iFCHdeZJ4wEX/ostWWSSNIif8RWF87d3aTYUh5XBuiv9JqJmHhfMoGH8CgYByaEG6wYeNv16/eQR07VkgJ55m/pUKcEwxIiU24rQSqC6ZOkSzeuJk86xl/a21LNS17pF3/tgLbMOseZ9aG7BqShV2Bc/zY9gxFrW+USpRypc7LmRmHOxVAhfUjK5P+Ywd5vmN4mspXcRvuCTGNOMaWh0HR8j/DeOPuUknRWYTaw==";

/// The modulus of the key above, base64url, as a JWKS carries it.
const MODULUS: &str = "0dl_NCPMEndGMIWJiPJQjiYM6lA8_D7QnGnktPMTFADXzn6OLO6jO9D78PPb-u4P0P4l83uqNArEmFdrzIphCF_DXslzyaW4NaQiv2LM_WLrrbwnSQrAvBLgFQDZBbqP0Tk8BJwDH95dL8o2p2z05bBeZJ3twMXhxvbyzL8Pkh1RkaAABUDhNczjG8U1hDBqQphAnGt6A00GDusxe-bGe8GhR7ChZ1gdHycheoVfV7Dt0e4OTPYnQpdryW723izEf0Q17ESF0yH0h8UoZRr35cSSIhR0JCDvn4gQYZ1cmlb9A30mm6fEQSOkwM7PPNyioeB1VBzjJowF4mksSq5UcQ";

// ----------------------------------------------------------------- fixtures

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
}

fn signing_key() -> EncodingKey {
    EncodingKey::from_rsa_der(&STANDARD.decode(PRIVATE_KEY_PKCS1_DER).expect("base64 der"))
}

fn published_keys(kid: &str) -> JwkSet {
    serde_json::from_value(json!({
        "keys": [{ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": MODULUS, "e": "AQAB" }]
    }))
    .expect("a jwks")
}

fn signed(claims: &Value, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_owned);

    encode(&header, claims, &signing_key()).expect("a signed id token")
}

/// An ID token with every claim right, for an issuer, audience and nonce the
/// caller names. Each test below spoils exactly one of them.
fn claims(issuer: &str, nonce: &str) -> Value {
    json!({
        "iss": issuer,
        "sub": SUBJECT,
        "aud": CLIENT_ID,
        "exp": unix_now() + 300,
        "iat": unix_now(),
        "nonce": nonce,
        "name": "Роман",
        "email": "roman@example.test",
        "roles": ["staff", "meetings"],
    })
}

fn expected<'a>(issuer: &'a str, nonce: &'a str) -> Expected<'a> {
    Expected {
        issuer,
        audience: CLIENT_ID,
        nonce,
    }
}

// ------------------------------------------------------- the id token itself

#[tokio::test]
async fn an_id_token_with_everything_right_says_who_signed_in() {
    let token = signed(&claims("https://sso.example.test", "a-nonce"), Some(KID));
    let person = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect("a token with every claim right");

    assert_eq!(person.subject, SUBJECT);
    assert_eq!(person.display_name(), "Роман");
    assert_eq!(
        person.roles,
        vec!["staff".to_owned(), "meetings".to_owned()]
    );
}

/// The forgery this refuses is the oldest one there is: name a symmetric
/// algorithm, and get a validator to check the provider's *public* key --
/// which anybody can fetch from the JWKS -- against the token as if it were an
/// HMAC secret. Anybody who can read the JWKS can then mint any identity they
/// like.
#[tokio::test]
async fn an_id_token_that_names_a_symmetric_algorithm_is_refused() {
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(KID.to_owned());
    let token = encode(
        &header,
        &claims("https://sso.example.test", "a-nonce"),
        &EncodingKey::from_secret(MODULUS.as_bytes()),
    )
    .expect("an hmac token");

    let refused = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token signed with the provider's own public key as an hmac secret");

    assert!(
        matches!(refused, IdTokenError::WrongAlgorithm(_)),
        "the algorithm the token named was taken as an instruction, got {refused}"
    );
}

/// `alg: none` is the other half of the same idea: a token with no signature
/// at all, and a validator that reads `alg` and does what it says.
#[tokio::test]
async fn an_id_token_with_no_signature_is_refused() {
    let header = URL_SAFE_NO_PAD.encode(json!({ "alg": "none", "typ": "JWT" }).to_string());
    let payload = URL_SAFE_NO_PAD.encode(claims("https://sso.example.test", "a-nonce").to_string());
    let token = format!("{header}.{payload}.");

    assert!(
        validate_id_token(
            &token,
            &published_keys(KID),
            &expected("https://sso.example.test", "a-nonce")
        )
        .is_err(),
        "an unsigned token was accepted as a statement about who somebody is"
    );
}

/// Everything about this token is right except the bytes at the end, which
/// belong to another one signed by the same key. Nothing but actually checking
/// the signature catches it.
#[tokio::test]
async fn an_id_token_whose_signature_belongs_to_another_token_is_refused() {
    let mine = signed(&claims("https://sso.example.test", "a-nonce"), Some(KID));
    let theirs = signed(
        &json!({
            "iss": "https://sso.example.test",
            "sub": "somebody-else",
            "aud": CLIENT_ID,
            "exp": unix_now() + 300,
            "nonce": "a-nonce",
        }),
        Some(KID),
    );

    let body = mine.rsplit_once('.').expect("a jwt").0;
    let signature = theirs.rsplit_once('.').expect("a jwt").1;
    let forged = format!("{body}.{signature}");

    let refused = validate_id_token(
        &forged,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token carrying somebody else's signature");

    assert!(
        matches!(refused, IdTokenError::BadSignature),
        "the signature was not checked, got {refused}"
    );
}

#[tokio::test]
async fn an_id_token_that_has_expired_is_refused() {
    let mut claims = claims("https://sso.example.test", "a-nonce");
    claims["exp"] = json!(unix_now() - 3600);
    let token = signed(&claims, Some(KID));

    let refused = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("an hour-old token");

    assert!(
        matches!(refused, IdTokenError::Expired),
        "`exp` was not checked, got {refused}"
    );
}

/// A token signed by a provider this deployment was never pointed at. Without
/// the `iss` check, any provider whose key ends up in the set is one that can
/// sign anybody in here.
#[tokio::test]
async fn an_id_token_from_another_provider_is_refused() {
    let token = signed(&claims("https://sso.elsewhere.test", "a-nonce"), Some(KID));

    let refused = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token from another issuer");

    assert!(
        matches!(refused, IdTokenError::WrongIssuer),
        "`iss` was not checked, got {refused}"
    );
}

/// One provider serves many clients. A token minted for the ticketing system
/// on the same provider is a genuine, correctly signed, unexpired token, and it
/// is not a statement to this client about anything.
#[tokio::test]
async fn an_id_token_for_another_client_is_refused() {
    let mut claims = claims("https://sso.example.test", "a-nonce");
    claims["aud"] = json!("some-other-client");
    let token = signed(&claims, Some(KID));

    let refused = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token for another client");

    assert!(
        matches!(refused, IdTokenError::WrongAudience),
        "`aud` was not checked, got {refused}"
    );
}

/// With several audiences the token has to name the one that asked for it.
/// OpenID Connect core 3.1.3.7.
#[tokio::test]
async fn an_id_token_for_several_audiences_has_to_say_which_one_asked() {
    let mut shared = claims("https://sso.example.test", "a-nonce");
    shared["aud"] = json!([CLIENT_ID, "the-ticketing-system"]);

    let refused = validate_id_token(
        &signed(&shared, Some(KID)),
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token for two clients naming neither as the party");
    assert!(
        matches!(refused, IdTokenError::WrongParty),
        "`azp` was not checked on a multi-audience token, got {refused}"
    );

    shared["azp"] = json!("the-ticketing-system");
    assert!(
        validate_id_token(
            &signed(&shared, Some(KID)),
            &published_keys(KID),
            &expected("https://sso.example.test", "a-nonce")
        )
        .is_err(),
        "a token that says another client asked for it was accepted here"
    );

    shared["azp"] = json!(CLIENT_ID);
    assert!(
        validate_id_token(
            &signed(&shared, Some(KID)),
            &published_keys(KID),
            &expected("https://sso.example.test", "a-nonce")
        )
        .is_ok(),
        "a token that names this client as the party was refused"
    );
}

/// The nonce is what ties a token to *this* sign-in. Without it, a token
/// captured from another sign-in -- the same person, the same client, entirely
/// valid -- can be replayed into a login this process started.
#[tokio::test]
async fn an_id_token_answering_a_different_sign_in_is_refused() {
    let token = signed(
        &claims("https://sso.example.test", "the-nonce-of-another-login"),
        Some(KID),
    );

    let refused = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "the-nonce-this-login-issued"),
    )
    .expect_err("a token minted for another sign-in");

    assert!(
        matches!(refused, IdTokenError::WrongNonce),
        "the nonce was not checked, got {refused}"
    );
}

#[tokio::test]
async fn an_id_token_with_no_nonce_at_all_is_refused() {
    let mut claims = claims("https://sso.example.test", "a-nonce");
    claims.as_object_mut().expect("an object").remove("nonce");

    let refused = validate_id_token(
        &signed(&claims, Some(KID)),
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token with no nonce");

    assert!(
        matches!(refused, IdTokenError::MissingClaim(ref claim) if claim == "nonce"),
        "a token with no nonce at all passed the nonce check, got {refused}"
    );
}

#[tokio::test]
async fn an_id_token_naming_a_key_the_provider_does_not_publish_is_refused() {
    let token = signed(
        &claims("https://sso.example.test", "a-nonce"),
        Some("a-key-that-was-never-published"),
    );

    let refused = validate_id_token(
        &token,
        &published_keys(KID),
        &expected("https://sso.example.test", "a-nonce"),
    )
    .expect_err("a token naming an unknown key");

    assert!(
        matches!(refused, IdTokenError::UnknownKey(ref kid) if kid == "a-key-that-was-never-published"),
        "got {refused}"
    );
}

/// A token with no `kid` is unambiguous when the provider publishes one key,
/// and is not when it publishes two. Trying each in turn would turn "it
/// verified against something" into "it verified".
#[tokio::test]
async fn a_token_with_no_key_named_is_only_taken_when_there_is_no_choice() {
    let token = signed(&claims("https://sso.example.test", "a-nonce"), None);

    assert!(
        validate_id_token(
            &token,
            &published_keys(KID),
            &expected("https://sso.example.test", "a-nonce")
        )
        .is_ok(),
        "a token with no kid was refused against a provider publishing exactly one key"
    );

    let mut two: JwkSet = published_keys(KID);
    let mut second = two.keys[0].clone();
    second.common.key_id = Some("a-second-key".to_owned());
    two.keys.push(second);

    assert!(
        matches!(
            validate_id_token(
                &token,
                &two,
                &expected("https://sso.example.test", "a-nonce")
            ),
            Err(IdTokenError::AmbiguousKey)
        ),
        "a token with no kid was tried against a set of keys"
    );
}

#[tokio::test]
async fn an_id_token_that_names_nobody_is_refused() {
    let mut claims = claims("https://sso.example.test", "a-nonce");
    claims["sub"] = json!("");

    assert!(
        validate_id_token(
            &signed(&claims, Some(KID)),
            &published_keys(KID),
            &expected("https://sso.example.test", "a-nonce")
        )
        .is_err(),
        "a token with an empty subject was accepted as somebody's identity"
    );
}

// ------------------------------------------------------- a fake provider

#[derive(Default)]
struct Staged {
    id_token: Option<String>,
    userinfo: Option<Value>,
    token_form: Option<String>,
    token_status: Option<u16>,
    issuer: Option<String>,
    jwks_kid: Option<String>,
    jwks_fetches: usize,
    ended_sessions: Vec<String>,
}

struct Fake {
    addr: SocketAddr,
    staged: Mutex<Staged>,
}

impl Fake {
    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn stage_id_token(&self, token: String) {
        self.staged.lock().unwrap().id_token = Some(token);
    }

    fn token_form(&self) -> HashMap<String, String> {
        let form = self
            .staged
            .lock()
            .unwrap()
            .token_form
            .clone()
            .expect("the front end asked the token endpoint for something");

        url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect()
    }
}

async fn fake_provider() -> Arc<Fake> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake = Arc::new(Fake {
        addr: listener.local_addr().unwrap(),
        staged: Mutex::new(Staged::default()),
    });

    let router = Router::new()
        .route("/.well-known/openid-configuration", get(fake_discovery))
        .route("/jwks", get(fake_jwks))
        .route("/token", post(fake_token))
        .route("/user_info", get(fake_user_info))
        .route("/end_session", get(fake_end_session))
        .with_state(Arc::clone(&fake));

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    fake
}

async fn fake_discovery(State(fake): State<Arc<Fake>>) -> Json<Value> {
    let base = fake.base();
    let issuer = fake
        .staged
        .lock()
        .unwrap()
        .issuer
        .clone()
        .unwrap_or_else(|| base.clone());

    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "userinfo_endpoint": format!("{base}/user_info"),
        "jwks_uri": format!("{base}/jwks"),
        "end_session_endpoint": format!("{base}/end_session"),
    }))
}

async fn fake_jwks(State(fake): State<Arc<Fake>>) -> Json<Value> {
    let kid = {
        let mut staged = fake.staged.lock().unwrap();
        staged.jwks_fetches += 1;
        staged.jwks_kid.clone().unwrap_or_else(|| KID.to_owned())
    };

    Json(json!({
        "keys": [{ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": MODULUS, "e": "AQAB" }]
    }))
}

async fn fake_token(State(fake): State<Arc<Fake>>, body: String) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let (id_token, status) = {
        let mut staged = fake.staged.lock().unwrap();
        staged.token_form = Some(body);
        (staged.id_token.clone(), staged.token_status)
    };

    if let Some(status) = status {
        return (
            axum::http::StatusCode::from_u16(status).unwrap(),
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
    }

    Json(json!({
        "access_token": "an-access-token",
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
    }))
    .into_response()
}

async fn fake_user_info(State(fake): State<Arc<Fake>>) -> Json<Value> {
    let staged = fake.staged.lock().unwrap().userinfo.clone();

    Json(staged.unwrap_or_else(|| {
        json!({ "sub": SUBJECT, "name": "Роман", "email": "roman@example.test", "roles": ["staff"] })
    }))
}

async fn fake_end_session(State(fake): State<Arc<Fake>>, uri: axum::http::Uri) -> &'static str {
    fake.staged
        .lock()
        .unwrap()
        .ended_sessions
        .push(uri.to_string());

    "signed out"
}

// -------------------------------------------------------- the front end

fn config(oidc: Option<OidcConfig>) -> Config {
    Config {
        api_key: API_KEY.to_owned(),
        api_secret: API_SECRET.to_owned(),
        ws_url: WS_URL.to_owned(),
        room: ROOM.to_owned(),
        port: 0,
        token_ttl: TOKEN_TTL,
        oidc,
    }
}

fn sign_in_with(issuer: &str, public_url: &str) -> OidcConfig {
    OidcConfig {
        issuer: issuer.trim_end_matches('/').to_owned(),
        client_id: CLIENT_ID.to_owned(),
        client_secret: CLIENT_SECRET.to_owned(),
        public_url: public_url.trim_end_matches('/').to_owned(),
        scopes: "openid profile email".to_owned(),
        session_ttl: Duration::from_secs(3600),
    }
}

/// A front end signing in against `fake`, with its own public URL pointed at
/// the port it is about to listen on -- which is what makes the redirect URI it
/// sends the provider the one a browser would actually come back to.
async fn front_signing_in_against(issuer: &str) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(config(Some(sign_in_with(
        issuer,
        &format!("http://{addr}"),
    ))));

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    addr
}

async fn front_without_sign_in() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(config(None));

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    addr
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    }

    /// Every `name=value` a `Set-Cookie` carried, without its attributes.
    fn cookies(&self) -> Vec<String> {
        self.head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| {
                value
                    .trim()
                    .split_once(';')
                    .map_or(value.trim(), |(pair, _)| pair)
                    .to_owned()
            })
            .collect()
    }

    /// The cookies worth carrying on to the next request: the ones with
    /// something in them.
    fn live_cookies(&self) -> Vec<String> {
        self.cookies()
            .into_iter()
            .filter(|cookie| !cookie.ends_with('='))
            .collect()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|_| panic!("a json body, got `{}`", self.body))
    }
}

async fn request(addr: SocketAddr, head: &str, extra: &[(&str, &str)], body: &str) -> Reply {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let mut request = format!("{head} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
        for (name, value) in extra {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        socket.write_all(request.as_bytes()).await.expect("request");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        String::from_utf8_lossy(&raw).into_owned()
    };

    let response = tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("http response did not arrive in time");

    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("an http response with a body");
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("a status line")
        .parse()
        .expect("a numeric status");

    Reply {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

async fn visit(addr: SocketAddr, path: &str) -> Reply {
    request(addr, &format!("GET {path}"), &[], "").await
}

async fn visit_with(addr: SocketAddr, path: &str, cookies: &[String]) -> Reply {
    let jar = cookies.join("; ");
    request(addr, &format!("GET {path}"), &[("Cookie", &jar)], "").await
}

async fn post_with(addr: SocketAddr, path: &str, cookies: &[String], body: &str) -> Reply {
    let jar = cookies.join("; ");
    request(addr, &format!("POST {path}"), &[("Cookie", &jar)], body).await
}

/// The whole round trip a browser makes, and what it collects on the way.
struct SignedIn {
    cookies: Vec<String>,
    landed_at: String,
    authorize: Url,
}

/// Signs somebody in the way a browser would: ask for a page, follow the
/// redirect to the provider, mint the ID token the provider would have minted
/// for that `nonce`, and come back to the callback carrying the login cookie.
async fn sign_in(front: SocketAddr, fake: &Arc<Fake>, going_to: &str) -> SignedIn {
    let sent_away = visit(front, going_to).await;
    assert_eq!(
        sent_away.status, 302,
        "a page asked for with no session was not answered with a way to sign in"
    );

    let authorize = Url::parse(&sent_away.header("location").expect("a location")).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    let cookies = sent_away.live_cookies();

    fake.stage_id_token(signed(&claims(&fake.base(), &query["nonce"]), Some(KID)));

    let back = visit_with(
        front,
        &format!(
            "/auth/callback?code=an-authorization-code&state={}",
            query["state"]
        ),
        &cookies,
    )
    .await;
    assert_eq!(
        back.status, 302,
        "the callback did not send the browser onwards: {}",
        back.body
    );

    SignedIn {
        cookies: back.live_cookies(),
        landed_at: back.header("location").expect("a location"),
        authorize,
    }
}

// ------------------------------------------------------------ the round trip

#[tokio::test]
async fn a_page_asked_for_with_no_session_is_sent_to_the_provider() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let answer = visit(front, "/r/standup").await;
    assert_eq!(answer.status, 302);

    let sent_to = Url::parse(&answer.header("location").expect("a location")).unwrap();
    let query: HashMap<_, _> = sent_to.query_pairs().into_owned().collect();

    assert_eq!(sent_to.path(), "/authorize");
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["response_mode"], "query");
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(
        query.contains_key("state"),
        "no state, so nothing ties the callback to this sign-in"
    );
    assert!(
        query.contains_key("nonce"),
        "no nonce, so a token from another sign-in would do"
    );
    assert_eq!(
        query["redirect_uri"],
        format!("http://{front}/auth/callback"),
        "the provider is told to come back somewhere this server does not answer"
    );

    let cookies = answer.cookies();
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("__Host-mediacore_login_")),
        "the browser was sent away with nothing to prove the callback comes back to it: {cookies:?}"
    );

    assert!(
        !answer.head.contains(CLIENT_SECRET),
        "the client secret went out with the redirect"
    );
}

/// The whole point of keeping the room on this side: a link to `/r/standup`
/// comes back to `/r/standup`, and the address bar carries nothing of the
/// sign-in.
#[tokio::test]
async fn the_room_somebody_asked_for_survives_the_round_trip() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let signed_in = sign_in(front, &fake, "/r/standup?video=1").await;

    assert_eq!(
        signed_in.landed_at, "/r/standup?video=1",
        "the room, or the camera this visit asked for, did not survive signing in"
    );
    for leftover in ["code=", "state=", "session", "token"] {
        assert!(
            !signed_in.landed_at.contains(leftover),
            "`{leftover}` was left in the address bar: {}",
            signed_in.landed_at
        );
    }
    assert!(
        signed_in
            .cookies
            .iter()
            .any(|cookie| cookie.starts_with("__Host-mediacore_session=")),
        "nobody was signed in: {:?}",
        signed_in.cookies
    );

    let page = visit_with(front, "/r/standup", &signed_in.cookies).await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains(r#"id="join-form""#));
}

/// The room lives in a map keyed by `state` and nowhere else. If it were in the
/// redirect or in a cookie, this is the request that would carry it.
#[tokio::test]
async fn where_somebody_was_going_is_never_in_the_browsers_hands() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let answer = visit(front, "/r/standup").await;
    let sent_to = answer.header("location").expect("a location");

    assert!(
        !sent_to.contains("standup"),
        "the room rode out to the provider in the redirect: {sent_to}"
    );
    assert!(
        !answer
            .head
            .to_lowercase()
            .replace("set-cookie:", "")
            .contains("standup"),
        "the room was written into a cookie the browser can edit: {}",
        answer.head
    );
}

#[tokio::test]
async fn the_code_is_redeemed_with_the_verifier_and_not_with_the_challenge() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let signed_in = sign_in(front, &fake, "/").await;
    let query: HashMap<_, _> = signed_in.authorize.query_pairs().into_owned().collect();
    let form = fake.token_form();

    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "an-authorization-code");
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(
        form["redirect_uri"],
        format!("http://{front}/auth/callback"),
        "the redirect uri the code is redeemed with has to be the one it was issued for"
    );

    let verifier = &form["code_verifier"];
    assert_eq!(
        pkce_challenge(verifier),
        query["code_challenge"],
        "the challenge in the redirect is not the hash of the verifier the code was redeemed \
         with, so pkce is decoration"
    );
    assert!(
        !signed_in.authorize.as_str().contains(verifier.as_str()),
        "the verifier went out through the browser, which is the one thing pkce keeps back"
    );
}

#[tokio::test]
async fn a_callback_with_a_state_this_server_never_issued_is_refused() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    for query in [
        "?code=a-code&state=deadbeef",
        "?code=a-code",
        "?state=deadbeef",
        "",
    ] {
        let answer = visit(front, &format!("/auth/callback{query}")).await;
        assert_eq!(
            answer.status, 400,
            "`/auth/callback{query}` was answered as if it belonged to a sign-in"
        );
        assert!(
            answer
                .cookies()
                .iter()
                .all(|cookie| !cookie.contains("mediacore_session=") || cookie.ends_with('=')),
            "a session was opened for a callback this server never started: {:?}",
            answer.cookies()
        );
    }
}

/// Somebody sends you a callback URL that works. Without the login cookie you
/// would finish *their* sign-in in *your* browser, and everything you did next
/// would be in their account.
#[tokio::test]
async fn a_callback_arriving_in_another_browser_is_refused() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let sent_away = visit(front, "/r/standup").await;
    let authorize = Url::parse(&sent_away.header("location").unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    fake.stage_id_token(signed(&claims(&fake.base(), &query["nonce"]), Some(KID)));

    let path = format!("/auth/callback?code=a-code&state={}", query["state"]);

    // The same callback, in a browser that never started the sign-in.
    let stranger = visit(front, &path).await;
    assert_eq!(
        stranger.status, 400,
        "a callback was finished in a browser that could not show the login cookie"
    );
    assert!(
        stranger.live_cookies().is_empty(),
        "somebody was signed in anyway: {:?}",
        stranger.live_cookies()
    );

    // And with a cookie of the right name carrying the wrong secret.
    let wrong = visit_with(
        front,
        &path,
        &[format!(
            "__Host-mediacore_login_{}=a-secret-this-browser-made-up",
            query["state"]
        )],
    )
    .await;
    assert_eq!(wrong.status, 400);
}

#[tokio::test]
async fn a_callback_cannot_be_used_twice() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let sent_away = visit(front, "/r/standup").await;
    let authorize = Url::parse(&sent_away.header("location").unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    let cookies = sent_away.live_cookies();
    fake.stage_id_token(signed(&claims(&fake.base(), &query["nonce"]), Some(KID)));

    let path = format!("/auth/callback?code=a-code&state={}", query["state"]);
    assert_eq!(visit_with(front, &path, &cookies).await.status, 302);

    let again = visit_with(front, &path, &cookies).await;
    assert_eq!(
        again.status, 400,
        "the same callback signed somebody in twice, so the code was redeemable twice"
    );
}

/// A person pressing Cancel is not an error, and it must not become a loop: an
/// answer that sent them back where they came from would send them straight to
/// the provider again.
#[tokio::test]
async fn somebody_who_cancels_lands_on_a_page_rather_than_in_a_loop() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let sent_away = visit(front, "/r/standup").await;
    let authorize = Url::parse(&sent_away.header("location").unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();

    let refused = visit_with(
        front,
        &format!(
            "/auth/callback?error=access_denied&state={}",
            query["state"]
        ),
        &sent_away.live_cookies(),
    )
    .await;

    assert_eq!(refused.status, 403);
    assert!(
        refused.header("location").is_none(),
        "a cancelled sign-in was answered with a redirect back into the sign-in"
    );
    assert!(refused.body.contains("cancelled"), "{}", refused.body);
}

// --------------------------------------------------- what a session is for

#[tokio::test]
async fn the_token_endpoint_stops_being_anonymous() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let refused = post_with(front, "/api/token", &[], r#"{"name":"Mallory"}"#).await;
    assert_eq!(
        refused.status, 401,
        "a browser with no session was given a join token"
    );
    assert!(
        refused.json()["error"]
            .as_str()
            .expect("a reason")
            .contains("Reload"),
        "the page is not told what to do about it: {}",
        refused.body
    );

    let signed_in = sign_in(front, &fake, "/").await;
    let minted = post_with(
        front,
        "/api/token",
        &signed_in.cookies,
        r#"{"name":"Mallory","room":"standup"}"#,
    )
    .await;
    assert_eq!(minted.status, 200, "{}", minted.body);

    let body = minted.json();
    assert_eq!(
        body["name"], "Роман",
        "the browser said who it was and was believed"
    );
    assert_eq!(
        body["identity"], SUBJECT,
        "the identity is not the provider's subject"
    );
    assert_eq!(body["room"], "demo.standup");

    let keys = HashMap::from([(API_KEY.to_owned(), API_SECRET.to_owned())]);
    let claims = mediacore_auth::verify_join(body["token"].as_str().unwrap(), &keys)
        .expect("the sfu accepts the token");
    assert_eq!(claims.identity(), SUBJECT);
    assert_eq!(claims.display_name(), "Роман");
    assert_eq!(claims.video.room, "demo.standup");
}

/// The question worth being sure about: a session running out does not reach
/// into a call. The join token was minted at join, has its own lifetime, and is
/// the SFU's to judge -- nothing about it is looked up here again.
#[tokio::test]
async fn a_session_that_ends_does_not_end_a_call() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;
    let signed_in = sign_in(front, &fake, "/r/standup").await;

    let joined = post_with(
        front,
        "/api/token",
        &signed_in.cookies,
        r#"{"room":"standup"}"#,
    )
    .await;
    let token = joined.json()["token"].as_str().expect("a token").to_owned();

    let out = visit_with(front, "/auth/logout", &signed_in.cookies).await;
    assert_eq!(out.status, 302);

    let keys = HashMap::from([(API_KEY.to_owned(), API_SECRET.to_owned())]);
    let claims = mediacore_auth::verify_join(&token, &keys)
        .expect("the token minted at join outlives the session it was minted from");
    assert!(
        claims.exp > unix_now() + TOKEN_TTL.as_secs() - 60,
        "the join token's life is not the token's own"
    );

    let after = post_with(
        front,
        "/api/token",
        &signed_in.cookies,
        r#"{"room":"standup"}"#,
    )
    .await;
    assert_eq!(
        after.status, 401,
        "the session survived being signed out, so nothing was signed out"
    );
}

#[tokio::test]
async fn the_lobby_is_told_who_is_signed_in_and_nothing_else_about_them() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;
    let signed_in = sign_in(front, &fake, "/").await;

    let answer = visit_with(front, "/api/config", &signed_in.cookies).await;
    let config = answer.json();

    assert_eq!(config["signIn"], true);
    assert_eq!(config["signedInAs"], "Роман");
    assert_eq!(
        answer.header("cache-control").as_deref(),
        Some("no-store"),
        "an answer naming the person at the keyboard may be kept by a shared cache"
    );

    for private in [SUBJECT, "staff", "an-access-token", CLIENT_SECRET] {
        assert!(
            !answer.body.contains(private),
            "`{private}` was handed to the page: {}",
            answer.body
        );
    }

    let stranger = visit(front, "/api/config").await;
    assert_eq!(stranger.json()["signedInAs"], Value::Null);
    assert_eq!(stranger.json()["signIn"], true);
}

// --------------------------------------------------------------- signing out

#[tokio::test]
async fn signing_out_ends_the_session_here_and_at_the_provider() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;
    let signed_in = sign_in(front, &fake, "/r/standup").await;

    let out = visit_with(front, "/auth/logout", &signed_in.cookies).await;
    assert_eq!(out.status, 302);

    let sent_to = Url::parse(&out.header("location").expect("a location")).unwrap();
    let query: HashMap<_, _> = sent_to.query_pairs().into_owned().collect();

    assert_eq!(sent_to.path(), "/end_session");
    assert!(
        !query["id_token_hint"].is_empty(),
        "the provider is not told which sign-in is ending, so it will sign the same person \
         straight back in without a prompt"
    );
    assert_eq!(
        query["post_logout_redirect_uri"],
        format!("http://{front}/"),
        "the provider is handed a post-logout uri other than the registered one, which it \
         refuses"
    );

    assert!(
        out.cookies()
            .iter()
            .any(|cookie| cookie == "__Host-mediacore_session="),
        "the browser was left holding a session cookie: {:?}",
        out.cookies()
    );

    let page = visit_with(front, "/r/standup", &signed_in.cookies).await;
    assert_eq!(
        page.status, 302,
        "the session was still good after signing out"
    );
}

#[tokio::test]
async fn a_return_url_somewhere_else_is_not_where_anybody_is_sent() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    // No `end_session_endpoint` published, so the local half is the one that
    // decides where the browser lands -- which is where a `returnUrl` matters.
    fake.staged.lock().unwrap().issuer = None;
    let front_no_end_session = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // A provider that cannot be reached at all publishes no end session
        // endpoint either, which is the same branch.
        let router = http::router(config(Some(sign_in_with(
            "http://127.0.0.1:1",
            &format!("http://{addr}"),
        ))));
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    };

    for elsewhere in [
        "https://evil.example/",
        "//evil.example",
        "https://evil.example/r/standup",
    ] {
        let out = visit(
            front_no_end_session,
            &format!("/auth/logout?returnUrl={}", urlencoded(elsewhere)),
        )
        .await;

        assert_eq!(out.status, 302);
        assert_eq!(
            out.header("location").as_deref(),
            Some(format!("http://{front_no_end_session}/").as_str()),
            "`{elsewhere}` was honoured, so this server forwards anywhere"
        );
    }

    let home = visit(
        front_no_end_session,
        "/auth/logout?returnUrl=%2Fr%2Fstandup",
    )
    .await;
    assert_eq!(
        home.header("location").as_deref(),
        Some(format!("http://{front_no_end_session}/r/standup").as_str()),
        "a path on this server was refused along with the rest"
    );

    let _ = front;
}

#[tokio::test]
async fn signing_out_is_something_you_do_here() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;
    let signed_in = sign_in(front, &fake, "/").await;

    let jar = signed_in.cookies.join("; ");
    let from_elsewhere = request(
        front,
        "GET /auth/logout",
        &[("Cookie", &jar), ("Sec-Fetch-Site", "cross-site")],
        "",
    )
    .await;

    assert_eq!(
        from_elsewhere.status, 403,
        "another site could sign this person out with an <img> tag"
    );

    let still_here = visit_with(front, "/api/config", &signed_in.cookies).await;
    assert_eq!(
        still_here.json()["signedInAs"],
        "Роман",
        "the session was ended by a request from another site"
    );
}

// --------------------------------------------- when the provider misbehaves

#[tokio::test]
async fn a_provider_that_cannot_be_reached_signs_nobody_in() {
    // Port 1 on loopback: nothing listens there, and connecting is refused
    // rather than hanging, which is the fast half of "unreachable".
    let front = front_signing_in_against("http://127.0.0.1:1").await;

    let answer = visit(front, "/r/standup").await;
    assert_eq!(
        answer.status, 503,
        "a provider that is down was answered with something other than 'nobody can sign in'"
    );
    assert!(
        answer.live_cookies().is_empty(),
        "a session was opened without a provider: {:?}",
        answer.live_cookies()
    );
    assert!(
        answer.body.contains("identity provider"),
        "the page does not say what is wrong: {}",
        answer.body
    );

    let refused = post_with(front, "/api/token", &[], r#"{"name":"Mallory"}"#).await;
    assert_eq!(
        refused.status, 401,
        "a provider being down made the token endpoint anonymous again"
    );
}

/// A provider serving somebody else's discovery document at this URL would
/// otherwise have its tokens accepted here. RFC 8414 section 3.3.
#[tokio::test]
async fn a_provider_that_describes_itself_as_somebody_else_is_refused() {
    let fake = fake_provider().await;
    fake.staged.lock().unwrap().issuer = Some("https://sso.elsewhere.test".to_owned());
    let front = front_signing_in_against(&fake.base()).await;

    let answer = visit(front, "/").await;
    assert_eq!(
        answer.status, 503,
        "a discovery document naming another issuer was used anyway"
    );
}

#[tokio::test]
async fn a_token_endpoint_that_refuses_the_code_signs_nobody_in() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    let sent_away = visit(front, "/").await;
    let authorize = Url::parse(&sent_away.header("location").unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    fake.staged.lock().unwrap().token_status = Some(400);

    let back = visit_with(
        front,
        &format!("/auth/callback?code=a-code&state={}", query["state"]),
        &sent_away.live_cookies(),
    )
    .await;

    assert_eq!(back.status, 403);
    assert!(
        back.live_cookies().is_empty(),
        "a session was opened without a token: {:?}",
        back.live_cookies()
    );
}

/// OpenID Connect 5.3.2: the two answers have to be about the same person, and
/// a mismatch means one of them is about somebody else.
#[tokio::test]
async fn user_info_about_somebody_else_refuses_the_whole_sign_in() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;
    fake.staged.lock().unwrap().userinfo =
        Some(json!({ "sub": "a-completely-different-person", "name": "Mallory" }));

    let sent_away = visit(front, "/").await;
    let authorize = Url::parse(&sent_away.header("location").unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    fake.stage_id_token(signed(&claims(&fake.base(), &query["nonce"]), Some(KID)));

    let back = visit_with(
        front,
        &format!("/auth/callback?code=a-code&state={}", query["state"]),
        &sent_away.live_cookies(),
    )
    .await;

    assert_eq!(back.status, 403);
    assert!(back.live_cookies().is_empty());
}

/// A key really does rotate, and a token naming the new one has to work. What
/// must not happen is one fetch of the key set per token naming a `kid` that
/// never existed.
#[tokio::test]
async fn a_key_that_rotated_is_fetched_once_and_not_once_per_token() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;

    sign_in(front, &fake, "/").await;
    let after_first = fake.staged.lock().unwrap().jwks_fetches;
    assert_eq!(after_first, 1, "the key set was not cached at all");

    // Every token from here names a key the published set does not carry.
    for _ in 0..4 {
        let sent_away = visit(front, "/").await;
        let authorize = Url::parse(&sent_away.header("location").unwrap()).unwrap();
        let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
        fake.stage_id_token(signed(
            &claims(&fake.base(), &query["nonce"]),
            Some("a-key-that-was-never-published"),
        ));

        let back = visit_with(
            front,
            &format!("/auth/callback?code=a-code&state={}", query["state"]),
            &sent_away.live_cookies(),
        )
        .await;
        assert_eq!(back.status, 403, "an unknown key signed somebody in");
    }

    let fetches = fake.staged.lock().unwrap().jwks_fetches;
    assert!(
        fetches <= after_first + 1,
        "{fetches} fetches of the key set for four bad tokens: a stream of invented `kid`s is a \
         way to make this server hammer the provider"
    );
}

#[tokio::test]
async fn nothing_of_the_client_secret_escapes() {
    let fake = fake_provider().await;
    let front = front_signing_in_against(&fake.base()).await;
    let signed_in = sign_in(front, &fake, "/r/standup").await;

    for path in ["/", "/r/standup", "/app.js", "/api/config"] {
        let answer = visit_with(front, path, &signed_in.cookies).await;
        assert!(
            !answer.body.contains(CLIENT_SECRET) && !answer.head.contains(CLIENT_SECRET),
            "{path} carried the client secret to the browser"
        );
    }

    let refused = visit(front, "/auth/callback?code=a-code&state=deadbeef").await;
    assert!(
        !refused.body.contains(CLIENT_SECRET),
        "a refusal page carried the client secret"
    );

    // It reaches exactly one place, and it is the token endpoint's body.
    assert_eq!(fake.token_form()["client_secret"], CLIENT_SECRET);

    let rendered = format!(
        "{:?}",
        config(Some(sign_in_with(&fake.base(), "http://x.test")))
    );
    assert!(
        !rendered.contains(CLIENT_SECRET),
        "a `{{config:?}}` in a log line would print the client secret: {rendered}"
    );
    assert!(rendered.contains("<redacted>"));
}

// ------------------------------------------------------ with the switch off

/// The other half of "a switch, not a mandate": with it off, none of this
/// exists.
#[tokio::test]
async fn with_the_switch_off_there_is_no_sign_in_at_all() {
    let front = front_without_sign_in().await;

    let page = visit(front, "/r/standup").await;
    assert_eq!(
        page.status, 200,
        "the page was not served without a session"
    );
    assert!(
        page.cookies().is_empty(),
        "a cookie was set: {:?}",
        page.cookies()
    );

    for path in ["/auth/callback?code=a&state=b", "/auth/logout"] {
        assert_eq!(
            visit(front, path).await.status,
            404,
            "{path} answers on a deployment that signs nobody in"
        );
    }

    let minted = post_with(
        front,
        "/api/token",
        &[],
        r#"{"name":"Alice","room":"standup"}"#,
    )
    .await;
    assert_eq!(
        minted.status, 200,
        "the token endpoint stopped being anonymous with sign-in switched off"
    );
    assert_eq!(minted.json()["name"], "Alice");

    let config = visit(front, "/api/config").await.json();
    assert_eq!(config["signIn"], false);
    assert_eq!(config["signedInAs"], Value::Null);
}

fn urlencoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
