use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

pub const DUPLICATE_IDENTITY_SEPARATOR: char = '#';

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DuplicateIdentity {
    #[default]
    Suffix,
    Displace,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParticipantKind {
    #[default]
    Standard,
    Egress,
    Agent,
}

impl ParticipantKind {
    pub fn to_proto(self) -> livekit::participant_info::Kind {
        match self {
            Self::Standard => livekit::participant_info::Kind::Standard,
            Self::Egress => livekit::participant_info::Kind::Egress,
            Self::Agent => livekit::participant_info::Kind::Agent,
        }
    }

    pub fn is_human(self) -> bool {
        matches!(self, Self::Standard)
    }
}

#[derive(Debug, Clone)]
pub struct RoomDefaults {
    pub auto_create: bool,
    pub empty_timeout: u32,
    pub departure_timeout: u32,
    pub max_participants: u32,
    pub min_humans: u32,
    pub end_grace: u32,
    pub duplicate_identity: DuplicateIdentity,
}

impl Default for RoomDefaults {
    fn default() -> Self {
        Self {
            auto_create: true,
            empty_timeout: 300,
            departure_timeout: 20,
            max_participants: 0,
            min_humans: 0,
            end_grace: 90,
            duplicate_identity: DuplicateIdentity::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RoomOptions {
    pub metadata: String,
    pub empty_timeout: Option<u32>,
    pub departure_timeout: Option<u32>,
    pub max_participants: Option<u32>,
    pub min_humans: Option<u32>,
    pub end_grace: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSettings {
    pub metadata: String,
    pub empty_timeout: u32,
    pub departure_timeout: u32,
    pub max_participants: u32,
    pub min_humans: u32,
    pub end_grace: u32,
}

impl RoomSettings {
    fn resolve(defaults: &RoomDefaults, options: RoomOptions) -> Self {
        Self {
            metadata: options.metadata,
            empty_timeout: options.empty_timeout.unwrap_or(defaults.empty_timeout),
            departure_timeout: options
                .departure_timeout
                .unwrap_or(defaults.departure_timeout),
            max_participants: options
                .max_participants
                .unwrap_or(defaults.max_participants),
            min_humans: options.min_humans.unwrap_or(defaults.min_humans),
            end_grace: options.end_grace.unwrap_or(defaults.end_grace),
        }
    }

    fn end_grace_ms(&self) -> i64 {
        i64::from(self.end_grace) * 1_000
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParticipantSpec {
    pub identity: String,
    pub name: String,
    pub metadata: String,
    pub attributes: HashMap<String, String>,
    pub permission: livekit::ParticipantPermission,
    pub kind: ParticipantKind,
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Profile {
    name: String,
    metadata: String,
    attributes: HashMap<String, String>,
    permission: livekit::ParticipantPermission,
}

/// A change to what a participant is called and what they are allowed to do.
///
/// Every field is optional because `UpdateParticipant` is a patch, not a
/// replacement: a caller that only revokes publishing must not blank out the
/// display name it never mentioned. Attributes follow LiveKit and are merged
/// key by key, an empty value deleting the key.
#[derive(Debug, Clone, Default)]
pub struct ProfileUpdate {
    pub name: Option<String>,
    pub metadata: Option<String>,
    pub attributes: HashMap<String, String>,
    pub permission: Option<livekit::ParticipantPermission>,
}

pub struct Participant {
    pub sid: String,
    pub identity: String,
    /// The identity the token asked for, before a duplicate was given a suffix.
    ///
    /// The suffix is a display decision, and `#` followed by digits is not a
    /// reserved namespace: `bob#7` is an identity a backoffice may legitimately
    /// mint, and every roster in the room carries that session's sid. Reading
    /// the ordinal back out of the effective identity would therefore let a
    /// plain token for `bob` claim to be the same person as `bob#7` and take
    /// their session away from them. So the ordinal is carried beside the
    /// identity instead of parsed out of it, and "the same person" is an exact
    /// comparison of what two tokens asked to be called.
    pub requested_identity: String,
    pub kind: ParticipantKind,
    pub joined_at_ms: i64,
    profile: Mutex<Profile>,
    version: AtomicU32,
    tracks: Mutex<Vec<PublishedTrack>>,
    subscriptions: Mutex<Subscriptions>,
    tx: Mutex<Option<SignalTx>>,
    close_reason: Mutex<Option<livekit::DisconnectReason>>,
}

impl Participant {
    pub fn to_proto(&self, state: livekit::participant_info::State) -> livekit::ParticipantInfo {
        let profile = self.profile.lock().expect("profile lock").clone();

        livekit::ParticipantInfo {
            sid: self.sid.clone(),
            identity: self.identity.clone(),
            state: state as i32,
            tracks: self.track_infos(),
            metadata: profile.metadata,
            joined_at: self.joined_at_ms / 1000,
            joined_at_ms: self.joined_at_ms,
            name: profile.name,
            version: self.version_counter(),
            permission: Some(profile.permission),
            kind: self.kind.to_proto() as i32,
            attributes: profile.attributes,
            ..Default::default()
        }
    }

    pub fn name(&self) -> String {
        self.profile.lock().expect("profile lock").name.clone()
    }

    pub fn metadata(&self) -> String {
        self.profile.lock().expect("profile lock").metadata.clone()
    }

    pub fn attributes(&self) -> HashMap<String, String> {
        self.profile
            .lock()
            .expect("profile lock")
            .attributes
            .clone()
    }

    pub fn permission(&self) -> livekit::ParticipantPermission {
        self.profile
            .lock()
            .expect("profile lock")
            .permission
            .clone()
    }

    pub fn is_hidden(&self) -> bool {
        self.profile.lock().expect("profile lock").permission.hidden
    }

    pub fn can_subscribe(&self) -> bool {
        self.profile
            .lock()
            .expect("profile lock")
            .permission
            .can_subscribe
    }

    pub fn can_publish_source(&self, source: livekit::TrackSource) -> bool {
        let profile = self.profile.lock().expect("profile lock");

        if !profile.permission.can_publish {
            return false;
        }
        if profile.permission.can_publish_sources.is_empty() {
            return true;
        }
        profile
            .permission
            .can_publish_sources
            .contains(&(source as i32))
    }

    /// Applies a patch, reporting whether anything actually moved.
    ///
    /// The report is what keeps an idempotent `UpdateParticipant` quiet: a
    /// backoffice that resends the same permissions on every render would
    /// otherwise flood the room with participant updates that say nothing.
    pub(crate) fn apply(&self, update: ProfileUpdate) -> bool {
        let mut profile = self.profile.lock().expect("profile lock");
        let mut changed = false;

        if let Some(name) = update.name
            && profile.name != name
        {
            profile.name = name;
            changed = true;
        }
        if let Some(metadata) = update.metadata
            && profile.metadata != metadata
        {
            profile.metadata = metadata;
            changed = true;
        }
        for (key, value) in update.attributes {
            if value.is_empty() {
                changed |= profile.attributes.remove(&key).is_some();
            } else if profile.attributes.get(&key) != Some(&value) {
                profile.attributes.insert(key, value);
                changed = true;
            }
        }
        if let Some(permission) = update.permission
            && profile.permission != permission
        {
            profile.permission = permission;
            changed = true;
        }

        drop(profile);

        if changed {
            self.bump_version();
        }

        changed
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

    pub fn set_track_live_layers(
        &self,
        track_sid: &str,
        layers: Vec<livekit::VideoLayer>,
    ) -> Option<PublishedTrack> {
        let track = {
            let mut tracks = self.tracks.lock().expect("track lock");
            let track = tracks.iter_mut().find(|track| track.sid == track_sid)?;

            if !track.set_live_layers(layers) {
                return None;
            }

            track.touch();
            track.clone()
        };

        self.bump_version();

        Some(track)
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

    /// Records that this participant asked *not* to receive these tracks.
    ///
    /// Dropping the intent alone is not enough, because the subscriber session
    /// auto-subscribes to everything it may see: an unsubscribe that arrives
    /// before that pass has reached the track is undone by the pass, and one
    /// that arrives after it is undone the next time the session's bookkeeping
    /// is reset. A refusal is the participant's own answer and it has to
    /// outlive both.
    pub fn decline_subscriptions<I>(&self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .decline(track_sids);
    }

    /// Forgets a track completely -- neither wanted nor refused.
    ///
    /// This is for a track that has left the room. Keeping the refusal would
    /// mean a publisher who unpublished and published again was never heard by
    /// somebody who had only ever said no to the earlier track.
    pub fn forget_subscriptions<I>(&self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .forget(track_sids);
    }

    pub fn declines_subscription(&self, track_sid: &str) -> bool {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .is_declined(track_sid)
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
    persistent: AtomicBool,
    settings: Mutex<RoomSettings>,
    participants: Mutex<HashMap<String, Arc<Participant>>>,
    ends_at_ms: Mutex<Option<i64>>,
    emptied_at_ms: Mutex<Option<i64>>,
    quorum_reached: AtomicBool,
    occupied_before: AtomicBool,
}

impl Room {
    fn new(name: String, settings: RoomSettings, persistent: bool) -> Self {
        let created_at_ms = now_ms();

        Self {
            sid: generate_sid("RM"),
            name,
            created_at_ms,
            persistent: AtomicBool::new(persistent),
            settings: Mutex::new(settings),
            participants: Mutex::new(HashMap::new()),
            ends_at_ms: Mutex::new(None),
            emptied_at_ms: Mutex::new(Some(created_at_ms)),
            quorum_reached: AtomicBool::new(false),
            occupied_before: AtomicBool::new(false),
        }
    }

    pub fn settings(&self) -> RoomSettings {
        self.settings.lock().expect("settings lock").clone()
    }

    pub fn metadata(&self) -> String {
        self.settings
            .lock()
            .expect("settings lock")
            .metadata
            .clone()
    }

    pub fn set_metadata(&self, metadata: &str) {
        {
            let mut settings = self.settings.lock().expect("settings lock");
            if settings.metadata == metadata {
                return;
            }
            settings.metadata = metadata.to_owned();
        }

        tracing::info!(room = %self.name, "room metadata updated");
        self.broadcast(
            livekit::signal_response::Message::RoomUpdate(livekit::RoomUpdate {
                room: Some(self.to_proto()),
            }),
            None,
        );
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent.load(Ordering::Relaxed)
    }

    /// Turns a room the first arrival created into one that was asked for.
    ///
    /// A room nobody created has no settings anyone chose -- it took the
    /// server defaults on the way in. So when `CreateRoom` names it, the
    /// request wins and the room stops closing itself when it empties. A room
    /// that was created on purpose is left alone instead, because that call is
    /// a retry as often as it is a first attempt, and a retry must not rewrite
    /// a room people are sitting in.
    ///
    /// Who may do this is the registry's decision, not the room's: see
    /// [`Adoption`].
    fn adopt(&self, settings: RoomSettings) {
        self.persistent.store(true, Ordering::Relaxed);

        let changed = {
            let mut current = self.settings.lock().expect("settings lock");
            let changed = *current != settings;
            *current = settings;
            changed
        };

        if changed {
            tracing::info!(
                room = %self.name,
                sid = %self.sid,
                "a room that was created by its first arrival is now a room that was asked for"
            );
            self.broadcast(
                livekit::signal_response::Message::RoomUpdate(livekit::RoomUpdate {
                    room: Some(self.to_proto()),
                }),
                None,
            );
        }

        self.reassess(now_ms());
    }

    /// Moves the session policy without saying anything on the wire.
    ///
    /// There is no field for `min_humans` or `end_grace` in LiveKit's `Room`,
    /// so there is nothing a `RoomUpdate` could carry: the policy travels in
    /// the room metadata, and it is the metadata broadcast that tells the room
    /// it changed. Each `None` leaves that half of the policy where it is.
    pub fn set_session_policy(&self, min_humans: Option<u32>, end_grace: Option<u32>) {
        {
            let mut settings = self.settings.lock().expect("settings lock");
            if let Some(min_humans) = min_humans {
                settings.min_humans = min_humans;
            }
            if let Some(end_grace) = end_grace {
                settings.end_grace = end_grace;
            }
        }

        self.reassess(now_ms());
    }

    pub fn to_proto(&self) -> livekit::Room {
        let settings = self.settings();
        let participants = self.participants();

        livekit::Room {
            sid: self.sid.clone(),
            name: self.name.clone(),
            empty_timeout: settings.empty_timeout,
            departure_timeout: settings.departure_timeout,
            max_participants: settings.max_participants,
            creation_time: self.created_at_ms / 1000,
            creation_time_ms: self.created_at_ms,
            metadata: settings.metadata,
            num_participants: participants.len() as u32,
            num_publishers: participants
                .iter()
                .filter(|participant| !participant.tracks().is_empty())
                .count() as u32,
            active_recording: participants
                .iter()
                .any(|participant| participant.kind == ParticipantKind::Egress),
            ..Default::default()
        }
    }

    pub fn participant_count(&self) -> usize {
        self.participants.lock().expect("room lock").len()
    }

    pub fn human_count(&self) -> usize {
        self.participants
            .lock()
            .expect("room lock")
            .values()
            .filter(|participant| participant.kind.is_human())
            .count()
    }

    pub fn session_ends_at_ms(&self) -> Option<i64> {
        *self.ends_at_ms.lock().expect("session lock")
    }

    /// When the room last became empty, or `None` while somebody is in it.
    pub fn emptied_at_ms(&self) -> Option<i64> {
        *self.emptied_at_ms.lock().expect("occupancy lock")
    }

    /// Re-reads both of the room's clocks after its membership or its policy
    /// changed.
    ///
    /// Every path that adds or removes a participant ends here, and so does
    /// every path that moves the policy, because a room that is counting down
    /// to its own end has to notice both kinds of change.
    fn reassess(&self, now_ms: i64) {
        self.note_occupancy(now_ms);
        self.reassess_session(now_ms);
    }

    /// Remembers when the room emptied, which is what the timeouts count from.
    fn note_occupancy(&self, now_ms: i64) {
        let occupied = self.participant_count() > 0;
        let mut emptied_at = self.emptied_at_ms.lock().expect("occupancy lock");

        if occupied {
            self.occupied_before.store(true, Ordering::Relaxed);
            *emptied_at = None;
        } else if emptied_at.is_none() {
            *emptied_at = Some(now_ms);
        }
    }

    fn reassess_session(&self, now_ms: i64) {
        let settings = self.settings();
        if settings.min_humans == 0 {
            if self
                .ends_at_ms
                .lock()
                .expect("session lock")
                .take()
                .is_some()
            {
                tracing::info!(
                    room = %self.name,
                    "the session policy was switched off, the countdown with it"
                );
            }
            return;
        }

        let humans = self.human_count();

        if humans >= settings.min_humans as usize {
            self.quorum_reached.store(true, Ordering::Relaxed);
            if self
                .ends_at_ms
                .lock()
                .expect("session lock")
                .take()
                .is_some()
            {
                tracing::info!(
                    room = %self.name,
                    humans,
                    "the room filled up again, the session is no longer ending"
                );
            }
            return;
        }

        if !self.quorum_reached.load(Ordering::Relaxed) {
            return;
        }

        let mut ends_at = self.ends_at_ms.lock().expect("session lock");
        if ends_at.is_none() {
            *ends_at = Some(now_ms + settings.end_grace_ms());
            tracing::info!(
                room = %self.name,
                humans,
                min_humans = settings.min_humans,
                grace_seconds = settings.end_grace,
                "too few humans left in the room, the session ends when the grace period runs out"
            );
        }
    }

    /// Whether the session policy has run out on this room.
    ///
    /// The deadline alone is not the answer. It is armed on the way down and
    /// disarmed by the arrival that restores the quorum, and the sweep reads
    /// both under the registry lock the arrival has to take -- so a deadline
    /// that expired in the same instant as somebody walked back in ends
    /// nothing. Reading the policy again also means a deadline that outlived
    /// the policy that armed it cannot fire.
    fn session_expired(&self, now_ms: i64) -> bool {
        let Some(ends_at) = self.session_ends_at_ms() else {
            return false;
        };
        if now_ms < ends_at {
            return false;
        }

        let settings = self.settings();
        settings.min_humans > 0 && self.human_count() < settings.min_humans as usize
    }

    /// Whether the room has been empty for longer than it is allowed to be.
    ///
    /// Only rooms somebody created are counted: a room that exists because an
    /// arrival opened it is already gone the moment they leave, which is a
    /// departure timeout of zero. `empty_timeout` is the wait for a room
    /// nobody has ever been in and `departure_timeout` the wait after the last
    /// person leaves, which is LiveKit's own division. Either at zero means an
    /// operator asked for a room that is never collected, and `DeleteRoom`
    /// stays the way to be rid of it.
    ///
    /// A room counting down to the end of its session is not idle, however
    /// long it has been empty. The grace period is a promise to the people who
    /// were in it -- come back within ninety seconds and the meeting is still
    /// yours -- and collecting the room at the twenty-second departure timeout
    /// would break that promise by taking its settings and its policy with it.
    /// The room still goes; it goes when it said it would.
    fn idle_expired(&self, now_ms: i64) -> bool {
        if !self.is_persistent() || self.participant_count() > 0 {
            return false;
        }
        if self.session_ends_at_ms().is_some() {
            return false;
        }

        let Some(emptied_at) = self.emptied_at_ms() else {
            return false;
        };

        let settings = self.settings();
        let timeout = if self.occupied_before.load(Ordering::Relaxed) {
            settings.departure_timeout
        } else {
            settings.empty_timeout
        };

        timeout > 0 && now_ms >= emptied_at + i64::from(timeout) * 1_000
    }

    fn take_participant_by_sid(&self, sid: &str) -> Option<Arc<Participant>> {
        let mut participants = self.participants.lock().expect("room lock");
        let identity = participants
            .iter()
            .find(|(_, participant)| participant.sid == sid)
            .map(|(identity, _)| identity.clone())?;

        participants.remove(&identity)
    }

    fn take_participant(&self, identity: &str) -> Option<Arc<Participant>> {
        self.participants
            .lock()
            .expect("room lock")
            .remove(identity)
    }

    fn take_participants(&self) -> Vec<Arc<Participant>> {
        self.participants
            .lock()
            .expect("room lock")
            .drain()
            .map(|(_, participant)| participant)
            .collect()
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

    /// Every connection the person behind `identity` has open, in the order
    /// they arrived -- and by identity where two arrived in the same
    /// millisecond, because two tabs opened together otherwise come back in a
    /// different order every time.
    ///
    /// One person can hold several sessions here, because a duplicate identity
    /// is suffixed rather than displaced, so a moderator naming `alice` means
    /// `alice`, `alice#2` and `alice#3` together. A name is read as a person
    /// first: every connection whose *token* asked to be called that. Only
    /// when no token asked for it is it read as the effective identity of one
    /// connection, which is how `alice#2` names the second tab and nobody
    /// else — and it is why `ListParticipants` reports the suffixed
    /// identities, since they are what a caller uses to be exact.
    ///
    /// The two steps are not one filter because `bob#7` can be both at once: a
    /// name a backoffice minted for one person and the suffix generated for
    /// another's seventh tab. Matching both would answer a moderator's
    /// "remove `bob#7`" by removing somebody who was never named.
    pub fn participants_of(&self, identity: &str) -> Vec<Arc<Participant>> {
        let participants = self.participants();
        let mut found: Vec<Arc<Participant>> = participants
            .iter()
            .filter(|participant| participant.requested_identity == identity)
            .cloned()
            .collect();

        if found.is_empty() {
            found = participants
                .into_iter()
                .filter(|participant| participant.identity == identity)
                .collect();
        }

        found.sort_by_key(|participant| (participant.joined_at_ms, participant.identity.clone()));
        found
    }

    /// Renames a participant, rewrites their metadata, or changes what they may
    /// do, and tells the room about it.
    ///
    /// Revoking permission is the half that has to do more than store a flag.
    /// A participant who may no longer publish keeps sending RTP until someone
    /// takes the track away from them, and one who may no longer subscribe
    /// keeps receiving until the intent behind their subscriptions is dropped,
    /// so both are undone here rather than left for the caller to remember.
    /// The tracks that were taken away go back to the caller in [`Updated`],
    /// because taking a track out of the room is only half of stopping it.
    pub fn update_participant(&self, identity: &str, update: ProfileUpdate) -> Option<Updated> {
        let participant = self.participant(identity)?;
        let was_hidden = participant.is_hidden();

        if !participant.apply(update) {
            return Some(Updated {
                participant,
                unpublished: Vec::new(),
            });
        }

        let unpublished = self.revoke_forbidden_tracks(&participant);

        if !participant.can_subscribe() {
            participant.set_desired_subscriptions(Vec::new());
        }

        let hidden = participant.is_hidden();
        let info = participant.to_proto(livekit::participant_info::State::Active);

        if hidden {
            participant.send(participant_update(vec![info]));
        } else {
            self.broadcast(participant_update(vec![info]), None);
        }

        if hidden && !was_hidden {
            self.conceal(&participant);
        }

        tracing::info!(room = %self.name, %identity, hidden, "participant updated");

        Some(Updated {
            participant,
            unpublished,
        })
    }

    /// Takes somebody who has just become hidden out of everybody's roster.
    ///
    /// A client only ever removes a participant on an update that says
    /// `DISCONNECTED`, so telling the room nothing would leave the person on
    /// every screen for the rest of the meeting -- and `depart` says nothing
    /// about a hidden participant either, so the tile would outlive them by
    /// the length of the meeting. Their media goes with them: nobody keeps
    /// receiving a publisher they are not supposed to be able to see.
    fn conceal(&self, participant: &Arc<Participant>) {
        let orphaned: Vec<String> = participant
            .tracks()
            .into_iter()
            .map(|track| track.sid)
            .collect();

        if !orphaned.is_empty() {
            for other in self.participants() {
                if other.sid == participant.sid {
                    continue;
                }
                other.forget_subscriptions(orphaned.iter().cloned());
            }
        }

        let info = participant.to_proto(livekit::participant_info::State::Disconnected);
        self.broadcast(participant_update(vec![info]), Some(&participant.sid));
    }

    fn revoke_forbidden_tracks(&self, participant: &Arc<Participant>) -> Vec<String> {
        let forbidden: Vec<String> = participant
            .tracks()
            .into_iter()
            .filter(|track| !participant.can_publish_source(track.source))
            .map(|track| track.sid)
            .collect();
        let mut unpublished = Vec::new();

        for track_sid in forbidden {
            if self.unpublish_track(&participant.sid, &track_sid).is_none() {
                continue;
            }

            tracing::info!(
                room = %self.name,
                identity = %participant.identity,
                track = %track_sid,
                "taking away a track the participant may no longer publish"
            );
            participant.send(livekit::signal_response::Message::TrackUnpublished(
                livekit::TrackUnpublishedResponse {
                    track_sid: track_sid.clone(),
                },
            ));
            unpublished.push(track_sid);
        }

        unpublished
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

    pub fn set_track_live_layers(
        &self,
        publisher_sid: &str,
        track_sid: &str,
        layers: Vec<livekit::VideoLayer>,
    ) -> Option<PublishedTrack> {
        let publisher = self.participant_by_sid(publisher_sid)?;
        let track = publisher.set_track_live_layers(track_sid, layers)?;
        self.announce(&publisher);

        Some(track)
    }

    pub fn can_subscribe(&self, subscriber_sid: &str, track_sid: &str) -> bool {
        let Some(subscriber) = self.participant_by_sid(subscriber_sid) else {
            return false;
        };
        if !subscriber.can_subscribe() {
            return false;
        }

        match self.track(track_sid) {
            Some((publisher, _)) => publisher.sid != subscriber.sid && !publisher.is_hidden(),
            None => false,
        }
    }

    pub fn subscribable_track_sids(&self, subscriber_sid: &str) -> Vec<String> {
        let Some(subscriber) = self.participant_by_sid(subscriber_sid) else {
            return Vec::new();
        };
        if !subscriber.can_subscribe() {
            return Vec::new();
        }

        self.participants()
            .into_iter()
            .filter(|publisher| publisher.sid != subscriber_sid)
            .filter(|publisher| !publisher.is_hidden())
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
            subscriber.decline_subscriptions(requested);
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
        if publisher.is_hidden() {
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

/// What an `UpdateParticipant` did, for a caller that has to finish the job.
///
/// The room can forget a track; only the SFU can stop forwarding the packets
/// that keep arriving for it. The room layer has no router -- it is where the
/// membership lives, not where the media does -- so the tracks it took away
/// come back here for whoever does.
pub struct Updated {
    pub participant: Arc<Participant>,
    pub unpublished: Vec<String>,
}

/// What one pass of the sweep took away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    /// Rooms whose session policy ran out. Everybody still inside was
    /// disconnected with `ROOM_CLOSED`.
    pub ended: Vec<String>,
    /// Rooms that stood empty for longer than their own timeout allows.
    pub closed: Vec<String>,
}

impl Swept {
    pub fn is_empty(&self) -> bool {
        self.ended.is_empty() && self.closed.is_empty()
    }
}

/// Who may settle a room that an arrival opened before anybody created it.
///
/// `roomCreate` carries no room name -- LiveKit's grant has no such field --
/// so a token that may create a room may create every room. Adoption rewrites
/// the settings and the session policy of a room that already exists, and a
/// token with no idea which room it is talking about must not be able to do
/// that to a room people are sitting in. A caller whose token also names the
/// room has said which one it means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Adoption {
    /// Settle an auto-created room only while nobody is in it.
    #[default]
    WhenEmpty,
    /// Settle it whatever it holds.
    Always,
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

    /// Creates a room, or returns the one that is already there.
    ///
    /// The whole decision is made under the registry lock, including the
    /// adoption: a room that gains its first participant between the check and
    /// the rewrite would otherwise be settled by a caller that was told it was
    /// empty. Nothing here blocks while the lock is held -- signalling a
    /// participant is a `try_send`.
    pub fn create(&self, name: &str, options: RoomOptions, adoption: Adoption) -> Arc<Room> {
        let mut rooms = self.rooms.lock().expect("registry lock");

        let Some(room) = rooms.get(name).cloned() else {
            let room = Arc::new(Room::new(
                name.to_owned(),
                RoomSettings::resolve(&self.defaults, options),
                true,
            ));
            rooms.insert(name.to_owned(), Arc::clone(&room));
            tracing::info!(room = %name, sid = %room.sid, "room created");
            return room;
        };

        if room.is_persistent() {
            tracing::debug!(room = %name, sid = %room.sid, "room already exists, keeping it as it is");
            return room;
        }

        if adoption == Adoption::WhenEmpty && room.participant_count() > 0 {
            tracing::warn!(
                room = %name,
                sid = %room.sid,
                participants = room.participant_count(),
                "leaving an occupied room as it was: the caller named no room, and the \
                 settings it asked for were not applied"
            );
            return room;
        }

        room.adopt(RoomSettings::resolve(&self.defaults, options));
        room
    }

    pub fn delete(&self, name: &str) -> Option<Arc<Room>> {
        let room = self.rooms.lock().expect("registry lock").remove(name)?;
        end_session(&room, livekit::DisconnectReason::RoomDeleted);

        Some(room)
    }

    pub fn remove_participant(&self, room_name: &str, identity: &str) -> Option<Arc<Participant>> {
        let (room, removed) = {
            let mut rooms = self.rooms.lock().expect("registry lock");
            let room = rooms.get(room_name).cloned()?;
            let removed = room.take_participant(identity);

            if removed.is_some() && room.participant_count() == 0 && !room.is_persistent() {
                rooms.remove(room_name);
                tracing::info!(room = %room_name, "room closed, no participants left");
            }

            (room, removed)
        };

        let removed = removed?;
        tracing::info!(
            room = %room_name,
            identity = %removed.identity,
            sid = %removed.sid,
            "participant removed"
        );

        removed.disconnect(livekit::DisconnectReason::ParticipantRemoved);
        depart(
            &room,
            &removed,
            livekit::DisconnectReason::ParticipantRemoved,
            None,
        );
        room.reassess(now_ms());

        Some(removed)
    }

    /// One pass over every room, ending the sessions that are over and closing
    /// the rooms that nobody is coming back to.
    ///
    /// This is the registry's only clock. Everything else about a room happens
    /// because somebody did something to it, and the two things that have to
    /// happen when nobody does anything -- a session policy running out and an
    /// empty room being collected -- both hang off this one call. It takes the
    /// time rather than reading it so that a test can be the clock.
    pub fn sweep(&self, now_ms: i64) -> Swept {
        let (ended, closed) = {
            let mut rooms = self.rooms.lock().expect("registry lock");

            let ended: Vec<Arc<Room>> = rooms
                .values()
                .filter(|room| room.session_expired(now_ms))
                .cloned()
                .collect();
            let closed: Vec<Arc<Room>> = rooms
                .values()
                .filter(|room| room.idle_expired(now_ms))
                .cloned()
                .collect();

            for room in ended.iter().chain(&closed) {
                rooms.remove(&room.name);
            }

            (ended, closed)
        };

        for room in &ended {
            end_session(room, livekit::DisconnectReason::RoomClosed);
        }
        for room in &closed {
            tracing::info!(
                room = %room.name,
                sid = %room.sid,
                "room closed, empty for longer than its timeout allows"
            );
        }

        let mut swept = Swept {
            ended: ended.into_iter().map(|room| room.name.clone()).collect(),
            closed: closed.into_iter().map(|room| room.name.clone()).collect(),
        };
        swept.ended.sort();
        swept.closed.sort();
        swept
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
                    let room = Arc::new(Room::new(
                        room_name.to_owned(),
                        RoomSettings::resolve(&self.defaults, RoomOptions::default()),
                        false,
                    ));
                    rooms.insert(room_name.to_owned(), Arc::clone(&room));
                    tracing::info!(
                        room = %room_name,
                        sid = %room.sid,
                        "room created by the first participant to arrive"
                    );
                    room
                }
                None => return Err(JoinError::RoomNotFound(room_name.to_owned())),
            };

            let settings = room.settings();
            let mut participants = room.participants.lock().expect("room lock");

            let identity = match resumed_identity(&participants, &spec) {
                Some(identity) => identity,
                None => match self.defaults.duplicate_identity {
                    DuplicateIdentity::Displace => spec.identity.clone(),
                    DuplicateIdentity::Suffix => distinct_identity(&participants, &spec.identity),
                },
            };

            if identity != spec.identity {
                tracing::info!(
                    room = %room_name,
                    requested = %spec.identity,
                    identity = %identity,
                    "identity is already in the room, this connection is given one of its own"
                );
            }

            let replaces = participants.contains_key(&identity);

            if !replaces
                && settings.max_participants > 0
                && participants.len() >= settings.max_participants as usize
            {
                return Err(JoinError::RoomFull(room_name.to_owned()));
            }

            let participant = Arc::new(Participant {
                sid: generate_sid("PA"),
                identity: identity.clone(),
                requested_identity: spec.identity.clone(),
                kind: spec.kind,
                joined_at_ms: now_ms(),
                profile: Mutex::new(Profile {
                    name: spec.name,
                    metadata: spec.metadata,
                    attributes: spec.attributes,
                    permission: spec.permission,
                }),
                version: AtomicU32::new(if replaces { 2 } else { 1 }),
                tracks: Mutex::new(Vec::new()),
                subscriptions: Mutex::new(Subscriptions::default()),
                tx: Mutex::new(Some(tx)),
                close_reason: Mutex::new(None),
            });
            let displaced = participants.insert(identity, Arc::clone(&participant));
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
            depart(
                &room,
                &displaced,
                livekit::DisconnectReason::DuplicateIdentity,
                Some(&participant.sid),
            );
        }

        let others = room
            .participants()
            .into_iter()
            .filter(|other| other.sid != participant.sid)
            .filter(|other| !other.is_hidden())
            .map(|other| other.to_proto(livekit::participant_info::State::Active))
            .collect();

        if !participant.is_hidden() {
            room.broadcast(
                participant_update(vec![
                    participant.to_proto(livekit::participant_info::State::Joined),
                ]),
                Some(&participant.sid),
            );
        }

        room.reassess(now_ms());

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

            let departed = room.take_participant_by_sid(sid);

            if departed.is_some() && room.participant_count() == 0 && !room.is_persistent() {
                rooms.remove(room_name);
                tracing::info!(room = %room_name, "room closed, no participants left");
            }

            (room, departed)
        };

        let Some(departed) = departed else {
            tracing::debug!(
                room = %room_name,
                %sid,
                "nothing to leave, the session was already replaced or removed"
            );
            return;
        };

        depart(
            &room,
            &departed,
            livekit::DisconnectReason::ClientInitiated,
            None,
        );
        room.reassess(now_ms());
    }
}

fn resumed_identity(
    participants: &HashMap<String, Arc<Participant>>,
    spec: &ParticipantSpec,
) -> Option<String> {
    let sid = spec.replaces.as_deref()?;

    participants
        .values()
        .find(|participant| {
            participant.sid == sid && participant.requested_identity == spec.identity
        })
        .map(|participant| participant.identity.clone())
}

fn distinct_identity(participants: &HashMap<String, Arc<Participant>>, requested: &str) -> String {
    if !participants.contains_key(requested) {
        return requested.to_owned();
    }

    for ordinal in 2..=u32::MAX {
        let candidate = format!("{requested}{DUPLICATE_IDENTITY_SEPARATOR}{ordinal}");
        if !participants.contains_key(&candidate) {
            return candidate;
        }
    }

    format!("{requested}{DUPLICATE_IDENTITY_SEPARATOR}{}", random_id())
}

fn end_session(room: &Room, reason: livekit::DisconnectReason) {
    let participants = room.take_participants();

    tracing::info!(
        room = %room.name,
        sid = %room.sid,
        participants = participants.len(),
        ?reason,
        "room ended"
    );

    for participant in &participants {
        participant.disconnect(reason);
    }
}

fn depart(
    room: &Room,
    departed: &Participant,
    reason: livekit::DisconnectReason,
    except: Option<&str>,
) {
    let orphaned: Vec<String> = departed
        .tracks()
        .into_iter()
        .map(|track| track.sid)
        .collect();

    if !orphaned.is_empty() {
        for participant in room.participants() {
            participant.forget_subscriptions(orphaned.iter().cloned());
        }
    }

    if departed.is_hidden() {
        return;
    }

    let mut info = departed.to_proto(livekit::participant_info::State::Disconnected);
    info.disconnect_reason = reason as i32;
    room.broadcast(participant_update(vec![info]), except);
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

pub fn reconnect(reason: livekit::DisconnectReason) -> livekit::signal_response::Message {
    livekit::signal_response::Message::Leave(livekit::LeaveRequest {
        can_reconnect: true,
        reason: reason as i32,
        action: livekit::leave_request::Action::Reconnect as i32,
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

    fn displacing() -> RoomDefaults {
        RoomDefaults {
            duplicate_identity: DuplicateIdentity::Displace,
            ..RoomDefaults::default()
        }
    }

    fn session_policy(min_humans: u32, end_grace: u32) -> RoomDefaults {
        RoomDefaults {
            min_humans,
            end_grace,
            ..RoomDefaults::default()
        }
    }

    fn recorder(identity: &str) -> ParticipantSpec {
        ParticipantSpec {
            kind: ParticipantKind::Egress,
            ..spec(identity)
        }
    }

    fn leaves(rx: &mut SignalRx) -> Vec<livekit::LeaveRequest> {
        let mut requests = Vec::new();

        while let Ok(response) = rx.try_recv() {
            if let Some(livekit::signal_response::Message::Leave(request)) = response.message {
                requests.push(request);
            }
        }

        requests
    }

    fn expect_leave(rx: &mut SignalRx) -> livekit::LeaveRequest {
        let mut requests = leaves(rx);
        assert_eq!(
            requests.len(),
            1,
            "expected one leave request, got {requests:?}"
        );

        requests.remove(0)
    }

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

    fn unpublished(rx: &mut SignalRx) -> Vec<String> {
        let mut track_sids = Vec::new();

        while let Ok(response) = rx.try_recv() {
            if let Some(livekit::signal_response::Message::TrackUnpublished(message)) =
                response.message
            {
                track_sids.push(message.track_sid);
            }
        }

        track_sids
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
        let registry = Registry::new(displacing());
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
    fn displacing_a_session_never_tells_the_successor_it_disconnected() {
        let registry = Registry::new(displacing());
        let (first_tx, _first_rx) = signal_channel();
        let (second_tx, mut second_rx) = signal_channel();

        registry.join("demo", spec("alice"), first_tx).unwrap();
        let second = registry.join("demo", spec("alice"), second_tx).unwrap();

        while let Ok(response) = second_rx.try_recv() {
            let Some(livekit::signal_response::Message::Update(update)) = response.message else {
                continue;
            };

            for info in update.participants {
                assert!(
                    info.state != livekit::participant_info::State::Disconnected as i32
                        || info.sid != second.participant.sid,
                    "the new session was told that it had disconnected"
                );
                assert_ne!(
                    info.disconnect_reason,
                    livekit::DisconnectReason::DuplicateIdentity as i32,
                    "the new session was handed the displacement notice meant for the room"
                );
            }
        }
    }

    #[test]
    fn displacing_a_session_tears_its_tracks_out_of_every_subscription() {
        let registry = Registry::new(displacing());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();
        let (again_tx, _again_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let published = alice
            .room
            .publish_track(&alice.participant.sid, &camera("cid-1"))
            .expect("alice may publish");
        bob.participant
            .add_desired_subscriptions([published.sid.clone()]);
        bob.participant.mark_subscription_active(&published.sid);
        drain(&mut bob_rx);

        let again = registry.join("demo", spec("alice"), again_tx).unwrap();
        assert_ne!(again.participant.sid, alice.participant.sid);

        assert!(
            !bob.participant
                .desired_subscriptions()
                .contains(&published.sid),
            "the subscription to a displaced session's track was left behind"
        );
        assert_eq!(
            bob.participant.subscription_diff().to_unsubscribe,
            vec![published.sid.clone()],
            "bob was never asked to tear the subscription down"
        );

        let update = expect_update(&mut bob_rx);
        assert_eq!(update.participants[0].sid, alice.participant.sid);
        assert_eq!(
            update.participants[0].state,
            livekit::participant_info::State::Disconnected as i32
        );
        assert_eq!(
            update.participants[0].disconnect_reason,
            livekit::DisconnectReason::DuplicateIdentity as i32
        );
    }

    #[test]
    fn a_displaced_session_leaving_late_changes_nothing() {
        let registry = Registry::new(displacing());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();
        let (again_tx, _again_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        let again = registry.join("demo", spec("alice"), again_tx).unwrap();

        let published = again
            .room
            .publish_track(&again.participant.sid, &camera("cid-2"))
            .expect("the new session may publish");
        bob.participant
            .add_desired_subscriptions([published.sid.clone()]);
        drain(&mut bob_rx);

        registry.leave("demo", &alice.participant.sid);

        assert_eq!(
            bob.participant.desired_subscriptions(),
            vec![published.sid],
            "the stale session took its successor's tracks down with it"
        );
        assert!(
            bob_rx.try_recv().is_err(),
            "a stale leave must not announce anything"
        );
        assert_eq!(
            registry
                .room("demo")
                .expect("the room is still open")
                .participant_count(),
            2
        );
    }

    #[test]
    fn a_displaced_session_cannot_evict_its_successor() {
        let registry = Registry::new(displacing());
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
        let (again_tx, _again_rx) = signal_channel();

        let result = registry.join("demo", spec("alice"), tx);
        assert!(matches!(result, Err(JoinError::RoomNotFound(_))));

        registry.create("demo", RoomOptions::default(), Adoption::Always);
        assert!(
            registry.join("demo", spec("alice"), again_tx).is_ok(),
            "with auto-create off, creating the room is the only way anybody gets in"
        );
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
    #[allow(deprecated)]
    fn the_layers_a_track_reports_follow_the_ones_that_are_arriving() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let mut request = camera("cid-1");
        request.layers = vec![
            layer(livekit::VideoQuality::Low, "q", 320, 180),
            layer(livekit::VideoQuality::Medium, "h", 640, 360),
            layer(livekit::VideoQuality::High, "f", 1280, 720),
        ];

        let published = bob
            .room
            .publish_track(&bob.participant.sid, &request)
            .expect("bob may publish");
        assert_eq!(published.to_proto().layers.len(), 3);
        assert!(published.to_proto().simulcast);
        drain(&mut alice_rx);

        let live = bob
            .room
            .set_track_live_layers(
                &bob.participant.sid,
                &published.sid,
                vec![
                    layer(livekit::VideoQuality::Low, "q", 320, 180),
                    layer(livekit::VideoQuality::Medium, "h", 640, 360),
                ],
            )
            .expect("the live layers are recorded");

        assert_eq!(live.live_layers.len(), 2);
        assert_eq!(live.layers.len(), 3, "the declared ladder is kept");
        assert!(
            live.to_proto().simulcast,
            "a track whose top layer went quiet is still a simulcast track"
        );

        let update = expect_update(&mut alice_rx);
        let reported = &update.participants[0].tracks[0].layers;
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[1].rid, "h");
        assert!(live.version > published.version);

        assert!(
            bob.room
                .set_track_live_layers(
                    &bob.participant.sid,
                    &published.sid,
                    vec![
                        layer(livekit::VideoQuality::Low, "q", 320, 180),
                        layer(livekit::VideoQuality::Medium, "h", 640, 360),
                    ],
                )
                .is_none(),
            "an unchanged layer set is not announced again"
        );

        assert_eq!(alice.room.participant_count(), 2);
    }

    fn layer(
        quality: livekit::VideoQuality,
        rid: &str,
        width: u32,
        height: u32,
    ) -> livekit::VideoLayer {
        livekit::VideoLayer {
            quality: quality as i32,
            rid: rid.to_owned(),
            width,
            height,
            ..Default::default()
        }
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

    #[test]
    fn creating_a_room_that_exists_returns_the_one_that_exists() {
        let registry = Registry::new(RoomDefaults::default());

        let first = registry.create(
            "demo",
            RoomOptions {
                max_participants: Some(2),
                ..RoomOptions::default()
            },
            Adoption::Always,
        );
        let again = registry.create(
            "demo",
            RoomOptions {
                max_participants: Some(9),
                ..RoomOptions::default()
            },
            Adoption::Always,
        );

        assert_eq!(first.sid, again.sid);
        assert_eq!(registry.rooms().len(), 1);
        assert_eq!(
            again.settings().max_participants,
            2,
            "a retried create rewrote the room somebody is already sitting in"
        );
    }

    #[test]
    fn a_created_room_carries_settings_of_its_own() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let room = registry.create(
            "demo",
            RoomOptions {
                metadata: "{\"minHumans\":2}".to_owned(),
                max_participants: Some(1),
                empty_timeout: Some(60),
                ..RoomOptions::default()
            },
            Adoption::Always,
        );

        let proto = room.to_proto();
        assert_eq!(proto.metadata, "{\"minHumans\":2}");
        assert_eq!(proto.max_participants, 1);
        assert_eq!(proto.empty_timeout, 60);
        assert_eq!(
            proto.departure_timeout, 20,
            "a setting the room did not override must still come from the registry"
        );

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        assert!(
            matches!(
                registry.join("demo", spec("bob"), bob_tx),
                Err(JoinError::RoomFull(_))
            ),
            "the room's own limit was ignored in favour of the registry's"
        );
        drain(&mut alice_rx);

        room.set_metadata("{\"minHumans\":3}");
        match alice_rx.try_recv().expect("a signal was queued").message {
            Some(livekit::signal_response::Message::RoomUpdate(update)) => {
                assert_eq!(update.room.unwrap().metadata, "{\"minHumans\":3}");
            }
            other => panic!("expected a room update, got {other:?}"),
        }

        room.set_metadata("{\"minHumans\":3}");
        assert!(
            alice_rx.try_recv().is_err(),
            "metadata that did not change was announced anyway"
        );
    }

    /// A room made by hand is somebody's intention, and it outlives the first
    /// person to walk out of it. A room that only exists because somebody
    /// walked in has nothing behind it once they walk out.
    #[test]
    fn a_room_that_was_created_outlives_its_participants() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        registry.create("standup", RoomOptions::default(), Adoption::Always);
        let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();
        registry.leave("standup", &alice.participant.sid);

        assert!(
            registry.room("standup").is_some(),
            "a room somebody created was thrown away behind their back"
        );

        let bob = registry.join("ad-hoc", spec("bob"), bob_tx).unwrap();
        registry.leave("ad-hoc", &bob.participant.sid);
        assert!(registry.room("ad-hoc").is_none());

        assert!(registry.delete("standup").is_some());
        assert!(registry.room("standup").is_none());
        assert!(registry.delete("standup").is_none());
    }

    /// A conference service makes a room per meeting. Without a timeout that
    /// somebody enforces, the registry and every `ListRooms` after it grow for
    /// as long as the process lives.
    #[test]
    fn a_created_room_nobody_ever_joined_is_collected() {
        let registry = Registry::new(RoomDefaults::default());
        let room = registry.create(
            "standup",
            RoomOptions {
                empty_timeout: Some(60),
                ..RoomOptions::default()
            },
            Adoption::Always,
        );
        let emptied_at = room.emptied_at_ms().expect("nobody is in it");

        assert!(
            registry.sweep(emptied_at + 59_000).is_empty(),
            "the room was collected before the timeout it reports to its callers"
        );
        assert_eq!(
            registry.sweep(emptied_at + 60_000).closed,
            vec!["standup".to_owned()],
            "empty_timeout is carried on the room and reported to every client; a room that \
             outlives it is a promise the server does not keep"
        );
        assert!(registry.room("standup").is_none());
    }

    /// `empty_timeout` waits for a room nobody has been in; `departure_timeout`
    /// waits after the last person leaves. LiveKit divides them the same way,
    /// and a meeting that is over should not be held open for the five minutes
    /// that were meant for one that has not started.
    #[test]
    fn a_created_room_is_collected_after_the_last_person_walks_out() {
        let registry = Registry::new(RoomDefaults::default());
        let (tx, _rx) = signal_channel();

        let room = registry.create("standup", RoomOptions::default(), Adoption::Always);
        let alice = registry.join("standup", spec("alice"), tx).unwrap();
        assert_eq!(
            room.emptied_at_ms(),
            None,
            "somebody is in the room, so nothing is counting down"
        );

        registry.leave("standup", &alice.participant.sid);
        let emptied_at = room.emptied_at_ms().expect("the room is empty again");

        assert!(registry.sweep(emptied_at + 19_000).is_empty());
        assert_eq!(
            registry.sweep(emptied_at + 20_000).closed,
            vec!["standup".to_owned()],
            "a room people have been in and left waits departure_timeout, not empty_timeout"
        );
    }

    /// A room whose settings a caller could not choose is the trap `auto_create`
    /// leaves, and adoption is what closes it -- but `roomCreate` names no
    /// room, so the caller has to.
    #[test]
    fn a_create_that_names_no_room_leaves_an_occupied_one_alone() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();
        drain(&mut alice_rx);

        let room = registry.create(
            "standup",
            RoomOptions {
                metadata: "the daily".to_owned(),
                max_participants: Some(5),
                min_humans: Some(2),
                ..RoomOptions::default()
            },
            Adoption::WhenEmpty,
        );

        assert_eq!(room.sid, alice.room.sid);
        assert_eq!(
            room.settings().max_participants,
            0,
            "a token with no idea which room it is talking about re-pointed the settings of a \
             meeting that was already running"
        );
        assert_eq!(room.settings().min_humans, 0, "and its session policy");
        assert_eq!(room.metadata(), "");
        assert!(!room.is_persistent());
        assert!(
            alice_rx.try_recv().is_err(),
            "nothing changed, so the room was told nothing"
        );
    }

    #[test]
    fn deleting_a_room_disconnects_everybody_in_it() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        registry.join("demo", spec("bob"), bob_tx).unwrap();
        drain(&mut alice_rx);
        drain(&mut bob_rx);

        let deleted = registry.delete("demo").expect("the room existed");

        assert!(registry.room("demo").is_none());
        assert_eq!(
            deleted.participant_count(),
            0,
            "a deleted room still holds the people it was carrying"
        );

        for (who, rx) in [("alice", &mut alice_rx), ("bob", &mut bob_rx)] {
            let leave = expect_leave(rx);
            assert_eq!(
                leave.reason,
                livekit::DisconnectReason::RoomDeleted as i32,
                "{who} was cut off without being told the room was deleted"
            );
            assert!(
                !leave.can_reconnect,
                "{who} was invited back into a room that no longer exists"
            );
        }

        registry.leave("demo", &alice.participant.sid);
        assert!(registry.room("demo").is_none());
    }

    #[test]
    fn removing_a_participant_tells_it_and_the_room() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();

        let published = bob
            .room
            .publish_track(&bob.participant.sid, &camera("cid-1"))
            .expect("bob may publish");
        alice
            .participant
            .add_desired_subscriptions([published.sid.clone()]);
        alice.participant.mark_subscription_active(&published.sid);
        drain(&mut alice_rx);
        drain(&mut bob_rx);

        let removed = registry
            .remove_participant("demo", "bob")
            .expect("bob was in the room");

        assert_eq!(removed.sid, bob.participant.sid);
        assert_eq!(
            expect_leave(&mut bob_rx).reason,
            livekit::DisconnectReason::ParticipantRemoved as i32,
            "bob was dropped without being told why"
        );

        let update = expect_update(&mut alice_rx);
        assert_eq!(update.participants[0].sid, bob.participant.sid);
        assert_eq!(
            update.participants[0].state,
            livekit::participant_info::State::Disconnected as i32
        );
        assert_eq!(
            update.participants[0].disconnect_reason,
            livekit::DisconnectReason::ParticipantRemoved as i32
        );

        assert!(
            alice.participant.desired_subscriptions().is_empty(),
            "alice is still subscribed to a track whose publisher was removed"
        );
        assert_eq!(
            alice.participant.subscription_diff().to_unsubscribe,
            vec![published.sid]
        );
        assert_eq!(alice.room.participant_count(), 1);
        assert!(registry.remove_participant("demo", "bob").is_none());
    }

    #[test]
    fn the_session_ends_when_too_few_humans_remain() {
        let registry = Registry::new(session_policy(2, 90));
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        assert_eq!(
            alice.room.session_ends_at_ms(),
            None,
            "a room with the humans it asked for is not on its way out"
        );

        registry.leave("demo", &bob.participant.sid);
        drain(&mut alice_rx);
        let ends_at = alice
            .room
            .session_ends_at_ms()
            .expect("the grace period is running");

        assert!(
            registry.sweep(ends_at - 1).is_empty(),
            "the room ended before the grace period it promised was over"
        );
        assert!(registry.room("demo").is_some());

        assert_eq!(registry.sweep(ends_at).ended, vec!["demo".to_owned()]);
        assert!(registry.room("demo").is_none());
        assert_eq!(
            expect_leave(&mut alice_rx).reason,
            livekit::DisconnectReason::RoomClosed as i32,
            "the last person left in the room was cut off without being told the session was over"
        );
        assert_eq!(alice.room.participant_count(), 0);
    }

    #[test]
    fn a_rejoin_inside_the_grace_period_calls_it_off() {
        let registry = Registry::new(session_policy(2, 90));
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();
        let (again_tx, _again_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        registry.leave("demo", &bob.participant.sid);

        let ends_at = alice
            .room
            .session_ends_at_ms()
            .expect("the grace period is running");
        drain(&mut alice_rx);

        registry.join("demo", spec("bob"), again_tx).unwrap();

        assert_eq!(
            alice.room.session_ends_at_ms(),
            None,
            "bob came back and the room is still counting down to its own end"
        );
        assert!(registry.sweep(ends_at + 60_000).is_empty());
        assert!(registry.room("demo").is_some());
        assert!(
            leaves(&mut alice_rx).is_empty(),
            "alice was thrown out of a room that filled up again"
        );
    }

    /// The policy counts people, not connections. A recorder sitting in the
    /// room keeps it from being empty and must not keep it from ending, which
    /// is the whole reason `ParticipantInfo.kind` is carried at all.
    #[test]
    fn a_recorder_is_not_one_of_the_humans_the_policy_counts() {
        let registry = Registry::new(session_policy(2, 90));
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();
        let (recorder_tx, mut recorder_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        registry.leave("demo", &bob.participant.sid);

        let scribe = registry
            .join("demo", recorder("scribe"), recorder_tx)
            .unwrap();
        assert_eq!(
            scribe
                .participant
                .to_proto(livekit::participant_info::State::Active)
                .kind,
            livekit::participant_info::Kind::Egress as i32
        );

        let ends_at = alice
            .room
            .session_ends_at_ms()
            .expect("the grace period is still running");
        assert_eq!(alice.room.participant_count(), 2);
        assert_eq!(
            alice.room.human_count(),
            1,
            "the recorder was counted as somebody who came to talk"
        );
        drain(&mut alice_rx);
        drain(&mut recorder_rx);

        assert_eq!(
            registry.sweep(ends_at).ended,
            vec!["demo".to_owned()],
            "a recorder in the room held the session open past its grace period"
        );
        assert_eq!(
            expect_leave(&mut alice_rx).reason,
            livekit::DisconnectReason::RoomClosed as i32
        );
        assert_eq!(
            expect_leave(&mut recorder_rx).reason,
            livekit::DisconnectReason::RoomClosed as i32
        );
    }

    /// A room that has not filled yet is waiting for people, not losing them.
    /// Arming the grace on the way up would end every scheduled meeting ninety
    /// seconds before the second person arrived.
    #[test]
    fn the_grace_period_does_not_start_before_the_room_has_filled() {
        let registry = Registry::new(session_policy(2, 90));
        let (tx, mut rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), tx).unwrap();

        assert_eq!(alice.room.session_ends_at_ms(), None);
        assert!(registry.sweep(now_ms() + 3_600_000).ended.is_empty());
        assert!(registry.room("demo").is_some());
        assert!(leaves(&mut rx).is_empty());
    }

    /// `livekit-client` keys its remote participants by identity, so two
    /// connections sharing one are invisible to every client in the room. The
    /// server hands the second one an identity of its own and keeps the
    /// display name, which is the half a person actually reads.
    #[test]
    fn a_second_connection_from_the_same_person_gets_an_identity_of_its_own() {
        let registry = Registry::new(RoomDefaults::default());
        let (first_tx, mut first_rx) = signal_channel();
        let (second_tx, _second_rx) = signal_channel();

        let mut alice = spec("alice");
        alice.name = "Alice".to_owned();

        let first = registry.join("demo", alice.clone(), first_tx).unwrap();
        let second = registry.join("demo", alice, second_tx).unwrap();

        assert_eq!(first.participant.identity, "alice");
        assert_eq!(second.participant.identity, "alice#2");
        assert_ne!(first.participant.sid, second.participant.sid);
        assert_eq!(
            second.participant.name(),
            "Alice",
            "the person's name belongs to the person, not to the connection"
        );
        assert_eq!(second.room.participant_count(), 2);
        assert_eq!(second.others.len(), 1);
        assert_eq!(second.others[0].identity, "alice");

        let update = expect_update(&mut first_rx);
        assert_eq!(update.participants[0].identity, "alice#2");
        assert_eq!(update.participants[0].name, "Alice");
        assert_eq!(
            update.participants[0].state,
            livekit::participant_info::State::Joined as i32,
            "the first tab was told its own person had left"
        );
        assert!(
            leaves(&mut first_rx).is_empty(),
            "opening a second tab threw the first one out"
        );
        assert_eq!(first.participant.close_reason(), None);
    }

    #[test]
    fn a_suffixed_identity_never_lands_on_one_that_is_taken() {
        let registry = Registry::new(RoomDefaults::default());
        let (first_tx, _first_rx) = signal_channel();
        let (taken_tx, _taken_rx) = signal_channel();
        let (third_tx, _third_rx) = signal_channel();

        registry.join("demo", spec("alice"), first_tx).unwrap();
        registry.join("demo", spec("alice#2"), taken_tx).unwrap();
        let third = registry.join("demo", spec("alice"), third_tx).unwrap();

        assert_eq!(
            third.participant.identity, "alice#3",
            "a token that already carries a suffix was displaced by a generated one"
        );
        assert_eq!(third.room.participant_count(), 3);
    }

    /// A client that reconnects sends the sid of the session it thinks it
    /// still has. Without honouring it, every dropped socket would come back
    /// as a new person and leave its own ghost sitting in the room until the
    /// signal timeout collected it.
    #[test]
    fn a_resuming_session_takes_back_the_identity_it_had() {
        let registry = Registry::new(RoomDefaults::default());
        let (first_tx, mut first_rx) = signal_channel();
        let (second_tx, mut second_rx) = signal_channel();
        let (again_tx, _again_rx) = signal_channel();

        let first = registry.join("demo", spec("alice"), first_tx).unwrap();
        let second = registry.join("demo", spec("alice"), second_tx).unwrap();
        assert_eq!(second.participant.identity, "alice#2");

        let resume = ParticipantSpec {
            replaces: Some(second.participant.sid.clone()),
            ..spec("alice")
        };
        let again = registry.join("demo", resume, again_tx).unwrap();

        assert_eq!(again.participant.identity, "alice#2");
        assert_eq!(
            again.room.participant_count(),
            2,
            "a session that came back was counted as a third person"
        );
        assert_eq!(
            expect_leave(&mut second_rx).reason,
            livekit::DisconnectReason::DuplicateIdentity as i32
        );
        assert_eq!(
            registry
                .room("demo")
                .unwrap()
                .participant("alice#2")
                .unwrap()
                .sid,
            again.participant.sid
        );
        assert!(
            leaves(&mut first_rx).is_empty(),
            "the other tab was thrown out by a session that was not it"
        );
        assert_eq!(first.participant.identity, "alice");
    }

    #[test]
    fn a_resume_claim_cannot_name_somebody_elses_session() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();
        let (mallory_tx, _mallory_rx) = signal_channel();

        registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        drain(&mut bob_rx);

        let claim = ParticipantSpec {
            replaces: Some(bob.participant.sid.clone()),
            ..spec("mallory")
        };
        let mallory = registry.join("demo", claim, mallory_tx).unwrap();

        assert_eq!(mallory.participant.identity, "mallory");
        assert_eq!(mallory.room.participant_count(), 3);
        assert!(
            leaves(&mut bob_rx).is_empty(),
            "a token for one identity evicted the session of another"
        );
        assert_eq!(
            registry
                .room("demo")
                .unwrap()
                .participant("bob")
                .unwrap()
                .sid,
            bob.participant.sid
        );
    }

    #[test]
    fn a_room_the_first_arrival_made_is_settled_by_a_create() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();

        assert!(!alice.room.is_persistent());

        let room = registry.create(
            "standup",
            RoomOptions {
                metadata: "the daily".to_owned(),
                max_participants: Some(5),
                ..RoomOptions::default()
            },
            Adoption::Always,
        );

        assert_eq!(
            room.sid, alice.room.sid,
            "the create made a second room beside the one alice is in"
        );
        assert_eq!(room.settings().max_participants, 5);
        assert_eq!(room.metadata(), "the daily");

        match alice_rx.try_recv().expect("a signal was queued").message {
            Some(livekit::signal_response::Message::RoomUpdate(update)) => {
                assert_eq!(update.room.unwrap().metadata, "the daily")
            }
            other => panic!("expected a room update, got {other:?}"),
        }

        registry.leave("standup", &alice.participant.sid);
        assert_eq!(
            registry.rooms().len(),
            1,
            "a room somebody asked for by name has to outlive the arrival who happened to open it, \
             or the settings and the session policy vanish the moment the room empties"
        );
    }

    #[test]
    fn one_person_holding_two_connections_is_found_by_either_name() {
        let registry = Registry::new(RoomDefaults::default());
        let (first_tx, _first_rx) = signal_channel();
        let (second_tx, _second_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let first = registry.join("demo", spec("alice"), first_tx).unwrap();
        let second = registry.join("demo", spec("alice"), second_tx).unwrap();
        registry.join("demo", spec("bob"), bob_tx).unwrap();

        let both: Vec<String> = first
            .room
            .participants_of("alice")
            .into_iter()
            .map(|participant| participant.identity.clone())
            .collect();
        assert_eq!(
            both,
            vec!["alice".to_owned(), "alice#2".to_owned()],
            "a caller who names the person means every session that person opened"
        );

        let one: Vec<String> = first
            .room
            .participants_of("alice#2")
            .into_iter()
            .map(|participant| participant.identity.clone())
            .collect();
        assert_eq!(one, vec![second.participant.identity.clone()]);
        assert!(first.room.participants_of("alic").is_empty());
        assert!(
            first.room.participants_of("alice#").is_empty(),
            "the suffix is a separator and an ordinal, and half of it names nobody"
        );
    }

    #[test]
    fn revoking_the_right_to_publish_takes_the_tracks_with_it() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        let track = alice
            .room
            .publish_track(&alice.participant.sid, &camera("cam"))
            .unwrap();
        bob.participant
            .add_desired_subscriptions([track.sid.clone()]);
        drain(&mut alice_rx);
        drain(&mut bob_rx);

        let updated = alice
            .room
            .update_participant(
                "alice",
                ProfileUpdate {
                    permission: Some(livekit::ParticipantPermission {
                        can_subscribe: true,
                        can_publish: false,
                        ..Default::default()
                    }),
                    ..ProfileUpdate::default()
                },
            )
            .expect("alice is in the room");

        assert_eq!(
            updated.unpublished,
            vec![track.sid.clone()],
            "the room forgot the track without telling the caller, so nothing can tell the \
             forwarder to stop sending the packets that are still arriving for it"
        );
        assert!(
            alice.participant.tracks().is_empty(),
            "a participant who may no longer publish keeps sending until the track is taken away \
             from them, so revoking the permission alone changes nothing anybody can hear"
        );
        assert_eq!(
            unpublished(&mut alice_rx),
            vec![track.sid.clone()],
            "alice's own client was never told to stop, so it keeps the camera on"
        );
        assert!(
            bob.participant.desired_subscriptions().is_empty(),
            "bob is still waiting for a track that no longer exists"
        );
    }

    #[test]
    fn revoking_the_right_to_subscribe_drops_what_was_wanted() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("demo", spec("bob"), bob_tx).unwrap();
        let track = alice
            .room
            .publish_track(&alice.participant.sid, &camera("cam"))
            .unwrap();
        bob.participant.add_desired_subscriptions([track.sid]);

        alice.room.update_participant(
            "bob",
            ProfileUpdate {
                permission: Some(livekit::ParticipantPermission {
                    can_subscribe: false,
                    can_publish: true,
                    ..Default::default()
                }),
                ..ProfileUpdate::default()
            },
        );

        assert!(
            bob.participant.desired_subscriptions().is_empty(),
            "bob keeps receiving what he already asked for unless the intent behind it is dropped"
        );
    }

    #[test]
    fn an_update_that_changes_nothing_is_not_announced() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let (bob_tx, mut bob_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        registry.join("demo", spec("bob"), bob_tx).unwrap();
        drain(&mut bob_rx);

        alice.room.update_participant(
            "alice",
            ProfileUpdate {
                name: Some("alice".to_owned()),
                ..ProfileUpdate::default()
            },
        );

        assert!(
            bob_rx.try_recv().is_err(),
            "a backoffice that resends the same profile on every render would flood the room with \
             updates that say nothing"
        );
    }

    #[test]
    fn an_attribute_set_to_nothing_is_deleted() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, _alice_rx) = signal_channel();
        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();

        alice.room.update_participant(
            "alice",
            ProfileUpdate {
                attributes: HashMap::from([
                    ("role".to_owned(), "host".to_owned()),
                    ("seat".to_owned(), "front".to_owned()),
                ]),
                ..ProfileUpdate::default()
            },
        );
        assert_eq!(alice.participant.attributes().len(), 2);

        alice.room.update_participant(
            "alice",
            ProfileUpdate {
                attributes: HashMap::from([("role".to_owned(), String::new())]),
                ..ProfileUpdate::default()
            },
        );

        let attributes = alice.participant.attributes();
        assert_eq!(
            attributes.get("seat").map(String::as_str),
            Some("front"),
            "an update names the attributes it touches and leaves the rest alone"
        );
        assert!(
            !attributes.contains_key("role"),
            "an empty value is how LiveKit spells a deletion, not how it spells a blank"
        );
    }

    #[test]
    fn a_hidden_participant_hears_about_themselves_and_nobody_else_does() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (ghost_tx, mut ghost_rx) = signal_channel();

        let alice = registry.join("demo", spec("alice"), alice_tx).unwrap();
        let ghost = ParticipantSpec {
            permission: livekit::ParticipantPermission {
                hidden: true,
                can_subscribe: true,
                ..Default::default()
            },
            ..spec("recorder")
        };
        registry.join("demo", ghost, ghost_tx).unwrap();
        drain(&mut alice_rx);
        drain(&mut ghost_rx);

        alice.room.update_participant(
            "recorder",
            ProfileUpdate {
                metadata: Some("recording to s3".to_owned()),
                ..ProfileUpdate::default()
            },
        );

        assert_eq!(
            expect_update(&mut ghost_rx).participants[0].metadata,
            "recording to s3"
        );
        assert!(
            alice_rx.try_recv().is_err(),
            "announcing the update would put a participant the room is not supposed to see into \
             everybody's roster"
        );
    }

    #[test]
    fn a_policy_that_arrives_after_the_room_did_still_ends_the_session() {
        let registry = Registry::new(RoomDefaults::default());
        let (alice_tx, mut alice_rx) = signal_channel();
        let (bob_tx, _bob_rx) = signal_channel();

        let room = registry.create("standup", RoomOptions::default(), Adoption::Always);
        let alice = registry.join("standup", spec("alice"), alice_tx).unwrap();
        let bob = registry.join("standup", spec("bob"), bob_tx).unwrap();
        assert_eq!(room.settings().min_humans, 0);

        room.set_session_policy(Some(2), Some(0));
        assert!(
            room.session_ends_at_ms().is_none(),
            "the room is full enough, there is nothing to end"
        );

        registry.leave("standup", &bob.participant.sid);
        assert!(
            room.session_ends_at_ms().is_some(),
            "a policy set on a room that already reached its quorum has to count that quorum as \
             reached, or the first departure after it arrives is never noticed"
        );
        drain(&mut alice_rx);

        assert_eq!(registry.sweep(now_ms()).ended, vec!["standup"]);
        assert_eq!(
            expect_leave(&mut alice_rx).reason,
            livekit::DisconnectReason::RoomClosed as i32
        );
        assert!(registry.rooms().is_empty());
        let _ = alice;
    }
}
