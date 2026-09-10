use super::*;

const SECRET: &str = "coturn-shared-secret";
const NOW: u64 = 1_700_000_000;
const TTL: u64 = 3_600;

/// The one test that proves anything about interoperability: the expected
/// credential was computed outside this crate, by
///
/// ```text
/// printf '%s' '1700003600:alice' \
///   | openssl dgst -sha1 -hmac 'coturn-shared-secret' -binary \
///   | openssl base64
/// ```
///
/// and pasted here. A test that recomputed it with the code under test would
/// pass just as happily against SHA-256, an unpadded alphabet or a username in
/// the other order, and every one of those is a credential coturn answers 401
/// to.
#[test]
fn a_minted_credential_is_the_one_openssl_computes() {
    let minted = mint(SECRET, "alice", NOW, TTL);

    assert_eq!(minted.username, "1700003600:alice");
    assert_eq!(minted.credential, "igdZ8kesMome7M9yvKcGWZZbLTc=");
}

#[test]
fn the_username_carries_the_expiry_the_caller_asked_for() {
    let minted = mint(SECRET, "alice", NOW, TTL);
    let (expiry, identity) = minted
        .username
        .split_once(':')
        .expect("coturn reads the expiry out of the username, so there has to be one");

    assert_eq!(
        expiry.parse::<u64>().expect("the expiry is a unix time"),
        NOW + TTL
    );
    assert_eq!(identity, "alice");
}

/// coturn splits on the **first** colon, so the rest of the username is opaque
/// to it and an identity with colons of its own is ordinary. Rewriting or
/// refusing one here would lock out identities this server accepts everywhere
/// else -- and the credential has to keep covering the whole username, or the
/// relay recomputes something different from what it was handed.
#[test]
fn an_identity_with_a_colon_in_it_still_mints() {
    let minted = mint(SECRET, "alice:laptop", NOW, TTL);

    assert_eq!(minted.username, "1700003600:alice:laptop");
    assert_eq!(minted.credential, "cUKHPYiLeBVHA3FPlkBqBl6S9zY=");
    assert_eq!(
        minted.credential,
        credential_for(SECRET, &minted.username),
        "the credential has to cover the username as a whole, colons and all"
    );
}

/// A token minted for the room service names nobody, and a recorder joining to
/// listen may be given an empty identity as well. That is a credential with an
/// expiry and no name on it, which coturn takes; it must not be a panic or a
/// username missing its separator.
#[test]
fn an_empty_identity_mints_a_credential_with_nobody_on_it() {
    let minted = mint(SECRET, "", NOW, TTL);

    assert_eq!(minted.username, "1700003600:");
    assert_eq!(minted.credential, "p4IECfnVctY7hy+kGHCNjRA5FiY=");
}

#[test]
fn two_identities_on_one_secret_get_two_credentials() {
    let alice = mint(SECRET, "alice", NOW, TTL);
    let bob = mint(SECRET, "bob", NOW, TTL);

    assert_ne!(
        alice.credential, bob.credential,
        "the identity is part of what is signed, so a relay can tell one \
         allocation from another in its own logs"
    );
}
