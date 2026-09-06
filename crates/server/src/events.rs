//! Turning the `events` block of the config into somewhere for events to go.
//!
//! The rest of the server knows only [`Bus`]: rooms announce what happens in
//! them and never learn whether anybody is listening. Everything about *who*
//! is listening is decided once, here, at startup.
//!
//! Nothing is started when nothing is configured. With no webhook endpoint and
//! no broker there is no outbox directory, no task and no cost -- and the
//! record of every meeting is kept either way, which is the reason it is safe
//! for the whole block to be absent.

use std::sync::Arc;

use anyhow::Context as _;
use mediacore_config::Config;
use mediacore_events::{Bus, Delivery, Outbox, Retry, Target, Webhook, WebhookSettings};

/// Builds the destinations and starts the delivery tasks, or answers `None`
/// because there are none. Must be called from inside a Tokio runtime.
///
/// Every failure here fails the start. A server that could not open its outbox
/// or could not build a client for an endpoint would run, emit, deliver
/// nothing and say so once in a line at boot that nobody reads again -- which
/// is the shape of outage this subsystem exists to prevent, arriving through
/// the front door.
pub fn start(config: &Config, bus: &Bus) -> anyhow::Result<Option<Arc<Delivery>>> {
    let settings = &config.events;

    // A broker named on a build with no producer in it. Refused rather than
    // ignored: an operator who wrote this down believes their bus is being
    // fed, and the whole point of the outbox is that nobody has to find out
    // afterwards that it was not.
    anyhow::ensure!(
        mediacore_events::KAFKA || settings.kafka.brokers.trim().is_empty(),
        "events.kafka.brokers names a broker, and this binary was built without the `kafka` \
         feature, so it has no producer to reach one with. Build with `--features kafka`, or \
         remove events.kafka.brokers"
    );

    if !settings.has_destinations() {
        tracing::info!(
            "no event destinations are configured; what happens in a room is still recorded and \
             still readable under /api/rooms/<room>/sessions"
        );
        return Ok(None);
    }

    let retry = Retry {
        attempts: settings.attempts,
        backoff: settings.backoff(),
        max_backoff: settings.max_backoff(),
    };
    let mut targets: Vec<Target> = Vec::new();

    if !settings.webhooks.urls.is_empty() {
        let secret = config
            .keys
            .get(&settings.webhooks.api_key)
            .with_context(|| {
                format!(
                    "events.webhooks.api_key `{}` is not one of this server's keys",
                    settings.webhooks.api_key
                )
            })?;

        targets.extend(
            Webhook::targets(&WebhookSettings {
                urls: settings.webhooks.urls.clone(),
                api_key: settings.webhooks.api_key.clone(),
                secret: secret.clone(),
                timeout: settings.timeout(),
                retry,
            })
            .context("building an http client for the webhook endpoints")?,
        );
    }

    #[cfg(feature = "kafka")]
    if let Some(target) =
        mediacore_events::kafka::Kafka::target(&mediacore_events::kafka::KafkaSettings {
            brokers: settings.kafka.brokers.clone(),
            topic: settings.kafka.topic.clone(),
            properties: settings.kafka.properties.clone(),
            timeout: settings.timeout(),
            retry,
        })
        .context("building the kafka producer")?
    {
        targets.push(target);
    }

    let outbox = Outbox::open(&settings.directory).with_context(|| {
        format!(
            "opening the event outbox in {}",
            settings.directory.display()
        )
    })?;

    Ok(Some(Delivery::start(
        bus,
        Arc::new(outbox),
        targets,
        settings.queue as usize,
    )))
}
