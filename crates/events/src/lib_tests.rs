
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
