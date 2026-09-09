
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

/// This server used to lift an `accountId` off a join token and write it
/// into the record of a meeting. It does not any more -- an account was one
/// customer's notion of who somebody is, and the platforms that embed this
/// server have their own -- but the tokens are minted by callers nobody
/// here controls, and a caller that has not been redeployed is still
/// sending both spellings of it.
///
/// A claim this server does not know about is not an error. It is a token
/// that carries something for somebody else, which is the ordinary case for
/// a JWT, and refusing it would lock out every issuer that has not caught
/// up yet.
#[test]
fn a_token_still_carrying_an_account_is_accepted_and_the_account_ignored() {
    let mut claims = join_claims(now() + 600);
    claims
        .attributes
        .insert("accountId".to_owned(), "AC_7".to_owned());

    let token = issue("secret-with-enough-length", &claims).unwrap();
    // Assembled as JSON rather than through `Claims`, because `Claims` has
    // no field for this any more: an issuer that still sets the top-level
    // claim is exactly what has to go on working, and only a token built
    // outside this crate's own type can prove that it does.
    let mut payload = serde_json::to_value(&claims).expect("the claims as json");
    payload["accountId"] = serde_json::Value::String("AC_1".to_owned());
    let with_claim = encode(
        &Header::new(Algorithm::HS256),
        &payload,
        &EncodingKey::from_secret(b"secret-with-enough-length"),
    )
    .expect("a token with a claim this server does not know");

    let verified = verify_join(&token, &keys()).expect("a token with an unknown attribute");
    assert_eq!(
        verified.attributes.get("accountId").map(String::as_str),
        Some("AC_7"),
        "an attribute this server does not read still belongs to whoever set it, and has to \
             reach the room untouched"
    );

    let verified = verify_join(&with_claim, &keys())
        .expect("a token minted before this server stopped having an account");
    assert_eq!(verified.identity(), "alice");
    assert_eq!(verified.video.room, "demo");
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
