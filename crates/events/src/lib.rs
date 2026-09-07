//! What happened, told to whoever is listening.
//!
//! An event is a **notification that state changed, and not the state**. The
//! record of a meeting is in `mediacore-session`, on a disk, and it is the
//! copy that answers questions afterwards; nothing here is ever the only copy
//! of a fact. That ordering is the whole design: a listener that was down at
//! half past six missed the event and can still ask what it missed, so an
//! event is allowed to be a best effort in a way a record is not.
//!
//! Three deliveries, and they are not alternatives.
//!
//! * **In process.** [`Bus::listen`] hands out a queue and [`Bus::emit`] puts
//!   every event into every queue. The queue is unbounded, so emitting never
//!   waits for a consumer and no event is ever dropped in front of one --
//!   which is what a consumer inside this process, recording for instance,
//!   needs and a network consumer cannot have.
//! * **Webhooks**, in LiveKit's shape and with LiveKit's signing, so a
//!   deployment already listening to LiveKit's webhooks does not know the
//!   difference. See [`webhook`].
//! * **Kafka**, behind the `kafka` feature, off by default. See [`kafka`].
//!
//! The two network deliveries go through [`Delivery`], which is itself just
//! another in-process listener -- and through [`Outbox`] before that, because
//! an event that is sent and lost is worse than one that was never promised. A
//! two minute broker outage would otherwise drop every meeting in those two
//! minutes, and nothing anywhere would say so.
//!
//! **Nothing here may sit on the hot path.** [`Bus::emit`] takes a lock, pushes
//! into unbounded queues and returns; it opens no file, makes no request and
//! never awaits. Everything slow -- the outbox write, the request, the retry,
//! the broker -- happens on tasks the participant paths do not touch, so a
//! webhook endpoint that is a black hole cannot delay somebody joining a room.
//! `crates/events/tests/delivery.rs` and `crates/server/tests/events.rs` are
//! the tests that go red if that stops being true.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mediacore_proto::mediacore;
use mediacore_session::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

pub mod delivery;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod log;
pub mod outbox;
pub mod webhook;

pub use delivery::{Delivery, DeliveryError, Retry, Sink, Target};
pub use outbox::Outbox;
pub use webhook::{Webhook, WebhookSettings};

/// Whether this binary was built with a Kafka producer in it.
///
/// The server reads this to refuse a config that names a broker on a build
/// that cannot reach one. Silently ignoring such a config would be a
/// deployment that believes its bus is fed and is not, which is exactly the
/// failure the outbox and the records exist to make impossible.
pub const KAFKA: bool = cfg!(feature = "kafka");

/// The prefix every event id carries, in the shape of the other ids on this
/// server -- and of LiveKit's, whose webhook events are `EV_` too, so a
/// consumer that keys on the id to throw duplicates away is not surprised.
pub const EVENT_ID_PREFIX: &str = "EV_";

/// What happened.
///
/// The first four are LiveKit's own names, spelled the way LiveKit spells
/// them, because a consumer that already listens to a LiveKit deployment
/// matches on these strings. The last three are for the concepts LiveKit does
/// not have: a room is a place and a session is the meeting that happened in
/// it, and `room_started` fires when the place is made, which is not when the
/// meeting began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    RoomStarted,
    RoomFinished,
    ParticipantJoined,
    ParticipantLeft,
    SessionStarted,
    SessionFinished,
    /// One participant's audio from one meeting is in the object store and can
    /// be fetched.
    ///
    /// Deliberately not `recording_finished`: what this says is not that a
    /// recorder stopped, it is that an object exists. The two are minutes apart
    /// -- the file is closed when the participant stops publishing and the
    /// object appears when the upload finishes -- and a consumer that fetched
    /// on the first one would find nothing there.
    RecordingReady,
}

impl Kind {
    /// The name on the wire. This is the string a consumer matches on, so it
    /// is spelled once, here.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoomStarted => "room_started",
            Self::RoomFinished => "room_finished",
            Self::ParticipantJoined => "participant_joined",
            Self::ParticipantLeft => "participant_left",
            Self::SessionStarted => "session_started",
            Self::SessionFinished => "session_finished",
            Self::RecordingReady => "recording_ready",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One thing that happened, with enough of the state attached to be useful
/// without a call back to this server.
///
/// The payloads are the wire types the room service already answers with, so
/// what a `participant_joined` carries is what `ListParticipants` would have
/// said about that person -- one shape for a consumer to learn, and one that
/// changes when the schema does rather than drifting away from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub kind: Kind,
    /// When it happened, in milliseconds. LiveKit's `createdAt` is in whole
    /// seconds and that is what goes on the wire; the milliseconds are kept
    /// here because two events inside one second are ordinary and the outbox
    /// has to be able to say which came first.
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<mediacore::Room>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participant: Option<mediacore::ParticipantInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<Ready>,
}

