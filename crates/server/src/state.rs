use std::sync::Arc;

use mediacore_config::{BweConfig, Config, DuplicateIdentity};
use mediacore_proto::mediacore;
use mediacore_room::{Registry, RoomDefaults};

use mediacore_sfu::SelectorConfig;
use mediacore_transport::{Bitrate, BweSettings};

use crate::bandwidth::Subscribers;
use crate::caretaker::Caretaker;
use crate::forward::{MediaRouter, SourceConfig, tracker_config};
use crate::media::MediaEngine;
use crate::publisher::Publishers;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<Registry>,
    pub media: Arc<MediaEngine>,
    pub publishers: Arc<Publishers>,
    pub router: Arc<MediaRouter>,
    pub bandwidth: Arc<Subscribers>,
    /// The registry's clock, held here so that it lives exactly as long as the
    /// server does. Nothing reads it; dropping the last `AppState` stops it.
    _caretaker: Arc<Caretaker>,
}

impl AppState {
    pub async fn start(config: Config) -> anyhow::Result<Self> {
        let media = MediaEngine::start(&config.rtc, subscriber_bwe(&config.video.bwe)).await?;

        Ok(Self::with_media(config, media))
    }

    /// Assembles the server around a media engine. Must be called from inside
    /// a Tokio runtime: the registry's sweep is a task, and it starts here.
    pub fn with_media(config: Config, media: MediaEngine) -> Self {
        let registry = Registry::new(RoomDefaults {
            auto_create: config.room.auto_create,
            empty_timeout: config.room.empty_timeout,
            departure_timeout: config.room.departure_timeout,
            max_participants: config.room.max_participants,
            min_humans: config.room.min_humans,
            end_grace: config.room.end_grace,
            duplicate_identity: duplicate_identity(config.room.duplicate_identity),
        });

        let registry = Arc::new(registry);
        let caretaker = Caretaker::start(Arc::clone(&registry), config.room.sweep_interval());

        Self {
            config: Arc::new(config),
            registry,
            media: Arc::new(media),
            publishers: Arc::new(Publishers::default()),
            router: Arc::new(MediaRouter::default()),
            bandwidth: Arc::new(Subscribers::default()),
            _caretaker: Arc::new(caretaker),
        }
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
