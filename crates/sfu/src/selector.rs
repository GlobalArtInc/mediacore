use std::time::{Duration, Instant};

use crate::ladder::{LayerLadder, VideoQuality};
use crate::layer::{LayerSet, SpatialLayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerPreference {
    Quality(VideoQuality),
    Size { width: u32, height: u32 },
}

impl Default for LayerPreference {
    fn default() -> Self {
        Self::Quality(VideoQuality::default())
    }
}

impl std::fmt::Display for LayerPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quality(quality) => write!(f, "{quality}"),
            Self::Size { width, height } => write!(f, "{width}x{height}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Allowance {
    #[default]
    Unlimited,
    Capped(SpatialLayer),
    Paused,
}

impl Allowance {
    pub fn is_paused(self) -> bool {
        matches!(self, Self::Paused)
    }

    pub fn cap(self) -> Option<SpatialLayer> {
        match self {
            Self::Unlimited => Some(SpatialLayer::MAX),
            Self::Capped(layer) => Some(layer),
            Self::Paused => None,
        }
    }

    pub fn rank(self) -> i16 {
        match self {
            Self::Paused => -1,
            Self::Capped(layer) => i16::from(layer.index()),
            Self::Unlimited => i16::MAX,
        }
    }
}

impl std::fmt::Display for Allowance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unlimited => write!(f, "unlimited"),
            Self::Capped(layer) => write!(f, "capped@{layer}"),
            Self::Paused => write!(f, "paused"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectorConfig {
    pub upgrade_hold: Duration,
}

impl SelectorConfig {
    pub const DEFAULT_UPGRADE_HOLD: Duration = Duration::from_secs(2);
}

impl Default for SelectorConfig {
    fn default() -> Self {
        Self {
            upgrade_hold: Self::DEFAULT_UPGRADE_HOLD,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    layer: SpatialLayer,
    since: Instant,
}

#[derive(Debug, Clone)]
pub struct LayerSelector {
    config: SelectorConfig,
    ladder: LayerLadder,
    preference: LayerPreference,
    allowance: Allowance,
    available: LayerSet,
    target: Option<SpatialLayer>,
    peak: Option<SpatialLayer>,
    pending: Option<Pending>,
}

impl LayerSelector {
    pub fn new(config: SelectorConfig) -> Self {
        Self {
            config,
            ladder: LayerLadder::new(),
            preference: LayerPreference::default(),
            allowance: Allowance::default(),
            available: LayerSet::EMPTY,
            target: None,
            peak: None,
            pending: None,
        }
    }

    pub fn config(&self) -> &SelectorConfig {
        &self.config
    }

    pub fn preference(&self) -> LayerPreference {
        self.preference
    }

    pub fn allowance(&self) -> Allowance {
        self.allowance
    }

    pub fn available(&self) -> LayerSet {
        self.available
    }

    pub fn target(&self) -> Option<SpatialLayer> {
        self.target
    }

    pub fn pending(&self) -> Option<SpatialLayer> {
        self.pending.map(|pending| pending.layer)
    }

    pub fn ladder(&self) -> &LayerLadder {
        &self.ladder
    }

    pub fn set_ladder(&mut self, ladder: LayerLadder, now: Instant) -> Option<SpatialLayer> {
        if self.ladder == ladder {
            return None;
        }

        self.ladder = ladder;
        self.decide(now, true)
    }

    pub fn set_preference(
        &mut self,
        preference: LayerPreference,
        now: Instant,
    ) -> Option<SpatialLayer> {
        if self.preference == preference {
            return None;
        }

        self.preference = preference;
        self.decide(now, true)
    }

    pub fn set_allowance(&mut self, allowance: Allowance, now: Instant) -> Option<SpatialLayer> {
        if self.allowance == allowance {
            return None;
        }

        self.allowance = allowance;
        self.decide(now, true)
    }

    pub fn update(&mut self, available: LayerSet, now: Instant) -> Option<SpatialLayer> {
        self.available = available;
        self.decide(now, false)
    }

    pub fn tick(&mut self, now: Instant) -> Option<SpatialLayer> {
        self.decide(now, false)
    }

    pub fn reset(&mut self) {
        self.target = None;
        self.peak = None;
        self.pending = None;
    }

    pub fn peak(&self) -> Option<SpatialLayer> {
        self.peak
    }

    fn is_ramping(&self, want: SpatialLayer) -> bool {
        self.peak.is_none_or(|peak| want > peak)
    }

    pub fn preferred_ceiling(&self) -> Option<SpatialLayer> {
        match self.preference {
            LayerPreference::Quality(quality) => self.ladder.layer_of(quality),
            LayerPreference::Size { width, height } => self
                .ladder
                .ceiling_for(width, height)
                .or(Some(SpatialLayer::MAX)),
        }
    }

    pub fn ceiling(&self) -> Option<SpatialLayer> {
        let preference = self.preferred_ceiling()?;

        match self.allowance {
            Allowance::Unlimited => Some(preference),
            Allowance::Capped(cap) => Some(preference.min(cap)),
            Allowance::Paused => None,
        }
    }

    pub fn desired(&self) -> Option<SpatialLayer> {
        let ceiling = self.ceiling()?;
        let fitting = self.available.highest_at_most(ceiling);

        match self.allowance {
            Allowance::Unlimited => fitting.or_else(|| self.available.lowest()),
            _ => fitting,
        }
    }

    fn decide(&mut self, now: Instant, immediate: bool) -> Option<SpatialLayer> {
        let Some(want) = self.desired() else {
            self.pending = None;
            return None;
        };

        let Some(target) = self.target else {
            return self.commit(want);
        };

        if want == target {
            self.pending = None;
            return None;
        }

        if want < target || immediate || self.is_ramping(want) {
            return self.commit(want);
        }

        match self.pending {
            Some(pending) if pending.layer == want => {
                if now.saturating_duration_since(pending.since) >= self.config.upgrade_hold {
                    self.commit(want)
                } else {
                    None
                }
            }
            _ => {
                self.pending = Some(Pending {
                    layer: want,
                    since: now,
                });
                None
            }
        }
    }

    fn commit(&mut self, layer: SpatialLayer) -> Option<SpatialLayer> {
        self.pending = None;

        if self.target == Some(layer) {
            return None;
        }

        tracing::debug!(
            target = %layer,
            available = %self.available,
            preference = %self.preference,
            "subscriber layer target changed"
        );

        self.target = Some(layer);
        self.peak = Some(self.peak.map_or(layer, |peak| peak.max(layer)));
        Some(layer)
    }
}

impl Default for LayerSelector {
    fn default() -> Self {
        Self::new(SelectorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: SpatialLayer = SpatialLayer::BASE;

    fn mid() -> SpatialLayer {
        SpatialLayer::new(1)
    }

    fn high() -> SpatialLayer {
        SpatialLayer::new(2)
    }

    fn all() -> LayerSet {
        LayerSet::all()
    }

    fn selector() -> LayerSelector {
        LayerSelector::new(SelectorConfig {
            upgrade_hold: Duration::from_secs(2),
        })
    }

    #[test]
    fn the_first_decision_takes_the_wanted_layer_without_a_ramp() {
        let now = Instant::now();
        let mut selector = selector();

        assert_eq!(selector.update(all(), now), Some(high()));
        assert_eq!(selector.target(), Some(high()));
        assert_eq!(selector.update(all(), now), None);
    }

    #[test]
    fn a_quality_request_is_honoured_immediately_in_both_directions() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);

        assert_eq!(
            selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now),
            Some(LOW)
        );
        assert_eq!(
            selector.set_preference(LayerPreference::Quality(VideoQuality::High), now),
            Some(high())
        );
        assert_eq!(
            selector.set_preference(LayerPreference::Quality(VideoQuality::High), now),
            None
        );
    }

    #[test]
    fn a_layer_that_goes_away_is_left_immediately() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);

        let left = selector.update(LayerSet::from_iter([LOW, mid()]), now);

        assert_eq!(left, Some(mid()));
        assert_eq!(selector.target(), Some(mid()));
    }

    #[test]
    fn the_first_climb_up_the_ladder_is_not_held_back() {
        let mut now = Instant::now();
        let mut selector = selector();

        assert_eq!(selector.update(LayerSet::of(LOW), now), Some(LOW));

        now += Duration::from_millis(20);
        assert_eq!(
            selector.update(LayerSet::from_iter([LOW, mid()]), now),
            Some(mid()),
            "a subscriber that has never been higher waited out the hold on its way up"
        );

        now += Duration::from_millis(20);
        assert_eq!(selector.update(all(), now), Some(high()));
        assert_eq!(selector.peak(), Some(high()));
        assert_eq!(selector.pending(), None);
    }

    #[test]
    fn a_layer_that_comes_back_is_taken_only_after_the_hold() {
        let mut now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);
        selector.update(LayerSet::from_iter([LOW, mid()]), now);

        now += Duration::from_millis(100);
        assert_eq!(selector.update(all(), now), None);
        assert_eq!(selector.pending(), Some(high()));

        now += Duration::from_millis(1_000);
        assert_eq!(selector.update(all(), now), None);

        now += Duration::from_millis(1_000);
        assert_eq!(selector.update(all(), now), Some(high()));
        assert_eq!(selector.pending(), None);
    }

    #[test]
    fn a_blinking_layer_never_pulls_the_subscriber_back_up() {
        let mut now = Instant::now();
        let mut selector = selector();
        let mut commits = Vec::new();

        commits.extend(selector.update(all(), now));
        commits.extend(selector.update(LayerSet::from_iter([LOW, mid()]), now));

        for _ in 0..10 {
            now += Duration::from_millis(700);
            commits.extend(selector.update(all(), now));
            now += Duration::from_millis(700);
            commits.extend(selector.update(LayerSet::from_iter([LOW, mid()]), now));
        }

        assert_eq!(commits, vec![high(), mid()]);
        assert_eq!(selector.target(), Some(mid()));
    }

    #[test]
    fn a_pending_upgrade_matures_on_a_tick_alone() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);
        selector.update(LayerSet::from_iter([LOW]), now);

        assert_eq!(selector.update(all(), now), None);
        assert_eq!(selector.tick(now + Duration::from_millis(500)), None);
        assert_eq!(selector.tick(now + Duration::from_secs(3)), Some(high()));
    }

    #[test]
    fn nothing_available_keeps_the_current_target() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);

        assert_eq!(selector.update(LayerSet::EMPTY, now), None);
        assert_eq!(selector.target(), Some(high()));
        assert_eq!(selector.desired(), None);
    }

    #[test]
    fn a_ceiling_with_nothing_below_it_overshoots_to_the_nearest_layer() {
        let now = Instant::now();
        let mut selector = selector();
        selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now);

        assert_eq!(
            selector.update(LayerSet::from_iter([mid(), high()]), now),
            Some(mid())
        );
    }

    #[test]
    fn quality_off_asks_for_no_layer_at_all() {
        let now = Instant::now();
        let mut selector = selector();

        selector.set_preference(LayerPreference::Quality(VideoQuality::Off), now);

        assert_eq!(selector.ceiling(), None);
        assert_eq!(selector.desired(), None);
        assert_eq!(selector.update(all(), now), None);
        assert_eq!(selector.target(), None);
    }

    #[test]
    fn a_quality_request_lands_on_the_layer_the_publisher_declared_it_as() {
        let now = Instant::now();
        let mut selector = selector();
        let ladder = LayerLadder::new()
            .with_quality(LOW, VideoQuality::Medium)
            .with_quality(mid(), VideoQuality::High);

        selector.set_ladder(ladder, now);
        selector.update(LayerSet::from_iter([LOW, mid()]), now);

        assert_eq!(
            selector.set_preference(LayerPreference::Quality(VideoQuality::Medium), now),
            Some(LOW),
            "the publisher's medium encoding is the bottom rung, not the middle one"
        );
        assert_eq!(
            selector.set_preference(LayerPreference::Quality(VideoQuality::High), now),
            Some(mid())
        );
    }

    #[test]
    fn an_unknown_layer_size_falls_back_to_the_top_layer() {
        let now = Instant::now();
        let mut selector = selector();

        selector.set_preference(
            LayerPreference::Size {
                width: 320,
                height: 180,
            },
            now,
        );

        assert_eq!(selector.ceiling(), Some(SpatialLayer::MAX));

        let ladder = LayerLadder::new()
            .with_size(LOW, 320, 180)
            .with_size(mid(), 640, 360)
            .with_size(high(), 1280, 720);

        assert_eq!(selector.set_ladder(ladder, now), None);
        assert_eq!(selector.ceiling(), Some(LOW));
        assert_eq!(selector.update(all(), now), Some(LOW));
    }

    #[test]
    fn a_smaller_video_size_downgrades_at_once() {
        let now = Instant::now();
        let ladder = LayerLadder::new()
            .with_size(LOW, 320, 180)
            .with_size(mid(), 640, 360)
            .with_size(high(), 1280, 720);
        let mut selector = selector();
        selector.set_ladder(ladder, now);
        selector.update(all(), now);

        assert_eq!(
            selector.set_preference(
                LayerPreference::Size {
                    width: 640,
                    height: 360
                },
                now
            ),
            Some(mid())
        );
    }

    #[test]
    fn an_allowance_caps_a_preference_but_never_raises_it() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);

        assert_eq!(
            selector.set_allowance(Allowance::Capped(LOW), now),
            Some(LOW)
        );
        assert_eq!(selector.ceiling(), Some(LOW));
        assert_eq!(
            selector.preferred_ceiling(),
            Some(high()),
            "the allowance is a second ceiling, not a new preference"
        );

        selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now);
        assert_eq!(
            selector.set_allowance(Allowance::Capped(high()), now),
            None,
            "an allowance above what the subscriber asked for changes nothing"
        );
        assert_eq!(selector.ceiling(), Some(LOW));
    }

    #[test]
    fn a_lifted_allowance_returns_to_what_the_subscriber_asked_for() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);
        selector.set_allowance(Allowance::Capped(LOW), now);

        assert_eq!(
            selector.set_allowance(Allowance::Unlimited, now),
            Some(high()),
            "a subscriber that gets bandwidth back does not have to ask again"
        );
        assert_eq!(selector.allowance(), Allowance::Unlimited);
    }

    #[test]
    fn a_capped_selector_does_not_overshoot_to_a_layer_above_the_cap() {
        let now = Instant::now();
        let mut selector = selector();
        selector.set_preference(LayerPreference::Quality(VideoQuality::Low), now);
        selector.set_allowance(Allowance::Capped(LOW), now);

        assert_eq!(
            selector.update(LayerSet::from_iter([mid(), high()]), now),
            None
        );
        assert_eq!(selector.desired(), None);
        assert_eq!(selector.target(), None);

        selector.set_allowance(Allowance::Unlimited, now);
        assert_eq!(
            selector.desired(),
            Some(mid()),
            "without a budget the old overshoot is still what a subscriber gets"
        );
    }

    #[test]
    fn a_paused_allowance_asks_for_no_layer_but_keeps_the_target_it_had() {
        let now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);

        assert_eq!(selector.set_allowance(Allowance::Paused, now), None);
        assert_eq!(selector.ceiling(), None);
        assert_eq!(selector.desired(), None);
        assert_eq!(selector.target(), Some(high()));
    }

    #[test]
    fn an_allowance_change_bypasses_the_upgrade_hold() {
        let mut now = Instant::now();
        let mut selector = selector();
        selector.update(all(), now);
        selector.set_allowance(Allowance::Capped(LOW), now);

        now += Duration::from_millis(100);

        assert_eq!(
            selector.set_allowance(Allowance::Capped(mid()), now),
            Some(mid()),
            "an allocator grant is not a blinking publisher and must not wait out the hold"
        );
    }
}
