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
//! Authorization: eyJhbGciOiJIUzI1NiIs…
//!
//! {"event":"participant_joined","room":{…},"participant":{…},
//!  "id":"EV_1f0c9a2b7d34","createdAt":"1757155200"}
//! ```
//!
//! The token is a short-lived JWT signed with the API secret, carrying the
//! base64 of the SHA-256 of the body in its `sha256` claim. It is the bare
//! token, with no `Bearer ` in front of it: LiveKit's `WebhookReceiver` hands
//! the header value straight to its token verifier without stripping a scheme,
//! so a `Bearer ` prefix makes every event unverifiable to every receiver
//! built on their SDK. The other direction is the opposite -- a *call* to the
//! room service does carry `Bearer` -- and it is the mismatch between the two
//! that makes this worth writing down. A receiver
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
    ///
    /// The token alone. `WebhookReceiver::receive` in `livekit-server-sdk`
    /// passes this value to `TokenVerifier::verify` unchanged, so anything in
    /// front of the token is part of the token as far as it is concerned.
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

        Ok(token)
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
#[path = "webhook_tests.rs"]
mod tests;
