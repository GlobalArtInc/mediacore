//! Who is signed in, and what a sign-in that has not finished yet is holding.
//!
//! Two short-lived things live here and neither of them is a token the browser
//! could use for anything else. A **login** is a sign-in in flight: it exists
//! between the redirect to the provider and the callback coming back, and it
//! holds the three secrets that make that round trip safe -- the `nonce` the ID
//! token has to echo, the PKCE verifier the code is redeemed with, and the
//! fingerprint of the cookie that proves the callback arrived in the browser
//! the login started in. A **session** is somebody signed in: their `sub`, the
//! name to show, the roles the provider named, and the ID token kept only so
//! logout can hand it back as `id_token_hint`.
//!
//! **The browser holds an opaque id and nothing else.** Not the `sub`, not the
//! roles, not the ID token: a cookie is copyable, readable by whoever can read
//! a disk, and outlives the tab it was set in. Everything about a person stays
//! in this process, keyed by 256 bits the browser cannot guess. The cookie is
//! signed on top of that -- HMAC over the id with a key this process made at
//! startup -- so a garbled or invented id is rejected without a lookup, and the
//! comparison is constant time because a signature check that returns early is
//! a signature check that can be walked.
//!
//! **The store is memory, and a restart signs everybody out.** That is the
//! honest shape for one process serving one stand: there is no second replica
//! for a login to be finished on, and a session that survived a restart would
//! have to be written somewhere, which is a database this crate does not have
//! and a file of live sessions nobody asked for. It costs a person one press of
//! the provider's already-signed-in page. Nobody in a call is dropped by it --
//! the LiveKit token was minted at join and is the SFU's to judge, not this
//! process's.
//!
//! Nothing here reaches the network or the clock beyond `Instant::now`, so all
//! of it is testable without either.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// The cookie a signed-in browser carries. `__Host-` is deliberate: a browser
/// refuses to accept the prefix unless the cookie is `Secure`, has `Path=/` and
/// carries no `Domain`, which pins it to exactly this origin. Without it a
/// sibling host under the same registrable domain -- anything that can set a
/// cookie for `.example.com` -- can plant a session cookie on this one.
pub const SESSION_COOKIE: &str = "__Host-mediacore_session";

/// One cookie per sign-in in flight, named after the login it belongs to, so
/// two tabs signing in at once do not overwrite each other's proof.
pub const LOGIN_COOKIE_PREFIX: &str = "__Host-mediacore_login_";

/// How long a sign-in may take. Long enough to find a password manager and a
/// second factor, short enough that a login left open in a tab is not a
/// standing invitation.
pub const LOGIN_TTL: Duration = Duration::from_secs(600);

pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// A sign-in in flight is 200 bytes and lives ten minutes, and starting one
/// costs an unauthenticated `GET /`. That is a slow leak rather than a fast
/// one, but it is still a leak, so it has a ceiling: past this many the oldest
/// are dropped, which costs whoever owned them one more press of Join and
/// costs this process nothing.
const MAX_PENDING_LOGINS: usize = 4096;

const TOKEN_BYTES: usize = 32;

/// A sign-in between the redirect out and the callback back.
#[derive(Clone)]
pub struct Login {
    /// Where this person was going before they were sent to sign in: a path on
    /// this server, checked by [`crate::oidc::local_path`] when it was stored.
    /// It lives here rather than in the URL or in a cookie because both of
    /// those are the browser's to edit, and this one decides where a `302`
    /// lands.
    pub return_to: String,
    /// What the ID token has to echo. A token that does not is a token minted
    /// for another sign-in, replayed into this one.
    pub nonce: String,
    /// PKCE. The code is worthless to whoever intercepts it without this, and
    /// it never leaves this process until the token request that spends it.
    pub verifier: String,
    /// `sha256` of the secret in the login cookie. The secret itself is in the
    /// browser and nowhere here, so a dump of this map does not let anybody
    /// finish somebody else's login.
    pub browser: String,
    expires_at: Instant,
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("return_to", &self.return_to)
            .finish_non_exhaustive()
    }
}