impl Event {
    fn new(kind: Kind) -> Self {
        Self {
            id: generate_id(),
            kind,
            created_at_ms: now_ms(),
            room: None,
            participant: None,
            session: None,
            recording: None,
        }
    }

    /// A room came into being -- because somebody created it, or because the
    /// first person to arrive opened it.
    pub fn room_started(room: mediacore::Room) -> Self {
        Self {
            room: Some(room),
            ..Self::new(Kind::RoomStarted)
        }
    }

    /// A room went away: deleted, swept up empty, or thrown away with the last
    /// person to leave it.
    pub fn room_finished(room: mediacore::Room) -> Self {
        Self {
            room: Some(room),
            ..Self::new(Kind::RoomFinished)
        }
    }

    pub fn participant_joined(
        room: mediacore::Room,
        participant: mediacore::ParticipantInfo,
    ) -> Self {
        Self {
            room: Some(room),
            participant: Some(participant),
            ..Self::new(Kind::ParticipantJoined)
        }
    }

    pub fn participant_left(
        room: mediacore::Room,
        participant: mediacore::ParticipantInfo,
    ) -> Self {
        Self {
            room: Some(room),
            participant: Some(participant),
            ..Self::new(Kind::ParticipantLeft)
        }
    }

    /// A meeting began: the room reached the number of people it needs.
    ///
    /// No `room` on this one, and none on [`Event::session_finished`] either.
    /// A meeting outlives the room it happened in -- a room nobody created is
    /// gone the moment the last person leaves, and the meeting they had is not
    /// -- so a `room` field would be present for some of these and absent for
    /// others, which is a worse contract than never having one. The record
    /// carries the room's name, which is the part of a room that was true
    /// throughout.
    pub fn session_started(session: Session) -> Self {
        Self {
            session: Some(session),
            ..Self::new(Kind::SessionStarted)
        }
    }

    /// A meeting ended: nobody came back before the grace period ran out, or
    /// the room was taken away under it.
    pub fn session_finished(session: Session) -> Self {
        Self {
            session: Some(session),
            ..Self::new(Kind::SessionFinished)
        }
    }

    /// One recording is in the object store.
    pub fn recording_ready(recording: Ready) -> Self {
        Self {
            recording: Some(recording),
            ..Self::new(Kind::RecordingReady)
        }
    }

    /// The name of the room this is about, for a log line and for a Kafka
    /// partition key.
    pub fn room_name(&self) -> &str {
        if let Some(room) = &self.room {
            return &room.name;
        }
        if let Some(session) = &self.session {
            return &session.room;
        }
        if let Some(recording) = &self.recording {
            return &recording.room;
        }

        ""
    }

    /// The bytes that go on the wire, in LiveKit's `WebhookEvent` shape.
    ///
    /// This is protobuf's own JSON mapping as `protojson` produces it and as
    /// LiveKit's receiver parses it: fields in field-number order, absent
    /// fields left out rather than sent as nulls, and a 64-bit integer as a
    /// **string** -- which is why `createdAt` is quoted and why the room's
    /// `creationTime` inside it is too. The nested objects are serialised by
    /// the codecs `crates/proto` generates from the schema, so they cannot
    /// drift from what the room service answers.
    ///
    /// Where this differs from LiveKit, and it is the only place: a
    /// `session_started` or `session_finished` carries a `session` object,
    /// which LiveKit's message has no field for. A receiver built on
    /// `protojson.Unmarshal` without `DiscardUnknown` will refuse to parse
    /// those two -- and they are two event names it has no case for anyway, so
    /// the extra field is only ever on the events such a receiver was going to
    /// ignore. The four events it does know are byte for byte what its own
    /// server would have sent.
    ///
    /// `dropped` is how many events for this destination were thrown away in
    /// front of this one. LiveKit's field, LiveKit's meaning; see
    /// [`Delivery`] for when this server throws one away.
    pub fn payload(&self, dropped: u32) -> Vec<u8> {
        let mut body = Map::new();
        body.insert(
            "event".to_owned(),
            Value::String(self.kind.as_str().to_owned()),
        );

        if let Some(room) = &self.room {
            body.insert("room".to_owned(), encoded(room));
        }
        if let Some(participant) = &self.participant {
            body.insert("participant".to_owned(), encoded(participant));
        }

        body.insert("id".to_owned(), Value::String(self.id.clone()));
        body.insert(
            "createdAt".to_owned(),
            Value::String((self.created_at_ms / 1_000).to_string()),
        );

        if dropped > 0 {
            body.insert("numDropped".to_owned(), Value::from(dropped));
        }
        if let Some(session) = &self.session {
            body.insert("session".to_owned(), encoded(session));
        }
        if let Some(recording) = &self.recording {
            body.insert("recording".to_owned(), encoded(recording));
        }

        serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
    }
}

