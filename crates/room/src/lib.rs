use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mediacore_events::{Bus, Event};
use mediacore_proto::mediacore;
use mediacore_session::{Member, SessionStore};
use tokio::sync::mpsc;

mod audience;
mod subscription;
mod track;

pub use audience::Audience;
pub use subscription::{SubscriptionDiff, Subscriptions};
pub use track::{
    MAX_PUBLISHED_TRACKS, PublishError, PublishedTrack, TRACK_SID_PREFIX, TrackKind, TrackVersion,
};

use track::generate_track_sid;

pub const SIGNAL_CHANNEL_CAPACITY: usize = 256;

pub type SignalTx = mpsc::Sender<mediacore::SignalResponse>;
pub type SignalRx = mpsc::Receiver<mediacore::SignalResponse>;

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
    pub fn disconnect_reason(&self) -> mediacore::DisconnectReason {
        mediacore::DisconnectReason::JoinFailure
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
    Sip,
    Agent,
}

impl ParticipantKind {
    pub fn to_proto(self) -> mediacore::participant_info::Kind {
        match self {
            Self::Standard => mediacore::participant_info::Kind::Standard,
            Self::Egress => mediacore::participant_info::Kind::Egress,
            Self::Sip => mediacore::participant_info::Kind::Sip,
            Self::Agent => mediacore::participant_info::Kind::Agent,
        }
    }

    /// The same kinds as they are spelled in a session record.
    ///
    /// The record has an enum of its own because the crate that writes records
    /// knows nothing about rooms -- the dependency runs one way -- so the two
    /// are matched up here, once.
    pub fn to_record(self) -> mediacore_session::Kind {
        match self {
            Self::Standard => mediacore_session::Kind::Standard,
            Self::Egress => mediacore_session::Kind::Egress,
            Self::Sip => mediacore_session::Kind::Sip,
            Self::Agent => mediacore_session::Kind::Agent,
        }
    }

