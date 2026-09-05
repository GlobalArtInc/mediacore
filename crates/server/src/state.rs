use std::sync::Arc;

use mediacore_config::Config;
use mediacore_room::{Registry, RoomDefaults};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<Registry>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let registry = Registry::new(RoomDefaults {
            auto_create: config.room.auto_create,
            empty_timeout: config.room.empty_timeout,
            departure_timeout: config.room.departure_timeout,
            max_participants: config.room.max_participants,
        });

        Self {
            config: Arc::new(config),
            registry: Arc::new(registry),
        }
    }
}
