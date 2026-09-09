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

/// One cookie per **browser**, not one per sign-in in flight.
///
/// It used to carry the `state` in its name, which read well -- two tabs
/// signing in at once each held their own proof -- and was a way for a browser
/// to end up holding hundreds of cookies for this origin: every signed-out page
/// load left a differently named one alive for ten minutes, and nothing but a
/// matching callback ever cleared one. Abandoned sign-ins, crawlers and
/// prefetches piled up on their own, and `<img src="https://host/r/a">` on any
/// page anywhere planted them deliberately, until the jar evicted cookies that
/// mattered or the `Cookie` header outgrew what a proxy in front would accept.
///
/// One name means a browser holds one, however many times it loads the page.
/// Two tabs signing in at once therefore share a secret, which costs nothing:
/// what the cookie proves is *which browser*, and it is the same browser.
pub const LOGIN_COOKIE: &str = "__Host-mediacore_login";

/// How long a sign-in may take. Long enough to find a password manager and a
/// second factor, short enough that a login left open in a tab is not a
/// standing invitation.
pub const LOGIN_TTL: Duration = Duration::from_secs(600);

pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// A sign-in in flight is 200 bytes and lives ten minutes, and starting one
/// costs an unauthenticated `GET /`. That is a leak, so it has a ceiling --
/// and what happens *at* the ceiling is the whole of the decision.
///
/// This used to drop the oldest entry to make room. The oldest entry is not the
/// most abandoned one; it is the one that has been waiting longest at the
/// provider, which is to say the person currently typing a password. Four
/// thousand `GET /`s take under a second from one host, so anybody could
/// evict any sign-in in flight, and a sustained stream made signing in
/// impossible for everybody rather than for the person flooding.
///
/// So a full map refuses the newcomer instead. Somebody who arrives during a
/// flood is told to try again in a moment, which is survivable; somebody
/// halfway through signing in is not interrupted, which is the part that was
/// not. Nothing an unauthenticated request does can reach an entry that is
/// already there.
pub const MAX_PENDING_LOGINS: usize = 4096;

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

/// Why a callback was not allowed to finish a sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotFinished {
    /// No live login of that name: never issued here, already spent, or ten
    /// minutes old.
    Unknown,
    /// The login is there and this is not the browser that started it. **It is
    /// left where it is**: a callback that fails this check has redeemed no
    /// code and spent nothing, and the person it belongs to is still at the
    /// provider with the same `state` in their address bar.
    WrongBrowser,
}

#[derive(Default)]
struct Store {
    logins: HashMap<String, Login>,
    sessions: HashMap<String, Session>,
    /// Whether the "no room for another sign-in" line has already been written
    /// for this episode. One line per episode and not one per request: a flood
    /// is exactly the moment not to write five thousand log lines a second.
    said_it_is_full: bool,
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

    /// Remembers a sign-in in flight under the `state` that will name it in the
    /// callback, and says whether there was room.
    ///
    /// The `state` is the caller's rather than this method's because the
    /// redirect to the provider is built *before* anything is remembered: a
    /// provider that cannot be reached should not leave behind a login nobody
    /// can ever finish, holding one of the four thousand places for the next
    /// ten minutes.
    ///
    /// `false` means the map is full, and the caller has to say so to the
    /// person rather than push somebody else out -- see [`MAX_PENDING_LOGINS`]
    /// for why that is the trade.
    pub fn start_login(&self, state: &str, login: Login) -> bool {
        let Ok(mut store) = self.store.lock() else {
            return false;
        };

        // Only when it matters. Sweeping the whole map on every request is an
        // O(n) walk paid for by the flood it is meant to survive; the entries
        // that ran out are checked again at `finish_login` anyway, so leaving
        // them lying about below the ceiling costs nothing but memory that is
        // already accounted for.
        if store.logins.len() >= MAX_PENDING_LOGINS {
            let now = Instant::now();
            store.logins.retain(|_, login| login.expires_at > now);
        }

        if store.logins.len() >= MAX_PENDING_LOGINS {
            if !store.said_it_is_full {
                store.said_it_is_full = true;
                tracing::warn!(
                    pending = store.logins.len(),
                    "more sign-ins are in flight than this process will hold, so nobody new can \
                     start one until some of them run out"
                );
            }

            return false;
        }

        store.said_it_is_full = false;
        store.logins.insert(state.to_owned(), login);

        true
    }

    /// Takes the login this `state` names, *once*, and only for a browser that
    /// can show what the sign-in started with.
    ///
    /// The order is the point, and `proved` is a closure rather than a second
    /// call so that both happen under one lock. Spending the login first and
    /// checking the browser afterwards -- which is what this did -- meant that
    /// anybody who learned an in-flight `state` could end that sign-in without
    /// holding anything at all, and a `state` is not a secret: it is in the
    /// address bar while the provider's page is open, and in the provider's
    /// access log for ever.
    ///
    /// Taking it once is still the whole of the protection against a code being
    /// redeemed twice: a callback replayed from a history entry or a kept link
    /// finds nothing the second time.
    pub fn finish_login(
        &self,
        state: &str,
        proved: impl FnOnce(&Login) -> bool,
    ) -> Result<Login, NotFinished> {
        let Ok(mut store) = self.store.lock() else {
            return Err(NotFinished::Unknown);
        };

        let Some(login) = store.logins.get(state) else {
            return Err(NotFinished::Unknown);
        };
        if login.expires_at <= Instant::now() {
            store.logins.remove(state);

            return Err(NotFinished::Unknown);
        }
        if !proved(login) {
            return Err(NotFinished::WrongBrowser);
        }

        store.logins.remove(state).ok_or(NotFinished::Unknown)
    }

    /// Whether some other sign-in in flight is still relying on this browser's
    /// secret.
    ///
    /// One cookie per browser means the callback cannot simply clear it when it
    /// is done: the other tab may be at the provider with a login of its own
    /// whose proof is that same secret. A lock this cannot take answers "yes",
    /// because a cookie left to run out its ten minutes is a smaller harm than
    /// a sign-in that cannot be finished.
    pub fn another_login_in(&self, browser: &str) -> bool {
        let Ok(store) = self.store.lock() else {
            return true;
        };

        let now = Instant::now();
        store
            .logins
            .values()
            .any(|login| login.expires_at > now && login.browser == browser)
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
    // The length was just checked to be even, so the remainder here is empty.
    let (pairs, _) = bytes.as_chunks::<2>();
    for pair in pairs {
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

/// The proof that the callback comes back to the browser the login left from.
/// `Path=/` because `__Host-` demands it; one name, for the reasons on
/// [`LOGIN_COOKIE`]. Setting it again with the secret the browser already holds
/// is how the ten minutes are restarted for a browser that is signing in again.
pub fn login_cookie(secret: &str) -> String {
    format!(
        "{LOGIN_COOKIE}={secret}; Max-Age={}; Path=/; HttpOnly; Secure; SameSite=Lax",
        LOGIN_TTL.as_secs()
    )
}

pub fn spent_login_cookie() -> String {
    format!("{LOGIN_COOKIE}=; Max-Age=0; Path=/; HttpOnly; Secure; SameSite=Lax")
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
