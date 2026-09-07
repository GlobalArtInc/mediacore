use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use mediacore_config::{BweConfig, Config, DuplicateIdentity};
use mediacore_events::{Bus, Delivery};
use mediacore_proto::mediacore;
use mediacore_room::{Registry, RoomDefaults};
use mediacore_session::{PostgresSettings, SessionStore};

use mediacore_sfu::SelectorConfig;
use mediacore_transport::{Bitrate, BweSettings};

use crate::bandwidth::Subscribers;
use crate::caretaker::Caretaker;
use crate::events;
use crate::forward::{MediaRouter, SourceConfig, tracker_config};
use crate::media::MediaEngine;
use crate::publisher::Publishers;
use crate::recording::Recording;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<Registry>,
    pub media: Arc<MediaEngine>,
    pub publishers: Arc<Publishers>,
    pub router: Arc<MediaRouter>,
    pub bandwidth: Arc<Subscribers>,
    /// Where the rooms announce what happens in them. Held here as well as in
    /// the registry so that `/healthz` can say how many events this server has
    /// emitted without going through a room.
    pub events: Arc<Bus>,
    /// The outbox and the tasks that empty it, or `None` when nothing outside
    /// this process has asked to be told anything. Owned here for the same
    /// reason the caretaker is: the last clone going takes the tasks with it.
    pub delivery: Option<Arc<Delivery>>,
    /// The recorder, the writer thread behind it and the task that uploads what
    /// it produces, or `None` when this deployment records nothing. Held here
    /// so that the media path can find a track's tap without a lookup through
    /// the registry, and so that the last clone going takes the tasks with it.
    pub recording: Option<Arc<Recording>>,
    /// The registry's clock, held here so that it lives exactly as long as the
    /// server does. Nothing reads it; dropping the last `AppState` stops it.
    _caretaker: Arc<Caretaker>,
}

impl AppState {
    pub async fn start(config: Config) -> anyhow::Result<Self> {
        let media = MediaEngine::start(&config.rtc, subscriber_bwe(&config.video.bwe)).await?;

        Self::with_media(config, media)
    }

    /// Assembles the server around a media engine. Must be called from inside
    /// a Tokio runtime: the registry's sweep is a task, and it starts here.
    ///
    /// Opening the session records and the event outbox are the two things
    /// here that can fail on a server whose media engine is already up, and
    /// both fail the start rather than being logged and carried on from. A
    /// server that answers `/api/rooms/<room>/sessions` with an empty list
    /// because it never managed to open its own directory is a server nobody
    /// finds out about until they are asked what happened at a meeting last
    /// week; a server that emits into an outbox it could not open is the same
    /// failure again, discovered during the outage the outbox was for.
    ///
    /// A database that is *down* is the one thing here that does not fail the
    /// start. It is not a mistake anybody made, it is an outage -- and nothing
    /// on the media path goes anywhere near the session store, so a server that
    /// refused to carry calls over it would be turning an outage that costs
    /// records into one that costs meetings. See
    /// [`mediacore_session::SessionStore::postgres`].
    ///
    /// The order matters in one place: the delivery machinery registers as a
    /// listener before the registry exists, so there is no window in which a
    /// room announces something nobody is queued to hear.
    pub fn with_media(config: Config, media: MediaEngine) -> anyhow::Result<Self> {
        let sessions = Arc::new(open_sessions(&config)?);
        tracing::info!(store = %sessions.describe(), "meeting records are kept here");

        let events = Arc::new(Bus::new());
        let delivery = events::start(&config, &events)?;
        // Before the registry, for the same reason the delivery machinery is:
        // it listens for meetings starting, and a listener registered after the
        // registry exists could miss the first one.
        let pending = crate::recording::prepare(&config, &events)?;

        let registry = Registry::new(
            RoomDefaults {
                auto_create: config.room.auto_create,
                empty_timeout: config.room.empty_timeout,
                departure_timeout: config.room.departure_timeout,
                max_participants: config.room.max_participants,
                min_humans: config.room.min_humans,
                end_grace: config.room.end_grace,
                record: config.recording.enabled,
                duplicate_identity: duplicate_identity(config.room.duplicate_identity),
            },
            Arc::clone(&sessions),
            Arc::clone(&events),
        );

        let registry = Arc::new(registry);
        let recording = pending.map(|pending| {
            Arc::new(crate::recording::start(
                pending, &registry, &sessions, &events,
            ))
        });
        let caretaker = Caretaker::start(Arc::clone(&registry), config.room.sweep_interval());

        Ok(Self {
            config: Arc::new(config),
            registry,
            media: Arc::new(media),
            publishers: Arc::new(Publishers::default()),
            router: Arc::new(MediaRouter::default()),
            bandwidth: Arc::new(Subscribers::default()),
            events,
            delivery,
            recording,
            _caretaker: Arc::new(caretaker),
        })
    }

    pub fn bwe_config(&self) -> &BweConfig {
        &self.config.video.bwe
    }

    pub fn source_config(&self, source: mediacore::TrackSource) -> SourceConfig {
        SourceConfig {
            pause_delay: self.config.video.dynacast_pause_delay(),
            tracker: tracker_config(source),
        }
    }

    pub fn selector_config(&self) -> SelectorConfig {
        SelectorConfig {
            upgrade_hold: self.config.video.layer_upgrade_hold(),
        }
    }
}

/// Builds whichever store the config named.
///
/// The url decides: with one, the records go to a database every process
/// shares; without one, to a directory this process owns. There is no third
/// choice and no switch to disagree with -- see
/// [`mediacore_config::SessionConfig`].
fn open_sessions(config: &Config) -> anyhow::Result<SessionStore> {
    let settings = &config.sessions;

    if settings.in_postgres() {
        return SessionStore::postgres(PostgresSettings {
            url: settings.postgres.url.clone(),
            // The node id is on every meeting this server holds and every piece
            // of work it claims, which is what makes "who was holding this when
            // it stopped" a question with an answer.
            node: config.node_id.clone(),
            connections: settings.postgres.connections as usize,
            timeout: Duration::from_millis(settings.postgres.timeout_ms.into()),
            lease: Duration::from_millis(settings.postgres.lease_ms.into()),
            attempts: settings.postgres.attempts,
        })
        .map_err(anyhow::Error::msg)
        .context("the session database");
    }

    SessionStore::files(&settings.directory).with_context(|| {
        format!(
            "opening the session records in {}",
            settings.directory.display()
        )
    })
}

fn duplicate_identity(setting: DuplicateIdentity) -> mediacore_room::DuplicateIdentity {
    match setting {
        DuplicateIdentity::Suffix => mediacore_room::DuplicateIdentity::Suffix,
        DuplicateIdentity::Displace => mediacore_room::DuplicateIdentity::Displace,
    }
}

pub fn subscriber_bwe(config: &BweConfig) -> Option<BweSettings> {
    config
        .enabled
        .then(|| BweSettings::new(Bitrate::kbps(config.initial_kbps.into())))
}
