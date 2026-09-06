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
mod tests {
    use super::*;
    use crate::ladder::{LayerLadder, VideoQuality};

    const BASE: SpatialLayer = SpatialLayer::BASE;
    const TICK: Duration = Duration::from_millis(250);

    fn mid() -> SpatialLayer {
        SpatialLayer::new(1)
    }

    fn high() -> SpatialLayer {
        SpatialLayer::new(2)
    }

    fn fresh() -> BandwidthAllocator<&'static str> {
        BandwidthAllocator::new(AllocatorConfig::default())
    }

    fn camera() -> VideoTrack {
        VideoTrack::default()
    }

    fn priced(prices: [u64; LAYER_COUNT]) -> [Option<Bitrate>; LAYER_COUNT] {
        prices.map(|kbps| Some(Bitrate::kbps(kbps)))
    }

    fn congested(now: Instant, budget: Bitrate) -> Conditions {
        Conditions::congested(now, budget)
    }

    fn clear_for(now: Instant, hold: Duration, budget: Bitrate) -> Conditions {
        Conditions {
            now,
            budget,
            state: CongestionState::Clear,
            state_since: Some(now - hold),
        }
    }

    fn layer_of(allocator: &BandwidthAllocator<&'static str>, key: &'static str) -> Allowance {
        allocator.grant(&key).expect("the track is enrolled")
    }

    #[test]
    fn every_track_gets_the_base_rung_before_any_track_gets_the_second() {
        let now = Instant::now();
        let mut allocator = fresh();

        for key in ["a", "b", "c"] {
            allocator.upsert_video(key, camera());
        }

        allocator.allocate(congested(now, Bitrate::kbps(950)));

        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(mid()));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(BASE));
        assert_eq!(layer_of(&allocator, "c"), Allowance::Capped(BASE));
    }

    #[test]
    fn a_screenshare_outranks_a_camera_at_every_rung() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video("zzz-screen", camera().with_priority(SCREENSHARE_PRIORITY));
        allocator.upsert_video("aaa-camera", camera());

        allocator.allocate(congested(now, Bitrate::kbps(800)));

        assert_eq!(layer_of(&allocator, "zzz-screen"), Allowance::Capped(mid()));
        assert_eq!(layer_of(&allocator, "aaa-camera"), Allowance::Capped(BASE));
    }

    #[test]
    fn a_single_layer_screenshare_is_paid_before_the_budget_is_split() {
        let now = Instant::now();
        let mut allocator = fresh();

        let screen = VideoTrack::new(LayerSet::of(BASE), Some(SpatialLayer::MAX))
            .with_priority(SCREENSHARE_PRIORITY)
            .exempt()
            .with_measured([Some(Bitrate::kbps(500)), None, None]);

        allocator.upsert_video("screen", screen);
        allocator.upsert_video("a", camera());
        allocator.upsert_video("b", camera());

        allocator.allocate(congested(now, Bitrate::kbps(800)));

        assert_eq!(
            layer_of(&allocator, "screen"),
            Allowance::Unlimited,
            "an exempt track is never capped"
        );
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(BASE));
        assert_eq!(allocator.spend(), Bitrate::kbps(800));
    }

    #[test]
    fn audio_is_reserved_before_video_is_allocated() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_audio("talker", AudioTrack::carrying(Some(Bitrate::kbps(60))));
        allocator.upsert_audio("silent", AudioTrack::carrying(None));

        assert_eq!(
            allocator.audio_reserve(),
            Bitrate::kbps(108),
            "a track that is silent right now still books the floor"
        );

        allocator.upsert_video("a", camera());
        allocator.allocate(congested(now, Bitrate::kbps(600)));

        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(BASE),
            "six hundred kilobits minus the audio reserve does not buy the mid rung"
        );

        let mut unreserved = fresh();
        unreserved.upsert_video("a", camera());
        unreserved.allocate(congested(now, Bitrate::kbps(600)));

        assert_eq!(
            layer_of(&unreserved, "a"),
            Allowance::Capped(mid()),
            "the same budget without audio subscriptions buys the mid rung"
        );
    }

    #[test]
    fn audio_survives_a_budget_that_cannot_cover_any_video() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_audio("talker", AudioTrack::carrying(Some(Bitrate::kbps(64))));
        allocator.upsert_video("a", camera());

        allocator.allocate(congested(now, Bitrate::kbps(30)));

        assert_eq!(allocator.audio_reserve(), Bitrate::kbps(64));
        assert_eq!(allocator.audio_tracks(), 1);
        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(BASE),
            "video overruns rather than blanking, and audio is never asked to give anything up"
        );
    }

    #[test]
    fn a_budget_that_fits_nothing_overruns_rather_than_blanking() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());

        allocator.allocate(congested(now, Bitrate::ZERO));

        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));
    }

    #[test]
    fn the_same_budget_pauses_the_lowest_priority_track_when_pausing_is_allowed() {
        let now = Instant::now();
        let mut allocator = BandwidthAllocator::new(AllocatorConfig {
            allow_pause: true,
            ..AllocatorConfig::default()
        });

        allocator.upsert_video("screen", camera().with_priority(SCREENSHARE_PRIORITY));
        allocator.upsert_video("camera", camera());

        allocator.allocate(congested(now, Bitrate::kbps(150)));

        assert_eq!(layer_of(&allocator, "screen"), Allowance::Capped(BASE));
        assert_eq!(layer_of(&allocator, "camera"), Allowance::Paused);
    }

    #[test]
    fn a_layer_with_no_measurement_is_priced_from_the_declared_bitrate() {
        let now = Instant::now();
        let mut allocator = fresh();
        let ladder = LayerLadder::new()
            .with_quality(BASE, VideoQuality::Low)
            .with_bitrate(BASE, 90_000)
            .with_bitrate(mid(), 260_000)
            .with_bitrate(high(), 900_000);

        allocator.upsert_video("a", camera().with_declared(ladder.declared_bitrates()));
        allocator.allocate(congested(now, Bitrate::kbps(300)));

        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(mid()),
            "the publisher declared a cheaper mid rung than the default ladder assumes"
        );
    }

    #[test]
    fn a_measurement_beats_what_the_publisher_declared() {
        let now = Instant::now();
        let mut allocator = fresh();

        let track = camera()
            .with_declared(priced([90, 260, 900]))
            .with_measured([None, Some(Bitrate::kbps(600)), None]);

        allocator.upsert_video("a", track);
        allocator.allocate(congested(now, Bitrate::kbps(300)));

        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(BASE),
            "the mid rung really costs six hundred, whatever the publisher declared"
        );
    }

    #[test]
    fn a_layer_with_neither_is_priced_from_the_default_ladder() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());

        allocator.allocate(congested(now, Bitrate::kbps(650)));

        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(mid()),
            "an unmeasured, undeclared layer must still be allocatable"
        );
    }

    #[test]
    fn a_downgrade_lands_on_the_tick_it_is_decided() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());

        allocator.allocate(clear_for(now, Duration::from_secs(10), Bitrate::MAX));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Unlimited);

        let changes = allocator.allocate(congested(now + TICK, Bitrate::kbps(200)));

        assert_eq!(
            changes,
            vec![Grant {
                key: "a",
                allowance: Allowance::Capped(BASE)
            }]
        );
    }

    #[test]
    fn a_budget_that_shrinks_steps_every_track_down_in_priority_order() {
        let mut now = Instant::now();
        let mut allocator = fresh();
        for key in ["a", "b"] {
            allocator.upsert_video(key, camera());
        }

        allocator.allocate(congested(now, Bitrate::kbps(3_000)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(high()));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(high()));

        now += TICK;
        allocator.allocate(congested(now, Bitrate::kbps(2_000)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(high()));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(mid()));

        now += TICK;
        allocator.allocate(congested(now, Bitrate::kbps(1_000)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(mid()));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(mid()));

        now += TICK;
        allocator.allocate(congested(now, Bitrate::kbps(300)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(BASE));
    }

    #[test]
    fn an_upgrade_to_a_rung_the_subscriber_never_held_needs_fifteen_percent_of_headroom() {
        let mut now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", VideoTrack::new(LayerSet::of(BASE), Some(high())));
        allocator.allocate(congested(now, Bitrate::kbps(150)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));

        allocator.upsert_video("a", camera());

        now += Duration::from_secs(5);
        allocator.allocate(clear_for(now, Duration::from_secs(4), Bitrate::kbps(500)));
        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(BASE),
            "a rung that was published after we started allocating has never been paid for here, and a budget that exactly covers its measured price leaves nothing for the switch"
        );

        now += TICK;
        allocator.allocate(clear_for(now, Duration::from_secs(4), Bitrate::kbps(600)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(mid()));

        now += TICK;
        allocator.allocate(congested(now, Bitrate::kbps(150)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));

        now += Duration::from_secs(3);
        allocator.allocate(clear_for(now, Duration::from_secs(3), Bitrate::kbps(500)));
        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(mid()),
            "the rung stopped being speculative the moment this subscriber was granted it and the link carried it"
        );
    }

    #[test]
    fn a_rung_the_subscriber_already_held_is_restored_as_soon_as_the_budget_covers_it() {
        let mut now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());
        allocator.allocate(congested(now, Bitrate::kbps(150)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));

        now += Duration::from_secs(5);
        allocator.allocate(clear_for(now, Duration::from_secs(4), Bitrate::kbps(500)));
        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(mid()),
            "the link carried this rung before the dip, so taking it back asks for room and not for room with a margin"
        );

        now += Duration::from_secs(2);
        allocator.allocate(clear_for(now, Duration::from_secs(6), Bitrate::kbps(1_500)));
        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Unlimited,
            "a link worth exactly the top rung must be able to reach the top rung"
        );
    }

    #[test]
    fn an_upgrade_waits_for_the_link_to_have_been_clear_for_a_while() {
        let mut now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());
        allocator.allocate(congested(now, Bitrate::kbps(150)));

        now += TICK;
        allocator.allocate(clear_for(now, Duration::from_millis(1_999), Bitrate::MAX));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));

        now += TICK;
        allocator.allocate(clear_for(now, Duration::from_secs(2), Bitrate::MAX));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Unlimited);
    }

    #[test]
    fn only_one_track_is_raised_per_tick() {
        let mut now = Instant::now();
        let mut allocator = fresh();
        for key in ["a", "b", "c"] {
            allocator.upsert_video(key, camera());
        }

        allocator.allocate(congested(now, Bitrate::kbps(450)));
        assert!(allocator.is_constrained());

        for expected in ["a", "b", "c"] {
            now += TICK;
            let changes = allocator.allocate(clear_for(now, Duration::from_secs(3), Bitrate::MAX));

            assert_eq!(
                changes,
                vec![Grant {
                    key: expected,
                    allowance: Allowance::Unlimited
                }]
            );
        }

        assert!(!allocator.is_constrained());
    }

    #[test]
    fn a_track_climbs_at_most_one_step_per_recovery_step() {
        let mut now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());
        allocator.allocate(congested(now, Bitrate::kbps(150)));

        now += Duration::from_secs(3);
        allocator.allocate(clear_for(now, Duration::from_secs(3), Bitrate::kbps(700)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(mid()));

        now += TICK;
        allocator.allocate(clear_for(now, Duration::from_secs(3), Bitrate::MAX));
        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Capped(mid()),
            "a track that just climbed must sit at its new rung for a moment"
        );

        now += Duration::from_secs(1);
        allocator.allocate(clear_for(now, Duration::from_secs(4), Bitrate::MAX));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Unlimited);
    }

    #[test]
    fn a_track_paused_for_bandwidth_resumes_at_its_lowest_layer_first() {
        let mut now = Instant::now();
        let mut allocator = BandwidthAllocator::new(AllocatorConfig {
            allow_pause: true,
            ..AllocatorConfig::default()
        });

        allocator.upsert_video("screen", camera().with_priority(SCREENSHARE_PRIORITY));
        allocator.upsert_video("camera", camera());
        allocator.allocate(congested(now, Bitrate::kbps(150)));
        assert_eq!(layer_of(&allocator, "camera"), Allowance::Paused);

        now += Duration::from_secs(3);
        allocator.allocate(clear_for(now, Duration::from_secs(3), Bitrate::kbps(400)));
        assert_eq!(
            layer_of(&allocator, "camera"),
            Allowance::Capped(BASE),
            "a resumed track comes back at the bottom of the ladder"
        );

        now += Duration::from_secs(2);
        allocator.allocate(clear_for(now, Duration::from_secs(5), Bitrate::MAX));
        assert_eq!(
            layer_of(&allocator, "screen"),
            Allowance::Unlimited,
            "the screenshare is served first on the way back up too"
        );

        now += Duration::from_secs(2);
        allocator.allocate(clear_for(now, Duration::from_secs(7), Bitrate::MAX));
        assert_eq!(layer_of(&allocator, "camera"), Allowance::Unlimited);
    }

    #[test]
    fn a_clear_link_caps_a_track_the_budget_cannot_pay_for() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());

        let changes = allocator.allocate(Conditions {
            now,
            budget: Bitrate::kbps(400),
            state: CongestionState::Clear,
            state_since: None,
        });

        assert_eq!(
            changes,
            vec![Grant {
                key: "a",
                allowance: Allowance::Capped(BASE)
            }],
            "a subscriber joining a slow link must not be handed everything until it falls over"
        );
    }

    #[test]
    fn a_clear_link_with_room_for_everything_caps_nothing() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());

        let changes = allocator.allocate(clear_for(now, Duration::from_secs(9), Bitrate::MAX));

        assert!(changes.is_empty());
        assert_eq!(layer_of(&allocator, "a"), Allowance::Unlimited);
    }

    #[test]
    fn an_affordable_upgrade_is_not_refused_because_the_others_were_priced_at_their_upgrades() {
        let mut now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video("big", camera().with_measured(priced([100, 800, 3_000])));
        allocator.upsert_video("one", camera().with_measured(priced([50, 150, 300])));
        allocator.upsert_video("two", camera().with_measured(priced([50, 150, 300])));

        allocator.allocate(congested(now, Bitrate::kbps(200)));
        for key in ["big", "one", "two"] {
            assert_eq!(layer_of(&allocator, key), Allowance::Capped(BASE));
        }

        now += Duration::from_secs(3);
        let changes =
            allocator.allocate(clear_for(now, Duration::from_secs(3), Bitrate::kbps(3_700)));

        assert_eq!(
            changes,
            vec![Grant {
                key: "big",
                allowance: Allowance::Unlimited
            }],
            "the budget covers the expensive track three times over next to what the others hold"
        );
    }

    #[test]
    fn each_upgrade_is_measured_against_what_the_earlier_ones_actually_took() {
        let mut now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video("big", camera().with_measured(priced([100, 800, 3_000])));
        allocator.upsert_video("one", camera().with_measured(priced([50, 150, 300])));
        allocator.upsert_video("two", camera().with_measured(priced([50, 150, 300])));

        allocator.allocate(congested(now, Bitrate::kbps(200)));
        for key in ["big", "one", "two"] {
            assert_eq!(layer_of(&allocator, key), Allowance::Capped(BASE));
        }

        let budget = Bitrate::kbps(3_640);
        let mut raised = Vec::new();

        for _ in 0..4 {
            now += Duration::from_secs(1);
            for grant in allocator.allocate(clear_for(now, Duration::from_secs(3), budget)) {
                raised.push(grant.key);
            }
        }

        assert_eq!(
            raised,
            vec!["big", "one", "two"],
            "the budget covers all three at their top rungs, and every raise is charged what the earlier ones really took rather than what they were provisionally offered"
        );
    }

    #[test]
    fn a_track_climbs_only_as_far_as_the_budget_the_others_leave_it() {
        let mut now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video("big", camera().with_measured(priced([100, 800, 3_000])));
        allocator.upsert_video("one", camera().with_measured(priced([50, 150, 300])));
        allocator.upsert_video("two", camera().with_measured(priced([50, 150, 300])));

        allocator.allocate(congested(now, Bitrate::kbps(200)));

        let budget = Bitrate::kbps(3_450);
        let mut raised = Vec::new();

        for _ in 0..4 {
            now += Duration::from_secs(1);
            for grant in allocator.allocate(clear_for(now, Duration::from_secs(3), budget)) {
                raised.push((grant.key, grant.allowance));
            }
        }

        assert_eq!(
            raised,
            vec![
                ("big", Allowance::Unlimited),
                ("one", Allowance::Unlimited),
                ("two", Allowance::Capped(mid())),
            ],
            "the last track climbs only as far as what the first two are really being paid leaves it"
        );
    }

    #[test]
    fn a_ceiling_with_nothing_underneath_it_is_not_a_bandwidth_decision() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video(
            "a",
            VideoTrack::new(LayerSet::of(high()), Some(BASE))
                .with_measured(priced([150, 500, 1_500])),
        );
        allocator.allocate(congested(now, Bitrate::kbps(3_000)));

        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Unlimited,
            "nothing published under the subscriber's own ceiling is not a budget decision"
        );
        assert_eq!(
            allocator.spend(),
            Bitrate::kbps(1_500),
            "the selector forwards the only rung there is, so the books must carry its price"
        );
    }

    #[test]
    fn a_track_the_allocator_cannot_choose_for_is_paid_before_the_budget_is_split() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video(
            "overshoot",
            VideoTrack::new(LayerSet::of(high()), Some(BASE))
                .with_measured(priced([150, 500, 1_500])),
        );
        allocator.upsert_video("camera", camera().with_measured(priced([150, 500, 1_500])));
        allocator.allocate(congested(now, Bitrate::kbps(1_700)));

        assert_eq!(
            layer_of(&allocator, "camera"),
            Allowance::Capped(BASE),
            "the pool left for a managed track is what the unmanaged one did not already take"
        );
    }

    #[test]
    fn a_track_that_stops_being_choosable_gives_its_cap_back_without_waiting_for_a_clear_link() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video("a", camera().with_measured(priced([150, 500, 1_500])));
        allocator.upsert_video("b", camera().with_measured(priced([150, 500, 1_500])));
        allocator.allocate(congested(now, Bitrate::kbps(400)));
        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));

        allocator.upsert_video(
            "a",
            VideoTrack::new(LayerSet::of(high()), Some(BASE))
                .with_measured(priced([150, 500, 1_500])),
        );

        let changes = allocator.allocate(congested(now + TICK, Bitrate::kbps(400)));

        assert!(
            changes.contains(&Grant {
                key: "a",
                allowance: Allowance::Unlimited
            }),
            "the allocator stopped being able to choose a layer for this track and must stop capping it, congested or not"
        );
        assert_eq!(
            allocator.spend(),
            Bitrate::kbps(1_650),
            "the pool is charged the rung the selector really forwards, and the grant now says the same"
        );
    }

    #[test]
    fn an_over_subscribed_subscriber_stops_overrunning_at_the_bound() {
        let now = Instant::now();
        let mut allocator = fresh();
        for key in ["a", "b", "c", "d"] {
            allocator.upsert_video(key, camera().with_measured(priced([100, 500, 1_500])));
        }

        allocator.allocate(congested(now, Bitrate::kbps(200)));

        assert_eq!(layer_of(&allocator, "a"), Allowance::Capped(BASE));
        assert_eq!(layer_of(&allocator, "b"), Allowance::Capped(BASE));
        assert_eq!(
            layer_of(&allocator, "c"),
            Allowance::Capped(BASE),
            "a budget of two hundred may be overrun to three hundred rather than blank a tile"
        );
        assert_eq!(
            layer_of(&allocator, "d"),
            Allowance::Paused,
            "refusing to pause is not a licence to queue two seconds of video"
        );
    }

    #[test]
    fn a_paused_track_resumes_as_soon_as_the_budget_has_room_for_it() {
        let mut now = Instant::now();
        let mut allocator = BandwidthAllocator::new(AllocatorConfig {
            allow_pause: true,
            ..AllocatorConfig::default()
        });

        allocator.upsert_video("screen", camera().with_priority(SCREENSHARE_PRIORITY));
        allocator.upsert_video("camera", camera());
        allocator.allocate(congested(now, Bitrate::kbps(150)));
        assert_eq!(layer_of(&allocator, "camera"), Allowance::Paused);

        now += Duration::from_secs(2);
        let changes = allocator.allocate(congested(now, Bitrate::kbps(340)));

        assert_eq!(
            changes,
            vec![Grant {
                key: "camera",
                allowance: Allowance::Capped(BASE)
            }],
            "a bandwidth pause is lifted by the budget having room, not by a return to clear"
        );
    }

    #[test]
    fn a_track_with_nothing_left_to_give_is_not_held_back() {
        let now = Instant::now();
        let mut allocator = fresh();

        allocator.upsert_video("starved", VideoTrack::new(LayerSet::of(high()), Some(BASE)));
        allocator.allocate(congested(now, Bitrate::kbps(3_000)));

        assert!(
            !allocator.is_constrained(),
            "a track the allocator never chose for is not a track it capped"
        );
        assert!(
            !allocator.is_held_back(),
            "a track that gets what the selector would give it anyway is not a bandwidth complaint"
        );

        allocator.upsert_video("camera", camera());
        allocator.allocate(congested(now, Bitrate::kbps(200)));

        assert!(allocator.is_held_back());
    }

    #[test]
    fn an_audio_track_that_is_carrying_nothing_books_no_floor() {
        let mut allocator = fresh();
        allocator.upsert_audio("speaking", AudioTrack::carrying(Some(Bitrate::kbps(40))));
        allocator.upsert_audio("muted", AudioTrack::silent());

        assert_eq!(
            allocator.audio_reserve(),
            Bitrate::kbps(48),
            "a subscription that cannot carry audio must not book room for it"
        );
    }

    #[test]
    fn holding_changes_nothing_at_all() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());
        allocator.allocate(congested(now, Bitrate::kbps(3_000)));
        let before = layer_of(&allocator, "a");

        let changes = allocator.allocate(Conditions {
            now: now + TICK,
            budget: Bitrate::kbps(100),
            state: CongestionState::Hold,
            state_since: Some(now),
        });

        assert!(changes.is_empty());
        assert_eq!(layer_of(&allocator, "a"), before);
    }

    #[test]
    fn a_cap_with_nothing_underneath_it_pauses_rather_than_overshoots() {
        let now = Instant::now();
        let mut allocator = BandwidthAllocator::new(AllocatorConfig {
            allow_pause: true,
            ..AllocatorConfig::default()
        });

        allocator.upsert_video(
            "a",
            VideoTrack::new(LayerSet::of(high()), Some(SpatialLayer::MAX)).with_measured([
                None,
                None,
                Some(Bitrate::kbps(1_500)),
            ]),
        );

        allocator.allocate(congested(now, Bitrate::kbps(200)));

        assert_eq!(
            layer_of(&allocator, "a"),
            Allowance::Paused,
            "a subscriber under a budget must never be handed a layer above its cap"
        );
    }

    #[test]
    fn a_track_the_subscriber_turned_off_is_not_a_bandwidth_pause() {
        let now = Instant::now();
        let mut allocator = BandwidthAllocator::new(AllocatorConfig {
            allow_pause: true,
            ..AllocatorConfig::default()
        });

        allocator.upsert_video("off", VideoTrack::new(LayerSet::all(), None));
        allocator.upsert_video("on", camera());

        allocator.allocate(congested(now, Bitrate::kbps(150)));

        assert_eq!(layer_of(&allocator, "off"), Allowance::Unlimited);
        assert_eq!(layer_of(&allocator, "on"), Allowance::Capped(BASE));
    }

    #[test]
    fn the_desired_bitrate_is_the_unconstrained_wish() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_audio("talker", AudioTrack::carrying(None));
        allocator.upsert_video("a", camera());
        allocator.upsert_video("b", camera().with_measured(priced([100, 300, 800])));

        assert_eq!(allocator.desired(), Bitrate::kbps(2_348));

        allocator.allocate(congested(now, Bitrate::kbps(300)));

        assert_eq!(
            allocator.desired(),
            Bitrate::kbps(2_348),
            "the prober must be told what we want, not what we settled for"
        );
        assert!(allocator.spend() < allocator.desired());
    }

    #[test]
    fn a_track_that_leaves_takes_its_grant_with_it() {
        let now = Instant::now();
        let mut allocator = fresh();
        allocator.upsert_video("a", camera());
        allocator.upsert_audio("a", AudioTrack::carrying(Some(Bitrate::kbps(50))));
        allocator.allocate(congested(now, Bitrate::kbps(200)));

        assert!(allocator.remove(&"a"));
        assert!(!allocator.remove(&"a"));
        assert_eq!(allocator.grant(&"a"), None);
        assert_eq!(allocator.spend(), Bitrate::ZERO);
        assert_eq!(allocator.video_tracks(), 0);
    }
}
