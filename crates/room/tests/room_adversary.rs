//! What an adversarial review of the room API found, kept as the tests that
//! would have caught it. Each of these passed against the broken behaviour
//! first; they assert the fixed behaviour now, and the messages say what a
//! person loses when one of them goes red again.

use std::ops::Deref;
use std::sync::Arc;

use mediacore_events::Bus;
use mediacore_proto::mediacore;
use mediacore_room::*;
use mediacore_session::SessionStore;

/// A registry with a session store of its own, and a directory that goes when
/// the test does. Every registry records what happens in its rooms somewhere,
/// so there is no registry without one.
struct Harness {
    registry: Registry,
    _records: tempfile::TempDir,
}

impl Deref for Harness {
    type Target = Registry;

    fn deref(&self) -> &Registry {
        &self.registry
    }
}

fn registry(defaults: RoomDefaults) -> Harness {
    let records = tempfile::TempDir::new().expect("a directory for session records");
    let sessions = SessionStore::files(records.path()).expect("an empty session store opens");

    Harness {
        registry: Registry::new(defaults, Arc::new(sessions), Arc::new(Bus::new())),
        _records: records,
    }
}

fn spec(identity: &str) -> ParticipantSpec {
    ParticipantSpec {
        identity: identity.to_owned(),
        name: identity.to_owned(),
        permission: mediacore::ParticipantPermission {
            can_subscribe: true,
            can_publish: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn leaves(rx: &mut SignalRx) -> Vec<mediacore::LeaveRequest> {
    let mut requests = Vec::new();
    while let Ok(response) = rx.try_recv() {
        if let Some(mediacore::signal_response::Message::Leave(request)) = response.message {
            requests.push(request);
        }
    }
    requests
}

fn updates(rx: &mut SignalRx) -> Vec<mediacore::ParticipantInfo> {
    let mut infos = Vec::new();
    while let Ok(response) = rx.try_recv() {
        if let Some(mediacore::signal_response::Message::Update(update)) = response.message {
            infos.extend(update.participants);
        }
    }
    infos
}

fn policy_metadata() -> RoomOptions {
    RoomOptions {
        metadata: r#"{"sessionPolicy":{"minHumans":2,"endGraceMs":90000}}"#.to_owned(),
        min_humans: Some(2),
        end_grace: Some(90),
        ..RoomOptions::default()
    }
}

/// The grace period is not something a room waits out only while somebody is
/// in it. A meeting that empties during the countdown ends on time, on nobody,
/// which is the whole reason the sweep has a clock of its own.
#[test]
fn a_grace_that_runs_out_on_an_empty_room_does_not_wait_for_the_next_person() {
    let registry = registry(RoomDefaults::default());
    let room = registry.create("meeting", policy_metadata(), Adoption::Always);

    let (alice_tx, _alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();
    let alice = registry.join("meeting", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("meeting", spec("bob"), bob_tx).unwrap();
    assert_eq!(room.human_count(), 2);
    assert_eq!(room.session_ends_at_ms(), None, "quorum, nothing ending");

    registry.leave("meeting", &bob.participant.sid);
    let armed = room.session_ends_at_ms().expect("the grace is armed");
    registry.leave("meeting", &alice.participant.sid);
    assert_eq!(
        room.session_ends_at_ms(),
        Some(armed),
        "the room is persistent, so it is still there and still counting down"
    );

    assert!(
        registry.sweep(armed - 1).is_empty(),
        "the room ended before the grace period it promised was over"
    );
    assert_eq!(
        registry.sweep(armed).ended,
        vec!["meeting".to_owned()],
        "an empty room's session has to end on time: the sweep is driven by a clock, not by \
         somebody being in the room to drive it"
    );
    assert!(registry.room("meeting").is_none());

    // Half an hour later, the second half of the meeting starts.
    let much_later = armed + 30 * 60 * 1000;
    let (carol_tx, mut carol_rx) = signal_channel();
    let carol = registry
        .join("meeting", spec("carol"), carol_tx)
        .expect("carol opens the meeting again");

    assert_eq!(
        carol.room.session_ends_at_ms(),
        None,
        "carol walked into a room carrying a deadline from a session that ended before she \
         arrived"
    );
    assert!(registry.sweep(much_later + 5_000).is_empty());
    assert!(
        leaves(&mut carol_rx).is_empty(),
        "carol was disconnected by a countdown that had nothing to do with her"
    );
}

/// Switching the policy off has to take the countdown with it, or a room is
/// destroyed under the people in it by a rule that no longer exists.
#[test]
fn switching_the_session_policy_off_calls_the_countdown_off() {
    let registry = registry(RoomDefaults::default());
    let room = registry.create("meeting", policy_metadata(), Adoption::Always);

    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, _bob_rx) = signal_channel();
    let _alice = registry.join("meeting", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("meeting", spec("bob"), bob_tx).unwrap();

    registry.leave("meeting", &bob.participant.sid);
    let armed = room.session_ends_at_ms().expect("the grace is armed");

    // The backoffice decides the meeting is open ended after all and sends
    // `{"sessionPolicy":{"minHumans":0}}` through UpdateRoomMetadata.
    registry.set_session_policy(&room, Some(0), None, None);
    assert_eq!(room.settings().min_humans, 0);
    assert_eq!(
        room.session_ends_at_ms(),
        None,
        "the policy is off and the countdown is still running"
    );

    assert!(
        registry.sweep(armed + 1).is_empty(),
        "a room with no session policy was ended by a deadline armed under the old one"
    );
    assert!(registry.room("meeting").is_some());
    assert!(
        leaves(&mut alice_rx).is_empty(),
        "alice was disconnected from a room whose policy was switched off"
    );
}

/// Hiding somebody has to reach the room, not only themselves: a client drops a
/// participant from its roster on a `DISCONNECTED` update and on nothing else.
#[test]
fn a_participant_hidden_by_the_api_leaves_everybody_elses_roster() {
    let registry = registry(RoomDefaults::default());
    let (alice_tx, mut alice_rx) = signal_channel();
    let (bob_tx, mut bob_rx) = signal_channel();
    let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
    let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
    while alice_rx.try_recv().is_ok() {}
    while bob_rx.try_recv().is_ok() {}

    alice
        .room
        .update_participant(
            "bob",
            ProfileUpdate {
                permission: Some(mediacore::ParticipantPermission {
                    can_subscribe: true,
                    can_publish: true,
                    hidden: true,
                    ..Default::default()
                }),
                ..ProfileUpdate::default()
            },
        )
        .expect("bob is in the room");

    let told = updates(&mut alice_rx);
    assert_eq!(
        told.len(),
        1,
        "alice was never told bob went hidden, so bob stays in her roster: {told:?}"
    );
    assert_eq!(told[0].identity, "bob");
    assert_eq!(
        told[0].state,
        mediacore::participant_info::State::Disconnected as i32,
        "a roster only ever loses somebody on a DISCONNECTED update; anything else leaves a tile \
         on the screen for a participant the room is not supposed to show"
    );

    let own = updates(&mut bob_rx);
    assert_eq!(
        own.len(),
        1,
        "bob is still in the room and has to be told what changed about him"
    );
    assert!(own[0].permission.as_ref().is_some_and(|it| it.hidden));

    registry.leave("demo", &bob.participant.sid);
    assert!(
        updates(&mut alice_rx).is_empty(),
        "bob was already gone from alice's roster; telling her again would put him back in it"
    );
}

/// `#` and digits are not a reserved namespace, so the suffix cannot be read
/// back as one. Two tokens are the same person only when they asked to be
/// called the same thing.
#[test]
fn a_token_for_bob_cannot_seize_the_session_of_the_person_called_bob_hash_7() {
    let registry = registry(RoomDefaults::default());

    // Two different people, both named by the backoffice, both legitimate.
    let (victim_tx, mut victim_rx) = signal_channel();
    let victim = registry.join("demo", spec("bob#7"), victim_tx).unwrap();
    let (bob_tx, _bob_rx) = signal_channel();
    let _bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

    // bob#7's participant sid is in every roster the room sends out, so bob has it.
    let stolen = victim.participant.sid.clone();

    let (attacker_tx, _attacker_rx) = signal_channel();
    let seized = registry
        .join(
            "demo",
            ParticipantSpec {
                replaces: Some(stolen.clone()),
                ..spec("bob")
            },
            attacker_tx,
        )
        .expect("the join itself is nobody's business to refuse");

    assert_eq!(
        seized.participant.identity, "bob#2",
        "a token whose sub is `bob` was seated in the room as `bob#7`: sids are broadcast to the \
         whole room, so claiming one is impersonation anybody in the meeting can do"
    );
    assert!(
        leaves(&mut victim_rx).is_empty(),
        "the real bob#7 was thrown out of the room by somebody else's reconnect"
    );
    assert_eq!(
        seized.room.participant("bob#7").expect("bob#7 is here").sid,
        stolen,
        "bob#7's session is still bob#7's"
    );
}

/// The other half of the same invariant: a moderator removing `bob` means the
/// person whose token says `bob`, however many tabs they have open, and never
/// the person the backoffice called `bob#7`.
#[test]
fn removing_bob_does_not_reach_the_person_called_bob_hash_7() {
    let registry = registry(RoomDefaults::default());
    let (first_tx, _first_rx) = signal_channel();
    let (second_tx, _second_rx) = signal_channel();
    let (victim_tx, _victim_rx) = signal_channel();

    let first = registry.join("demo", spec("bob"), first_tx).unwrap();
    let second = registry.join("demo", spec("bob"), second_tx).unwrap();
    registry.join("demo", spec("bob#7"), victim_tx).unwrap();

    assert_eq!(second.participant.identity, "bob#2");

    let named: Vec<String> = first
        .room
        .participants_of("bob")
        .into_iter()
        .map(|participant| participant.identity.clone())
        .collect();
    assert_eq!(
        named,
        vec!["bob".to_owned(), "bob#2".to_owned()],
        "removing bob has to take both of bob's tabs and nobody else's session with them"
    );

    let victim: Vec<String> = first
        .room
        .participants_of("bob#7")
        .into_iter()
        .map(|participant| participant.identity.clone())
        .collect();
    assert_eq!(
        victim,
        vec!["bob#7".to_owned()],
        "a name a token asked for names that person, whatever it looks like"
    );
}
