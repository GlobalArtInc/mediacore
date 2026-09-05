use mediacore_proto::livekit::{self, signal_response};
use mediacore_room::Room;
use mediacore_sfu::{
    LAYER_COUNT, LayerLadder, LayerPreference, LayerSet, SpatialLayer, VideoQuality,
};
use mediacore_transport::str0m::media::{KeyframeRequestKind, Rid};

use crate::state::AppState;

const RID_LADDERS: [[&str; LAYER_COUNT]; 2] = [["q", "h", "f"], ["2", "1", "0"]];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    layer: SpatialLayer,
    rid: Option<Rid>,
    quality: VideoQuality,
    width: u32,
    height: u32,
    bitrate: u32,
}

#[derive(Debug, Clone, Default)]
pub struct LayerMap {
    entries: Vec<Entry>,
    surplus: Vec<Rid>,
}

impl LayerMap {
    pub fn build(declared: &[livekit::VideoLayer], rids: &[Rid]) -> Self {
        if rids.is_empty() {
            return Self::single(declared);
        }

        let ladder = ascending(declared);
        let ordered = ordered_rids(rids, &ladder);
        let depth = ordered.len().min(LAYER_COUNT);
        let (kept, surplus) = ordered.split_at(depth);

        if !surplus.is_empty() {
            tracing::warn!(
                offered = ordered.len(),
                kept = depth,
                ?surplus,
                "the publisher offered a deeper ladder than this server carries"
            );
        }

        let aligned = ladder.len() == kept.len();
        let unnamed = kept.iter().any(|rid| named(*rid, &ladder).is_none());

        if !aligned && !ladder.is_empty() && unnamed {
            tracing::warn!(
                declared = ladder.len(),
                offered = kept.len(),
                "the publisher declared a ladder of another depth than it offered rids for, \
                 falling back to the well known ladder"
            );
        }

        let entries = kept
            .iter()
            .enumerate()
            .map(|(position, rid)| {
                let layer = SpatialLayer::new(position as u8);
                let declared = named(*rid, &ladder)
                    .or_else(|| aligned.then(|| ladder.get(position).copied()).flatten());

                Entry {
                    layer,
                    rid: Some(*rid),
                    quality: declared
                        .map(|declared| declared_quality(declared.quality))
                        .unwrap_or_else(|| fallback_quality(layer)),
                    width: declared.map(|declared| declared.width).unwrap_or_default(),
                    height: declared.map(|declared| declared.height).unwrap_or_default(),
                    bitrate: declared
                        .map(|declared| declared.bitrate)
                        .unwrap_or_default(),
                }
            })
            .collect();

        Self {
            entries,
            surplus: surplus.to_vec(),
        }
    }