/// Somebody signed in.
#[derive(Clone)]
pub struct Session {
    /// The provider's `sub`, and the identity every token this person joins
    /// with carries. Never the email: an address changes hands and a `sub`
    /// does not.
    pub subject: String,
    pub display_name: String,
    /// Read from the provider and not enforced anywhere. See the README:
    /// with sign-in on, anybody who can sign in can enter any room whose name
    /// they know.
    pub roles: Vec<String>,
    /// Kept for one purpose: `id_token_hint` at the provider's
    /// `end_session_endpoint`, so logging out here logs out there rather than
    /// bouncing straight back in. It is never rendered and never sent to the
    /// browser.
    id_token: String,
    expires_at: Instant,
}

impl Session {
    pub fn id_token(&self) -> &str {
        &self.id_token
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("subject", &self.subject)
            .field("display_name", &self.display_name)
            .field("roles", &self.roles)
            .finish_non_exhaustive()
    }
}

/// What a completed sign-in puts in a session.
#[derive(Debug, Clone)]
pub struct Identity {
    pub subject: String,
    pub display_name: String,
    pub roles: Vec<String>,
    pub id_token: String,
}

#[derive(Default)]
struct Store {
    logins: HashMap<String, Login>,
    sessions: HashMap<String, Session>,
}

pub struct Sessions {
    store: Mutex<Store>,
    ttl: Duration,
    /// The cookie signing key, made at startup and never written down. A
    /// restart invalidates every cookie signed with the old one, which matters
    /// not at all: the sessions those cookies named went with the process.
    key: [u8; 32],
}

impl fmt::Debug for Sessions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (logins, sessions) = match self.store.lock() {
            Ok(store) => (store.logins.len(), store.sessions.len()),
            Err(_) => (0, 0),
        };

        f.debug_struct("Sessions")
            .field("ttl", &self.ttl)
            .field("logins", &logins)
            .field("sessions", &sessions)
            .finish_non_exhaustive()
    }
}

impl Sessions {
    pub fn new(ttl: Duration) -> Self {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);

