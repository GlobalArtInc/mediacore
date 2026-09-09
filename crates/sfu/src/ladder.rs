use crate::bitrate::Bitrate;
use crate::layer::{LAYER_COUNT, SpatialLayer};

const SIZE_TOLERANCE_NUMERATOR: u32 = 9;
const SIZE_TOLERANCE_DENOMINATOR: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoQuality {
    Low,
    Medium,
    #[default]
    High,
    Off,
}

impl VideoQuality {
    pub fn rank(self) -> Option<u8> {
        match self {
            Self::Low => Some(0),
            Self::Medium => Some(1),
            Self::High => Some(2),
            Self::Off => None,
        }
    }

    fn of_rank(rank: u8) -> Self {
        match rank {
            0 => Self::Low,
            1 => Self::Medium,
            _ => Self::High,
        }
    }
}

impl std::fmt::Display for VideoQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Off => write!(f, "off"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Rung {
    quality: Option<VideoQuality>,
    size: Option<(u32, u32)>,
    bitrate: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerLadder {
    rungs: [Rung; LAYER_COUNT],
}

impl LayerLadder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_size(&mut self, layer: SpatialLayer, width: u32, height: u32) {
        self.rungs[layer.slot()].size = (width > 0 && height > 0).then_some((width, height));
    }

    pub fn with_size(mut self, layer: SpatialLayer, width: u32, height: u32) -> Self {
        self.set_size(layer, width, height);
        self
    }

    pub fn set_quality(&mut self, layer: SpatialLayer, quality: VideoQuality) {
        self.rungs[layer.slot()].quality = quality.rank().map(|_| quality);
    }

    pub fn with_quality(mut self, layer: SpatialLayer, quality: VideoQuality) -> Self {
        self.set_quality(layer, quality);
        self
    }

    pub fn set_bitrate(&mut self, layer: SpatialLayer, bitrate: u32) {
        self.rungs[layer.slot()].bitrate = (bitrate > 0).then_some(bitrate);
    }

    pub fn with_bitrate(mut self, layer: SpatialLayer, bitrate: u32) -> Self {
        self.set_bitrate(layer, bitrate);
        self
    }

    pub fn size(&self, layer: SpatialLayer) -> Option<(u32, u32)> {
        self.rungs[layer.slot()].size
    }

    pub fn declared_bitrate(&self, layer: SpatialLayer) -> Option<Bitrate> {
        self.rungs[layer.slot()]
            .bitrate
            .map(|bitrate| Bitrate::bps(u64::from(bitrate)))
    }

    pub fn declared_bitrates(&self) -> [Option<Bitrate>; LAYER_COUNT] {
        std::array::from_fn(|slot| self.declared_bitrate(SpatialLayer::new(slot as u8)))
    }

    pub fn is_empty(&self) -> bool {
        self.rungs.iter().all(|rung| rung == &Rung::default())
    }

    pub fn quality_of(&self, layer: SpatialLayer) -> VideoQuality {
        self.rungs[layer.slot()]
            .quality
            .unwrap_or_else(|| VideoQuality::of_rank(layer.index()))
    }

    pub fn layer_of(&self, quality: VideoQuality) -> Option<SpatialLayer> {
        let wanted = quality.rank()?;
        let declared = self.declared();

        if declared.is_empty() {
            return Some(SpatialLayer::new(wanted));
        }

        declared
            .iter()
            .filter(|(_, rank)| *rank >= wanted)
            .min_by_key(|(layer, rank)| (*rank, *layer))
            .or_else(|| declared.iter().max_by_key(|(layer, rank)| (*rank, *layer)))
            .map(|(layer, _)| *layer)
    }

    fn declared(&self) -> Vec<(SpatialLayer, u8)> {
        (0..LAYER_COUNT as u8)
            .map(SpatialLayer::new)
            .filter_map(|layer| {
                let quality = self.rungs[layer.slot()].quality?;
                Some((layer, quality.rank()?))
            })
            .collect()
    }

    fn shortest_side(size: (u32, u32)) -> u32 {
        size.0.min(size.1)
    }

    pub fn ceiling_for(&self, width: u32, height: u32) -> Option<SpatialLayer> {
        let wanted = Self::shortest_side((width, height)) * SIZE_TOLERANCE_NUMERATOR
            / SIZE_TOLERANCE_DENOMINATOR;
        let mut highest = None;

        for slot in 0..LAYER_COUNT {
            let layer = SpatialLayer::new(slot as u8);
            let Some(size) = self.rungs[slot].size else {
                continue;
            };

            highest = Some(layer);

            if Self::shortest_side(size) >= wanted {
                return Some(layer);
            }
        }

        highest
    }
}

#[cfg(test)]
#[path = "ladder_tests.rs"]
mod tests;
