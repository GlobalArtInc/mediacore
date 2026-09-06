//! The Kafka destination, against no broker at all.
//!
//! Only compiled with `--features kafka`, which is how the producer itself is
//! compiled. There is no broker in this workspace and there is not meant to be
//! one: what these assert is the behaviour the deployment actually has today,
//! which is a topic that is configured and pointed at nothing yet. It has to
//! start, stay quiet, cost the people in the room nothing, and give up in
//! bounded time rather than wedging the queue behind it for ever.
#![cfg(feature = "kafka")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mediacore_events::kafka::{Kafka, KafkaSettings};
use mediacore_events::{Bus, Delivery, Event, Outbox, Retry};
use mediacore_proto::mediacore;

/// A port nothing is listening on. Not a hostname that does not resolve: a
/// refused connection is the failure a broker being down actually produces.
const NOWHERE: &str = "127.0.0.1:1";

fn settings(brokers: &str, attempts: u32) -> KafkaSettings {
    KafkaSettings {
        brokers: brokers.to_owned(),
        topic: "mediacore.test".to_owned(),
        properties: HashMap::from([("socket.timeout.ms".to_owned(), "1000".to_owned())]),
        timeout: Duration::from_millis(300),
        retry: Retry {
            attempts,
            backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(20),
        },
    }
}

fn event(room: &str) -> Event {
    Event::room_started(mediacore::Room {
        sid: "RM_000000000000".to_owned(),
        name: room.to_owned(),
        ..Default::default()
    })
}

#[test]
fn no_broker_configured_is_no_destination_at_all() {
    let target = Kafka::target(&settings("", 3)).expect("an empty broker list is not an error");

    assert!(
        target.is_none(),
        "a deployment that configured no broker got a kafka destination anyway, and every event \
         it ever emits will be queued for one"
    );
}

/// The state this is shipping in: a topic configured, and nothing at the other
/// end of it yet. Building the producer must not touch the network, emitting
/// must not wait for it, and one event must not tie the queue up for ever.
#[tokio::test]
async fn a_broker_that_is_not_there_is_given_up_on_rather_than_waited_for() {
    let started = Instant::now();
    let target = Kafka::target(&settings(NOWHERE, 2))
        .expect("the producer could not be built")
        .expect("a broker was configured and no destination was built");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "building the producer took {:?}, so it went looking for a broker while the server was \
         starting",
        started.elapsed()
    );
    assert_eq!(target.sink.name(), "kafka:mediacore.test");

    let directory = tempfile::TempDir::new().unwrap();
    let outbox = Arc::new(Outbox::open(directory.path()).expect("the outbox opens"));
    let bus = Bus::new();
    let delivery = Delivery::start(&bus, outbox, vec![target], 8);

    let started = Instant::now();
    bus.emit(event("standup"));
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "announcing an event took {:?} with the broker unreachable. Somebody joining a room \
         waits for that",
        started.elapsed()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while delivery.undelivered() == 0 {
        assert!(
            Instant::now() < deadline,
            "the event was still being offered to a broker that is not there; an unbounded retry \
             never reaches the event behind it"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(delivery.undelivered(), 1);
    assert_eq!(
        delivery.delivered(),
        0,
        "an event was counted as delivered to a broker that does not exist"
    );
}
