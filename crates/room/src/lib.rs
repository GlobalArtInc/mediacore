use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mediacore_proto::livekit;
use tokio::sync::mpsc;

mod subscription;
mod track;

pub use subscription::{SubscriptionDiff, Subscriptions};
pub use track::{
    MAX_PUBLISHED_TRACKS, PublishError, PublishedTrack, TRACK_SID_PREFIX, TrackKind, TrackVersion,
};

use track::generate_track_sid;

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
    tracks: Mutex<Vec<PublishedTrack>>,
    subscriptions: Mutex<Subscriptions>,
    tx: Mutex<Option<SignalTx>>,
    close_reason: Mutex<Option<livekit::DisconnectReason>>,
}

impl Participant {
    pub fn to_proto(&self, state: livekit::participant_info::State) -> livekit::ParticipantInfo {
        livekit::ParticipantInfo {
            sid: self.sid.clone(),
            identity: self.identity.clone(),
            state: state as i32,
            tracks: self.track_infos(),
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

    pub fn can_publish_source(&self, source: livekit::TrackSource) -> bool {
        if !self.permission.can_publish {
            return false;
        }
        if self.permission.can_publish_sources.is_empty() {
            return true;
        }
        self.permission
            .can_publish_sources
            .contains(&(source as i32))
    }

    pub fn tracks(&self) -> Vec<PublishedTrack> {
        self.tracks.lock().expect("track lock").clone()
    }

    pub fn track_infos(&self) -> Vec<livekit::TrackInfo> {
        self.tracks
            .lock()
            .expect("track lock")
            .iter()
            .map(PublishedTrack::to_proto)
            .collect()
    }

    pub fn track(&self, track_sid: &str) -> Option<PublishedTrack> {
        self.tracks
            .lock()
            .expect("track lock")
            .iter()
            .find(|track| track.sid == track_sid)
            .cloned()
    }

    pub fn track_by_cid(&self, cid: &str) -> Option<PublishedTrack> {
        self.tracks
            .lock()
            .expect("track lock")
            .iter()
            .find(|track| track.cid == cid)
            .cloned()
    }

    pub fn publish_track(
        &self,
        request: &livekit::AddTrackRequest,
    ) -> Result<PublishedTrack, PublishError> {
        let Some(kind) = TrackKind::from_proto(request.r#type) else {
            return Err(PublishError::UnsupportedKind(request.r#type));
        };
        let source =
            livekit::TrackSource::try_from(request.source).unwrap_or(livekit::TrackSource::Unknown);

        if !self.can_publish_source(source) {
            return Err(PublishError::NotAllowed(source));
        }

        let track = {
            let mut tracks = self.tracks.lock().expect("track lock");
            let republished = tracks.iter().position(|track| track.cid == request.cid);

            match republished {
                Some(index) => {
                    let mut track = PublishedTrack::new(
                        tracks[index].sid.clone(),
                        kind,
                        source,
                        request,
                        &self.sid,
                        &self.identity,
                    );
                    track.version = tracks[index].version.next();
                    tracks[index] = track.clone();
                    track
                }
                None => {
                    if tracks.len() >= MAX_PUBLISHED_TRACKS {
                        return Err(PublishError::TooManyTracks(tracks.len()));
                    }

                    let track = PublishedTrack::new(
                        generate_track_sid(kind, source),
                        kind,
                        source,
                        request,
                        &self.sid,
                        &self.identity,
                    );
                    tracks.push(track.clone());
                    track
                }
            }
        };

        self.bump_version();
        tracing::debug!(
            identity = %self.identity,
            track = %track.sid,
            cid = %track.cid,
            kind = ?track.kind,
            "track published"
        );

        Ok(track)
    }

    pub fn unpublish_track(&self, track_sid: &str) -> Option<PublishedTrack> {
        let track = {
            let mut tracks = self.tracks.lock().expect("track lock");
            let index = tracks.iter().position(|track| track.sid == track_sid)?;
            tracks.remove(index)
        };

        self.bump_version();
        tracing::debug!(
            identity = %self.identity,
            track = %track.sid,
            "track unpublished"
        );

        Some(track)
    }

    pub fn set_track_muted(&self, track_sid: &str, muted: bool) -> Option<PublishedTrack> {
        self.update_track(track_sid, |track| track.muted = muted)
    }

    pub fn set_track_mid(&self, track_sid: &str, mid: &str) -> Option<PublishedTrack> {
        self.update_track(track_sid, |track| track.mid = Some(mid.to_owned()))
    }

    pub fn set_track_mime_type(&self, track_sid: &str, mime_type: &str) -> Option<PublishedTrack> {
        self.update_track(track_sid, |track| {
            track.mime_type = Some(mime_type.to_owned())
        })
    }

    pub fn set_desired_subscriptions<I>(&self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .set_desired(track_sids);
    }

    pub fn add_desired_subscriptions<I>(&self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .add_desired(track_sids);
    }

    pub fn remove_desired_subscriptions<I>(&self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .remove_desired(track_sids);
    }

    pub fn mark_subscription_active(&self, track_sid: &str) {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .mark_active(track_sid);
    }

    pub fn mark_subscription_inactive(&self, track_sid: &str) {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .mark_inactive(track_sid);
    }

    pub fn desired_subscriptions(&self) -> Vec<String> {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .desired()
    }

    pub fn active_subscriptions(&self) -> Vec<String> {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .active()
    }

    pub fn subscription_diff(&self) -> SubscriptionDiff {
        self.subscriptions.lock().expect("subscription lock").diff()
    }

    fn update_track<F>(&self, track_sid: &str, update: F) -> Option<PublishedTrack>
    where
        F: FnOnce(&mut PublishedTrack),
    {
        let track = {
            let mut tracks = self.tracks.lock().expect("track lock");
            let track = tracks.iter_mut().find(|track| track.sid == track_sid)?;
            update(track);
            track.touch();
            track.clone()
        };

        self.bump_version();

        Some(track)
    }

    pub fn send(&self, message: livekit::signal_response::Message) -> bool {
        let droppable = is_droppable(&message);
        let name = message_name(&message);
        let response = livekit::SignalResponse {
            message: Some(message),
        };

        let outcome = {
            let tx = self.tx.lock().expect("signal lock");
            match tx.as_ref() {
                Some(tx) => tx.try_send(response).err(),
                None => Some(mpsc::error::TrySendError::Closed(response)),
            }
        };

        match outcome {
            None => true,
            Some(mpsc::error::TrySendError::Closed(_)) => false,
            Some(mpsc::error::TrySendError::Full(_)) if droppable => {
                tracing::warn!(
                    identity = %self.identity,
                    sid = %self.sid,
                    message = name,
                    "signal queue is full, dropping a droppable message"
                );
                false
            }
            Some(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    identity = %self.identity,
                    sid = %self.sid,
                    message = name,
                    "signal queue is full on a control message, closing the session"
                );
                self.close_signal(livekit::DisconnectReason::SignalClose);
                false
            }
        }
    }

    pub fn close_signal(&self, reason: livekit::DisconnectReason) {
        let closed = self.tx.lock().expect("signal lock").take().is_some();

        if !closed {
            return;
        }

        *self.close_reason.lock().expect("close reason lock") = Some(reason);
    }

    pub fn close_reason(&self) -> Option<livekit::DisconnectReason> {
        *self.close_reason.lock().expect("close reason lock")
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

    pub fn participant_by_sid(&self, sid: &str) -> Option<Arc<Participant>> {
        self.participants
            .lock()
            .expect("room lock")
            .values()
            .find(|participant| participant.sid == sid)
            .cloned()
    }

    pub fn track(&self, track_sid: &str) -> Option<(Arc<Participant>, PublishedTrack)> {
        self.participants()
            .into_iter()
            .find_map(|participant| Some((Arc::clone(&participant), participant.track(track_sid)?)))
    }

    pub fn publish_track(
        &self,
        publisher_sid: &str,
        request: &livekit::AddTrackRequest,
    ) -> Result<PublishedTrack, PublishError> {
        let publisher = self
            .participant_by_sid(publisher_sid)
            .ok_or_else(|| PublishError::UnknownParticipant(publisher_sid.to_owned()))?;

        let track = publisher.publish_track(request)?;
        self.announce(&publisher);

        Ok(track)
    }

    pub fn unpublish_track(&self, publisher_sid: &str, track_sid: &str) -> Option<PublishedTrack> {
        let publisher = self.participant_by_sid(publisher_sid)?;
        let track = publisher.unpublish_track(track_sid)?;

        for participant in self.participants() {
            participant.remove_desired_subscriptions([track_sid.to_owned()]);
        }
        self.announce(&publisher);

        Some(track)
    }

    pub fn set_track_muted(
        &self,
        publisher_sid: &str,
        track_sid: &str,
        muted: bool,
    ) -> Option<PublishedTrack> {
        let publisher = self.participant_by_sid(publisher_sid)?;
        let track = publisher.set_track_muted(track_sid, muted)?;
        self.announce(&publisher);

        Some(track)
    }

    pub fn can_subscribe(&self, subscriber_sid: &str, track_sid: &str) -> bool {
        let Some(subscriber) = self.participant_by_sid(subscriber_sid) else {
            return false;
        };
        if !subscriber.permission.can_subscribe {
            return false;
        }

        match self.track(track_sid) {
            Some((publisher, _)) => publisher.sid != subscriber.sid,
            None => false,
        }
    }

    pub fn subscribable_track_sids(&self, subscriber_sid: &str) -> Vec<String> {
        let Some(subscriber) = self.participant_by_sid(subscriber_sid) else {
            return Vec::new();
        };
        if !subscriber.permission.can_subscribe {
            return Vec::new();
        }

        self.participants()
            .into_iter()
            .filter(|publisher| publisher.sid != subscriber_sid)
            .flat_map(|publisher| {
                publisher
                    .tracks()
                    .into_iter()
                    .map(|track| track.sid)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn update_subscriptions(
        &self,
        subscriber_sid: &str,
        request: &livekit::UpdateSubscription,
    ) -> Vec<String> {
        let mut requested = request.track_sids.clone();
        for tracks in &request.participant_tracks {
            requested.extend(tracks.track_sids.iter().cloned());
        }
        requested.sort();
        requested.dedup();

        let Some(subscriber) = self.participant_by_sid(subscriber_sid) else {
            return requested;
        };

        if !request.subscribe {
            subscriber.remove_desired_subscriptions(requested);
            return Vec::new();
        }

        let (allowed, refused): (Vec<String>, Vec<String>) = requested
            .into_iter()
            .partition(|track_sid| self.can_subscribe(subscriber_sid, track_sid));

        subscriber.add_desired_subscriptions(allowed);
        if !refused.is_empty() {
            tracing::debug!(
                identity = %subscriber.identity,
                tracks = ?refused,
                "refusing subscription to tracks the participant may not receive"
            );
        }

        refused
    }

    fn announce(&self, publisher: &Participant) {
        if publisher.permission.hidden {
            return;
        }

        let info = publisher.to_proto(livekit::participant_info::State::Active);
        self.broadcast(participant_update(vec![info]), Some(&publisher.sid));
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
                tracks: Mutex::new(Vec::new()),
                subscriptions: Mutex::new(Subscriptions::default()),
                tx: Mutex::new(Some(tx)),
                close_reason: Mutex::new(None),
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

        let orphaned: Vec<String> = departed
            .tracks()
            .into_iter()
            .map(|track| track.sid)
            .collect();
        if !orphaned.is_empty() {
            for participant in room.participants() {
                participant.remove_desired_subscriptions(orphaned.iter().cloned());
            }
        }

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

fn is_droppable(message: &livekit::signal_response::Message) -> bool {
    use livekit::signal_response::Message;

    matches!(
        message,
        Message::Pong(_)
            | Message::PongResp(_)
            | Message::SpeakersChanged(_)
            | Message::ConnectionQuality(_)
            | Message::StreamStateUpdate(_)
            | Message::SubscribedQualityUpdate(_)
    )
}

fn message_name(message: &livekit::signal_response::Message) -> &'static str {
    use livekit::signal_response::Message;

    match message {
        Message::Join(_) => "join",
        Message::Answer(_) => "answer",
        Message::Offer(_) => "offer",
        Message::Trickle(_) => "trickle",
        Message::Update(_) => "update",
        Message::TrackPublished(_) => "track_published",
        Message::TrackUnpublished(_) => "track_unpublished",
        Message::Leave(_) => "leave",
        Message::Mute(_) => "mute",
        Message::RoomUpdate(_) => "room_update",
        Message::RequestResponse(_) => "request_response",
        Message::Pong(_) | Message::PongResp(_) => "pong",
        Message::SpeakersChanged(_) => "speakers_changed",
        Message::ConnectionQuality(_) => "connection_quality",
        Message::StreamStateUpdate(_) => "stream_state_update",
        Message::SubscribedQualityUpdate(_) => "subscribed_quality_update",
        _ => "other",
    }
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
    format!("{prefix}_{}", random_id())
}

pub(crate) fn random_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
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

    fn add_track(
        cid: &str,
        kind: livekit::TrackType,
        source: livekit::TrackSource,
    ) -> livekit::AddTrackRequest {
        livekit::AddTrackRequest {
            cid: cid.to_owned(),
            name: cid.to_owned(),
            r#type: kind as i32,
            source: source as i32,
            ..Default::default()
        }
    }

    fn camera(cid: &str) -> livekit::AddTrackRequest {
        add_track(cid, livekit::TrackType::Video, livekit::TrackSource::Camera)
    }

    fn drain(rx: &mut SignalRx) {
        while rx.try_recv().is_ok() {}
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
    fn publishing_surfaces_the_track_to_other_participants() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        drain(&mut alice_rx);

        let published = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .expect("bob may publish");

        assert!(published.sid.starts_with(TRACK_SID_PREFIX));
        assert_eq!(published.publisher_identity, "bob");
        assert!(!published.is_negotiated());

        let update = expect_update(&mut alice_rx);
        assert_eq!(update.participants[0].identity, "bob");
        assert_eq!(update.participants[0].tracks.len(), 1);
        assert_eq!(update.participants[0].tracks[0].sid, published.sid);
        assert_eq!(update.participants[0].tracks[0].name, "cid-1");
        assert_eq!(
            update.participants[0].tracks[0].r#type,
            livekit::TrackType::Video as i32
        );

        assert_eq!(
            bob.room
                .track(&published.sid)
                .map(|(publisher, _)| publisher.sid.clone()),
            Some(bob.participant.sid.clone())
        );
    }

    #[test]
    fn the_publisher_does_not_hear_about_its_own_track() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        drain(&mut bob_rx);

        bob.room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .unwrap();

        assert!(bob_rx.try_recv().is_err());
    }

    #[test]
    fn unpublishing_removes_the_track() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let published = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .unwrap();
        drain(&mut alice_rx);

        let removed = bob
            .room
            .unpublish_track(&bob.participant.sid, &published.sid)
            .expect("the track was published");

        assert_eq!(removed.sid, published.sid);
        assert!(bob.participant.tracks().is_empty());
        assert!(bob.room.track(&published.sid).is_none());
        assert!(
            expect_update(&mut alice_rx).participants[0]
                .tracks
                .is_empty()
        );
        assert!(
            bob.room
                .unpublish_track(&bob.participant.sid, &published.sid)
                .is_none()
        );
    }

    #[test]
    fn muting_updates_the_track() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let published = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .unwrap();
        assert!(!published.muted);
        drain(&mut alice_rx);

        let muted = bob
            .room
            .set_track_muted(&bob.participant.sid, &published.sid, true)
            .expect("the track was published");

        assert!(muted.muted);
        assert!(muted.version > published.version);
        assert!(bob.participant.track(&published.sid).unwrap().muted);

        let update = expect_update(&mut alice_rx);
        assert!(update.participants[0].tracks[0].muted);

        let unmuted = bob
            .room
            .set_track_muted(&bob.participant.sid, &published.sid, false)
            .unwrap();
        assert!(!unmuted.muted);
        assert!(!expect_update(&mut alice_rx).participants[0].tracks[0].muted);
    }

    #[test]
    fn every_change_bumps_the_participant_version() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        drain(&mut alice_rx);

        let mut versions = Vec::new();
        let published = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .unwrap();
        versions.push(expect_update(&mut alice_rx).participants[0].version);

        bob.room
            .set_track_muted(&bob.participant.sid, &published.sid, true)
            .unwrap();
        versions.push(expect_update(&mut alice_rx).participants[0].version);

        bob.participant.set_track_mid(&published.sid, "0").unwrap();

        bob.room
            .unpublish_track(&bob.participant.sid, &published.sid)
            .unwrap();
        versions.push(expect_update(&mut alice_rx).participants[0].version);

        assert!(
            versions.windows(2).all(|pair| pair[1] > pair[0]),
            "versions did not increase: {versions:?}"
        );
    }

    #[test]
    fn the_negotiated_mid_lands_on_the_track() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let published = alice.participant.publish_track(&camera("cid-1")).unwrap();

        let negotiated = alice
            .participant
            .set_track_mid(&published.sid, "0")
            .expect("the track was published");
        alice
            .participant
            .set_track_mime_type(&published.sid, "video/VP8");

        assert!(negotiated.is_negotiated());
        let info = &alice.participant.track_infos()[0];
        assert_eq!(info.mid, "0");
        assert_eq!(info.mime_type, "video/VP8");
        assert_eq!(
            alice
                .participant
                .track_by_cid("cid-1")
                .map(|track| track.sid),
            Some(published.sid)
        );
    }

    #[test]
    fn republishing_the_same_client_id_keeps_the_track_sid() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let first = alice.participant.publish_track(&camera("cid-1")).unwrap();
        let second = alice.participant.publish_track(&camera("cid-1")).unwrap();

        assert_eq!(first.sid, second.sid);
        assert!(second.version > first.version);
        assert_eq!(alice.participant.tracks().len(), 1);
    }

    #[test]
    fn a_participant_without_the_publish_grant_is_refused() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let mut listener = spec("bob");
        listener.permission.can_publish = false;

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", listener, bob_tx).unwrap();

        let refused = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .expect_err("bob may not publish");
        assert!(matches!(refused, PublishError::NotAllowed(_)));
        assert_eq!(
            refused.reason(),
            livekit::request_response::Reason::NotAllowed
        );
        assert!(bob.participant.tracks().is_empty());

        let mut screen_only = spec("carol");
        screen_only.permission.can_publish_sources = vec![livekit::TrackSource::ScreenShare as i32];
        let (carol_tx, _carol_rx) = signal_channel();
        let carol = registry.join("demo", screen_only, carol_tx).unwrap();

        assert!(matches!(
            carol.participant.publish_track(&camera("cid-2")),
            Err(PublishError::NotAllowed(_))
        ));
        assert!(
            carol
                .participant
                .publish_track(&add_track(
                    "cid-3",
                    livekit::TrackType::Video,
                    livekit::TrackSource::ScreenShare
                ))
                .is_ok()
        );

        let data = add_track(
            "cid-4",
            livekit::TrackType::Data,
            livekit::TrackSource::Unknown,
        );
        assert!(matches!(
            alice.participant.publish_track(&data),
            Err(PublishError::UnsupportedKind(_))
        ));
    }

