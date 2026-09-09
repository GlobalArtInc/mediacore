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
#[path = "layer_tests.rs"]
mod tests;
