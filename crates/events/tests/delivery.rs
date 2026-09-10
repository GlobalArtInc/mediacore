//! Events leaving this process, against a real socket.
//!
//! The unit tests in the crate cover what an event looks like and what the
//! outbox does with one. These cover the part that only shows up once there is
//! something at the other end: what a receiver actually gets, what happens
//! when it says no, what happens when it says nothing at all, and what is
//! still owed after the process that promised it has gone.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mediacore_events::webhook::{self, Webhook, WebhookSettings};
use mediacore_events::{Bus, Delivery, Event, Outbox, Retry};
use mediacore_proto::mediacore;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";

/// One request an endpoint received.
#[derive(Debug, Clone)]
struct Received {
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map_or("", String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "an endpoint was posted something that is not json ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// Somebody else's HTTP endpoint, with the manners a test needs it to have.
///
/// `status` is changed while it is running, which is what lets one test be an
/// endpoint that is down and then the same endpoint after it comes back --
/// same url, because an outbox record names the destination by its url and a
/// second listener on a second port would be a different destination.
struct Endpoint {
    url: String,
    status: Arc<AtomicU16>,
    received: Arc<Mutex<Vec<Received>>>,
    connections: Arc<AtomicUsize>,
}

/// The status an endpoint answers with to say "accept the connection and then
/// say nothing at all" -- the failure a liveness check never sees.
const SILENT: u16 = 0;

impl Endpoint {
    async fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a socket");
        let addr: SocketAddr = listener.local_addr().unwrap();

        let endpoint = Self {
            url: format!("http://{addr}/hook"),
            status: Arc::new(AtomicU16::new(status)),
            received: Arc::new(Mutex::new(Vec::new())),
            connections: Arc::new(AtomicUsize::new(0)),
        };

        let status = Arc::clone(&endpoint.status);
        let received = Arc::clone(&endpoint.received);
        let connections = Arc::clone(&endpoint.connections);

        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                connections.fetch_add(1, Ordering::Relaxed);

                let status = Arc::clone(&status);
                let received = Arc::clone(&received);
                tokio::spawn(serve(socket, status, received));
            }
        });

        endpoint
    }

    fn answers(&self, status: u16) {
        self.status.store(status, Ordering::Relaxed);
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

async fn serve(mut socket: TcpStream, status: Arc<AtomicU16>, received: Arc<Mutex<Vec<Received>>>) {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];

    let split = loop {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
        }
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break split;
        }
    };

    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let headers: HashMap<String, String> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();

    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = raw[split + 4..].to_vec();
    while body.len() < length {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&buffer[..read]),
        }
    }

    received.lock().unwrap().push(Received { headers, body });

    let status = status.load(Ordering::Relaxed);
    if status == SILENT {
        // Accepted, read, and never answered. The connection is held open so
        // that only the client's own timeout ends it.
        std::future::pending::<()>().await;
        return;
    }

    let _ = socket
        .write_all(
            format!("HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
    let _ = socket.flush().await;
}

fn event(room: &str) -> Event {
    Event::room_started(mediacore::Room {
        sid: "RM_000000000000".to_owned(),
        name: room.to_owned(),
        creation_time: 1_757_155_200,
        ..Default::default()
    })
}

fn settings(urls: Vec<String>, retry: Retry) -> WebhookSettings {
    WebhookSettings {
        urls,
        api_key: API_KEY.to_owned(),
        secret: API_SECRET.to_owned(),
        timeout: Duration::from_millis(300),
        retry,
    }
}

fn impatient(attempts: u32) -> Retry {
    Retry {
        attempts,
        backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(20),
    }
}

fn delivery(bus: &Bus, directory: &std::path::Path, settings: &WebhookSettings) -> Arc<Delivery> {
    let outbox = Arc::new(Outbox::open(directory).expect("the outbox opens"));
    let targets = Webhook::targets(settings).expect("the endpoints are built");

    Delivery::start(bus, outbox, targets, 8)
}

/// Waits for something to become true rather than sleeping long enough to be
/// safe. A test that sleeps for a second on every machine to be safe on the
/// slowest one is a test nobody runs.
async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        if ready().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    panic!("{what}");
}