    #[test]
    fn desired_subscriptions_are_diffed_against_the_active_ones() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let camera_track = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .unwrap();
        let microphone_track = bob
            .room
            .publish_track(
                &bob.participant.sid,
                &add_track(
                    "cid-2",
                    livekit::TrackType::Audio,
                    livekit::TrackSource::Microphone,
                ),
            )
            .unwrap();

        let mut wanted = bob.room.subscribable_track_sids(&alice.participant.sid);
        wanted.sort();
        let mut published = vec![camera_track.sid.clone(), microphone_track.sid.clone()];
        published.sort();
        assert_eq!(wanted, published);

        alice.participant.set_desired_subscriptions(wanted.clone());
        let diff = alice.participant.subscription_diff();
        assert_eq!(diff.to_subscribe, wanted);
        assert!(diff.to_unsubscribe.is_empty());

        alice
            .participant
            .mark_subscription_active(&camera_track.sid);
        let diff = alice.participant.subscription_diff();
        assert_eq!(diff.to_subscribe, vec![microphone_track.sid.clone()]);
        assert!(diff.to_unsubscribe.is_empty());

        alice
            .participant
            .mark_subscription_active(&microphone_track.sid);
        assert!(alice.participant.subscription_diff().is_empty());

        bob.room
            .unpublish_track(&bob.participant.sid, &camera_track.sid)
            .unwrap();
        let diff = alice.participant.subscription_diff();
        assert!(diff.to_subscribe.is_empty());
        assert_eq!(
            diff.to_unsubscribe,
            vec![camera_track.sid.clone()],
            "an unpublished track stays active until the transport tears it down"
        );