        Self {
            store: Mutex::new(Store::default()),
            ttl,
            key,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Remembers a sign-in and answers with the `state` that names it. The
    /// state is 256 random bits: it is what comes back in the callback's query
    /// string, so it has to be unguessable and it has to be safe in a URL.
    pub fn start_login(&self, login: Login) -> String {
        let state = random_token();
        let Ok(mut store) = self.store.lock() else {
            return state;
        };

        let now = Instant::now();
        store.logins.retain(|_, login| login.expires_at > now);

        if store.logins.len() >= MAX_PENDING_LOGINS {
            // Oldest first, because the oldest is the one most likely to have
            // been abandoned. Dropping the *new* one would let a flood lock
            // out the people arriving during it.
            let oldest = store
                .logins
                .iter()
                .min_by_key(|(_, login)| login.expires_at)
                .map(|(state, _)| state.clone());

            if let Some(oldest) = oldest {
                tracing::warn!(
                    pending = store.logins.len(),
                    "more sign-ins are in flight than this process keeps, dropping the oldest"
                );
                store.logins.remove(&oldest);
            }
        }

        store.logins.insert(state.clone(), login);
        state
    }

    /// Takes the login this `state` names, and takes it *once*. A callback
    /// replayed -- from a history entry, from a link somebody kept -- finds
    /// nothing the second time, which is the whole of the protection against a
    /// code being redeemed twice.
    pub fn finish_login(&self, state: &str) -> Option<Login> {
        let mut store = self.store.lock().ok()?;
        let login = store.logins.remove(state)?;

        (login.expires_at > Instant::now()).then_some(login)
    }

    /// Opens a session and answers with the cookie value for it: the id, a
    /// dot, and the signature over the id.
    pub fn open(&self, identity: Identity) -> String {
        let id = random_token();
        let session = Session {
            subject: identity.subject,
            display_name: identity.display_name,
            roles: identity.roles,
            id_token: identity.id_token,
            expires_at: Instant::now() + self.ttl,
        };

        if let Ok(mut store) = self.store.lock() {
            let now = Instant::now();
            store.sessions.retain(|_, kept| kept.expires_at > now);
            store.sessions.insert(id.clone(), session);
        }

        self.sign(&id)
    }

    /// The session a cookie names, or nothing -- for a cookie that was never
    /// signed here, a session that expired, and a session that was closed,
    /// with no way for the browser to tell the three apart.
    pub fn lookup(&self, cookie: &str) -> Option<Session> {
        let id = self.unsign(cookie)?;
        let store = self.store.lock().ok()?;
        let session = store.sessions.get(&id)?;

        (session.expires_at > Instant::now()).then(|| session.clone())
    }

    /// Closes a session and answers with what it held, so logout can reach for
    /// the ID token it needs one last time.
    pub fn close(&self, cookie: &str) -> Option<Session> {
        let id = self.unsign(cookie)?;
        let mut store = self.store.lock().ok()?;

        store.sessions.remove(&id)
    }

    fn sign(&self, id: &str) -> String {
        format!("{id}.{}", self.tag(id))
    }

    /// The id a cookie carries, if this process signed it. The comparison is
    /// the MAC's own, which is constant time; a `==` here would leak how much
    /// of a forged signature was right.
    fn unsign(&self, cookie: &str) -> Option<String> {
        let (id, signature) = cookie.trim().rsplit_once('.')?;
        let signature = decode_hex(signature)?;

        let mut mac = self.mac();
        mac.update(id.as_bytes());
        mac.verify_slice(&signature).ok()?;

        Some(id.to_owned())
    }

    fn tag(&self, id: &str) -> String {
        let mut mac = self.mac();
        mac.update(id.as_bytes());

        encode_hex(&mac.finalize().into_bytes())
    }

    fn mac(&self) -> Hmac<Sha256> {
        // HMAC takes a key of any length, so this cannot fail; the signature
        // says otherwise only because it is generic over key size.
        Hmac::<Sha256>::new_from_slice(&self.key).expect("hmac accepts a key of any length")
    }
}

impl Login {
    pub fn new(return_to: String, nonce: String, verifier: String, browser: String) -> Self {
        Self {
            return_to,
            nonce,
            verifier,
            browser,
            expires_at: Instant::now() + LOGIN_TTL,
        }
    }
}

/// 256 bits, hex. Hex rather than base64url because this value goes into a
/// cookie name, a query string and a PKCE verifier, and hex needs no thought
/// in any of the three -- it is inside the unreserved set RFC 7636 allows a
/// verifier, and 64 characters is inside its 43-128.
pub fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);

    encode_hex(&bytes)
}

/// What is stored about a secret instead of the secret.
pub fn fingerprint(secret: &str) -> String {
    encode_hex(&Sha256::digest(secret.as_bytes()))
}

/// Whether two secrets match, without saying how far the comparison got. Rust
/// string equality returns at the first differing byte; over enough tries that
/// is a way to guess one byte at a time.
pub fn same_secret(left: &str, right: &str) -> bool {
    // Hashed first, so the comparison is over two fixed-length digests and the
    // lengths of the secrets themselves say nothing either.
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());

    let mut different = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        different |= a ^ b;
    }

    different == 0
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble(byte >> 4));
        out.push(nibble(byte & 0x0f));
    }
    out
}

fn nibble(value: u8) -> char {
    char::from_digit(u32::from(value), 16).unwrap_or('0')
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }

    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }

    Some(out)
}