/// How many records the outbox is still holding, counted from the disk rather
/// than from the process that wrote them -- which is the only way to ask the
/// question a restart asks.
fn owed_on_disk(directory: &std::path::Path) -> usize {
    std::fs::read_dir(directory)
        .expect("the outbox directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count()
}

/// The whole contract with a receiver, in one test: the content type LiveKit
/// sends, a token a LiveKit receiver can verify, a digest over the body it
/// just read, and a body its generated client can parse.
#[tokio::test]
async fn an_event_arrives_signed_the_way_a_livekit_receiver_expects() {
    let endpoint = Endpoint::start(200).await;
    let directory = tempfile::TempDir::new().unwrap();
    let bus = Bus::new();
    let _delivery = delivery(
        &bus,
        directory.path(),
        &settings(vec![endpoint.url.clone()], impatient(3)),
    );

    bus.emit(event("standup"));
    until("the endpoint was never posted to", async || {
        !endpoint.received().is_empty()
    })
    .await;

    let request = endpoint.received().remove(0);
    assert_eq!(
        request.header("content-type"),
        webhook::CONTENT_TYPE,
        "a receiver filtering on LiveKit's content type, or a proxy in front of one, sees \
         something else entirely"
    );

    let token = request.header("authorization").to_owned();
    assert!(
        !token.starts_with("Bearer "),
        "LiveKit's WebhookReceiver verifies the header value as it stands, so a scheme in front \
         of the token is part of the token to it and the signature never checks out"
    );
    let keys = HashMap::from([(API_KEY.to_owned(), API_SECRET.to_owned())]);
    let claims = mediacore_auth::verify(&token, &keys)
        .expect("a receiver holding this server's key could not verify the request");

    assert_eq!(
        claims.sha256,
        webhook::digest(&request.body),
        "the digest is not over the bytes that arrived, so every receiver treats the request as \
         tampered with"
    );

    let body = request.json();
    assert_eq!(body["event"], "room_started");
    assert_eq!(body["room"]["name"], "standup");
    assert_eq!(
        body["room"]["creationTime"], "1757155200",
        "protobuf's json mapping sends an int64 as a string, and a generated LiveKit client \
         refuses a number where it expects one"
    );
}

/// An endpoint that says no says no for ever. Retrying without a bound wedges
/// that destination's queue on one event and punishes every event behind it;
/// the bound is what turns "this endpoint is broken" into a log line and a
/// number instead of a stall.
#[tokio::test]
async fn an_endpoint_that_refuses_is_tried_a_bounded_number_of_times_and_then_let_go() {
    let endpoint = Endpoint::start(500).await;
    let directory = tempfile::TempDir::new().unwrap();
    let bus = Bus::new();
    let delivery = delivery(
        &bus,
        directory.path(),
        &settings(vec![endpoint.url.clone()], impatient(3)),
    );

    bus.emit(event("standup"));
    until("the event was never given up on", async || {
        delivery.undelivered() == 1
    })
    .await;

    assert_eq!(
        endpoint.received().len(),
        3,
        "an endpoint answering 500 was asked {} times; the policy says three, and an unbounded \
         retry never reaches the next event",
        endpoint.received().len()
    );

    // Struck off, not kept. A record left behind for a destination that has
    // already given up would be handed back to it by every restart from now
    // until somebody deleted the file by hand.
    until(
        "the abandoned event was left in the outbox for ever",
        async || owed_on_disk(directory.path()) == 0,
    )
    .await;

    // And the queue is free: the event behind the wedged one goes out.
    endpoint.answers(200);
    bus.emit(event("payroll"));
    until(
        "the event behind the abandoned one never went out",
        async || {
            endpoint
                .received()
                .iter()
                .any(|request| request.json()["room"]["name"] == "payroll")
        },
    )
    .await;
}

/// The failure the outbox exists for, start to finish: an endpoint that is
/// away, a process that stops while it is still owed something, and the same
/// endpoint answering when the next process asks.
///
/// Without the disk in the middle of this, an event promised during a two
/// minute outage is gone at the end of it and no line anywhere says which ones
/// they were.
#[tokio::test]
async fn an_event_a_failed_endpoint_never_took_goes_out_after_a_restart() {
    let endpoint = Endpoint::start(503).await;
    let directory = tempfile::TempDir::new().unwrap();

    {
        let bus = Bus::new();
        // Patient on purpose: the first attempt fails, and the process stops
        // during the wait before the second, which is where a process
        // restarted in the middle of an outage actually stops.
        let _delivery = delivery(
            &bus,
            directory.path(),
            &settings(
                vec![endpoint.url.clone()],
                Retry {
                    attempts: 5,
                    backoff: Duration::from_secs(30),
                    max_backoff: Duration::from_secs(30),
                },
            ),
        );

        bus.emit(event("standup"));
        until("the endpoint was never tried", async || {
            !endpoint.received().is_empty()
        })
        .await;
        // The process ends here: the tasks go with the delivery.
    }

    assert_eq!(
        owed_on_disk(directory.path()),
        1,
        "an event promised to an endpoint that had not taken it was not on the disk when the \
         process stopped, so it is gone and nothing will ever say so"
    );

    // The endpoint comes back, at the same url -- an outbox record names its
    // destination by url, and a second listener on a second port would be a
    // different destination that was never owed anything.
    endpoint.answers(200);
    let tried_before = endpoint.received().len();

    let bus = Bus::new();
    let _delivery = delivery(
        &bus,
        directory.path(),
        &settings(vec![endpoint.url.clone()], impatient(2)),
    );

    until(
        "the event promised by the last process never went out",
        async || {
            endpoint
                .received()
                .iter()
                .skip(tried_before)
                .any(|request| request.json()["room"]["name"] == "standup")
        },
    )
    .await;
    until("the delivered event was left in the outbox", async || {
        owed_on_disk(directory.path()) == 0
    })
    .await;
}

/// The other half of the same rule: an event this server has stopped asking
/// about is struck off. Keeping it would mean every restart from now until
/// somebody deleted the file by hand handing it back to a destination that
/// already refused it five times.
#[tokio::test]
async fn an_event_that_was_given_up_on_is_not_owed_again_after_a_restart() {
    let endpoint = Endpoint::start(500).await;
    let directory = tempfile::TempDir::new().unwrap();

    {
        let bus = Bus::new();
        let delivery = delivery(
            &bus,
            directory.path(),
            &settings(vec![endpoint.url.clone()], impatient(2)),
        );

        bus.emit(event("standup"));
        until("the event was never given up on", async || {
            delivery.undelivered() == 1
        })
        .await;
    }

    until("the abandoned event was left in the outbox", async || {
        owed_on_disk(directory.path()) == 0
    })
    .await;

    endpoint.answers(200);
    let tried_before = endpoint.received().len();

    let bus = Bus::new();
    let _delivery = delivery(
        &bus,
        directory.path(),
        &settings(vec![endpoint.url.clone()], impatient(2)),
    );
    bus.emit(event("payroll"));

    until("the new event never went out", async || {
        endpoint
            .received()
            .iter()
            .any(|request| request.json()["room"]["name"] == "payroll")
    })
    .await;
    assert_eq!(
        endpoint.received().len() - tried_before,
        1,
        "an event this server had already given up on was posted again by the next process, and \
         would be by every process after it"
    );
}

/// Two endpoints, one of which accepts the connection and then says nothing.
/// This is the failure that a "is it up" check never sees, and the one that
/// would take the whole pipeline with it if there were one queue rather than
/// one per destination.
#[tokio::test]
async fn a_destination_that_never_answers_does_not_hold_up_the_one_that_does() {
    let black_hole = Endpoint::start(SILENT).await;
    let working = Endpoint::start(200).await;
    let directory = tempfile::TempDir::new().unwrap();
    let bus = Bus::new();

    let _delivery = delivery(
        &bus,
        directory.path(),
        &settings(
            vec![black_hole.url.clone(), working.url.clone()],
            Retry {
                attempts: 5,
                backoff: Duration::from_secs(30),
                max_backoff: Duration::from_secs(30),
            },
        ),
    );

    bus.emit(event("standup"));
    bus.emit(event("payroll"));

    until(
        "the working endpoint waited behind the black hole",
        async || working.received().len() == 2,
    )
    .await;
    assert!(
        black_hole.connections() > 0,
        "the black hole was never even tried, so this test proves nothing about waiting for it"
    );
}

/// The hot path, measured. Emitting is called from the middle of somebody
/// joining a room, and every destination in this test is a socket that accepts
/// and never answers -- so if any part of delivery were being awaited by the
/// caller, this would take the endpoint timeout per event instead of
/// microseconds.
#[tokio::test]
async fn emitting_is_not_slowed_down_by_destinations_that_are_all_failing() {
    let first = Endpoint::start(SILENT).await;
    let second = Endpoint::start(SILENT).await;
    let directory = tempfile::TempDir::new().unwrap();
    let bus = Bus::new();

    let _delivery = delivery(
        &bus,
        directory.path(),
        &settings(vec![first.url.clone(), second.url.clone()], impatient(5)),
    );

    // Enough to overrun the queue depth several times over, so this measures
    // the path that throws an event away as well as the one that queues it.
    let started = Instant::now();
    for index in 0..2_000 {
        bus.emit(event(&format!("room-{index}")));
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "announcing two thousand events took {elapsed:?} with every destination hanging. \
         Somebody joining a room waits for that, and the room is the one thing that must never \
         wait for a webhook"
    );
    assert_eq!(bus.emitted(), 2_000);
}

/// A destination that falls far enough behind loses events, and the next one
/// it does take says how many.
///
/// `numDropped` is LiveKit's field for exactly this, and it is the difference
/// between a consumer knowing it has a hole and inferring one from a gap in
/// the timestamps -- which it cannot do, because a quiet room and a lost event
/// look identical from the outside.
#[tokio::test]
async fn a_destination_that_falls_behind_is_told_how_many_it_missed() {
    let endpoint = Endpoint::start(SILENT).await;
    let directory = tempfile::TempDir::new().unwrap();
    let bus = Bus::new();

    let delivery = delivery(
        &bus,
        directory.path(),
        &WebhookSettings {
            urls: vec![endpoint.url.clone()],
            api_key: API_KEY.to_owned(),
            secret: API_SECRET.to_owned(),
            // Long enough that the worker is still holding the first event
            // while the queue behind it fills and overflows.
            timeout: Duration::from_millis(1_500),
            retry: impatient(1),
        },
    );

    // The queue is eight deep, so most of these have nowhere to go.
    for index in 0..40 {
        bus.emit(event(&format!("room-{index}")));
    }

    until("no event was ever thrown away", async || {
        delivery.dropped() > 0
    })
    .await;
    let missed = delivery.dropped();
    assert!(
        missed >= 20,
        "only {missed} events were thrown away out of forty into a queue eight deep, so this \
         test is not measuring what it thinks it is"
    );

    endpoint.answers(200);
    until(
        "no event ever reported the ones that went missing",
        async || {
            endpoint
                .received()
                .iter()
                .any(|request| request.json().get("numDropped").is_some())
        },
    )
    .await;

    let reported = endpoint
        .received()
        .into_iter()
        .find_map(|request| request.json().get("numDropped").and_then(Value::as_u64))
        .expect("a numDropped");
    assert_eq!(
        reported, missed,
        "the consumer was told it missed {reported} events and it missed {missed}"
    );
}
