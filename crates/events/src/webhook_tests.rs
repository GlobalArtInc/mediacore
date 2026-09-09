
use super::*;
use mediacore_proto::mediacore;
use std::collections::HashMap;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";

fn webhook() -> Webhook {
    Webhook {
        url: "http://127.0.0.1:1/hook".to_owned(),
        api_key: API_KEY.to_owned(),
        secret: API_SECRET.to_owned(),
        client: reqwest::Client::new(),
    }
}

fn event() -> Event {
    Event::room_started(mediacore::Room {
        sid: "RM_000000000000".to_owned(),
        name: "standup".to_owned(),
        ..Default::default()
    })
}

/// What a LiveKit receiver does, step for step: take the bearer token,
/// verify it with the secret belonging to the key in `iss`, then check
/// that the base64 SHA-256 of the body it read equals the token's `sha256`
/// claim. If any of this stops being true, an existing deployment starts
/// answering 401 to every event and nothing here would otherwise notice.
#[test]
fn a_receiver_verifies_the_token_and_the_body_the_way_livekit_does() {
    let webhook = webhook();
    let event = event();
    let body = event.payload(0);

    let header = webhook.authorization(&body).expect("the webhook is signed");
    let token = header
        .strip_prefix("Bearer ")
        .expect("LiveKit's receiver reads the token after `Bearer `, and there is none");

    let keys = HashMap::from([(API_KEY.to_owned(), API_SECRET.to_owned())]);
    let claims = mediacore_auth::verify(token, &keys)
        .expect("a receiver holding this server's own key could not verify the token");

    assert_eq!(
        claims.iss, API_KEY,
        "the receiver cannot tell which secret to check"
    );
    assert_eq!(
        claims.sha256,
        STANDARD.encode(Sha256::digest(&body)),
        "the digest claim is not the base64 sha256 of the body, so every receiver rejects \
             the request as tampered with"
    );
    assert!(
        claims.exp > 0,
        "a token with no expiry is a signed request that can be replayed for ever"
    );
}

/// A signature over a body that is not the body sent is a signature over
/// nothing: it would verify against anything an intermediary substituted.
#[test]
fn the_signature_is_over_the_bytes_that_are_actually_sent() {
    let webhook = webhook();
    let body = event().payload(0);
    let tampered = {
        let mut tampered = body.clone();
        tampered.extend_from_slice(b" ");
        tampered
    };

    let header = webhook.authorization(&body).unwrap();
    let keys = HashMap::from([(API_KEY.to_owned(), API_SECRET.to_owned())]);
    let claims = mediacore_auth::verify(header.strip_prefix("Bearer ").unwrap(), &keys).unwrap();

    assert_ne!(
        claims.sha256,
        digest(&tampered),
        "a body somebody changed on the way still matches the digest that was signed"
    );
}

#[test]
fn every_configured_endpoint_becomes_a_destination_named_by_its_url() {
    let targets = Webhook::targets(&WebhookSettings {
        urls: vec![
            "https://first.test/hook".to_owned(),
            "https://second.test/hook".to_owned(),
        ],
        api_key: API_KEY.to_owned(),
        secret: API_SECRET.to_owned(),
        timeout: Duration::from_secs(5),
        retry: Retry::default(),
    })
    .expect("the endpoints are built");

    assert_eq!(
        targets
            .iter()
            .map(|target| target.sink.name().to_owned())
            .collect::<Vec<String>>(),
        vec!["https://first.test/hook", "https://second.test/hook"],
        "a destination is named by something other than its url, so an outbox record written \
             today names something else after the config is edited"
    );
}
