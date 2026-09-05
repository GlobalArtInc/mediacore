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

    pub fn size(&self, layer: SpatialLayer) -> Option<(u32, u32)> {
        self.rungs[layer.slot()].size
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
mod tests {
    use super::*;

    const LOW: SpatialLayer = SpatialLayer::BASE;

    fn mid() -> SpatialLayer {
        SpatialLayer::new(1)
    }

    fn high() -> SpatialLayer {
        SpatialLayer::new(2)
    }

    #[test]
    fn an_undeclared_ladder_falls_back_to_the_well_known_one() {
        let ladder = LayerLadder::new();

        assert!(ladder.is_empty());
        assert_eq!(ladder.quality_of(LOW), VideoQuality::Low);
        assert_eq!(ladder.quality_of(mid()), VideoQuality::Medium);
        assert_eq!(ladder.quality_of(high()), VideoQuality::High);
        assert_eq!(ladder.layer_of(VideoQuality::Low), Some(LOW));
        assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(mid()));
        assert_eq!(ladder.layer_of(VideoQuality::High), Some(high()));
        assert_eq!(ladder.layer_of(VideoQuality::Off), None);
    }

    #[test]
    fn every_declared_rung_answers_for_the_quality_it_was_declared_with() {
        let ladders = [
            vec![
                (LOW, VideoQuality::Low),
                (mid(), VideoQuality::Medium),
                (high(), VideoQuality::High),
            ],
            vec![(LOW, VideoQuality::Medium), (mid(), VideoQuality::High)],
            vec![(LOW, VideoQuality::Low), (mid(), VideoQuality::High)],
            vec![(LOW, VideoQuality::Low), (mid(), VideoQuality::Medium)],
            vec![(LOW, VideoQuality::High)],
        ];

        for declared in ladders {
            let ladder = declared
                .iter()
                .fold(LayerLadder::new(), |ladder, (layer, quality)| {
                    ladder.with_quality(*layer, *quality)
                });

            for (layer, quality) in declared {
                assert_eq!(
                    ladder.quality_of(layer),
                    quality,
                    "the ladder forgot what layer {layer} was declared as"
                );
                assert_eq!(
                    ladder.layer_of(quality),
                    Some(layer),
                    "layer {layer} was declared {quality}, but asking for {quality} landed elsewhere"
                );
            }
        }
    }

    #[test]
    fn a_short_ladder_answers_from_what_the_publisher_declared() {
        let ladder = LayerLadder::new()
            .with_quality(LOW, VideoQuality::Medium)
            .with_quality(mid(), VideoQuality::High);

        assert_eq!(
            ladder.layer_of(VideoQuality::Medium),
            Some(LOW),
            "the publisher's medium encoding is the bottom rung of this ladder"
        );
        assert_eq!(ladder.layer_of(VideoQuality::High), Some(mid()));
        assert_eq!(
            ladder.layer_of(VideoQuality::Low),
            Some(LOW),
            "a quality below everything published is answered by the smallest rung"
        );
        assert_eq!(ladder.quality_of(LOW), VideoQuality::Medium);
        assert_eq!(ladder.quality_of(mid()), VideoQuality::High);
    }

    #[test]
    fn a_quality_above_the_ladder_is_answered_by_its_top_rung() {
        let ladder = LayerLadder::new()
            .with_quality(LOW, VideoQuality::Low)
            .with_quality(mid(), VideoQuality::Medium);

        assert_eq!(ladder.layer_of(VideoQuality::High), Some(mid()));
        assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(mid()));
        assert_eq!(ladder.layer_of(VideoQuality::Low), Some(LOW));
    }

    #[test]
    fn a_quality_between_two_rungs_rounds_up() {
        let ladder = LayerLadder::new()
            .with_quality(LOW, VideoQuality::Low)
            .with_quality(mid(), VideoQuality::High);

        assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(mid()));
    }

    #[test]
    fn a_single_rung_answers_every_quality() {
        let ladder = LayerLadder::new().with_quality(LOW, VideoQuality::High);

        assert_eq!(ladder.layer_of(VideoQuality::Low), Some(LOW));
        assert_eq!(ladder.layer_of(VideoQuality::Medium), Some(LOW));
        assert_eq!(ladder.layer_of(VideoQuality::High), Some(LOW));
        assert_eq!(ladder.layer_of(VideoQuality::Off), None);
    }

    #[test]
    fn a_video_size_maps_to_the_smallest_rung_that_covers_it() {
        let ladder = LayerLadder::new()
            .with_size(LOW, 320, 180)
            .with_size(mid(), 640, 360)
            .with_size(high(), 1280, 720);

        assert_eq!(ladder.ceiling_for(320, 180), Some(LOW));
        assert_eq!(ladder.ceiling_for(400, 240), Some(mid()));
        assert_eq!(ladder.ceiling_for(640, 360), Some(mid()));
        assert_eq!(ladder.ceiling_for(1920, 1080), Some(high()));
        assert_eq!(ladder.ceiling_for(1280, 720), Some(high()));
        assert_eq!(LayerLadder::new().ceiling_for(320, 180), None);
    }

    #[test]
    fn a_rung_without_a_size_is_not_offered_to_a_size_request() {
        let ladder = LayerLadder::new()
            .with_quality(LOW, VideoQuality::Low)
            .with_size(mid(), 640, 360)
            .with_size(LOW, 0, 0);

        assert!(!ladder.is_empty());
        assert_eq!(ladder.size(LOW), None);
        assert_eq!(ladder.ceiling_for(320, 180), Some(mid()));
    }
}
