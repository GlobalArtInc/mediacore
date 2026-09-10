//! The pass a client is handed for somebody else's TURN server.
//!
//! This is the other half of what this crate does. Everything beside it checks
//! a credential minted elsewhere against a secret we share with whoever minted
//! it; here we are the one minting, and the secret is shared with a coturn next
//! door rather than with a caller. Neither this server nor this crate ever sees
//! the credential again -- coturn recomputes it from the same secret and decides
//! on its own.
//!
//! The scheme is draft-uberti-behave-turn-rest-00, which is what coturn's
//! `use-auth-secret` implements and what every TURN deployment worth talking to
//! calls "the REST API". It is **fixed at HMAC-SHA1** and there is no variant of
//! it on SHA-256: the digest here is not a choice this server gets to make, and
//! changing it would produce credentials coturn refuses with no way to tell why.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// The pair a client puts in its `RTCIceServer` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCredential {
    /// `<unix expiry seconds>:<identity>`. coturn reads the expiry out of it,
    /// so this is data and not a name it looks up anywhere.
    pub username: String,
    pub credential: String,
}

/// Mints a credential a coturn started with `use-auth-secret` will accept until
/// `now_seconds + ttl_seconds`.
///
/// **The time is a parameter and not the clock.** One join mints one of these
/// and hands it straight to a client, so reading the clock in here would cost
/// nothing -- except that it would make this function untestable against a known
/// answer, and a known answer computed by openssl is the only thing that proves
/// the credential is one coturn accepts rather than one this server agrees with
/// itself about.
///
/// **The identity goes in raw.** coturn splits the username on the first colon
/// and treats everything after it as opaque: it is there so a relay's logs and
/// its per-user limits name the person holding the allocation, and it is not
/// part of the check. So an identity carrying colons of its own is fine, and
/// rejecting or rewriting one here would lock out identities that are perfectly
/// legal everywhere else in this server.
pub fn mint(secret: &str, identity: &str, now_seconds: u64, ttl_seconds: u64) -> TurnCredential {
    let expiry = now_seconds.saturating_add(ttl_seconds);
    let username = format!("{expiry}:{identity}");
    let credential = credential_for(secret, &username);

    TurnCredential {
        username,
        credential,
    }
}

/// The half coturn recomputes when it is offered a username.
///
/// Standard base64 with its padding, which is what the draft says and what
/// coturn compares against byte for byte -- the url-safe alphabet this crate
/// uses for JWT payloads would be refused by every relay.
pub fn credential_for(secret: &str, username: &str) -> String {
    // HMAC takes a key of any length, so this cannot fail; the signature says
    // otherwise only because it is generic over key size.
    let mut mac =
        Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("hmac accepts a key of any length");
    mac.update(username.as_bytes());

    STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
