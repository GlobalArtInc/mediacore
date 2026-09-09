use std::time::{Duration, Instant};

use crate::bitrate::Bitrate;
use crate::layer::{LAYER_COUNT, LayerSet, SpatialLayer};
use crate::rate::RateWindow;

const MAX_CATCH_UP_CYCLES: u128 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerTrackerConfig {
    pub samples_required: u32,
    pub cycles_required: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackerConfig {
    pub cycle: Duration,
    pub rate_window: Duration,
    pub layers: [LayerTrackerConfig; LAYER_COUNT],
}

impl TrackerConfig {
    pub fn video() -> Self {
        Self {
            cycle: Duration::from_millis(500),
            rate_window: Duration::from_secs(1),
            layers: [
                LayerTrackerConfig {
                    samples_required: 1,
                    cycles_required: 4,
                },
                LayerTrackerConfig {
                    samples_required: 5,
                    cycles_required: 20,
                },
                LayerTrackerConfig {
                    samples_required: 5,
                    cycles_required: 20,
                },
            ],
        }
    }

    pub fn screenshare() -> Self {
        let layer = LayerTrackerConfig {
            samples_required: 1,
            cycles_required: 1,
        };

        Self {
            cycle: Duration::from_secs(2),
            rate_window: Duration::from_secs(4),
            layers: [layer; LAYER_COUNT],
        }
    }

    fn layer(&self, layer: SpatialLayer) -> LayerTrackerConfig {
        let config = self.layers[layer.slot()];

        LayerTrackerConfig {
            samples_required: config.samples_required.max(1),
            cycles_required: config.cycles_required.max(1),
        }
    }
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self::video()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerStatus {
    #[default]
    Stopped,
    Active,
}

impl std::fmt::Display for LayerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped"),
            Self::Active => write!(f, "active"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerChanges {
    pub activated: LayerSet,
    pub stopped: LayerSet,
}

impl LayerChanges {
    pub fn is_empty(&self) -> bool {
        self.activated.is_empty() && self.stopped.is_empty()
    }

    fn record(&mut self, layer: SpatialLayer, status: LayerStatus) {
        match status {
            LayerStatus::Active => {
                self.stopped.remove(layer);
                self.activated.insert(layer);
            }
            LayerStatus::Stopped => {
                self.activated.remove(layer);
                self.stopped.insert(layer);
            }
        }
    }

    fn merge(&mut self, other: Self) {
        for layer in other.activated.iter() {
            self.record(layer, LayerStatus::Active);
        }
        for layer in other.stopped.iter() {
            self.record(layer, LayerStatus::Stopped);
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct LayerState {
    seen: bool,
    status: LayerStatus,
    samples: u32,
    good_cycles: u32,
    packets: u64,
    bytes: u64,
    rate: RateWindow,
    last_packet: Option<Instant>,
    cycle_started: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct LayerTracker {
    config: TrackerConfig,
    layers: [LayerState; LAYER_COUNT],
    paused: bool,
}

impl LayerTracker {
    pub fn new(config: TrackerConfig) -> Self {
        Self {
            layers: [LayerState {
                rate: RateWindow::new(config.rate_window),
                ..LayerState::default()
            }; LAYER_COUNT],
            config,
            paused: false,
        }
    }

    pub fn config(&self) -> &TrackerConfig {
        &self.config
    }

    pub fn observe(
        &mut self,
        layer: SpatialLayer,
        payload_len: usize,
        now: Instant,
    ) -> LayerChanges {
        let mut changes = self.tick(now);

        if self.paused || payload_len == 0 {
            return changes;
        }

        let state = &mut self.layers[layer.slot()];
        state.packets += 1;
        state.bytes += payload_len as u64;
        state.rate.observe(payload_len, now);
        state.last_packet = Some(now);
        state.samples += 1;

        if !state.seen {
            state.seen = true;
            state.samples = 1;
            state.good_cycles = 0;
            state.cycle_started = Some(now);

            if state.status != LayerStatus::Active {
                state.status = LayerStatus::Active;
                changes.record(layer, LayerStatus::Active);
            }
        }

        changes
    }

    pub fn tick(&mut self, now: Instant) -> LayerChanges {
        let mut changes = LayerChanges::default();

        if self.paused {
            return changes;
        }

        let cycle = self.config.cycle.as_nanos().max(1);

        for slot in 0..LAYER_COUNT {
            let Some(started) = self.layers[slot].cycle_started else {
                continue;
            };

            let elapsed = now.saturating_duration_since(started).as_nanos();
            let full = elapsed / cycle;

            if full == 0 {
                continue;
            }

            let remainder = u64::try_from(elapsed - full * cycle).unwrap_or(0);
            self.layers[slot].cycle_started = now
                .checked_sub(Duration::from_nanos(remainder))
                .or(Some(now));

            let layer = SpatialLayer::new(slot as u8);
            for _ in 0..full.min(MAX_CATCH_UP_CYCLES) {
                self.check(layer, &mut changes);
            }
        }

        changes
    }

    fn check(&mut self, layer: SpatialLayer, changes: &mut LayerChanges) {
        let config = self.config.layer(layer);
        let state = &mut self.layers[layer.slot()];

        if !state.seen {
            return;
        }

        if state.samples >= config.samples_required {
            state.good_cycles = state.good_cycles.saturating_add(1);
        } else {
            state.good_cycles = 0;
        }
        state.samples = 0;

        let next = if state.good_cycles == 0 {
            LayerStatus::Stopped
        } else if state.good_cycles >= config.cycles_required {
            LayerStatus::Active
        } else {
            return;
        };

        if state.status != next {
            state.status = next;
            changes.record(layer, next);
            tracing::debug!(%layer, status = %next, "published layer changed state");
        }
    }

    pub fn status(&self, layer: SpatialLayer) -> LayerStatus {
        self.layers[layer.slot()].status
    }

    pub fn is_active(&self, layer: SpatialLayer) -> bool {
        self.status(layer) == LayerStatus::Active
    }

    pub fn active(&self) -> LayerSet {
        (0..LAYER_COUNT as u8)
            .map(SpatialLayer::new)
            .filter(|layer| self.is_active(*layer))
            .collect()
    }

    pub fn seen(&self) -> LayerSet {
        (0..LAYER_COUNT as u8)
            .map(SpatialLayer::new)
            .filter(|layer| self.layers[layer.slot()].seen)
            .collect()
    }

    pub fn packets(&self, layer: SpatialLayer) -> u64 {
        self.layers[layer.slot()].packets
    }

    pub fn bytes(&self, layer: SpatialLayer) -> u64 {
        self.layers[layer.slot()].bytes
    }

    pub fn rate(&self, layer: SpatialLayer, overhead_bytes: u32, now: Instant) -> Option<Bitrate> {
        self.layers[layer.slot()]
            .rate
            .bits_per_second(overhead_bytes, now)
    }

    pub fn rates(&self, overhead_bytes: u32, now: Instant) -> [Option<Bitrate>; LAYER_COUNT] {
        std::array::from_fn(|slot| self.rate(SpatialLayer::new(slot as u8), overhead_bytes, now))
    }

    pub fn last_packet(&self, layer: SpatialLayer) -> Option<Instant> {
        self.layers[layer.slot()].last_packet
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn set_paused(&mut self, paused: bool) -> LayerChanges {
        if self.paused == paused {
            return LayerChanges::default();
        }

        self.paused = paused;
        self.reset()
    }

    pub fn reset_layer(&mut self, layer: SpatialLayer) -> LayerChanges {
        let mut changes = LayerChanges::default();
        let window = self.config.rate_window;
        let state = &mut self.layers[layer.slot()];
        let was = state.status;

        *state = LayerState {
            packets: state.packets,
            bytes: state.bytes,
            rate: RateWindow::new(window),
            ..LayerState::default()
        };

        if was != LayerStatus::Stopped {
            changes.record(layer, LayerStatus::Stopped);
        }

        changes
    }

    pub fn reset(&mut self) -> LayerChanges {
        let mut changes = LayerChanges::default();

        for slot in 0..LAYER_COUNT {
            changes.merge(self.reset_layer(SpatialLayer::new(slot as u8)));
        }

        changes
    }
}

impl Default for LayerTracker {
    fn default() -> Self {
        Self::new(TrackerConfig::default())
    }
}

#[cfg(test)]
#[path = "tracker_tests.rs"]
mod tests;