/// A payload object, or `null` if it somehow could not be encoded.
///
/// These are structs of strings, numbers and vectors of the same, so there is
/// no value they hold that JSON cannot express and this never takes the second
/// branch. It exists so that the one thing an encoder could do to a delivery
/// -- fail it -- is not a thing it can do.
fn encoded<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// One participant's audio from one meeting, in the object store.
///
/// Written out field by field rather than by embedding the store's own
/// `Recording`, for two reasons. The row carries `localPath`, which names a
/// file on one node's disk and is of no use and some risk to anybody outside
/// this server; and an event is a contract with consumers, so it should not
/// change every time a column does.
///
/// Everything a consumer needs to fetch the object and know what it is: where
/// it is, which meeting it belongs to, whose voice it is, and where it sits in
/// the meeting. Everything here is also queryable at
/// `GET /api/sessions/<id>/recordings`, which is what a consumer that was down
/// when this was sent asks instead -- an event notifies, it is never the only
/// copy of a fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ready {
    /// `RE_` and twelve hex digits.
    pub id: String,
    /// The meeting, as `GET /api/sessions/<id>` spells it. Never the room: a
    /// room holds another meeting tomorrow and this is not a recording of that
    /// one.
    pub session: String,
    /// The room's name, for a consumer that routes by it.
    pub room: String,
    /// The participant's sid, which is what the meeting's attendance list is
    /// keyed by, so this recording can be lined up with the stretch it was made
    /// during.
    pub participant: String,
    /// What the room called them: `alice`, or `alice#2` for a second tab.
    pub identity: String,
    /// Which file of this participant's this is, from zero. More than one means
    /// they left and came back, or they talked for long enough that the file
    /// was rolled over.
    pub segment: u32,
    pub bucket: String,
    pub object_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// `audio/ogg`, Opus inside it, exactly as it came off the wire -- nothing
    /// in this server decodes it.
    pub content_type: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// When the recording started and stopped, on the same clock as the
    /// meeting.
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    /// Where this file starts inside the meeting, in milliseconds from the
    /// moment the meeting did. This is what lines several participants' files up
    /// against one another.
    pub offset_ms: i64,
    /// How long the audio is, silences included: a participant who said nothing
    /// for two minutes has two minutes of file, because the length comes from
    /// their own sender's clock and not from the bytes written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// Where events are announced, and the only thing the room layer knows about
/// any of this.
///
/// Listeners register once, at startup, and are handed a queue of their own.
/// Emitting pushes a shared pointer into each queue and returns: it takes no
/// I/O, waits for nothing and cannot fail, which is what lets it be called
/// from the middle of somebody joining a room.
///
/// The queues are **unbounded**, deliberately. A bounded queue drops, and the
/// consumers here are inside this process -- the delivery machinery below,
/// and recording later -- where dropping is the failure the outbox exists to
/// prevent. The cost is memory if a consumer stops draining, and the consumer
/// that could is this crate's own, which drains by writing a small file and
/// handing on. The first thing it does with an event is make it durable, so
/// the queue is only ever as long as the disk is slow.
#[derive(Default)]
pub struct Bus {
    listeners: Mutex<Vec<mpsc::UnboundedSender<Arc<Event>>>>,
    emitted: AtomicU64,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// A queue of every event emitted from now on.
    ///
    /// From now on, not from the beginning: a listener that registers after
    /// the server has been running sees what happens next. Everything that
    /// registers does so at startup, before a socket is open.
    pub fn listen(&self) -> Listener {
        let (tx, rx) = mpsc::unbounded_channel();
        self.listeners.lock().expect("event listeners").push(tx);

        Listener { events: rx }
    }

    /// Announces one event to every listener. Never blocks, never fails.
    pub fn emit(&self, event: Event) {
        let event = Arc::new(event);
        self.emitted.fetch_add(1, Ordering::Relaxed);

        // A send fails only when the receiving half has been dropped, which
        // means that listener is gone for good -- so the failure is also how
        // the list is kept from growing a dead entry per departed consumer.
        self.listeners
            .lock()
            .expect("event listeners")
            .retain(|listener| listener.send(Arc::clone(&event)).is_ok());
    }

