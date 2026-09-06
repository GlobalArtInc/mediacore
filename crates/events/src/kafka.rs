//! Events onto a Kafka topic, for a deployment that has a bus.
//!
//! **Behind a Cargo feature, off by default, and not compiled unless asked
//! for.** `rdkafka` links librdkafka into the binary and pulls a C toolchain
//! into the image that builds it, and an operator who does not run a bus
//! should pay for neither. `cargo build --features kafka` on
//! `mediacore-server` is the whole of turning it on; without it this module
//! does not exist and a config that names a broker is refused at startup
//! rather than quietly ignored, because a bus that is silently not there is
//! the failure this is meant to avoid.
//!
//! The body is the same bytes the webhooks post -- one event shape, two
//! transports -- so a consumer reading the topic and a consumer receiving the
//! webhook parse the same thing. The key is the room's name, which is what
//! puts one room's events on one partition and therefore in order; without it
//! a `participant_left` and the `participant_joined` before it can land on
//! different partitions and be read in either order.
//!
//! **It has to start with nothing to connect to.** The producer is built
//! without touching the network -- librdkafka connects in the background --
//! so a server configured with a broker that is not there yet starts, stays
//! quiet, and fails deliveries with a timeout that the retry policy bounds.
//! Nothing here is ever awaited by a participant path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use rdkafka::ClientConfig;
use rdkafka::config::RDKafkaLogLevel;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::util::Timeout;

use crate::delivery::{DeliveryError, Retry, Sink, Target};
use crate::{Event, log::quoted};

/// Where the topic is and how to reach it.
#[derive(Debug, Clone)]
pub struct KafkaSettings {
    /// `host:port` pairs, comma separated, as librdkafka spells them. Empty
    /// means no broker is configured and no destination is built.
    pub brokers: String,
    pub topic: String,
    /// Anything else librdkafka takes, passed through unread: this is where
    /// `security.protocol`, `ssl.ca.location` and the rest of a real
    /// deployment's connection lives. Naming each of them in this server's own
    /// config would mean a new release every time a broker wanted a setting we
    /// had not thought of.
    pub properties: HashMap<String, String>,
    /// How long one attempt may take, both to be accepted into librdkafka's
    /// queue and to be acknowledged by the broker.
    pub timeout: Duration,
    pub retry: Retry,
}

/// The topic, as a destination.
pub struct Kafka {
    name: String,
    topic: String,
    producer: FutureProducer,
    timeout: Duration,
}

impl Kafka {
    /// The destination, or `None` when no broker is configured.
    pub fn target(settings: &KafkaSettings) -> Result<Option<Target>, rdkafka::error::KafkaError> {
        if settings.brokers.trim().is_empty() {
            tracing::debug!("no kafka broker is configured");
            return Ok(None);
        }

        let mut config = ClientConfig::new();
        config.set("bootstrap.servers", &settings.brokers);
        // What librdkafka waits before giving up on a message, and the reason
        // a broker that is not there costs a bounded wait rather than the five
        // minutes it defaults to. Our own attempt timeout is the same number,
        // so whichever fires first fires at the same moment.
        config.set(
            "message.timeout.ms",
            settings.timeout.as_millis().max(1).to_string(),
        );
        for (key, value) in &settings.properties {
            config.set(key, value);
        }
        // librdkafka is talkative about a broker it cannot reach, and it says
        // so through the `log` crate, which this binary installs no logger
        // for. Turning it down as well means a deployment that does add one
        // does not get a line a second about a broker it knows is coming.
        config.set_log_level(RDKafkaLogLevel::Error);

        let producer: FutureProducer = config.create()?;
        let name = format!("kafka:{}", settings.topic);

        tracing::info!(
            brokers = %quoted(&settings.brokers),
            topic = %quoted(&settings.topic),
            attempts = settings.retry.attempts,
            timeout_ms = settings.timeout.as_millis() as u64,
            "events will be produced to this topic"
        );

        Ok(Some(Target::new(
            Arc::new(Self {
                name,
                topic: settings.topic.clone(),
                producer,
                timeout: settings.timeout,
            }),
            settings.retry,
        )))
    }
}

impl Sink for Kafka {
    fn name(&self) -> &str {
        &self.name
    }

    fn deliver<'a>(
        &'a self,
        event: &'a Event,
        dropped: u32,
    ) -> BoxFuture<'a, Result<(), DeliveryError>> {
        Box::pin(async move {
            let body = event.payload(dropped);
            let room = event.room_name();

            let record = FutureRecord::to(&self.topic).payload(&body);
            // Keyed by room, so one room's events share a partition and are
            // read in the order they happened. A room name is never empty on
            // any of the six events, and an unkeyed message would be spread
            // round the partitions if one ever were.
            let record = if room.is_empty() {
                record
            } else {
                record.key(room)
            };

            self.producer
                .send(record, Timeout::After(self.timeout))
                .await
                .map(|_| ())
                .map_err(|(error, _)| DeliveryError::new(error))
        })
    }
}
