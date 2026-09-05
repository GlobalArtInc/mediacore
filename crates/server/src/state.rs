use std::sync::Arc;

use mediacore_config::Config;
use mediacore_room::{Registry, RoomDefaults};

use crate::media::MediaEngine;
use crate::publisher::Publishers;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<Registry>,
    pub media: Arc<MediaEngine>,
    pub publishers: Arc<Publishers>,
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
        }
    }
}