/// The cookie a signed-in browser is given. Every attribute here is load
/// bearing:
///
///   `HttpOnly`  the page's javascript has no use for this value and must not
///               be able to read it -- an XSS that can read the cookie is an
///               XSS that can sign in as this person from anywhere
///   `Secure`    it never travels over plain http; `__Host-` refuses to be set
///               without it anyway
///   `SameSite=Lax`  a cross-site `POST` carries no session, so nothing on
///               another origin can mint a token as this person. `Lax` and not
///               `Strict` because arriving from the provider's redirect is a
///               top-level cross-site navigation and `Strict` would drop the
///               cookie exactly there
///   `Path=/`    required by `__Host-`, and right anyway: `/`, `/r/<room>` and
///               `/api/token` all need it
pub fn session_cookie(value: &str, max_age: u64) -> String {
    format!("{SESSION_COOKIE}={value}; Max-Age={max_age}; Path=/; HttpOnly; Secure; SameSite=Lax")
}

/// The same cookie with nothing in it and no life left. A logout that only
/// forgot the session on this side would leave a browser presenting a cookie
/// for ever, and a person watching the network wondering why.
pub fn spent_session_cookie() -> String {
    session_cookie("", 0)
}

pub fn login_cookie_name(state: &str) -> String {
    format!("{LOGIN_COOKIE_PREFIX}{state}")
}

/// The proof that the callback comes back to the browser the login left from.
/// `Path=/` because `__Host-` demands it; the name carries the state, so it is
/// still one cookie per sign-in.
pub fn login_cookie(state: &str, secret: &str) -> String {
    format!(
        "{}={secret}; Max-Age={}; Path=/; HttpOnly; Secure; SameSite=Lax",
        login_cookie_name(state),
        LOGIN_TTL.as_secs()
    )
}