    fn single(declared: &[livekit::VideoLayer]) -> Self {
        let ladder = ascending(declared);

        if ladder.len() > 1 {
            tracing::warn!(
                declared = ladder.len(),
                "the publisher declared several layers on a track that carries no rid, \
                 describing it by its top layer"
            );
        }

        let declared = ladder.last().copied();

        Self {
            entries: vec![Entry {
                layer: SpatialLayer::BASE,
                rid: None,
                quality: VideoQuality::High,
                width: declared.map(|layer| layer.width).unwrap_or_default(),
                height: declared.map(|layer| layer.height).unwrap_or_default(),
                bitrate: declared.map(|layer| layer.bitrate).unwrap_or_default(),
            }],
            surplus: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_simulcast(&self) -> bool {
        self.entries.len() > 1
    }

    pub fn layer_of(&self, rid: Option<Rid>) -> Option<SpatialLayer> {
        if let Some(entry) = self.entries.iter().find(|entry| entry.rid == rid) {
            return Some(entry.layer);
        }

        if rid.is_some_and(|rid| self.surplus.contains(&rid)) {
            return None;
        }

        self.entries.first().map(|entry| entry.layer)
    }

    pub fn rid_of(&self, layer: SpatialLayer) -> Option<Rid> {
        self.entries
            .iter()
            .find(|entry| entry.layer == layer)
            .and_then(|entry| entry.rid)
    }

    pub fn quality_of(&self, layer: SpatialLayer) -> VideoQuality {
        self.ladder().quality_of(layer)
    }

    pub fn rids(&self) -> Vec<(SpatialLayer, Rid)> {
        self.entries
            .iter()
            .filter_map(|entry| entry.rid.map(|rid| (entry.layer, rid)))
            .collect()
    }

    pub fn surplus(&self) -> &[Rid] {
        &self.surplus
    }

    pub fn all(&self) -> LayerSet {
        self.entries.iter().map(|entry| entry.layer).collect()
    }

    pub fn ladder(&self) -> LayerLadder {
        self.entries
            .iter()
            .fold(LayerLadder::new(), |ladder, entry| {
                ladder.with_quality(entry.layer, entry.quality).with_size(
                    entry.layer,
                    entry.width,
                    entry.height,
                )
            })
    }

    pub fn subscribed_qualities(
        &self,
        highest: Option<SpatialLayer>,
    ) -> Vec<livekit::SubscribedQuality> {
        let ceiling = highest
            .map(|layer| self.quality_of(layer))
            .and_then(VideoQuality::rank);

        [VideoQuality::Low, VideoQuality::Medium, VideoQuality::High]
            .into_iter()
            .map(|quality| livekit::SubscribedQuality {
                quality: proto_quality(quality) as i32,
                enabled: quality
                    .rank()
                    .zip(ceiling)
                    .is_some_and(|(rank, ceiling)| rank <= ceiling),
            })
            .collect()
    }

    pub fn video_layers(
        &self,
        present: LayerSet,
        ssrcs: &[Option<u32>; LAYER_COUNT],
    ) -> Vec<livekit::VideoLayer> {
        self.entries
            .iter()
            .filter(|entry| present.contains(entry.layer))
            .map(|entry| livekit::VideoLayer {
                quality: proto_quality(entry.quality) as i32,
                width: entry.width,
                height: entry.height,
                bitrate: entry.bitrate,
                ssrc: ssrcs[entry.layer.slot()].unwrap_or_default(),
                spatial_layer: i32::from(entry.layer.index()),
                rid: entry.rid.map(|rid| rid.to_string()).unwrap_or_default(),
                repair_ssrc: 0,
            })
            .collect()
    }
}

fn ascending(declared: &[livekit::VideoLayer]) -> Vec<&livekit::VideoLayer> {
    let mut ladder: Vec<&livekit::VideoLayer> = declared.iter().collect();
    ladder.sort_by_key(|layer| quality_rank(layer.quality));
    ladder
}

fn named<'a>(rid: Rid, ladder: &[&'a livekit::VideoLayer]) -> Option<&'a livekit::VideoLayer> {
    let name = rid.to_string();

    ladder
        .iter()
        .copied()
        .find(|layer| !layer.rid.is_empty() && layer.rid == name)
}

fn ordered_rids(rids: &[Rid], ladder: &[&livekit::VideoLayer]) -> Vec<Rid> {
    let mut ordered: Vec<(usize, usize, Rid)> = rids
        .iter()
        .enumerate()
        .map(|(index, rid)| (rank(*rid, index, ladder), index, *rid))
        .collect();

    ordered.sort_by_key(|(rank, index, _)| (*rank, *index));
    ordered.into_iter().map(|(_, _, rid)| rid).collect()
}

fn rank(rid: Rid, index: usize, ladder: &[&livekit::VideoLayer]) -> usize {
    if let Some(layer) = named(rid, ladder) {
        return quality_rank(layer.quality);
    }

    let name = rid.to_string();
    for known in RID_LADDERS {
        if let Some(position) = known.iter().position(|known| *known == name) {
            return position;
        }
    }

    index
}

fn fallback_quality(layer: SpatialLayer) -> VideoQuality {
    LayerLadder::new().quality_of(layer)
}

fn requested_quality(quality: i32) -> VideoQuality {
    match livekit::VideoQuality::try_from(quality).unwrap_or(livekit::VideoQuality::High) {
        livekit::VideoQuality::Low => VideoQuality::Low,
        livekit::VideoQuality::Medium => VideoQuality::Medium,
        livekit::VideoQuality::High => VideoQuality::High,
        livekit::VideoQuality::Off => VideoQuality::Off,
    }
}

fn declared_quality(quality: i32) -> VideoQuality {
    match requested_quality(quality) {
        VideoQuality::Off => VideoQuality::High,
        quality => quality,
    }
}

fn quality_rank(quality: i32) -> usize {
    declared_quality(quality).rank().unwrap_or_default() as usize
}

fn proto_quality(quality: VideoQuality) -> livekit::VideoQuality {
    match quality {
        VideoQuality::Low => livekit::VideoQuality::Low,
        VideoQuality::Medium => livekit::VideoQuality::Medium,
        VideoQuality::High => livekit::VideoQuality::High,
        VideoQuality::Off => livekit::VideoQuality::Off,
    }
}