    /// Whether this one is a person who came to the meeting.
    ///
    /// Somebody on a telephone is somebody in the room: a call that dialled in
    /// has to count towards the floor a room opens on, or a meeting held
    /// entirely over the phone is a meeting the server never starts and never
    /// records.
    pub fn is_human(self) -> bool {
        matches!(self, Self::Standard | Self::Sip)
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
    /// Whether a room's meetings are recorded when the room does not say.
    pub record: bool,
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
            record: false,
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
    pub record: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSettings {
    pub metadata: String,
    pub empty_timeout: u32,
    pub departure_timeout: u32,
    pub max_participants: u32,
    pub min_humans: u32,
    pub end_grace: u32,
    /// Whether meetings held in this room are recorded.
    ///
    /// Beside `min_humans` rather than anywhere else, because the two are one
    /// decision: recording follows the meeting, and how many people make a
    /// meeting is the only threshold there is. A room where everybody sits in
    /// one place and joins from one account sets `min_humans` to one and is
    /// recorded like any other -- there is no second minimum that could refuse
    /// it, which is exactly the rule that lost meetings next door.
    pub record: bool,
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
            record: options.record.unwrap_or(defaults.record),
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
    pub permission: mediacore::ParticipantPermission,
    pub kind: ParticipantKind,
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Profile {
    name: String,
    metadata: String,
    attributes: HashMap<String, String>,
    permission: mediacore::ParticipantPermission,
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
    pub permission: Option<mediacore::ParticipantPermission>,
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
    audience: Mutex<Audience>,
    tx: Mutex<Option<SignalTx>>,
    close_reason: Mutex<Option<mediacore::DisconnectReason>>,
}

impl Participant {
    pub fn to_proto(
        &self,
        state: mediacore::participant_info::State,
    ) -> mediacore::ParticipantInfo {
        let profile = self.profile.lock().expect("profile lock").clone();

        mediacore::ParticipantInfo {
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

    pub fn permission(&self) -> mediacore::ParticipantPermission {
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

    /// Whether this publisher lets `subscriber` hear one of their tracks.
    pub fn admits(&self, subscriber: &Participant, track_sid: &str) -> bool {
        self.audience.lock().expect("audience lock").admits(
            &subscriber.sid,
            &subscriber.identity,
            track_sid,
        )
    }

    pub(crate) fn set_audience(&self, permission: &mediacore::SubscriptionPermission) {
        self.audience.lock().expect("audience lock").set(permission);
    }

    pub fn can_update_metadata(&self) -> bool {
        self.profile
            .lock()
            .expect("profile lock")
            .permission
            .can_update_metadata
    }

    /// How many bytes of name, metadata and attributes this participant would
    /// carry once a patch is applied.
    ///
    /// Measured against the result rather than against the patch: attributes
    /// are merged key by key, so a participant who may set them could otherwise
    /// grow their profile without limit one small patch at a time.
    pub fn profile_bytes_with(&self, update: &ProfileUpdate) -> usize {
        let profile = self.profile.lock().expect("profile lock");

        let name = update.name.as_deref().unwrap_or(&profile.name).len();
        let metadata = update
            .metadata
            .as_deref()
            .unwrap_or(&profile.metadata)
            .len();

        let mut attributes: HashMap<&str, &str> = profile
            .attributes
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();

        for (key, value) in &update.attributes {
            if value.is_empty() {
                attributes.remove(key.as_str());
            } else {
                attributes.insert(key.as_str(), value.as_str());
            }
        }

        name + metadata
            + attributes
                .iter()
                .map(|(key, value)| key.len() + value.len())
                .sum::<usize>()
    }

    pub fn can_publish_data(&self) -> bool {
        self.profile
            .lock()
            .expect("profile lock")
            .permission
            .can_publish_data
    }

    pub fn can_publish_source(&self, source: mediacore::TrackSource) -> bool {
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

    pub fn track_infos(&self) -> Vec<mediacore::TrackInfo> {
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
        request: &mediacore::AddTrackRequest,
    ) -> Result<PublishedTrack, PublishError> {
        let Some(kind) = TrackKind::from_proto(request.r#type) else {
            return Err(PublishError::UnsupportedKind(request.r#type));
        };
        let source = mediacore::TrackSource::try_from(request.source)
            .unwrap_or(mediacore::TrackSource::Unknown);

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
        layers: Vec<mediacore::VideoLayer>,
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

    pub fn send(&self, message: mediacore::signal_response::Message) -> bool {
        let droppable = is_droppable(&message);
        let name = message_name(&message);
        let response = mediacore::SignalResponse {
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
                self.close_signal(mediacore::DisconnectReason::SignalClose);
                false
            }
        }
    }

    pub fn close_signal(&self, reason: mediacore::DisconnectReason) {
        let closed = self.tx.lock().expect("signal lock").take().is_some();

        if !closed {
            return;
        }

        *self.close_reason.lock().expect("close reason lock") = Some(reason);
    }

    pub fn close_reason(&self) -> Option<mediacore::DisconnectReason> {
        *self.close_reason.lock().expect("close reason lock")
    }

    pub fn disconnect(&self, reason: mediacore::DisconnectReason) {
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
    emptied_at_ms: Mutex<Option<i64>>,
    occupied_before: AtomicBool,
    /// Where what happens in this room is written down.
    ///
    /// The room holds it rather than the registry alone because every path
    /// that changes who is in a room already ends at [`Room::reassess`] --
    /// joining, leaving, being removed, being displaced, and a policy that
    /// arrives after the room did. Recording from there means a new path
    /// cannot be added that moves people about and forgets to say so.
    sessions: Arc<SessionStore>,
    /// Where what happens in this room is announced, for anybody outside this
    /// process who wants to hear about it as it happens.
    ///
    /// Beside the record and never instead of it. Emitting cannot fail, cannot
    /// block and cannot be waited on, so it is safe to do from the middle of
    /// somebody joining -- and it is allowed to be that cheap precisely
    /// because the record above is the copy that answers questions afterwards.
    events: Arc<Bus>,
}

impl Room {
    fn new(
        name: String,
        settings: RoomSettings,
        persistent: bool,
        sessions: Arc<SessionStore>,
        events: Arc<Bus>,
    ) -> Self {
        let created_at_ms = now_ms();

        Self {
            sid: generate_sid("RM"),
            name,
            created_at_ms,
            persistent: AtomicBool::new(persistent),
            settings: Mutex::new(settings),
            participants: Mutex::new(HashMap::new()),
            emptied_at_ms: Mutex::new(Some(created_at_ms)),
            occupied_before: AtomicBool::new(false),
            sessions,
            events,
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
            mediacore::signal_response::Message::RoomUpdate(mediacore::RoomUpdate {
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
    /// [`Adoption`]. So is calling [`Room::reassess`] afterwards, which this
    /// deliberately does not do: settling a room can change how many people it
    /// needs, which can start a meeting, which writes a record to a disk --
    /// and the registry holds its own lock across this call.
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
                mediacore::signal_response::Message::RoomUpdate(mediacore::RoomUpdate {
                    room: Some(self.to_proto()),
                }),
                None,
            );
        }
    }

    /// Moves the session policy without saying anything on the wire.
    ///
    /// There is no field for `min_humans` or `end_grace` in LiveKit's `Room`,
    /// so there is nothing a `RoomUpdate` could carry: the policy travels in
    /// the room metadata, and it is the metadata broadcast that tells the room
    /// it changed. Each `None` leaves that half of the policy where it is.
    ///
    /// Private, and settling the room afterwards is deliberately not done
    /// here: raising the floor over an occupied room can end the meeting in
    /// it, and every decision about a meeting is taken under the registry
    /// lock so that it cannot be taken against a roster that has moved on.
    /// [`Registry::set_session_policy`] is the way in.
    fn store_session_policy(
        &self,
        min_humans: Option<u32>,
        end_grace: Option<u32>,
        record: Option<bool>,
    ) {
        let mut settings = self.settings.lock().expect("settings lock");
        if let Some(min_humans) = min_humans {
            settings.min_humans = min_humans;
        }
        if let Some(end_grace) = end_grace {
            settings.end_grace = end_grace;
        }
        if let Some(record) = record {
            settings.record = record;
        }
    }

    /// Whether meetings held here are recorded.
    ///
    /// **Read when a meeting begins, and again whenever a track is published.**
    /// The first read is the one that decides: it is what binds the meeting to
    /// the recorder, and a meeting that was not bound records nothing however
    /// many tracks arrive afterwards. The second is a filter on the media path
    /// -- a track published into a room that says no gets no tap at all, so
    /// such a room costs exactly what it cost before recording existed.
    ///
    /// So a room whose policy changes while a meeting is running is a room in
    /// an odd state, and it is worth saying plainly which odd state. Turning
    /// recording **off** mid-meeting stops taps being opened for tracks
    /// published after it, while the tracks already being recorded carry on to
    /// the end of the meeting -- the recording of that meeting is of part of
    /// it, and nothing in it says which part. Turning it **on** mid-meeting
    /// opens taps whose packets go nowhere: the recorder was never told the
    /// meeting began, so every one of them is discarded, and the track says so
    /// when it ends rather than at the moment somebody changed the setting.
    ///
    /// Neither is worth a mechanism to fix. A policy applied to a meeting
    /// already in progress has no honest answer -- there is no recording of the
    /// first half to produce -- and the rule that *is* honest is the one this
    /// server keeps: recording follows the meeting, and a change takes effect
    /// with the next one.
    pub fn records(&self) -> bool {
        self.settings().record
    }

    pub fn to_proto(&self) -> mediacore::Room {
        let settings = self.settings();
        let participants = self.participants();

        mediacore::Room {
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

    /// When the meeting in this room ends if nobody comes back.
    ///
    /// Read out of the record store rather than kept here. The store is where
    /// a meeting begins and ends -- it is the thing that knows whether there
    /// is one -- and a room that armed a second deadline on a second condition
    /// disagreed with it: the store's fired on a room whose own countdown had
    /// never started, and closed the record under people who were still
    /// talking. One deadline, decided once, read here.
    pub fn session_ends_at_ms(&self) -> Option<i64> {
        self.sessions.ends_at(&self.name)
    }

    /// When the room last became empty, or `None` while somebody is in it.
    pub fn emptied_at_ms(&self) -> Option<i64> {
        *self.emptied_at_ms.lock().expect("occupancy lock")
    }

    /// Tells the room's clocks that its membership or its policy changed.
    ///
    /// Every path that adds or removes a participant ends here, and so does
    /// every path that moves the policy, because a room that is counting down
    /// to its own end has to notice both kinds of change.
    ///
    /// Called with the registry lock held, always. The roster it reads and the
    /// record it writes have to be one step: two threads letting two people
    /// into the same room used to snapshot a roster each and then race to hand
    /// them over, and the older snapshot landing second wrote a record saying
    /// people had left and come back at moments they sat still.
    ///
    /// Returns the meeting if this change started one, so the caller can
    /// announce it once it has let the lock go. Nothing is announced from
    /// under the lock.
    #[must_use = "a meeting that started here has to be announced"]
    fn reassess(&self, now_ms: i64) -> Option<mediacore_session::Session> {
        self.note_occupancy(now_ms);
        self.note_session(now_ms)
    }

    /// Tells the session store who is in the room now.
    ///
    /// The floor the store is given is the room's own `min_humans`, and the
    /// store raises a zero to one: a room whose operator asked for no policy
    /// at all still holds meetings, and a meeting begins when the first person
    /// arrives. Reporting the policy verbatim and letting the store decide
    /// what a zero means keeps the two halves of "when is there a meeting" in
    /// one place instead of splitting them across two crates.
    ///
    /// The whole roster goes every time rather than the one person who moved,
    /// because this is called from paths that move more than one -- a
    /// displaced session leaves in the same breath its replacement arrives --
    /// and because a diff cannot be forgotten the way an event can.
    fn note_session(&self, now_ms: i64) -> Option<mediacore_session::Session> {
        let settings = self.settings();
        let roster: Vec<Member> = self
            .participants()
            .into_iter()
            .map(|participant| Member {
                sid: participant.sid.clone(),
                identity: participant.identity.clone(),
                name: participant.name(),
                kind: participant.kind.to_record(),
                // Whatever the platform above put on this participant, on its
                // way to the record of the meeting and read nowhere else. See
                // `mediacore_session::Attendance::attributes`.
                attributes: participant.attributes(),
                joined_at_ms: participant.joined_at_ms,
            })
            .collect();

        // The store decides whether this roster is the start of a meeting.
        // Announcing it is this layer's job and the caller's moment: it
        // happens once the registry lock is let go, and after the record is
        // in the store, so nothing is announced that could not then be looked
        // up.
        self.sessions.note(
            &self.name,
            &roster,
            settings.min_humans,
            settings.end_grace_ms(),
            &settings.metadata,
            now_ms,
        )
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

    /// Whether the session policy has run out on this room.
    ///
    /// The deadline alone is not the answer. It is armed on the way down and
    /// disarmed by the arrival that restores the quorum, and the sweep reads
    /// both under the registry lock the arrival has to take -- so a deadline
    /// that expired in the same instant as somebody walked back in ends
    /// nothing. Reading the policy again also means a deadline that outlived
    /// the policy that armed it cannot fire.
    ///
    /// The deadline is the meeting's, and the policy is what turns it into the
    /// room's as well. Every room has a meeting deadline -- a room with no
    /// policy at all still holds meetings and they still end -- and only a
    /// room that asked for a floor is taken away when one runs out.
    fn session_expired(&self, now_ms: i64) -> bool {
        let settings = self.settings();
        if settings.min_humans == 0 {
            return false;
        }

        let Some(ends_at) = self.session_ends_at_ms() else {
            return false;
        };

        now_ms >= ends_at && self.human_count() < settings.min_humans as usize
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
        // The policy's deadline and not the meeting's. Every room has a
        // meeting counting down in it after the last person leaves; only a
        // room whose policy would take it away has a reason to outstay its
        // departure timeout for one.
        if self.settings().min_humans > 0 && self.session_ends_at_ms().is_some() {
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

    /// Who a data packet goes to, given the identities its sender named.
    ///
    /// An empty list is the whole room, which is what `livekit-client` means by
    /// leaving `destination_identities` unset. A named identity resolves
    /// through [`Room::participants_of`], so `alice` reaches every connection
    /// the person called alice has, suffix and all, and a token minted for the
    /// literal identity `bob#7` reaches that person rather than bob's seventh
    /// tab. The sender is never a recipient of their own packet, even when they
    /// named themselves.
    pub fn data_recipients(
        &self,
        sender_sid: Option<&str>,
        destinations: &[String],
    ) -> Vec<Arc<Participant>> {
        let mut found = if destinations.is_empty() {
            self.participants()
        } else {
            destinations
                .iter()
                .flat_map(|identity| self.participants_of(identity))
                .collect()
        };

        found.retain(|participant| Some(participant.sid.as_str()) != sender_sid);
        found.sort_by(|left, right| left.sid.cmp(&right.sid));
        found.dedup_by(|left, right| left.sid == right.sid);
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
        let info = participant.to_proto(mediacore::participant_info::State::Active);

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

        let info = participant.to_proto(mediacore::participant_info::State::Disconnected);
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
            participant.send(mediacore::signal_response::Message::TrackUnpublished(
                mediacore::TrackUnpublishedResponse {
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
        request: &mediacore::AddTrackRequest,
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
        layers: Vec<mediacore::VideoLayer>,
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
            Some((publisher, _)) => {
                publisher.sid != subscriber.sid
                    && !publisher.is_hidden()
                    && publisher.admits(&subscriber, track_sid)
            }
            None => false,
        }
    }

    /// Records who a publisher lets listen, and cuts off whoever may no longer.
    ///
    /// The permission is a gate rather than an intent: it is asked after the
    /// subscriber's own `canSubscribe` and after an admin `UpdateSubscriptions`,
    /// so the publisher's word is stricter than all three and nothing routes
    /// around it. Which is why the intent behind a subscription already running
    /// is dropped here too: the gate stops the next subscription, and only a
    /// vanished intent stops the one already forwarding.
    ///
    /// The intent is removed rather than declined. A decline is the listener's
    /// own answer and survives auto-subscription on purpose, so declining on
    /// their behalf would leave them with a black tile when the publisher opens
    /// the audience again.
    ///
    /// Answers with how many subscriptions were cut, or `None` when no session
    /// in this room carries that sid.
    pub fn set_subscription_permission(
        &self,
        publisher_sid: &str,
        permission: &mediacore::SubscriptionPermission,
    ) -> Option<usize> {
        let publisher = self.participant_by_sid(publisher_sid)?;
        let tracks: Vec<String> = publisher
            .tracks()
            .into_iter()
            .map(|track| track.sid)
            .collect();
        let listeners: Vec<Arc<Participant>> = self
            .participants()
            .into_iter()
            .filter(|participant| participant.sid != publisher.sid)
            .collect();

        let before: Vec<Vec<bool>> = listeners
            .iter()
            .map(|listener| {
                tracks
                    .iter()
                    .map(|track_sid| self.can_subscribe(&listener.sid, track_sid))
                    .collect()
            })
            .collect();

        publisher.set_audience(permission);

        let mut cut = 0;

        for (listener, was) in listeners.iter().zip(before) {
            for (track_sid, was_allowed) in tracks.iter().zip(was) {
                let allowed = self.can_subscribe(&listener.sid, track_sid);

                if allowed == was_allowed {
                    continue;
                }

                if !allowed {
                    listener.remove_desired_subscriptions([track_sid.clone()]);
                    cut += 1;
                }

                listener.send(subscription_permission_update(
                    &publisher.sid,
                    track_sid,
                    allowed,
                ));
            }
        }

        tracing::info!(
            room = %self.name,
            identity = %publisher.identity,
            cut,
            "the publisher said who may listen"
        );

        Some(cut)
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
                    .filter(|track| publisher.admits(&subscriber, &track.sid))
                    .map(|track| track.sid)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn update_subscriptions(
        &self,
        subscriber_sid: &str,
        request: &mediacore::UpdateSubscription,
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

        let info = publisher.to_proto(mediacore::participant_info::State::Active);
        self.broadcast(participant_update(vec![info]), Some(&publisher.sid));
    }

    pub fn broadcast(&self, message: mediacore::signal_response::Message, except: Option<&str>) {
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
    pub others: Vec<mediacore::ParticipantInfo>,
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
    /// Meetings whose grace period ran out, by session id.
    ///
    /// Not the same list as `ended`, and that is the point. A room with a
    /// session policy ends and its meeting ends with it, so it appears in
    /// both. A room without one -- which is every room in a deployment that
    /// never configured a policy -- keeps existing while its meeting closes,
    /// or has already been thrown away because the last person left, and only
    /// appears here.
    pub sessions: Vec<String>,
}

impl Swept {
    pub fn is_empty(&self) -> bool {
        self.ended.is_empty() && self.closed.is_empty() && self.sessions.is_empty()
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

pub struct Registry {
    defaults: RoomDefaults,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    sessions: Arc<SessionStore>,
    events: Arc<Bus>,
}

impl Registry {
    pub fn new(defaults: RoomDefaults, sessions: Arc<SessionStore>, events: Arc<Bus>) -> Self {
        Self {
            defaults,
            rooms: Mutex::new(HashMap::new()),
            sessions,
            events,
        }
    }

    /// Where what happens in these rooms is announced.
    ///
    /// A room's own events go out through the room; the two that belong to no
    /// room in particular -- a meeting ending after the room it was in has
    /// been thrown away -- go out through here.
    pub fn events(&self) -> &Arc<Bus> {
        &self.events
    }

    /// Every meeting this server has held, including the ones running now.
    ///
    /// A meeting outlives the room it happened in -- a room nobody created is
    /// gone the moment the last person leaves, and the record of what they did
    /// there is not -- so this is reached through the registry rather than
    /// through a room.
    pub fn sessions(&self) -> &Arc<SessionStore> {
        &self.sessions
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
        let (room, opened, started) = {
            let mut rooms = self.rooms.lock().expect("registry lock");

            let (room, adopted, opened) = match rooms.get(name).cloned() {
                None => {
                    let room = Arc::new(Room::new(
                        name.to_owned(),
                        RoomSettings::resolve(&self.defaults, options),
                        true,
                        Arc::clone(&self.sessions),
                        Arc::clone(&self.events),
                    ));
                    rooms.insert(name.to_owned(), Arc::clone(&room));
                    tracing::info!(room = %name, sid = %room.sid, "room created");
                    (room, false, true)
                }
                Some(room) if room.is_persistent() => {
                    tracing::debug!(
                        room = %name,
                        sid = %room.sid,
                        "room already exists, keeping it as it is"
                    );
                    (room, false, false)
                }
                Some(room) if adoption == Adoption::WhenEmpty && room.participant_count() > 0 => {
                    tracing::warn!(
                        room = %name,
                        sid = %room.sid,
                        participants = room.participant_count(),
                        "leaving an occupied room as it was: the caller named no room, and the \
                         settings it asked for were not applied"
                    );
                    (room, false, false)
                }
                Some(room) => {
                    room.adopt(RoomSettings::resolve(&self.defaults, options));
                    (room, true, false)
                }
            };

            // Under the same lock as the adoption. Settling a room can change
            // how many people it needs, which can start or end a meeting, and
            // that decision has to be taken against the roster this lock is
            // holding still -- not against one that has moved on by the time
            // it lands. Nothing here blocks: the record is written by a thread
            // of its own.
            let started = adopted.then(|| room.reassess(now_ms())).flatten();

            (room, opened, started)
        };

        // Announcements are outside the lock, in the order the things
        // happened. `room_started` only for a room that did not exist a moment
        // ago: a `CreateRoom` is a retry as often as it is a first attempt, and
        // a consumer counting rooms must not count one twice because a
        // caller's socket timed out.
        if opened {
            self.events.emit(Event::room_started(room.to_proto()));
        }
        if let Some(started) = started {
            self.events.emit(Event::session_started(started));
        }

        room
    }

    /// Moves a room's session policy, and settles what that means for the
    /// meeting in it.
    ///
    /// On the registry rather than on the room because raising the floor over
    /// an occupied room can end the meeting being held in it, and every
    /// decision about a meeting is taken under this lock so that it is taken
    /// against a roster nobody is halfway through changing.
    pub fn set_session_policy(
        &self,
        room: &Arc<Room>,
        min_humans: Option<u32>,
        end_grace: Option<u32>,
        record: Option<bool>,
    ) {
        let started = {
            let _rooms = self.rooms.lock().expect("registry lock");
            room.store_session_policy(min_humans, end_grace, record);
            room.reassess(now_ms())
        };

        if let Some(started) = started {
            self.events.emit(Event::session_started(started));
        }
    }

    /// Takes a room away, and the meeting in it with it.
    ///
    /// The meeting is closed on the spot rather than left to serve out its
    /// grace period: the grace is a promise that whoever comes back inside it
    /// finds the same meeting, and a deleted room is the one case where there
    /// is nothing to come back to. The record stays, because the meeting still
    /// happened.
    pub fn delete(&self, name: &str) -> Option<Arc<Room>> {
        self.close(name, mediacore::DisconnectReason::RoomDeleted)
    }

    /// The same, with the reason the people in it are given.
    ///
    /// `delete` is somebody asking for the room to stop existing, and that is
    /// what its occupants are told. A node letting go of its rooms on the way
    /// out is not that: the room is not being deleted and the meeting is not
    /// over as far as anybody outside is concerned, so the reason has to say
    /// the server is stopping. A client reads that field and decides whether
    /// to come back; told the room was deleted, it would not.
    ///
    /// The meeting record ends the same way either way -- `Ending::Closed`, at
    /// the time it really stopped -- because in both cases somebody watched it
    /// end. What must not happen is the meeting being left unfinished for a
    /// reaper to close later as a restart, with an end time nobody observed.
    pub fn close(
        &self,
        name: &str,
        reason: mediacore::DisconnectReason,
    ) -> Option<Arc<Room>> {
        // Taking the room away and closing its meeting are one step. Between
        // the two, an arrival would find no room, build another under the same
        // name and open a second meeting in it -- which the close would then
        // end, leaving somebody sitting in a room whose record says the
        // meeting is over.
        let (room, ended) = {
            let mut rooms = self.rooms.lock().expect("registry lock");
            let room = rooms.remove(name)?;
            let ended = self.sessions.close(name, now_ms());
            (room, ended)
        };

        end_session(&room, reason);

        // In the order it happened: everybody left, then the meeting they were
        // at ended, then the place it was in stopped existing. A consumer
        // building a timeline out of these reads that order and would have to
        // undo it if `room_finished` came first.
        if let Some(session) = ended {
            self.events.emit(Event::session_finished(session));
        }
        self.events.emit(Event::room_finished(room.to_proto()));

        Some(room)
    }

    pub fn remove_participant(&self, room_name: &str, identity: &str) -> Option<Arc<Participant>> {
        let (room, removed, closed, started) = {
            let mut rooms = self.rooms.lock().expect("registry lock");
            let room = rooms.get(room_name).cloned()?;
            let removed = room.take_participant(identity);
            let mut closed = false;

            if removed.is_some() && room.participant_count() == 0 && !room.is_persistent() {
                rooms.remove(room_name);
                closed = true;
                tracing::info!(room = %room_name, "room closed, no participants left");
            }

            // Under the same lock as the removal, so the record is written
            // against the room this call left behind and not one that has
            // moved on since.
            let started = removed.is_some().then(|| room.reassess(now_ms())).flatten();

            (room, removed, closed, started)
        };

        let removed = removed?;
        tracing::info!(
            room = %room_name,
            identity = %removed.identity,
            sid = %removed.sid,
            "participant removed"
        );

        removed.disconnect(mediacore::DisconnectReason::ParticipantRemoved);
        depart(
            &room,
            &removed,
            mediacore::DisconnectReason::ParticipantRemoved,
            None,
        );
        if let Some(started) = started {
            self.events.emit(Event::session_started(started));
        }
        if closed {
            self.events.emit(Event::room_finished(room.to_proto()));
        }

        Some(removed)
    }

    /// One pass over every room, ending the meetings that are over and closing
    /// the rooms that nobody is coming back to.
    ///
    /// This is the registry's only clock. Everything else about a room happens
    /// because somebody did something to it, and the three things that have to
    /// happen when nobody does anything -- a meeting's grace period running
    /// out, a session policy running out, an empty room being collected -- all
    /// hang off this one call. It takes the time rather than reading it so
    /// that a test can be the clock.
    ///
    /// The meetings are ended first. A room with a session policy and the
    /// meeting inside it are counting the same grace period down from the same
    /// instant, so both expire on the same tick; ending the meeting before the
    /// room is emptied means its record closes with everybody still in it
    /// rather than after they have all been disconnected.
    ///
    /// All of it under the registry lock, which is the lock an arrival takes
    /// and does not let go of until it has said so. Reading the deadlines
    /// outside it left a window in which somebody who came back inside the
    /// grace was overtaken: their meeting was ended a moment before they were
    /// counted, and the arrival then opened a second one, so one interrupted
    /// meeting was recorded as two.
    pub fn sweep(&self, now_ms: i64) -> Swept {
        let (sessions, ended, closed) = {
            let mut rooms = self.rooms.lock().expect("registry lock");

            // The rooms first, while the deadlines that decide them are still
            // armed: expiring the meetings clears those deadlines, and a room
            // reads its own from the meeting.
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

            let sessions = self.sessions.expire(now_ms);

            (sessions, ended, closed)
        };

        // The meetings first, in the same order and for the same reason they
        // were ended in: a meeting whose room is about to go closed with
        // everybody still in it, so the announcement of its end belongs before
        // the announcements of everybody being disconnected.
        let mut ended_sessions = Vec::new();
        for session in sessions {
            ended_sessions.push(session.id.clone());
            self.events.emit(Event::session_finished(session));
        }

        for room in &ended {
            end_session(room, mediacore::DisconnectReason::RoomClosed);
            self.events.emit(Event::room_finished(room.to_proto()));
        }
        for room in &closed {
            tracing::info!(
                room = %room.name,
                sid = %room.sid,
                "room closed, empty for longer than its timeout allows"
            );
            self.events.emit(Event::room_finished(room.to_proto()));
        }

        let mut swept = Swept {
            ended: ended.into_iter().map(|room| room.name.clone()).collect(),
            closed: closed.into_iter().map(|room| room.name.clone()).collect(),
            sessions: ended_sessions,
        };
        swept.ended.sort();
        swept.closed.sort();
        swept.sessions.sort();
        swept
    }

    pub fn join(
        &self,
        room_name: &str,
        spec: ParticipantSpec,
        tx: SignalTx,
    ) -> Result<Joined, JoinError> {
        let mut opened = false;
        let (room, participant, displaced, started) = {
            let mut rooms = self.rooms.lock().expect("registry lock");

            let room = match rooms.get(room_name) {
                Some(room) => Arc::clone(room),
                None if self.defaults.auto_create => {
                    let room = Arc::new(Room::new(
                        room_name.to_owned(),
                        RoomSettings::resolve(&self.defaults, RoomOptions::default()),
                        false,
                        Arc::clone(&self.sessions),
                        Arc::clone(&self.events),
                    ));
                    rooms.insert(room_name.to_owned(), Arc::clone(&room));
                    tracing::info!(
                        room = %room_name,
                        sid = %room.sid,
                        "room created by the first participant to arrive"
                    );
                    opened = true;
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
                audience: Mutex::new(Audience::default()),
                tx: Mutex::new(Some(tx)),
                close_reason: Mutex::new(None),
            });
            let displaced = participants.insert(identity, Arc::clone(&participant));
            drop(participants);

            // Still under the registry lock, and that is the point. The
            // arrival and the record of it are one step: a sweep that read the
            // deadlines between the two would end the meeting this person was
            // walking back into, and the record would then show one
            // interrupted meeting as two.
            let started = room.reassess(now_ms());

            (room, participant, displaced, started)
        };

        // Announced before the arrival that caused it, so a consumer never
        // sees somebody join a room it has not been told about.
        if opened {
            self.events.emit(Event::room_started(room.to_proto()));
        }

        if let Some(displaced) = displaced {
            tracing::info!(
                room = %room_name,
                identity = %displaced.identity,
                sid = %displaced.sid,
                "displacing an earlier session with the same identity"
            );
            displaced.disconnect(mediacore::DisconnectReason::DuplicateIdentity);
            depart(
                &room,
                &displaced,
                mediacore::DisconnectReason::DuplicateIdentity,
                Some(&participant.sid),
            );
        }

        let others = room
            .participants()
            .into_iter()
            .filter(|other| other.sid != participant.sid)
            .filter(|other| !other.is_hidden())
            .map(|other| other.to_proto(mediacore::participant_info::State::Active))
            .collect();

        if !participant.is_hidden() {
            room.broadcast(
                participant_update(vec![
                    participant.to_proto(mediacore::participant_info::State::Joined),
                ]),
                Some(&participant.sid),
            );
        }

        // Before the reassess, so the arrival is announced before the meeting
        // it may have started: a `session_started` whose first attendee has
        // not been announced yet reads as a meeting nobody came to.
        //
        // Hidden participants are announced too, unlike on the wire. A
        // recorder is invisible to the people in the room on purpose; it is
        // not invisible to whoever is billing for the recording.
        self.events.emit(Event::participant_joined(
            room.to_proto(),
            participant.to_proto(mediacore::participant_info::State::Active),
        ));

        if let Some(started) = started {
            self.events.emit(Event::session_started(started));
        }

        Ok(Joined {
            room,
            participant,
            others,
        })
    }

    pub fn leave(&self, room_name: &str, sid: &str) {
        let (room, departed, closed) = {
            let mut rooms = self.rooms.lock().expect("registry lock");
            let Some(room) = rooms.get(room_name).cloned() else {
                return;
            };

            let departed = room.take_participant_by_sid(sid);
            let mut closed = false;

            if departed.is_some() && room.participant_count() == 0 && !room.is_persistent() {
                rooms.remove(room_name);
                closed = true;
                tracing::info!(room = %room_name, "room closed, no participants left");
            }

            // Under the same lock as the departure, so the grace period this
            // starts is armed against the room the departure left behind. A
            // departure counted after somebody else's arrival had already been
            // counted would arm a countdown over a room that had filled up
            // again.
            if departed.is_some() {
                let started = room.reassess(now_ms());
                debug_assert!(started.is_none(), "somebody leaving started a meeting");
            }

            (room, departed, closed)
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
            mediacore::DisconnectReason::ClientInitiated,
            None,
        );

        // A room nobody created goes when its last participant does, and it is
        // announced here rather than inside `depart` because `depart` runs for
        // every departure and this happens only on the last one. The meeting
        // is not over: it holds its grace period open in case they come back,
        // and `session_finished` follows from the sweep if they do not.
        if closed {
            self.events.emit(Event::room_finished(room.to_proto()));
        }
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

fn end_session(room: &Room, reason: mediacore::DisconnectReason) {
    let participants = room.take_participants();

    tracing::info!(
        room = %room.name,
        sid = %room.sid,
        participants = participants.len(),
        ?reason,
        "room ended"
    );

    // One snapshot of the room for all of them, taken after the room was
    // emptied. Asking the room again per participant would be quadratic in a
    // room of five hundred, and every one of those answers would be the same.
    let emptied = room.to_proto();
    for participant in &participants {
        participant.disconnect(reason);

        let mut info = participant.to_proto(mediacore::participant_info::State::Disconnected);
        info.disconnect_reason = reason as i32;
        room.events
            .emit(Event::participant_left(emptied.clone(), info));
    }
}

fn depart(
    room: &Room,
    departed: &Participant,
    reason: mediacore::DisconnectReason,
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

    let mut info = departed.to_proto(mediacore::participant_info::State::Disconnected);
    info.disconnect_reason = reason as i32;
    room.events
        .emit(Event::participant_left(room.to_proto(), info.clone()));

    // Hidden from the room, not from whoever is listening outside it: the
    // people in the room were never told a recorder was there, and a service
    // that has to know a recording stopped is not one of the people in the
    // room.
    if departed.is_hidden() {
        return;
    }

    room.broadcast(participant_update(vec![info]), except);
}

pub fn participant_update(
    participants: Vec<mediacore::ParticipantInfo>,
) -> mediacore::signal_response::Message {
    mediacore::signal_response::Message::Update(mediacore::ParticipantUpdate { participants })
}

fn subscription_permission_update(
    publisher_sid: &str,
    track_sid: &str,
    allowed: bool,
) -> mediacore::signal_response::Message {
    mediacore::signal_response::Message::SubscriptionPermissionUpdate(
        mediacore::SubscriptionPermissionUpdate {
            participant_sid: publisher_sid.to_owned(),
            track_sid: track_sid.to_owned(),
            allowed,
        },
    )
}

fn is_droppable(message: &mediacore::signal_response::Message) -> bool {
    use mediacore::signal_response::Message;

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

fn message_name(message: &mediacore::signal_response::Message) -> &'static str {
    use mediacore::signal_response::Message;

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
        Message::SubscriptionPermissionUpdate(_) => "subscription_permission_update",
        _ => "other",
    }
}

/// Whether somebody told to go should come back.
///
/// The client reads this and acts on it: `Reconnect` sends it round again,
/// `Disconnect` is final and it stops. Answering `Disconnect` to everything --
/// which this did until a node draining on a rolling deploy told every meeting
/// in the cluster never to return -- makes every transient fault permanent.
///
/// The split is whether the reason is about this connection or about the
/// person. A node stopping, a socket that went quiet, media that failed: the
/// meeting is still there and whoever it was should come back to it. Being
/// removed, the room being deleted or closed, a second connection under one
/// identity, a token that would not do: coming back would produce the same
/// answer, and a client retrying against it is a client in a loop.
///
/// Matched without a wildcard on purpose. A reason added to the protocol later
/// stops this compiling, which is the moment to decide what it means rather
/// than inheriting whichever default happened to be written here.
fn comes_back(reason: mediacore::DisconnectReason) -> bool {
    use mediacore::DisconnectReason as Why;

    match reason {
        Why::ServerShutdown
        | Why::ConnectionTimeout
        | Why::MediaFailure
        | Why::SignalClose
        | Why::UnknownReason => true,

        Why::ClientInitiated
        | Why::DuplicateIdentity
        | Why::ParticipantRemoved
        | Why::RoomDeleted
        | Why::RoomClosed
        | Why::StateMismatch
        | Why::JoinFailure
        | Why::Migration
        | Why::UserUnavailable
        | Why::UserRejected
        | Why::SipTrunkFailure
        | Why::AgentError => false,
    }
}

pub fn leave(reason: mediacore::DisconnectReason) -> mediacore::signal_response::Message {
    let again = comes_back(reason);

    mediacore::signal_response::Message::Leave(mediacore::LeaveRequest {
        can_reconnect: again,
        reason: reason as i32,
        action: match again {
            true => mediacore::leave_request::Action::Reconnect as i32,
            false => mediacore::leave_request::Action::Disconnect as i32,
        },
        regions: None,
    })
}

pub fn reconnect(reason: mediacore::DisconnectReason) -> mediacore::signal_response::Message {
    mediacore::signal_response::Message::Leave(mediacore::LeaveRequest {
        can_reconnect: true,
        reason: reason as i32,
        action: mediacore::leave_request::Action::Reconnect as i32,
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
#[path = "lib_tests.rs"]
mod tests;
