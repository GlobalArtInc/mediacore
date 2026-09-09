use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::bitrate::Bitrate;
use crate::congestion::CongestionState;
use crate::layer::{LAYER_COUNT, LayerSet, SpatialLayer};
use crate::selector::Allowance;

pub const SCREENSHARE_PRIORITY: u8 = 255;
pub const CAMERA_PRIORITY: u8 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorConfig {
    pub allow_pause: bool,
    pub audio_floor: Bitrate,
    pub recovery_hold: Duration,
    pub recovery_step: Duration,
    pub upgrade_headroom_percent: u64,
    pub max_overrun_percent: u64,
    pub default_prices: [Bitrate; LAYER_COUNT],
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self {
            allow_pause: false,
            audio_floor: Bitrate::kbps(48),
            recovery_hold: Duration::from_secs(2),
            recovery_step: Duration::from_secs(1),
            upgrade_headroom_percent: 115,
            max_overrun_percent: 150,
            default_prices: [Bitrate::kbps(150), Bitrate::kbps(500), Bitrate::kbps(1_500)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioTrack {
    pub rate: Option<Bitrate>,
    pub carrying: bool,
}

impl AudioTrack {
    pub fn carrying(rate: Option<Bitrate>) -> Self {
        Self {
            rate,
            carrying: true,
        }
    }

    pub fn silent() -> Self {
        Self {
            rate: None,
            carrying: false,
        }
    }

    fn reserve(&self, floor: Bitrate) -> Bitrate {
        let measured = self.rate.unwrap_or(Bitrate::ZERO);

        if self.carrying {
            measured.max(floor)
        } else {
            measured
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTrack {
    pub priority: u8,
    pub exempt: bool,
    pub available: LayerSet,
    pub ceiling: Option<SpatialLayer>,
    pub measured: [Option<Bitrate>; LAYER_COUNT],
    pub declared: [Option<Bitrate>; LAYER_COUNT],
}

impl VideoTrack {
    pub fn new(available: LayerSet, ceiling: Option<SpatialLayer>) -> Self {
        Self {
            priority: CAMERA_PRIORITY,
            exempt: false,
            available,
            ceiling,
            measured: [None; LAYER_COUNT],
            declared: [None; LAYER_COUNT],
        }
    }

    pub fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    pub fn exempt(mut self) -> Self {
        self.exempt = true;
        self
    }

    pub fn with_measured(mut self, measured: [Option<Bitrate>; LAYER_COUNT]) -> Self {
        self.measured = measured;
        self
    }

    pub fn with_declared(mut self, declared: [Option<Bitrate>; LAYER_COUNT]) -> Self {
        self.declared = declared;
        self
    }
}

impl Default for VideoTrack {
    fn default() -> Self {
        Self::new(LayerSet::all(), Some(SpatialLayer::MAX))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conditions {
    pub now: Instant,
    pub budget: Bitrate,
    pub state: CongestionState,
    pub state_since: Option<Instant>,
}

impl Conditions {
    pub fn clear(now: Instant) -> Self {
        Self {
            now,
            budget: Bitrate::MAX,
            state: CongestionState::Clear,
            state_since: None,
        }
    }

    pub fn congested(now: Instant, budget: Bitrate) -> Self {
        Self {
            now,
            budget,
            state: CongestionState::Congested,
            state_since: Some(now),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant<K> {
    pub key: K,
    pub allowance: Allowance,
}

#[derive(Debug, Clone)]
struct Track {
    input: VideoTrack,
    granted: Allowance,
    held: Option<SpatialLayer>,
    provisional: Option<SpatialLayer>,
    last_raise: Option<Instant>,
}

impl Track {
    fn new(input: VideoTrack) -> Self {
        let mut track = Self {
            input,
            granted: Allowance::Unlimited,
            held: None,
            provisional: None,
            last_raise: None,
        };
        track.held = track.selected(Allowance::Unlimited);

        track
    }

    fn hand(&mut self, allowance: Allowance) {
        self.granted = allowance;
        self.held = self.held.max(self.selected(allowance));
    }

    fn restores(&self, allowance: Allowance) -> bool {
        match (self.selected(allowance), self.held) {
            (Some(rung), Some(held)) => rung <= held,
            _ => false,
        }
    }

    fn is_idle(&self) -> bool {
        self.input.ceiling.is_none() || self.input.available.is_empty()
    }

    fn is_managed(&self) -> bool {
        !self.is_idle() && !self.input.exempt && self.fitting().is_some()
    }

    fn price(&self, layer: SpatialLayer, config: &AllocatorConfig) -> Bitrate {
        let slot = layer.slot();

        self.input.measured[slot]
            .or(self.input.declared[slot])
            .unwrap_or(config.default_prices[slot])
    }

    fn fitting(&self) -> Option<SpatialLayer> {
        let ceiling = self.input.ceiling?;
        self.input.available.highest_at_most(ceiling)
    }

    fn selected(&self, allowance: Allowance) -> Option<SpatialLayer> {
        let ceiling = self.input.ceiling?;
        let cap = allowance.cap()?;
        let fitting = self.input.available.highest_at_most(ceiling.min(cap));

        match allowance {
            Allowance::Unlimited => fitting.or_else(|| self.input.available.lowest()),
            _ => fitting,
        }
    }

    fn spend(&self, allowance: Allowance, config: &AllocatorConfig) -> Bitrate {
        self.selected(allowance)
            .map(|layer| self.price(layer, config))
            .unwrap_or(Bitrate::ZERO)
    }

    fn provisional_price(&self, config: &AllocatorConfig) -> Bitrate {
        self.provisional
            .map(|layer| self.price(layer, config))
            .unwrap_or(Bitrate::ZERO)
    }
}

#[derive(Debug, Clone)]
pub struct BandwidthAllocator<K: Ord + Clone> {
    config: AllocatorConfig,
    video: BTreeMap<K, Track>,
    audio: BTreeMap<K, AudioTrack>,
}

impl<K: Ord + Clone> BandwidthAllocator<K> {
    pub fn new(config: AllocatorConfig) -> Self {
        Self {
            config,
            video: BTreeMap::new(),
            audio: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> &AllocatorConfig {
        &self.config
    }

    pub fn upsert_video(&mut self, key: K, track: VideoTrack) {
        match self.video.get_mut(&key) {
            Some(existing) => existing.input = track,
            None => {
                self.video.insert(key, Track::new(track));
            }
        }
    }

    pub fn upsert_audio(&mut self, key: K, audio: AudioTrack) {
        self.audio.insert(key, audio);
    }

    pub fn remove(&mut self, key: &K) -> bool {
        let video = self.video.remove(key).is_some();
        let audio = self.audio.remove(key).is_some();

        video || audio
    }

    pub fn video_tracks(&self) -> usize {
        self.video.len()
    }

    pub fn audio_tracks(&self) -> usize {
        self.audio.len()
    }

    pub fn grant(&self, key: &K) -> Option<Allowance> {
        self.video.get(key).map(|track| track.granted)
    }

    pub fn is_constrained(&self) -> bool {
        self.video
            .values()
            .any(|track| track.granted != Allowance::Unlimited)
    }

    pub fn is_held_back(&self) -> bool {
        self.video
            .values()
            .any(|track| track.selected(track.granted) != track.selected(Allowance::Unlimited))
    }

    pub fn audio_reserve(&self) -> Bitrate {
        self.audio
            .values()
            .map(|audio| audio.reserve(self.config.audio_floor))
            .sum()
    }

    fn unmanaged_spend(&self) -> Bitrate {
        self.video
            .values()
            .filter(|track| !track.is_idle() && !track.is_managed())
            .map(|track| track.spend(Allowance::Unlimited, &self.config))
            .sum()
    }

    pub fn spend(&self) -> Bitrate {
        self.video
            .values()
            .map(|track| track.spend(track.granted, &self.config))
            .sum::<Bitrate>()
            .saturating_add(self.audio_reserve())
    }

    pub fn desired(&self) -> Bitrate {
        self.video
            .values()
            .map(|track| track.spend(Allowance::Unlimited, &self.config))
            .sum::<Bitrate>()
            .saturating_add(self.audio_reserve())
    }

    pub fn allocate(&mut self, conditions: Conditions) -> Vec<Grant<K>> {
        if conditions.state == CongestionState::Hold {
            return Vec::new();
        }

        self.provision(conditions.budget);

        self.commit(conditions)
    }

    fn provision(&mut self, budget: Bitrate) {
        let config = self.config;
        let pool = budget
            .saturating_sub(self.audio_reserve())
            .saturating_sub(self.unmanaged_spend());
        let overrun = pool.scaled(config.max_overrun_percent, 100);

        for track in self.video.values_mut() {
            track.provisional = None;
        }

        let order = self.order();
        let mut spent = Bitrate::ZERO;
        let mut served = 0usize;

        for slot in 0..LAYER_COUNT {
            let rung = SpatialLayer::new(slot as u8);

            for key in &order {
                let Some(track) = self.video.get_mut(key) else {
                    continue;
                };

                if !track.is_managed() {
                    continue;
                }

                let Some(ceiling) = track.input.ceiling else {
                    continue;
                };

                if rung > ceiling || !track.input.available.contains(rung) {
                    continue;
                }

                let cost = track
                    .price(rung, &config)
                    .saturating_sub(track.provisional_price(&config));
                let after = spent.saturating_add(cost);

                if after <= pool {
                    if track.provisional.is_none() {
                        served += 1;
                    }

                    track.provisional = Some(rung);
                    spent = after;
                    continue;
                }

                if config.allow_pause || track.provisional.is_some() {
                    continue;
                }

                if served > 0 && after > overrun {
                    continue;
                }

                track.provisional = Some(rung);
                spent = after;
                served += 1;
            }
        }
    }

    fn commit(&mut self, conditions: Conditions) -> Vec<Grant<K>> {
        let config = self.config;
        let order = self.order();
        let mut changes = Vec::new();
        let mut candidates: Vec<K> = Vec::new();

        for key in &order {
            let Some(track) = self.video.get_mut(key) else {
                continue;
            };

            let target = target_allowance(track, conditions.state);

            if target == track.granted {
                continue;
            }

            if target.rank() > track.granted.rank() && track.is_managed() {
                candidates.push(key.clone());
                continue;
            }

            track.hand(target);
            changes.push(Grant {
                key: key.clone(),
                allowance: target,
            });
        }

        candidates.sort_by_key(|key| !self.video[key].granted.is_paused());

        let committed = self.spend();

        for key in candidates {
            let Some(track) = self.video.get_mut(&key) else {
                continue;
            };

            if !may_raise(track, &conditions, &config) {
                continue;
            }

            if let Some(last) = track.last_raise
                && conditions.now.saturating_duration_since(last) < config.recovery_step
            {
                continue;
            }

            let target = target_allowance(track, conditions.state);
            let price = track.spend(target, &config);
            let others = committed.saturating_sub(track.spend(track.granted, &config));
            let room = conditions.budget.saturating_sub(others);

            let asking = if track.restores(target) {
                price
            } else {
                price.scaled(config.upgrade_headroom_percent, 100)
            };

            if asking > room {
                continue;
            }

            track.hand(target);
            track.last_raise = Some(conditions.now);
            changes.push(Grant {
                key,
                allowance: target,
            });

            break;
        }

        changes
    }

    fn order(&self) -> Vec<K> {
        let mut keys: Vec<K> = self.video.keys().cloned().collect();

        keys.sort_by(|left, right| {
            let a = &self.video[left];
            let b = &self.video[right];

            b.input
                .priority
                .cmp(&a.input.priority)
                .then(b.input.ceiling.cmp(&a.input.ceiling))
                .then(left.cmp(right))
        });

        keys
    }
}

fn may_raise(track: &Track, conditions: &Conditions, config: &AllocatorConfig) -> bool {
    if track.granted.is_paused() {
        return true;
    }

    if conditions.state != CongestionState::Clear {
        return false;
    }

    match conditions.state_since {
        Some(since) => conditions.now.saturating_duration_since(since) >= config.recovery_hold,
        None => true,
    }
}

fn target_allowance(track: &Track, state: CongestionState) -> Allowance {
    if !track.is_managed() {
        return Allowance::Unlimited;
    }

    match (track.provisional, track.fitting()) {
        (Some(rung), best) if state.is_clear() && Some(rung) == best => Allowance::Unlimited,
        (Some(rung), _) => Allowance::Capped(rung),
        (None, _) => Allowance::Paused,
    }
}

impl<K: Ord + Clone> Default for BandwidthAllocator<K> {
    fn default() -> Self {
        Self::new(AllocatorConfig::default())
    }
}

#[cfg(test)]
#[path = "alloc_tests.rs"]
mod tests;
