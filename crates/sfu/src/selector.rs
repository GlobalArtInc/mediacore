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
#[path = "selector_tests.rs"]
mod tests;
