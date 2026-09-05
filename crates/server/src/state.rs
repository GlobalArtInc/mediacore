use std::sync::Arc;

use mediacore_config::Config;
use mediacore_proto::livekit;
use mediacore_room::{Registry, RoomDefaults};

use mediacore_sfu::SelectorConfig;

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
}

impl AppState {
    pub async fn start(config: Config) -> anyhow::Result<Self> {
        let media = MediaEngine::start(&config.rtc).await?;

        Ok(Self::with_media(config, media))
    }

    pub fn with_media(config: Config, media: MediaEngine) -> Self {
        let registry = Registry::new(RoomDefaults {
            auto_create: config.room.auto_create,
            empty_timeout: config.room.empty_timeout,
            departure_timeout: config.room.departure_timeout,
            max_participants: config.room.max_participants,
        });

        Self {
            config: Arc::new(config),
            registry: Arc::new(registry),
            media: Arc::new(media),
            publishers: Arc::new(Publishers::default()),
            router: Arc::new(MediaRouter::default()),
        }
    }

    pub fn source_config(&self, source: livekit::TrackSource) -> SourceConfig {
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