    /// How many events this server has announced since it started.
    pub fn emitted(&self) -> u64 {
        self.emitted.load(Ordering::Relaxed)
    }
}

/// One consumer's queue of events, in the order they were emitted.
pub struct Listener {
    events: mpsc::UnboundedReceiver<Arc<Event>>,
}

impl Listener {
    /// The next event, or `None` once the bus itself is gone.
    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        self.events.recv().await
    }

    /// The next event if one is already waiting. For a consumer that is not a
    /// task -- a test, mostly.
    pub fn try_recv(&mut self) -> Option<Arc<Event>> {
        self.events.try_recv().ok()
    }
}

fn generate_id() -> String {
    format!(
        "{EVENT_ID_PREFIX}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(name: &str) -> mediacore::Room {
        mediacore::Room {
            sid: "RM_000000000000".to_owned(),
            name: name.to_owned(),
            creation_time: 1_757_155_200,
            num_participants: 2,
            ..Default::default()
        }
    }

    fn session(id: &str) -> Session {
        Session {
            id: id.to_owned(),
            room: "standup".to_owned(),
            started_at_ms: 1_757_155_200_000,
            ended_at_ms: None,
            ended_by: None,
            participants: Vec::new(),
            metadata: None,
        }
    }

    /// The reason the in-process emitter is not a broadcast channel. A
    /// broadcast channel that a consumer falls behind on drops the oldest
    /// events and tells the consumer a number; the consumer inside this
    /// process that will hang off this is recording, and "the first ninety
    /// seconds are missing" is not a recording.
    #[test]
    fn a_listener_that_is_not_draining_loses_nothing() {
        let bus = Bus::new();
        let mut listener = bus.listen();

        for index in 0..10_000 {
            bus.emit(Event::room_started(room(&format!("room-{index}"))));
        }

        let seen: Vec<String> = std::iter::from_fn(|| listener.try_recv())
            .map(|event| event.room_name().to_owned())
            .collect();

        assert_eq!(
            seen.len(),
            10_000,
            "a listener that had not got round to reading yet lost events, so nothing in this \
             process can be built on hearing all of them"
        );
        assert_eq!(seen[0], "room-0", "the events arrived out of order");
        assert_eq!(seen[9_999], "room-9999");
        assert_eq!(bus.emitted(), 10_000);
    }

    #[test]
    fn every_listener_hears_every_event() {
        let bus = Bus::new();
        let mut first = bus.listen();
        let mut second = bus.listen();

        bus.emit(Event::room_started(room("standup")));

        assert_eq!(
            first.try_recv().expect("the first listener").kind,
            Kind::RoomStarted
        );
        assert_eq!(
            second
                .try_recv()
                .expect("the second listener heard nothing")
                .kind,
            Kind::RoomStarted,
            "one listener took the event away from the other"
        );
    }

    /// A consumer that goes away must not cost the server anything, because
    /// the server carries on emitting whether anybody is listening or not.
    #[test]
    fn a_listener_that_goes_away_is_forgotten() {
        let bus = Bus::new();
        drop(bus.listen());

        bus.emit(Event::room_started(room("standup")));
        bus.emit(Event::room_started(room("standup")));

        assert!(
            bus.listeners.lock().unwrap().is_empty(),
            "a departed listener left its queue behind, so a server that hands out one per \
             restart of a subsystem grows a queue per restart"
        );
    }

    /// The four LiveKit events have to be what LiveKit's own server would have
    /// sent, down to the quotes around a 64-bit integer. A consumer parsing
    /// these with a generated LiveKit client is the entire point.
    #[test]
    fn a_livekit_event_is_shaped_the_way_protojson_shapes_one() {
        let event = Event::participant_joined(
            room("standup"),
            mediacore::ParticipantInfo {
                sid: "PA_000000000000".to_owned(),
                identity: "alice".to_owned(),
                ..Default::default()
            },
        );
        let body: Value = serde_json::from_slice(&event.payload(0)).expect("the payload is json");

        assert_eq!(body["event"], "participant_joined");
        assert_eq!(body["room"]["name"], "standup");
        assert_eq!(
            body["room"]["creationTime"], "1757155200",
            "protobuf's json mapping sends a 64-bit integer as a string, and a generated LiveKit \
             client refuses a number where it expects one"
        );
        assert_eq!(body["participant"]["identity"], "alice");
        assert!(body["id"].as_str().unwrap().starts_with(EVENT_ID_PREFIX));
        assert!(
            body["createdAt"].is_string(),
            "createdAt is an int64 and went out as a number: {}",
            body["createdAt"]
        );
        assert!(
            body.get("numDropped").is_none(),
            "a field at its default value was sent, which protojson never does and a strict \
             receiver does not expect"
        );
        assert!(
            body.get("session").is_none(),
            "a LiveKit event carried a field LiveKit has no place for, so a receiver that \
             refuses unknown fields refuses an event its own server would have sent"
        );
    }

    #[test]
    fn a_session_event_carries_the_meeting_and_no_room() {
        let event = Event::session_finished(session("SE_000000000000"));
        let body: Value = serde_json::from_slice(&event.payload(0)).expect("the payload is json");

        assert_eq!(body["event"], "session_finished");
        assert_eq!(body["session"]["id"], "SE_000000000000");
        assert_eq!(
            body["session"]["room"], "standup",
            "a meeting outlives its room, so the room's name has to be in the meeting"
        );
        assert!(body.get("room").is_none());
    }

    fn ready() -> Ready {
        Ready {
            id: "RE_0123456789ab".to_owned(),
            session: "SE_000000000000".to_owned(),
            room: "standup".to_owned(),
            participant: "PA_000000000000".to_owned(),
            identity: "alice".to_owned(),
            segment: 0,
            bucket: "meetings".to_owned(),
            object_key: "SE_000000000000/PA_000000000000-0.ogg".to_owned(),
            bytes: Some(6_291_456),
            content_type: "audio/ogg".to_owned(),
            sample_rate: 48_000,
            channels: 1,
            started_at_ms: 1_757_155_200_000,
            ended_at_ms: Some(1_757_156_740_000),
            offset_ms: 0,
            duration_ms: Some(1_540_000),
        }
    }

    /// A consumer has to be able to fetch the object and know what it is
    /// without calling back, and has to be told the room, which a recording
    /// row does not carry.
    #[test]
    fn a_recording_event_says_where_the_object_is_and_what_it_is_of() {
        let event = Event::recording_ready(ready());
        let body: Value = serde_json::from_slice(&event.payload(0)).expect("the payload is json");

        assert_eq!(body["event"], "recording_ready");
        assert_eq!(body["recording"]["bucket"], "meetings");
        assert_eq!(
            body["recording"]["objectKey"],
            "SE_000000000000/PA_000000000000-0.ogg"
        );
        assert_eq!(body["recording"]["session"], "SE_000000000000");
        assert_eq!(
            body["recording"]["room"], "standup",
            "a consumer that routes by room has nothing else to route on: a recording row names \
             the meeting and the participant's sid and no room at all"
        );
        assert_eq!(body["recording"]["identity"], "alice");
        assert_eq!(body["recording"]["durationMs"], 1_540_000);
        assert!(
            body["recording"].get("localPath").is_none(),
            "the path on one node's disk is of no use outside this server and some risk in it, so \
             the event is built field by field rather than by handing over the row"
        );
        assert!(
            body["createdAt"]
                .as_str()
                .is_some_and(|seconds| seconds.parse::<u64>().is_ok()),
            "the envelope stays LiveKit's, including a 64-bit field travelling as a quoted \
             string; a receiver built on protojson refuses a number there"
        );
    }

    #[test]
    fn a_recording_event_names_the_room_it_partitions_by() {
        assert_eq!(
            Event::recording_ready(ready()).room_name(),
            "standup",
            "a broker partitions on this, and an event with no key goes wherever it likes -- so \
             one meeting's recordings would arrive out of order with the meeting's own events"
        );
    }

    #[test]
    fn events_thrown_away_in_front_of_one_are_counted_on_it() {
        let event = Event::room_finished(room("standup"));
        let body: Value = serde_json::from_slice(&event.payload(7)).expect("the payload is json");

        assert_eq!(
            body["numDropped"], 7,
            "a destination was told nothing about the events it did not get"
        );
    }

    #[test]
    fn an_event_survives_the_outbox_round_trip() {
        let event = Event::participant_left(
            room("standup"),
            mediacore::ParticipantInfo {
                sid: "PA_000000000000".to_owned(),
                identity: "alice".to_owned(),
                ..Default::default()
            },
        );

        let written = serde_json::to_vec(&event).expect("an event encodes");
        let read: Event = serde_json::from_slice(&written).expect("an event decodes");

        assert_eq!(
            read, event,
            "an event came back from the outbox as something other than what went in, so a \
             restart delivers a different event from the one that was promised"
        );
    }
}
