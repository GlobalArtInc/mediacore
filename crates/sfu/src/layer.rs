#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SpatialLayer(u8);

impl SpatialLayer {
    pub const BASE: Self = Self(0);
    pub const MAX_INDEX: u8 = 2;

    pub fn new(index: u8) -> Self {
        Self(index.min(Self::MAX_INDEX))
    }

    pub fn index(self) -> u8 {
        self.0
    }
}

impl std::fmt::Display for SpatialLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerDecision {
    Forward,
    Switch,
    AwaitKeyframe,
    WrongLayer,
}

#[derive(Debug, Clone)]
pub struct LayerSelector {
    target: SpatialLayer,
    current: Option<SpatialLayer>,
    gated: bool,
}

impl LayerSelector {
    pub fn new(gated: bool) -> Self {
        Self {
            target: SpatialLayer::BASE,
            current: None,
            gated,
        }
    }

    pub fn target(&self) -> SpatialLayer {
        self.target
    }

    pub fn current(&self) -> Option<SpatialLayer> {
        self.current
    }

    pub fn is_latched(&self) -> bool {
        self.current.is_some()
    }

    pub fn request(&mut self, layer: SpatialLayer) -> bool {
        if self.target == layer {
            return false;
        }

        self.target = layer;
        true
    }

    pub fn reset(&mut self) {
        self.current = None;
    }

    pub fn select(&mut self, layer: SpatialLayer, keyframe: bool) -> LayerDecision {
        if self.current == Some(layer) && layer == self.target {
            return LayerDecision::Forward;
        }

        if layer != self.target {
            return match self.current {
                Some(current) if current == layer => LayerDecision::Forward,
                _ => LayerDecision::WrongLayer,
            };
        }

        if self.gated && !keyframe {
            return match self.current {
                Some(current) if current == layer => LayerDecision::Forward,
                _ => LayerDecision::AwaitKeyframe,
            };
        }

        self.current = Some(layer);
        LayerDecision::Switch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gated_selector_waits_for_a_keyframe_before_latching() {
        let mut selector = LayerSelector::new(true);

        assert_eq!(
            selector.select(SpatialLayer::BASE, false),
            LayerDecision::AwaitKeyframe
        );
        assert!(!selector.is_latched());

        assert_eq!(
            selector.select(SpatialLayer::BASE, true),
            LayerDecision::Switch
        );
        assert_eq!(
            selector.select(SpatialLayer::BASE, false),
            LayerDecision::Forward
        );
    }

    #[test]
    fn an_ungated_selector_latches_on_the_first_packet() {
        let mut selector = LayerSelector::new(false);

        assert_eq!(
            selector.select(SpatialLayer::BASE, false),
            LayerDecision::Switch
        );
        assert_eq!(selector.current(), Some(SpatialLayer::BASE));
    }

    #[test]
    fn a_single_layer_never_switches_again() {
        let mut selector = LayerSelector::new(true);
        selector.select(SpatialLayer::BASE, true);

        for _ in 0..10 {
            assert_eq!(
                selector.select(SpatialLayer::BASE, false),
                LayerDecision::Forward
            );
        }

        assert!(!selector.request(SpatialLayer::BASE));
    }

    #[test]
    fn a_requested_layer_is_taken_on_its_next_keyframe() {
        let mut selector = LayerSelector::new(true);
        selector.select(SpatialLayer::BASE, true);

        assert!(selector.request(SpatialLayer::new(1)));
        assert_eq!(
            selector.select(SpatialLayer::BASE, false),
            LayerDecision::Forward
        );
        assert_eq!(
            selector.select(SpatialLayer::new(1), false),
            LayerDecision::AwaitKeyframe
        );
        assert_eq!(
            selector.select(SpatialLayer::new(1), true),
            LayerDecision::Switch
        );
        assert_eq!(
            selector.select(SpatialLayer::BASE, false),
            LayerDecision::WrongLayer
        );
    }

    #[test]
    fn a_reset_selector_needs_a_keyframe_again() {
        let mut selector = LayerSelector::new(true);
        selector.select(SpatialLayer::BASE, true);
        selector.reset();

        assert_eq!(
            selector.select(SpatialLayer::BASE, false),
            LayerDecision::AwaitKeyframe
        );
    }

    #[test]
    fn layer_indexes_are_clamped() {
        assert_eq!(SpatialLayer::new(9).index(), SpatialLayer::MAX_INDEX);
        assert_eq!(SpatialLayer::default(), SpatialLayer::BASE);
    }
}
