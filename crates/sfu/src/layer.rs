#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SpatialLayer(u8);

impl SpatialLayer {
    pub const BASE: Self = Self(0);
    pub const MAX_INDEX: u8 = 2;
    pub const MAX: Self = Self(Self::MAX_INDEX);

    pub fn new(index: u8) -> Self {
        Self(index.min(Self::MAX_INDEX))
    }

    pub fn index(self) -> u8 {
        self.0
    }

    pub fn slot(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for SpatialLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub const LAYER_COUNT: usize = SpatialLayer::MAX_INDEX as usize + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LayerSet(u8);

impl LayerSet {
    pub const EMPTY: Self = Self(0);

    pub fn all() -> Self {
        Self((1 << LAYER_COUNT) - 1)
    }

    pub fn of(layer: SpatialLayer) -> Self {
        Self(1 << layer.index())
    }

    pub fn insert(&mut self, layer: SpatialLayer) -> bool {
        let bit = 1 << layer.index();
        let missing = self.0 & bit == 0;
        self.0 |= bit;
        missing
    }

    pub fn remove(&mut self, layer: SpatialLayer) -> bool {
        let bit = 1 << layer.index();
        let present = self.0 & bit != 0;
        self.0 &= !bit;
        present
    }

    pub fn contains(&self, layer: SpatialLayer) -> bool {
        self.0 & (1 << layer.index()) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn len(&self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = SpatialLayer> + '_ {
        (0..LAYER_COUNT as u8)
            .map(SpatialLayer::new)
            .filter(|layer| self.contains(*layer))
    }

    pub fn highest(&self) -> Option<SpatialLayer> {
        self.highest_at_most(SpatialLayer::MAX)
    }

    pub fn lowest(&self) -> Option<SpatialLayer> {
        (0..LAYER_COUNT as u8)
            .map(SpatialLayer::new)
            .find(|layer| self.contains(*layer))
    }

    pub fn highest_at_most(&self, ceiling: SpatialLayer) -> Option<SpatialLayer> {
        (0..=ceiling.index())
            .rev()
            .map(SpatialLayer::new)
            .find(|layer| self.contains(*layer))
    }

    pub fn upto(ceiling: SpatialLayer) -> Self {
        Self((1 << (ceiling.index() + 1)) - 1)
    }
}

impl FromIterator<SpatialLayer> for LayerSet {
    fn from_iter<T: IntoIterator<Item = SpatialLayer>>(iter: T) -> Self {
        let mut set = Self::EMPTY;
        for layer in iter {
            set.insert(layer);
        }
        set
    }
}

impl std::fmt::Display for LayerSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        for (position, layer) in self.iter().enumerate() {
            if position > 0 {
                write!(f, ",")?;
            }
            write!(f, "{layer}")?;
        }
        write!(f, "]")
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
pub struct LayerLatch {
    target: SpatialLayer,
    current: Option<SpatialLayer>,
    gated: bool,
}

impl LayerLatch {
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

    pub fn is_switching(&self) -> bool {
        self.current != Some(self.target)
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
    fn a_gated_latch_waits_for_a_keyframe_before_taking_a_layer() {
        let mut latch = LayerLatch::new(true);

        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::AwaitKeyframe
        );
        assert!(!latch.is_latched());

        assert_eq!(
            latch.select(SpatialLayer::BASE, true),
            LayerDecision::Switch
        );
        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::Forward
        );
    }

    #[test]
    fn an_ungated_latch_takes_the_first_packet() {
        let mut latch = LayerLatch::new(false);

        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::Switch
        );
        assert_eq!(latch.current(), Some(SpatialLayer::BASE));
    }

    #[test]
    fn a_single_layer_never_switches_again() {
        let mut latch = LayerLatch::new(true);
        latch.select(SpatialLayer::BASE, true);

        for _ in 0..10 {
            assert_eq!(
                latch.select(SpatialLayer::BASE, false),
                LayerDecision::Forward
            );
        }

        assert!(!latch.request(SpatialLayer::BASE));
        assert!(!latch.is_switching());
    }

    #[test]
    fn a_requested_layer_is_taken_on_its_next_keyframe() {
        let mut latch = LayerLatch::new(true);
        latch.select(SpatialLayer::BASE, true);

        assert!(latch.request(SpatialLayer::new(1)));
        assert!(latch.is_switching());
        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::Forward
        );
        assert_eq!(
            latch.select(SpatialLayer::new(1), false),
            LayerDecision::AwaitKeyframe
        );
        assert_eq!(
            latch.select(SpatialLayer::new(1), true),
            LayerDecision::Switch
        );
        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::WrongLayer
        );
        assert!(!latch.is_switching());
    }

    #[test]
    fn a_reset_latch_needs_a_keyframe_again() {
        let mut latch = LayerLatch::new(true);
        latch.select(SpatialLayer::BASE, true);
        latch.reset();

        assert_eq!(
            latch.select(SpatialLayer::BASE, false),
            LayerDecision::AwaitKeyframe
        );
    }

    #[test]
    fn layer_indexes_are_clamped() {
        assert_eq!(SpatialLayer::new(9).index(), SpatialLayer::MAX_INDEX);
        assert_eq!(SpatialLayer::default(), SpatialLayer::BASE);
        assert_eq!(SpatialLayer::MAX.index(), SpatialLayer::MAX_INDEX);
    }

    #[test]
    fn a_layer_set_keeps_its_members_in_order() {
        let mut set = LayerSet::EMPTY;

        assert!(set.is_empty());
        assert!(set.insert(SpatialLayer::new(2)));
        assert!(!set.insert(SpatialLayer::new(2)));
        set.insert(SpatialLayer::BASE);

        assert_eq!(set.len(), 2);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![SpatialLayer::BASE, SpatialLayer::new(2)]
        );
        assert_eq!(set.lowest(), Some(SpatialLayer::BASE));
        assert_eq!(set.highest(), Some(SpatialLayer::new(2)));
        assert_eq!(set.to_string(), "[0,2]");

        assert!(set.remove(SpatialLayer::new(2)));
        assert!(!set.remove(SpatialLayer::new(2)));
        assert_eq!(set.highest(), Some(SpatialLayer::BASE));
    }

    #[test]
    fn a_ceiling_picks_the_best_layer_at_or_below_it() {
        let set: LayerSet = [SpatialLayer::BASE, SpatialLayer::new(2)]
            .into_iter()
            .collect();

        assert_eq!(
            set.highest_at_most(SpatialLayer::new(1)),
            Some(SpatialLayer::BASE)
        );
        assert_eq!(
            set.highest_at_most(SpatialLayer::new(2)),
            Some(SpatialLayer::new(2))
        );

        let high: LayerSet = [SpatialLayer::new(2)].into_iter().collect();
        assert_eq!(high.highest_at_most(SpatialLayer::BASE), None);
    }

    #[test]
    fn upto_covers_every_layer_below_the_ceiling() {
        assert_eq!(LayerSet::upto(SpatialLayer::BASE).len(), 1);
        assert_eq!(LayerSet::upto(SpatialLayer::new(1)).len(), 2);
        assert_eq!(LayerSet::upto(SpatialLayer::MAX), LayerSet::all());
        assert_eq!(LayerSet::all().len(), LAYER_COUNT);
    }
}
