use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mediacore_proto::livekit;
use tokio::sync::mpsc;

pub const SIGNAL_CHANNEL_CAPACITY: usize = 256;

pub type SignalTx = mpsc::Sender<livekit::SignalResponse>;
pub type SignalRx = mpsc::Receiver<livekit::SignalResponse>;

pub fn signal_channel() -> (SignalTx, SignalRx) {
    mpsc::channel(SIGNAL_CHANNEL_CAPACITY)
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("room `{0}` does not exist")]
    RoomNotFound(String),
    #[error("room `{0}` is full")]
    RoomFull(String),
}

impl JoinError {
    pub fn disconnect_reason(&self) -> livekit::DisconnectReason {
        livekit::DisconnectReason::JoinFailure
    }
}

#[derive(Debug, Clone)]
pub struct RoomDefaults {
    pub auto_create: bool,
    pub empty_timeout: u32,
    pub departure_timeout: u32,
    pub max_participants: u32,
}

impl Default for RoomDefaults {
    fn default() -> Self {
        Self {
            auto_create: true,
            empty_timeout: 300,
            departure_timeout: 20,
            max_participants: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParticipantSpec {
    pub identity: String,
    pub name: String,
    pub metadata: String,
    pub attributes: HashMap<String, String>,
    pub permission: livekit::ParticipantPermission,
}

pub struct Participant {
    pub sid: String,
    pub identity: String,
    pub name: String,
    pub metadata: String,
    pub attributes: HashMap<String, String>,
    pub permission: livekit::ParticipantPermission,
    pub joined_at_ms: i64,
    version: AtomicU32,
    tx: SignalTx,
}

impl Participant {
    pub fn to_proto(&self, state: livekit::participant_info::State) -> livekit::ParticipantInfo {
        livekit::ParticipantInfo {
            sid: self.sid.clone(),
            identity: self.identity.clone(),
            state: state as i32,
            metadata: self.metadata.clone(),
            joined_at: self.joined_at_ms / 1000,
            joined_at_ms: self.joined_at_ms,
            name: self.name.clone(),
            version: self.version_counter(),
            permission: Some(self.permission.clone()),
            kind: livekit::participant_info::Kind::Standard as i32,
            attributes: self.attributes.clone(),
            ..Default::default()
        }
    }

    pub fn send(&self, message: livekit::signal_response::Message) -> bool {
        let response = livekit::SignalResponse {
            message: Some(message),
        };

        match self.tx.try_send(response) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    identity = %self.identity,
                    sid = %self.sid,
                    "signal queue is full, dropping message"
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn disconnect(&self, reason: livekit::DisconnectReason) {
        self.send(leave(reason));
    }

    pub fn bump_version(&self) -> u32 {
        self.version.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn version_counter(&self) -> u32 {
        self.version.load(Ordering::Relaxed)
    }
}

pub struct Room {
    pub sid: String,
    pub name: String,
    pub created_at_ms: i64,
    defaults: RoomDefaults,
    participants: Mutex<HashMap<String, Arc<Participant>>>,
}

impl Room {
    fn new(name: String, defaults: RoomDefaults) -> Self {
        Self {
            sid: generate_sid("RM"),
            name,
            created_at_ms: now_ms(),
            defaults,
            participants: Mutex::new(HashMap::new()),
        }
    }

    pub fn to_proto(&self) -> livekit::Room {
        livekit::Room {
            sid: self.sid.clone(),
            name: self.name.clone(),
            empty_timeout: self.defaults.empty_timeout,
            departure_timeout: self.defaults.departure_timeout,
            max_participants: self.defaults.max_participants,
            creation_time: self.created_at_ms / 1000,
            creation_time_ms: self.created_at_ms,
            num_participants: self.participant_count() as u32,
            ..Default::default()
        }
    }

    pub fn participant_count(&self) -> usize {
        self.participants.lock().expect("room lock").len()
    }

    pub fn participants(&self) -> Vec<Arc<Participant>> {
        self.participants
            .lock()
            .expect("room lock")
            .values()
            .cloned()
            .collect()
    }

    pub fn participant(&self, identity: &str) -> Option<Arc<Participant>> {
        self.participants
            .lock()
            .expect("room lock")
            .get(identity)
            .cloned()
    }

    pub fn broadcast(&self, message: livekit::signal_response::Message, except: Option<&str>) {
        for participant in self.participants() {
            if Some(participant.sid.as_str()) == except {
                continue;
            }
            participant.send(message.clone());
        }
    }
}

pub struct Joined {
    pub room: Arc<Room>,
    pub participant: Arc<Participant>,
    pub others: Vec<livekit::ParticipantInfo>,
}

#[derive(Default)]
pub struct Registry {
    defaults: RoomDefaults,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
}

impl Registry {
    pub fn new(defaults: RoomDefaults) -> Self {
        Self {
            defaults,
            rooms: Mutex::new(HashMap::new()),
        }
    }

    pub fn rooms(&self) -> Vec<Arc<Room>> {
        self.rooms
            .lock()
            .expect("registry lock")
            .values()
            .cloned()
            .collect()
    }

    pub fn room(&self, name: &str) -> Option<Arc<Room>> {
        self.rooms.lock().expect("registry lock").get(name).cloned()
    }

    pub fn join(
        &self,
        room_name: &str,
        spec: ParticipantSpec,
        tx: SignalTx,
    ) -> Result<Joined, JoinError> {
        let (room, participant, displaced) = {
            let mut rooms = self.rooms.lock().expect("registry lock");

            let room = match rooms.get(room_name) {
                Some(room) => Arc::clone(room),
                None if self.defaults.auto_create => {
                    let room = Arc::new(Room::new(room_name.to_owned(), self.defaults.clone()));
                    rooms.insert(room_name.to_owned(), Arc::clone(&room));
                    tracing::info!(room = %room_name, sid = %room.sid, "room created");
                    room
                }
                None => return Err(JoinError::RoomNotFound(room_name.to_owned())),
            };

            let mut participants = room.participants.lock().expect("room lock");
            let replaces = participants.contains_key(&spec.identity);

            if !replaces
                && self.defaults.max_participants > 0
                && participants.len() >= self.defaults.max_participants as usize
            {
                return Err(JoinError::RoomFull(room_name.to_owned()));
            }

            let participant = Arc::new(Participant {
                sid: generate_sid("PA"),
                identity: spec.identity.clone(),
                name: spec.name,
                metadata: spec.metadata,
                attributes: spec.attributes,
                permission: spec.permission,
                joined_at_ms: now_ms(),
                version: AtomicU32::new(if replaces { 2 } else { 1 }),
                tx,
            });
            let displaced = participants.insert(spec.identity, Arc::clone(&participant));
            drop(participants);

            (room, participant, displaced)
        };

        if let Some(displaced) = displaced {
            tracing::info!(
                room = %room_name,
                identity = %displaced.identity,
                sid = %displaced.sid,
                "displacing an earlier session with the same identity"
            );
            displaced.disconnect(livekit::DisconnectReason::DuplicateIdentity);
        }

        let others = room
            .participants()
            .into_iter()
            .filter(|other| other.sid != participant.sid)
            .filter(|other| !other.permission.hidden)
            .map(|other| other.to_proto(livekit::participant_info::State::Active))
            .collect();

        if !participant.permission.hidden {
            room.broadcast(
                participant_update(vec![
                    participant.to_proto(livekit::participant_info::State::Joined),
                ]),
                Some(&participant.sid),
            );
        }

        Ok(Joined {
            room,
            participant,
            others,
        })
    }

    pub fn leave(&self, room_name: &str, sid: &str) {
        let (room, departed) = {
            let mut rooms = self.rooms.lock().expect("registry lock");
            let Some(room) = rooms.get(room_name).cloned() else {
                return;
            };

            let departed = {
                let mut participants = room.participants.lock().expect("room lock");
                let identity = participants
                    .iter()
                    .find(|(_, participant)| participant.sid == sid)
                    .map(|(identity, _)| identity.clone());

                match identity {
                    Some(identity) => participants.remove(&identity),
                    None => None,
                }
            };

            if departed.is_some() && room.participant_count() == 0 {
                rooms.remove(room_name);
                tracing::info!(room = %room_name, "room closed, no participants left");
            }

            (room, departed)
        };

        let Some(departed) = departed else {
            return;
        };

        if !departed.permission.hidden {
            let mut info = departed.to_proto(livekit::participant_info::State::Disconnected);
            info.disconnect_reason = livekit::DisconnectReason::ClientInitiated as i32;
            room.broadcast(participant_update(vec![info]), None);
        }
    }
}

pub fn participant_update(
    participants: Vec<livekit::ParticipantInfo>,
) -> livekit::signal_response::Message {
    livekit::signal_response::Message::Update(livekit::ParticipantUpdate { participants })
}

pub fn leave(reason: livekit::DisconnectReason) -> livekit::signal_response::Message {
    livekit::signal_response::Message::Leave(livekit::LeaveRequest {
        can_reconnect: false,
        reason: reason as i32,
        action: livekit::leave_request::Action::Disconnect as i32,
        regions: None,
    })
}

fn generate_sid(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(identity: &str) -> ParticipantSpec {
        ParticipantSpec {
            identity: identity.to_owned(),
            name: identity.to_owned(),
            permission: livekit::ParticipantPermission {
                can_subscribe: true,
                can_publish: true,
                can_publish_data: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn expect_update(rx: &mut SignalRx) -> livekit::ParticipantUpdate {
        match rx.try_recv().expect("a signal was queued").message {
            Some(livekit::signal_response::Message::Update(update)) => update,
            other => panic!("expected a participant update, got {other:?}"),
        }
    }

    #[test]
    fn second_participant_sees_the_first() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        assert_eq!(bob.others.len(), 1);
        assert_eq!(bob.others[0].identity, "alice");
        assert_eq!(bob.room.participant_count(), 2);
        assert_eq!(expect_update(&mut alice_rx).participants[0].identity, "bob");
    }

    #[test]
    fn leaving_notifies_others_and_closes_empty_room() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        let _ = alice_rx.try_recv();

        registry.leave("demo", &bob.participant.sid);

        let update = expect_update(&mut alice_rx);
        assert_eq!(
            update.participants[0].state,
            livekit::participant_info::State::Disconnected as i32
        );

        registry.leave("demo", &alice.participant.sid);
        assert!(registry.room("demo").is_none());
    }

    #[test]
    fn rejoining_displaces_the_earlier_session() {
        let registry = Registry::new(RoomDefaults::default());
        let (first_tx, mut first_rx) = signal_channel();
        let (second_tx, _second_rx) = signal_channel();

        let first = registry.join("demo", spec("alice"), first_tx).unwrap();
        let second = registry.join("demo", spec("alice"), second_tx).unwrap();

        assert_ne!(first.participant.sid, second.participant.sid);
        assert_eq!(second.room.participant_count(), 1);

        match first_rx.try_recv().expect("a signal was queued").message {
            Some(livekit::signal_response::Message::Leave(request)) => {
                assert_eq!(
                    request.reason,
                    livekit::DisconnectReason::DuplicateIdentity as i32
                );
            }
            other => panic!("expected a leave request, got {other:?}"),
        }
    }

    #[test]
    fn a_displaced_session_cannot_evict_its_successor() {
        let registry = Registry::new(RoomDefaults::default());
        let (first_tx, _first_rx) = signal_channel();
        let (second_tx, _second_rx) = signal_channel();

        let first = registry.join("demo", spec("alice"), first_tx).unwrap();
        let second = registry.join("demo", spec("alice"), second_tx).unwrap();

        registry.leave("demo", &first.participant.sid);

        let room = registry.room("demo").expect("room is still open");
        assert_eq!(room.participant_count(), 1);
        assert_eq!(
            room.participant("alice").unwrap().sid,
            second.participant.sid
        );
    }

    #[test]
    fn honours_disabled_auto_create() {
        let registry = Registry::new(RoomDefaults {
            auto_create: false,
            ..RoomDefaults::default()
        });
        let (tx, _rx) = signal_channel();

        let result = registry.join("demo", spec("alice"), tx);
        assert!(matches!(result, Err(JoinError::RoomNotFound(_))));
    }

    #[test]
    fn honours_the_participant_limit() {
        let registry = Registry::new(RoomDefaults {
            max_participants: 1,
            ..RoomDefaults::default()
        });
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let result = registry.join("demo", spec("bob"), bob_tx);

        assert!(matches!(result, Err(JoinError::RoomFull(_))));
    }
}