pub fn spent_login_cookie(state: &str) -> String {
    format!(
        "{}=; Max-Age=0; Path=/; HttpOnly; Secure; SameSite=Lax",
        login_cookie_name(state)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            subject: "3f6b1c88-0000-4000-8000-abcdefabcdef".to_owned(),
            display_name: "Роман".to_owned(),
            roles: vec!["staff".to_owned()],
            id_token: "an.id.token".to_owned(),
        }
    }

    fn sessions() -> Sessions {
        Sessions::new(Duration::from_secs(60))
    }

    #[test]
    fn a_session_is_found_by_the_cookie_it_was_opened_with() {
        let sessions = sessions();
        let cookie = sessions.open(identity());
        let found = sessions.lookup(&cookie).expect("the session just opened");

        assert_eq!(found.subject, identity().subject);
        assert_eq!(found.display_name, "Роман");
        assert_eq!(found.roles, vec!["staff".to_owned()]);
    }

    /// The cookie is an id and a signature over it and carries nothing else.
    /// A browser that could read who it is out of its own cookie is a browser
    /// that could edit it.
    #[test]
    fn the_cookie_says_nothing_about_the_person() {
        let sessions = sessions();
        let cookie = sessions.open(identity());

        for private in ["Роман", "abcdefabcdef", "staff", "an.id.token"] {
            assert!(
                !cookie.contains(private),
                "the session cookie carries `{private}`, which the browser now holds and could \
                 edit: {cookie}"
            );
        }
    }

    #[test]
    fn a_cookie_this_process_did_not_sign_names_nothing() {
        let sessions = sessions();
        let cookie = sessions.open(identity());
        let (id, signature) = cookie.rsplit_once('.').unwrap();

        for forged in [
            format!("{id}.{}", "0".repeat(signature.len())),
            format!("{id}."),
            id.to_owned(),
            format!("{id}.not-hex-at-all"),
            format!("{}.{signature}", random_token()),
            String::new(),
        ] {
            assert!(
                sessions.lookup(&forged).is_none(),
                "`{forged}` was accepted as a session cookie"
            );
        }

        assert!(
            sessions.lookup(&cookie).is_some(),
            "the cookie this process signed stopped working"
        );
    }

    /// Two processes are two key,s so a cookie carried from one to the other
    /// names nothing. That is what a restart does, and it is the reason the
    /// README says a restart signs everybody out rather than leaving it to be
    /// discovered.
    #[test]
    fn a_cookie_from_another_process_names_nothing() {
        let cookie = sessions().open(identity());

        assert!(
            sessions().lookup(&cookie).is_none(),
            "a session cookie signed by another process was honoured"
        );
    }

    #[test]
    fn a_session_that_has_run_out_is_gone() {
        let sessions = Sessions::new(Duration::from_millis(0));
        let cookie = sessions.open(identity());

        assert!(
            sessions.lookup(&cookie).is_none(),
            "a session past its lifetime was still answering"
        );
    }

    #[test]
    fn closing_a_session_hands_back_what_it_held_once() {
        let sessions = sessions();
        let cookie = sessions.open(identity());

        let closed = sessions.close(&cookie).expect("the session that was open");
        assert_eq!(
            closed.id_token(),
            "an.id.token",
            "logout cannot tell the provider which sign-in is ending"
        );
        assert!(
            sessions.close(&cookie).is_none() && sessions.lookup(&cookie).is_none(),
            "a closed session came back"
        );
    }

    /// The callback spends the login. A second callback carrying the same
    /// `state` -- a reload, a back button, somebody's copy of the link -- finds
    /// nothing, and the authorization code it carries can never be redeemed
    /// twice.
    #[test]
    fn a_login_can_only_be_finished_once() {
        let sessions = sessions();
        let state = sessions.start_login(Login::new(
            "/r/standup".to_owned(),
            "a-nonce".to_owned(),
            "a-verifier".to_owned(),
            fingerprint("the-cookie"),
        ));

        let login = sessions
            .finish_login(&state)
            .expect("the login just started");
        assert_eq!(login.return_to, "/r/standup");
        assert_eq!(login.nonce, "a-nonce");
        assert!(
            sessions.finish_login(&state).is_none(),
            "the same state finished a login twice, so a code could be redeemed twice"
        );
    }

    #[test]
    fn a_state_this_process_never_issued_finishes_nothing() {
        let sessions = sessions();
        sessions.start_login(Login::new(
            "/".to_owned(),
            "a-nonce".to_owned(),
            "a-verifier".to_owned(),
            fingerprint("the-cookie"),
        ));

        assert!(sessions.finish_login(&random_token()).is_none());
        assert!(sessions.finish_login("").is_none());
    }

    #[test]
    fn two_sign_ins_at_once_do_not_stand_on_each_other() {
        let sessions = sessions();
        let first = sessions.start_login(Login::new(
            "/r/standup".to_owned(),
            "nonce-one".to_owned(),
            "verifier-one".to_owned(),
            fingerprint("cookie-one"),
        ));
        let second = sessions.start_login(Login::new(
            "/r/retro".to_owned(),
            "nonce-two".to_owned(),
            "verifier-two".to_owned(),
            fingerprint("cookie-two"),
        ));

        assert_ne!(first, second);
        assert_eq!(
            sessions.finish_login(&second).unwrap().return_to,
            "/r/retro"
        );
        assert_eq!(
            sessions.finish_login(&first).unwrap().return_to,
            "/r/standup",
            "a second tab signing in threw away the first tab's room"
        );

        assert_ne!(
            login_cookie_name(&first),
            login_cookie_name(&second),
            "two sign-ins at once share a cookie name, so the second overwrites the first's proof"
        );
    }

    #[test]
    fn a_login_that_was_never_finished_does_not_pile_up_for_ever() {
        let sessions = sessions();
        for _ in 0..(MAX_PENDING_LOGINS + 16) {
            sessions.start_login(Login::new(
                "/".to_owned(),
                random_token(),
                random_token(),
                fingerprint("the-cookie"),
            ));
        }

        let pending = sessions.store.lock().unwrap().logins.len();
        assert!(
            pending <= MAX_PENDING_LOGINS,
            "{pending} sign-ins are in flight, which is more than this process said it would keep"
        );
    }

    #[test]
    fn two_tokens_are_never_the_same() {
        let token = random_token();

        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert_ne!(token, random_token());
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "a token has characters that would have to be escaped in a url or a cookie: {token}"
        );
        assert!(
            (43..=128).contains(&token.len()),
            "a token is not a length RFC 7636 allows a PKCE verifier: {}",
            token.len()
        );
    }

    #[test]
    fn a_stored_fingerprint_is_not_the_secret() {
        let secret = random_token();
        let stored = fingerprint(&secret);

        assert!(
            !stored.contains(&secret),
            "the login store keeps the browser's own secret, so reading it is enough to finish \
             somebody else's sign-in"
        );
        assert_eq!(stored.len(), 64);
        assert_ne!(stored, fingerprint(&random_token()));
    }

    #[test]
    fn secrets_are_compared_whole() {
        assert!(same_secret("a-nonce", "a-nonce"));
        assert!(!same_secret("a-nonce", "a-nonce "));
        assert!(!same_secret("a-nonce", "b-nonce"));
        assert!(!same_secret("", "a-nonce"));
        assert!(same_secret("", ""));
    }

    /// Every attribute on the session cookie is doing something. Losing one
    /// quietly is how a cookie becomes readable by a script, or sent on a
    /// cross-site request, or accepted from a sibling host.
    #[test]
    fn the_session_cookie_is_locked_down() {
        let cookie = session_cookie("value", 3600);

        for attribute in [
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Path=/",
            "Max-Age=3600",
        ] {
            assert!(
                cookie.contains(attribute),
                "the session cookie lost `{attribute}`: {cookie}"
            );
        }
        assert!(
            cookie.starts_with("__Host-"),
            "the session cookie lost its `__Host-` prefix, so a sibling host can set one: {cookie}"
        );
        assert!(
            !cookie.contains("Domain="),
            "a `Domain` widens the cookie to every host under it: {cookie}"
        );

        assert!(spent_session_cookie().contains("Max-Age=0"));
        assert!(login_cookie("a-state", "a-secret").contains("HttpOnly; Secure; SameSite=Lax"));
        assert!(spent_login_cookie("a-state").contains("Max-Age=0"));
    }

    #[test]
    fn nothing_the_provider_said_reaches_a_log_through_a_debug() {
        let sessions = sessions();
        let cookie = sessions.open(identity());
        let session = sessions.lookup(&cookie).unwrap();

        let rendered = format!("{session:?}");
        assert!(
            !rendered.contains("an.id.token"),
            "the id token was printed, and it is a bearer credential at the provider: {rendered}"
        );
        assert!(rendered.contains("Роман"), "{rendered}");

        let login = Login::new(
            "/r/standup".to_owned(),
            "the-nonce".to_owned(),
            "the-verifier".to_owned(),
            fingerprint("the-cookie"),
        );
        let rendered = format!("{login:?}");
        for secret in ["the-nonce", "the-verifier"] {
            assert!(
                !rendered.contains(secret),
                "`{secret}` was printed, and it is what makes this sign-in this sign-in: \
                 {rendered}"
            );
        }

        let rendered = format!("{sessions:?}");
        assert!(
            !rendered.contains(&cookie) && !rendered.contains("Роман"),
            "the session store printed what it holds: {rendered}"
        );
    }

    #[test]
    fn hex_survives_a_round_trip() {
        for bytes in [
            vec![],
            vec![0],
            vec![0xff, 0x00, 0x10],
            (0u8..=255).collect(),
        ] {
            assert_eq!(decode_hex(&encode_hex(&bytes)).as_deref(), Some(&bytes[..]));
        }

        assert_eq!(decode_hex("abc"), None, "an odd number of nibbles decoded");
        assert_eq!(decode_hex("zz"), None, "a non-hex pair decoded");
    }
}