        alice
            .participant
            .mark_subscription_inactive(&camera_track.sid);
        assert!(alice.participant.subscription_diff().is_empty());
        assert_eq!(
            alice.participant.active_subscriptions(),
            vec![microphone_track.sid]
        );
    }

    #[test]
    fn a_departing_publisher_takes_its_tracks_out_of_the_intent() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let track = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .unwrap();
        alice
            .participant
            .set_desired_subscriptions([track.sid.clone()]);
        alice.participant.mark_subscription_active(&track.sid);

        registry.leave("demo", &bob.participant.sid);

        assert!(alice.participant.desired_subscriptions().is_empty());
        assert_eq!(
            alice.participant.subscription_diff().to_unsubscribe,
            vec![track.sid]
        );
    }

    #[test]
    fn the_room_answers_who_may_subscribe_to_what() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let mut deaf = spec("bob");
        deaf.permission.can_subscribe = false;

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", deaf, bob_tx).unwrap();

        let track = alice
            .room
            .publish_track(&alice.participant.sid, &camera("cid-1"))
            .unwrap();

        assert!(!alice.room.can_subscribe(&bob.participant.sid, &track.sid));
        assert!(
            !alice.room.can_subscribe(&alice.participant.sid, &track.sid),
            "a participant does not subscribe to its own track"
        );
        assert!(!alice.room.can_subscribe("PA_unknown", &track.sid));
        assert!(
            !alice
                .room
                .can_subscribe(&bob.participant.sid, "TR_VCmissing")
        );
        assert!(
            alice
                .room
                .subscribable_track_sids(&bob.participant.sid)
                .is_empty()
        );

        let request = livekit::UpdateSubscription {
            track_sids: vec![track.sid.clone()],
            subscribe: true,
            participant_tracks: vec![livekit::ParticipantTracks {
                participant_sid: alice.participant.sid.clone(),
                track_sids: vec![track.sid.clone()],
            }],
        };
        assert_eq!(
            alice
                .room
                .update_subscriptions(&bob.participant.sid, &request),
            vec![track.sid.clone()]
        );
        assert!(bob.participant.desired_subscriptions().is_empty());
    }

    #[test]
    fn a_subscription_request_records_intent_for_allowed_tracks() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let track = alice
            .room
            .publish_track(&alice.participant.sid, &camera("cid-1"))
            .unwrap();

        let subscribe = livekit::UpdateSubscription {
            track_sids: vec![track.sid.clone(), "TR_VCmissing".to_owned()],
            subscribe: true,
            participant_tracks: Vec::new(),
        };
        assert_eq!(
            alice
                .room
                .update_subscriptions(&bob.participant.sid, &subscribe),
            vec!["TR_VCmissing".to_owned()]
        );
        assert_eq!(
            bob.participant.desired_subscriptions(),
            vec![track.sid.clone()]
        );

        let unsubscribe = livekit::UpdateSubscription {
            track_sids: vec![track.sid.clone()],
            subscribe: false,
            participant_tracks: Vec::new(),
        };
        assert!(
            alice
                .room
                .update_subscriptions(&bob.participant.sid, &unsubscribe)
                .is_empty()
        );
        assert!(bob.participant.desired_subscriptions().is_empty());
    }

    fn pong() -> livekit::signal_response::Message {
        livekit::signal_response::Message::Pong(1)
    }

    fn answer() -> livekit::signal_response::Message {
        livekit::signal_response::Message::Answer(livekit::SessionDescription {
            r#type: "answer".to_owned(),
            sdp: "v=0\r\n".to_owned(),
            ..Default::default()
        })
    }

    #[test]
    fn a_droppable_message_is_dropped_when_the_client_cannot_keep_up() {
        let registry = Registry::new(RoomDefaults::default());
        let (tx, mut rx) = signal_channel();
        let alice = registry.join("demo", spec("alice"), tx).unwrap();

        for _ in 0..SIGNAL_CHANNEL_CAPACITY {
            assert!(alice.participant.send(pong()));
        }

        assert!(!alice.participant.send(pong()));
        assert_eq!(alice.participant.close_reason(), None);
        assert!(rx.try_recv().is_ok(), "the session is still open");
    }

    #[test]
    fn a_control_message_that_cannot_be_queued_ends_the_session() {
        let registry = Registry::new(RoomDefaults::default());
        let (tx, mut rx) = signal_channel();
        let alice = registry.join("demo", spec("alice"), tx).unwrap();

        for _ in 0..SIGNAL_CHANNEL_CAPACITY {
            assert!(alice.participant.send(pong()));
        }

        assert!(!alice.participant.send(answer()));
        assert_eq!(
            alice.participant.close_reason(),
            Some(livekit::DisconnectReason::SignalClose)
        );

        assert!(!alice.participant.send(answer()));
        assert!(!alice.participant.send(participant_update(Vec::new())));

        let mut delivered = 0;
        while rx.try_recv().is_ok() {
            delivered += 1;
        }
        assert_eq!(delivered, SIGNAL_CHANNEL_CAPACITY);
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)),
            "the signal stream must end so the session is torn down"
        );
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
