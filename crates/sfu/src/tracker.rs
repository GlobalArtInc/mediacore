use std::time::{Duration, Instant};

use crate::layer::{LAYER_COUNT, LayerSet, SpatialLayer};

const MAX_CATCH_UP_CYCLES: u128 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerTrackerConfig {
    pub samples_required: u32,
    pub cycles_required: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackerConfig {
    pub cycle: Duration,
    pub layers: [LayerTrackerConfig; LAYER_COUNT],
}

impl TrackerConfig {
    pub fn video() -> Self {
        Self {
            cycle: Duration::from_millis(500),
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
            config,
            layers: [LayerState::default(); LAYER_COUNT],
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
        let state = &mut self.layers[layer.slot()];
        let was = state.status;

        *state = LayerState {
            packets: state.packets,
            bytes: state.bytes,
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
mod tests {
    use super::*;

    const BASE: SpatialLayer = SpatialLayer::BASE;

    fn high() -> SpatialLayer {
        SpatialLayer::new(2)
    }

    fn fast() -> TrackerConfig {
        let layer = LayerTrackerConfig {
            samples_required: 2,
            cycles_required: 3,
        };

        TrackerConfig {
            cycle: Duration::from_millis(100),
            layers: [layer; LAYER_COUNT],
        }
    }

    fn feed(
        tracker: &mut LayerTracker,
        layer: SpatialLayer,
        packets: u32,
        from: Instant,
        step: Duration,
    ) -> Instant {
        let mut at = from;
        for _ in 0..packets {
            tracker.observe(layer, 100, at);
            at += step;
        }
        at
    }

    #[test]
    fn the_first_packet_of_a_layer_declares_it_active() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        let changes = tracker.observe(BASE, 120, now);

        assert!(changes.activated.contains(BASE));
        assert!(tracker.is_active(BASE));
        assert_eq!(tracker.active(), LayerSet::of(BASE));
        assert_eq!(tracker.packets(BASE), 1);
        assert_eq!(tracker.bytes(BASE), 120);
    }

    #[test]
    fn padding_does_not_keep_a_layer_alive() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        assert!(tracker.observe(BASE, 0, now).is_empty());
        assert!(!tracker.is_active(BASE));
        assert_eq!(tracker.packets(BASE), 0);
    }

    #[test]
    fn a_layer_that_stops_arriving_is_declared_gone_after_one_cycle() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        let at = feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));
        assert!(tracker.is_active(BASE));

        let changes = tracker.tick(at + Duration::from_millis(200));

        assert!(changes.stopped.contains(BASE));
        assert!(!tracker.is_active(BASE));
        assert!(tracker.active().is_empty());
        assert!(tracker.seen().contains(BASE));
    }

    #[test]
    fn a_thin_layer_is_declared_gone_even_while_a_trickle_arrives() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        tracker.observe(high(), 100, now);
        assert!(tracker.is_active(high()));

        let changes = tracker.observe(high(), 100, now + Duration::from_millis(150));

        assert!(changes.stopped.contains(high()));
        assert!(!tracker.is_active(high()));
    }

    #[test]
    fn a_layer_that_resumes_needs_several_good_cycles_before_it_counts() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));
        tracker.tick(now + Duration::from_millis(300));
        assert!(!tracker.is_active(BASE));

        let mut at = now + Duration::from_millis(300);
        let mut activated = None;

        for cycle in 0..5 {
            for _ in 0..4 {
                let changes = tracker.observe(BASE, 100, at);
                if changes.activated.contains(BASE) && activated.is_none() {
                    activated = Some(cycle);
                }
                at += Duration::from_millis(25);
            }
        }

        assert_eq!(
            activated,
            Some(3),
            "the layer must survive three full cycles before it is trusted again"
        );
        assert!(tracker.is_active(BASE));
    }

    #[test]
    fn one_layer_stopping_leaves_the_others_alone() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        let mut at = now;
        for _ in 0..12 {
            tracker.observe(BASE, 100, at);
            tracker.observe(high(), 100, at);
            at += Duration::from_millis(20);
        }

        assert_eq!(tracker.active(), LayerSet::from_iter([BASE, high()]));

        for _ in 0..12 {
            tracker.observe(BASE, 100, at);
            at += Duration::from_millis(20);
        }

        assert_eq!(tracker.active(), LayerSet::of(BASE));
        assert!(!tracker.is_active(high()));
    }

    #[test]
    fn a_long_silence_reports_the_stop_once() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));

        let changes = tracker.tick(now + Duration::from_secs(30));
        assert!(changes.stopped.contains(BASE));

        assert!(tracker.tick(now + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn a_layer_that_starts_mid_cycle_is_judged_on_a_cycle_of_its_own() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        feed(&mut tracker, BASE, 5, now, Duration::from_millis(20));

        let changes = tracker.observe(high(), 100, now + Duration::from_millis(90));
        assert!(changes.activated.contains(high()));

        let changes = tracker.observe(BASE, 100, now + Duration::from_millis(100));
        assert!(
            !changes.stopped.contains(high()),
            "the layer was judged on a cycle that started before it existed"
        );
        assert!(tracker.is_active(high()));

        let mut at = now + Duration::from_millis(110);
        for _ in 0..4 {
            tracker.observe(BASE, 100, at);
            tracker.observe(high(), 100, at);
            at += Duration::from_millis(20);
        }

        assert!(tracker.tick(now + Duration::from_millis(200)).is_empty());
        assert!(tracker.is_active(high()));
        assert!(tracker.is_active(BASE));
    }

    #[test]
    fn a_layer_put_back_to_fresh_is_trusted_by_its_next_packet() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        feed(&mut tracker, high(), 4, now, Duration::from_millis(20));
        tracker.tick(now + Duration::from_millis(300));

        assert!(!tracker.is_active(high()));
        assert!(tracker.seen().contains(high()));

        assert!(tracker.reset_layer(high()).is_empty());
        assert!(!tracker.seen().contains(high()));

        let changes = tracker.observe(high(), 100, now + Duration::from_millis(320));

        assert!(changes.activated.contains(high()));
        assert!(tracker.is_active(high()));
        assert_eq!(
            tracker.packets(high()),
            5,
            "the layer counters belong to the track, not to one run of it"
        );
    }

    #[test]
    fn resetting_one_layer_leaves_the_others_where_they_are() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        tracker.observe(BASE, 100, now);
        tracker.observe(high(), 100, now);
        assert_eq!(tracker.active(), LayerSet::from_iter([BASE, high()]));

        let changes = tracker.reset_layer(high());

        assert!(changes.stopped.contains(high()));
        assert!(!changes.stopped.contains(BASE));
        assert_eq!(tracker.active(), LayerSet::of(BASE));
    }

    #[test]
    fn pausing_the_tracker_drops_every_layer_and_starts_over() {
        let now = Instant::now();
        let mut tracker = LayerTracker::new(fast());

        feed(&mut tracker, BASE, 4, now, Duration::from_millis(20));
        let changes = tracker.set_paused(true);

        assert!(changes.stopped.contains(BASE));
        assert!(tracker.active().is_empty());
        assert!(
            tracker
                .observe(BASE, 100, now + Duration::from_millis(100))
                .is_empty()
        );
        assert!(!tracker.is_active(BASE));

        tracker.set_paused(false);
        let changes = tracker.observe(BASE, 100, now + Duration::from_millis(200));
        assert!(changes.activated.contains(BASE));
    }
}