pub fn preference_of(settings: &livekit::UpdateTrackSettings) -> LayerPreference {
    if settings.width > 0 && settings.height > 0 {
        return LayerPreference::Size {
            width: settings.width,
            height: settings.height,
        };
    }

    LayerPreference::Quality(requested_quality(settings.quality))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityUpdate {
    pub track_sid: String,
    pub publisher_sid: String,
    pub enabled: LayerSet,
}

#[allow(deprecated)]
pub fn quality_message(
    track_sid: &str,
    codec: &str,
    qualities: Vec<livekit::SubscribedQuality>,
) -> signal_response::Message {
    signal_response::Message::SubscribedQualityUpdate(livekit::SubscribedQualityUpdate {
        track_sid: track_sid.to_owned(),
        subscribed_qualities: qualities.clone(),
        subscribed_codecs: vec![livekit::SubscribedCodec {
            codec: codec.to_owned(),
            qualities,
        }],
    })
}

pub async fn apply_quality_update(state: &AppState, room: &Room, update: &QualityUpdate) -> bool {
    let Some(source) = state.router.source(&update.track_sid) else {
        return false;
    };

    source
        .reconcile(&update.track_sid, |message, enabled| {
            let Some(publisher) = room.participant_by_sid(&update.publisher_sid) else {
                tracing::debug!(
                    track = %update.track_sid,
                    publisher = %update.publisher_sid,
                    "the publisher of this track is no longer in the room"
                );
                return false;
            };

            tracing::info!(
                track = %update.track_sid,
                publisher = %publisher.identity,
                %enabled,
                "telling the publisher which layers are subscribed"
            );

            publisher.send(message)
        })
        .await
}

pub fn keyframe_kind() -> KeyframeRequestKind {
    KeyframeRequestKind::Pli
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(entries: &[(livekit::VideoQuality, &str, u32, u32)]) -> Vec<livekit::VideoLayer> {
        entries
            .iter()
            .map(|(quality, rid, width, height)| livekit::VideoLayer {
                quality: *quality as i32,
                rid: (*rid).to_owned(),
                width: *width,
                height: *height,
                ..Default::default()
            })
            .collect()
    }

    fn rids(names: &[&str]) -> Vec<Rid> {
        names.iter().map(|name| Rid::from(*name)).collect()
    }

    fn ladder() -> Vec<livekit::VideoLayer> {
        declared(&[
            (livekit::VideoQuality::Low, "q", 320, 180),
            (livekit::VideoQuality::Medium, "h", 640, 360),
            (livekit::VideoQuality::High, "f", 1280, 720),
        ])
    }

    fn nameless_ladder() -> Vec<livekit::VideoLayer> {
        declared(&[
            (livekit::VideoQuality::Low, "", 320, 180),
            (livekit::VideoQuality::Medium, "", 640, 360),
            (livekit::VideoQuality::High, "", 1280, 720),
        ])
    }

    #[test]
    fn a_declared_ladder_maps_every_rid_to_its_own_layer() {
        let map = LayerMap::build(&ladder(), &rids(&["q", "h", "f"]));

        assert_eq!(map.len(), 3);
        assert!(map.is_simulcast());
        assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
        assert_eq!(
            map.layer_of(Some(Rid::from("h"))),
            Some(SpatialLayer::new(1))
        );
        assert_eq!(map.layer_of(Some(Rid::from("f"))), Some(SpatialLayer::MAX));
        assert_eq!(map.rid_of(SpatialLayer::MAX), Some(Rid::from("f")));
        assert_eq!(map.all(), LayerSet::all());
        assert!(map.surplus().is_empty());
    }

    #[test]
    fn the_offer_order_does_not_decide_which_layer_is_the_low_one() {
        let map = LayerMap::build(&ladder(), &rids(&["f", "q", "h"]));

        assert_eq!(
            map.layer_of(Some(Rid::from("q"))),
            Some(SpatialLayer::BASE),
            "the quality the publisher declared decides, not the order it offered in"
        );
        assert_eq!(map.layer_of(Some(Rid::from("f"))), Some(SpatialLayer::MAX));
    }

    #[test]
    fn a_publisher_that_names_no_rid_still_gives_every_layer_its_size() {
        let map = LayerMap::build(&nameless_ladder(), &rids(&["q", "h", "f"]));
        let ladder = map.ladder();

        assert_eq!(
            ladder.size(SpatialLayer::BASE),
            Some((320, 180)),
            "a real browser client never fills VideoLayer.rid, so every layer lost its size"
        );
        assert_eq!(ladder.size(SpatialLayer::new(1)), Some((640, 360)));
        assert_eq!(ladder.size(SpatialLayer::MAX), Some((1280, 720)));
        assert_eq!(ladder.ceiling_for(320, 180), Some(SpatialLayer::BASE));
        assert_eq!(ladder.ceiling_for(640, 360), Some(SpatialLayer::new(1)));
        assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Low);
    }

    #[test]
    fn a_nameless_ladder_offered_out_of_order_still_lands_on_the_right_layers() {
        let map = LayerMap::build(&nameless_ladder(), &rids(&["f", "h", "q"]));

        assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
        assert_eq!(map.ladder().size(SpatialLayer::MAX), Some((1280, 720)));
    }

    #[test]
    fn a_declaration_that_does_not_line_up_with_the_rids_is_not_guessed_at() {
        let map = LayerMap::build(&nameless_ladder(), &rids(&["h", "f"]));
        let ladder = map.ladder();

        assert_eq!(map.len(), 2);
        assert_eq!(
            ladder.size(SpatialLayer::BASE),
            None,
            "three nameless layers say nothing about which two encodings these rids are"
        );
        assert_eq!(ladder.size(SpatialLayer::new(1)), None);
        assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Low);
        assert_eq!(map.quality_of(SpatialLayer::new(1)), VideoQuality::Medium);
    }

    #[test]
    fn a_declaration_that_names_its_rids_survives_a_shorter_offer() {
        let map = LayerMap::build(&ladder(), &rids(&["h", "f"]));
        let sizes = map.ladder();

        assert_eq!(sizes.size(SpatialLayer::BASE), Some((640, 360)));
        assert_eq!(sizes.size(SpatialLayer::new(1)), Some((1280, 720)));
        assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Medium);
        assert_eq!(map.quality_of(SpatialLayer::new(1)), VideoQuality::High);
    }

    #[test]
    fn a_track_without_rids_is_described_by_the_top_layer_declared_for_it() {
        let map = LayerMap::build(&nameless_ladder(), &[]);

        assert_eq!(map.len(), 1);
        assert_eq!(
            map.ladder().size(SpatialLayer::BASE),
            Some((1280, 720)),
            "a single stream carries the full picture, not the smallest declared one"
        );
        assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::High);
    }

    #[test]
    fn rids_nobody_declared_fall_back_to_the_well_known_ladder() {
        let map = LayerMap::build(&[], &rids(&["f", "q"]));

        assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
        assert_eq!(
            map.layer_of(Some(Rid::from("f"))),
            Some(SpatialLayer::new(1)),
            "two rids occupy the two lowest layers whatever they are called"
        );

        let unknown = LayerMap::build(&[], &rids(&["left", "right"]));
        assert_eq!(
            unknown.layer_of(Some(Rid::from("left"))),
            Some(SpatialLayer::BASE)
        );
        assert_eq!(
            unknown.layer_of(Some(Rid::from("right"))),
            Some(SpatialLayer::new(1))
        );
    }

    #[test]
    fn a_ladder_deeper_than_this_server_carries_keeps_its_three_lowest_rids() {
        let map = LayerMap::build(&[], &rids(&["q", "h", "f", "x"]));

        assert_eq!(map.len(), 3, "a fourth rid was folded onto the top layer");
        assert_eq!(map.layer_of(Some(Rid::from("f"))), Some(SpatialLayer::MAX));
        assert_eq!(map.rid_of(SpatialLayer::MAX), Some(Rid::from("f")));
        assert_eq!(
            map.layer_of(Some(Rid::from("x"))),
            None,
            "a rid the ladder does not carry must not be counted as one that it does"
        );
        assert_eq!(map.surplus(), &rids(&["x"])[..]);
        assert_eq!(map.rids().len(), 3);
    }

    #[test]
    fn a_track_without_rids_is_one_layer_and_answers_for_every_packet() {
        let map = LayerMap::build(
            &declared(&[(livekit::VideoQuality::High, "", 1280, 720)]),
            &[],
        );

        assert_eq!(map.len(), 1);
        assert!(!map.is_simulcast());
        assert_eq!(map.layer_of(None), Some(SpatialLayer::BASE));
        assert_eq!(map.layer_of(Some(Rid::from("q"))), Some(SpatialLayer::BASE));
        assert_eq!(map.rid_of(SpatialLayer::BASE), None);
        assert!(map.rids().is_empty());
    }

    #[test]
    fn the_reported_layers_carry_the_ssrc_of_the_stream_that_arrived() {
        let map = LayerMap::build(&ladder(), &rids(&["q", "h", "f"]));
        let mut ssrcs = [None; LAYER_COUNT];
        ssrcs[0] = Some(11);
        ssrcs[1] = Some(22);

        let present = LayerSet::from_iter([SpatialLayer::BASE, SpatialLayer::new(1)]);
        let layers = map.video_layers(present, &ssrcs);

        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].rid, "q");
        assert_eq!(layers[0].ssrc, 11);
        assert_eq!(layers[0].width, 320);
        assert_eq!(layers[0].quality, livekit::VideoQuality::Low as i32);
        assert_eq!(layers[1].ssrc, 22);
        assert_eq!(layers[1].spatial_layer, 1);
    }

    #[test]
    fn the_size_a_subscriber_asks_for_picks_the_layer_that_covers_it() {
        let ladder = LayerMap::build(&ladder(), &rids(&["q", "h", "f"])).ladder();

        assert_eq!(ladder.ceiling_for(320, 180), Some(SpatialLayer::BASE));
        assert_eq!(ladder.ceiling_for(640, 360), Some(SpatialLayer::new(1)));
        assert_eq!(ladder.ceiling_for(1920, 1080), Some(SpatialLayer::MAX));
    }

    #[test]
    fn a_size_request_wins_over_the_quality_field() {
        let settings = livekit::UpdateTrackSettings {
            quality: livekit::VideoQuality::High as i32,
            width: 640,
            height: 360,
            ..Default::default()
        };

        assert_eq!(
            preference_of(&settings),
            LayerPreference::Size {
                width: 640,
                height: 360
            }
        );

        let settings = livekit::UpdateTrackSettings {
            quality: livekit::VideoQuality::Low as i32,
            ..Default::default()
        };
        assert_eq!(
            preference_of(&settings),
            LayerPreference::Quality(VideoQuality::Low)
        );
    }

    #[test]
    fn the_publisher_is_told_about_every_quality_below_the_one_it_must_send() {
        let map = LayerMap::build(&ladder(), &rids(&["q", "h", "f"]));
        let qualities = map.subscribed_qualities(Some(SpatialLayer::new(1)));

        assert_eq!(qualities.len(), 3);
        assert!(qualities[0].enabled);
        assert!(qualities[1].enabled);
        assert!(!qualities[2].enabled);

        let off = map.subscribed_qualities(None);
        assert_eq!(off.len(), 3);
        assert!(off.iter().all(|quality| !quality.enabled));
    }

    #[test]
    fn the_layer_a_quality_asks_for_is_the_layer_that_reports_that_quality() {
        let map = LayerMap::build(
            &declared(&[
                (livekit::VideoQuality::Medium, "", 640, 360),
                (livekit::VideoQuality::High, "", 1280, 720),
            ]),
            &rids(&["q", "h"]),
        );
        let ladder = map.ladder();

        for (asked, layer) in [
            (livekit::VideoQuality::Low, SpatialLayer::BASE),
            (livekit::VideoQuality::Medium, SpatialLayer::BASE),
            (livekit::VideoQuality::High, SpatialLayer::new(1)),
        ] {
            let settings = livekit::UpdateTrackSettings {
                quality: asked as i32,
                ..Default::default()
            };
            let LayerPreference::Quality(quality) = preference_of(&settings) else {
                panic!("a quality request must not turn into a size request");
            };

            assert_eq!(
                ladder.layer_of(quality),
                Some(layer),
                "a subscriber asking for {quality} landed on a layer the publisher never \
                 declared as {quality}"
            );
        }

        assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Medium);
        assert_eq!(map.quality_of(SpatialLayer::new(1)), VideoQuality::High);
    }

    #[test]
    fn the_publisher_keeps_the_encoding_its_subscribers_are_actually_on() {
        let map = LayerMap::build(
            &declared(&[
                (livekit::VideoQuality::Medium, "", 640, 360),
                (livekit::VideoQuality::High, "", 1280, 720),
            ]),
            &rids(&["q", "h"]),
        );

        assert_eq!(map.quality_of(SpatialLayer::BASE), VideoQuality::Medium);

        let qualities = map.subscribed_qualities(Some(SpatialLayer::BASE));

        assert!(qualities[0].enabled);
        assert!(
            qualities[1].enabled,
            "the publisher was told to stop the encoding its subscribers are receiving"
        );
        assert!(!qualities[2].enabled);

        let qualities = map.subscribed_qualities(Some(SpatialLayer::new(1)));
        assert!(qualities[1].enabled);
        assert!(qualities[2].enabled);
    }
}
