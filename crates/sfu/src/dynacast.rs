use std::collections::BTreeMap;

use crate::ladder::{LayerLadder, VideoQuality};
use crate::layer::{LayerSet, SpatialLayer};

#[derive(Debug, Clone)]
pub struct TrackDemand<K: Ord> {
    subscribers: BTreeMap<K, Option<SpatialLayer>>,
}

impl<K: Ord> TrackDemand<K> {
    pub fn new() -> Self {
        Self {
            subscribers: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, subscriber: K, wanted: Option<SpatialLayer>) -> bool {
        let before = self.wanted();
        self.subscribers.insert(subscriber, wanted);
        before != self.wanted()
    }

    pub fn remove(&mut self, subscriber: &K) -> bool {
        let before = self.wanted();
        self.subscribers.remove(subscriber);
        before != self.wanted()
    }

    pub fn clear(&mut self) -> bool {
        let before = self.wanted();
        self.subscribers.clear();
        before != self.wanted()
    }

    pub fn subscribers(&self) -> usize {
        self.subscribers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.subscribers.is_empty()
    }

    pub fn wanted(&self) -> LayerSet {
        self.subscribers.values().flatten().copied().collect()
    }

    pub fn highest(&self) -> Option<SpatialLayer> {
        self.wanted().highest()
    }

    pub fn enabled(&self) -> LayerSet {
        match self.highest() {
            Some(layer) => LayerSet::upto(layer),
            None => LayerSet::EMPTY,
        }
    }

    pub fn quality(&self, ladder: &LayerLadder) -> VideoQuality {
        match self.highest() {
            Some(layer) => ladder.quality_of(layer),
            None => VideoQuality::Off,
        }
    }
}

impl<K: Ord> Default for TrackDemand<K> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "dynacast_tests.rs"]
mod tests;
