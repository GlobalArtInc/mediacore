//! Events posted to somebody else's HTTP endpoint, the way LiveKit posts
//! them.
//!
//! The shape is LiveKit's `WebhookEvent` and the signature is LiveKit's, down
//! to the header names and the claim the digest goes in, so a deployment
//! already receiving LiveKit's webhooks receives ours without touching its
//! receiver -- including the receiver in `livekit/protocol`, which is the one
//! most of them are. That compatibility is the point of the whole module: an
//! integration that has to be rewritten to point at this server is an
//! integration that does not get pointed at this server.
//!
//! ```text
//! POST /hook HTTP/1.1
//! Content-Type: application/webhook+json
//! Authorization: Bearer eyJhbGciOiJIUzI1NiIs…
//!
//! {"event":"participant_joined","room":{…},"participant":{…},
//!  "id":"EV_1f0c9a2b7d34","createdAt":"1757155200"}
//! ```
//!
//! The token is a short-lived JWT signed with the API secret, carrying the
//! base64 of the SHA-256 of the body in its `sha256` claim. A receiver
//! verifies the signature to know the request came from this server, and the
//! digest to know the body it just read is the body that was signed -- the
//! second half is what makes the first half worth anything, because a
//! signature over nothing in particular would be replayable against any body
//! at all.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::future::BoxFuture;
use mediacore_auth::Claims;
use sha2::{Digest, Sha256};

use crate::delivery::{DeliveryError, Retry, Sink, Target};
use crate::{Event, log::quoted};

/// LiveKit's content type for a webhook, and not `application/json`.
///
/// It is what its own notifier sends, and a receiver that filters on it -- or
/// a proxy in front of one -- treats `application/json` as something else
/// entirely. There is nothing to be gained by being more conventional than the
/// thing we are being compatible with.
pub const CONTENT_TYPE: &str = "application/webhook+json";

/// How long the token on a request is good for.
///
/// Five minutes, as LiveKit's notifier signs for. It only has to outlive the
/// request it is attached to; a longer life is a signed body somebody who
/// captured it can replay for longer.
const TOKEN_LIFETIME_SECONDS: u64 = 300;

/// Where the events go and who they are signed by.
#[derive(Debug, Clone)]
pub struct WebhookSettings {
    /// One endpoint per URL, each with a queue and a worker of its own: a slow
    /// endpoint delays only itself.
    pub urls: Vec<String>,
    /// The API key the token is issued under, which is what tells the receiver
    /// which secret to check it with. It has to be one of this server's own
    /// keys, and the config refuses to start if it is not.
    pub api_key: String,
    pub secret: String,
    /// How long one attempt may take. It bounds the worst case for a
    /// destination that accepts a connection and then says nothing, which is
    /// the failure a plain "is it up" check never sees.
    pub timeout: Duration,
    pub retry: Retry,
}

/// One endpoint.
pub struct Webhook {
    url: String,
    api_key: String,
    secret: String,
    client: reqwest::Client,
}

impl Webhook {
    /// One [`Target`] per configured URL, sharing nothing but the settings.
    ///
    /// A `reqwest::Client` each rather than one between them: the connection
    /// pool is per client, and an endpoint that holds its connections open
    /// while doing nothing must not be able to starve another endpoint of
    /// them.
    pub fn targets(settings: &WebhookSettings) -> Result<Vec<Target>, reqwest::Error> {
        let mut targets = Vec::new();

        for url in &settings.urls {
            let webhook = Self {
                url: url.clone(),
                api_key: settings.api_key.clone(),
                secret: settings.secret.clone(),
                client: reqwest::Client::builder()
                    .timeout(settings.timeout)
                    .build()?,
            };

            tracing::info!(
                endpoint = %quoted(url),
                api_key = %quoted(&settings.api_key),
                attempts = settings.retry.attempts,
                timeout_ms = settings.timeout.as_millis() as u64,
                "events will be posted to this endpoint"
            );
            targets.push(Target::new(Arc::new(webhook), settings.retry));
        }

        if targets.is_empty() {
            tracing::debug!("no webhook endpoints are configured");
        }

        Ok(targets)
    }

    /// The `Authorization` header's value for one body.
    fn authorization(&self, body: &[u8]) -> Result<String, DeliveryError> {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| DeliveryError::new(format!("the clock is before the epoch: {error}")))?
            .as_secs();

        let claims = Claims {
            exp: issued_at + TOKEN_LIFETIME_SECONDS,
            iss: self.api_key.clone(),
            sha256: digest(body),
            ..Claims::default()
        };

        let token = mediacore_auth::issue(&self.secret, &claims).map_err(|error| {
            DeliveryError::new(format!("the webhook could not be signed: {error}"))
        })?;

        Ok(format!("Bearer {token}"))
    }
}

impl Sink for Webhook {
    fn name(&self) -> &str {
        &self.url
    }

    fn deliver<'a>(
        &'a self,
        event: &'a Event,
        dropped: u32,
    ) -> BoxFuture<'a, Result<(), DeliveryError>> {
        Box::pin(async move {
            let body = event.payload(dropped);
            let authorization = self.authorization(&body)?;

            let response = self
                .client
                .post(&self.url)
                .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE)
                .header(reqwest::header::AUTHORIZATION, authorization)
                .body(body)
                .send()
                .await
                .map_err(DeliveryError::new)?;

            let status = response.status();
            if status.is_success() {
                return Ok(());
            }

            // The status and nothing else. A body is written by whoever is at
            // the other end of a URL an operator typed, and a log line is not
            // the place to find out what they put in it.
            Err(DeliveryError::new(format!(
                "the endpoint answered {status}"
            )))
        })
    }
}

/// The digest of a body, as it goes into the token's `sha256` claim: base64
/// with padding, over the raw bytes that were sent.
///
/// Public because verifying one of these is the other half of the contract,
/// and a receiver written in Rust should not have to work out which base64
/// alphabet was meant. The test below is the executable statement of what
/// LiveKit's receiver computes.
pub fn digest(body: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(body))
}

#[cfg(test)]
mod tests {
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
        let claims =
            mediacore_auth::verify(header.strip_prefix("Bearer ").unwrap(), &keys).unwrap();

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
}
