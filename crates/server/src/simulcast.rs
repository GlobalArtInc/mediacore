use mediacore_proto::mediacore::{self, signal_response};
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
    pub fn build(declared: &[mediacore::VideoLayer], rids: &[Rid]) -> Self {
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

    fn single(declared: &[mediacore::VideoLayer]) -> Self {
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
                ladder
                    .with_quality(entry.layer, entry.quality)
                    .with_size(entry.layer, entry.width, entry.height)
                    .with_bitrate(entry.layer, entry.bitrate)
            })
    }

    pub fn subscribed_qualities(
        &self,
        highest: Option<SpatialLayer>,
    ) -> Vec<mediacore::SubscribedQuality> {
        let ceiling = highest
            .map(|layer| self.quality_of(layer))
            .and_then(VideoQuality::rank);

        [VideoQuality::Low, VideoQuality::Medium, VideoQuality::High]
            .into_iter()
            .map(|quality| mediacore::SubscribedQuality {
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
    ) -> Vec<mediacore::VideoLayer> {
        self.entries
            .iter()
            .filter(|entry| present.contains(entry.layer))
            .map(|entry| mediacore::VideoLayer {
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

fn ascending(declared: &[mediacore::VideoLayer]) -> Vec<&mediacore::VideoLayer> {
    let mut ladder: Vec<&mediacore::VideoLayer> = declared.iter().collect();
    ladder.sort_by_key(|layer| quality_rank(layer.quality));
    ladder
}

fn named<'a>(rid: Rid, ladder: &[&'a mediacore::VideoLayer]) -> Option<&'a mediacore::VideoLayer> {
    let name = rid.to_string();

    ladder
        .iter()
        .copied()
        .find(|layer| !layer.rid.is_empty() && layer.rid == name)
}

fn ordered_rids(rids: &[Rid], ladder: &[&mediacore::VideoLayer]) -> Vec<Rid> {
    let mut ordered: Vec<(usize, usize, Rid)> = rids
        .iter()
        .enumerate()
        .map(|(index, rid)| (rank(*rid, index, ladder), index, *rid))
        .collect();

    ordered.sort_by_key(|(rank, index, _)| (*rank, *index));
    ordered.into_iter().map(|(_, _, rid)| rid).collect()
}

fn rank(rid: Rid, index: usize, ladder: &[&mediacore::VideoLayer]) -> usize {
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
    match mediacore::VideoQuality::try_from(quality).unwrap_or(mediacore::VideoQuality::High) {
        mediacore::VideoQuality::Low => VideoQuality::Low,
        mediacore::VideoQuality::Medium => VideoQuality::Medium,
        mediacore::VideoQuality::High => VideoQuality::High,
        mediacore::VideoQuality::Off => VideoQuality::Off,
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

fn proto_quality(quality: VideoQuality) -> mediacore::VideoQuality {
    match quality {
        VideoQuality::Low => mediacore::VideoQuality::Low,
        VideoQuality::Medium => mediacore::VideoQuality::Medium,
        VideoQuality::High => mediacore::VideoQuality::High,
        VideoQuality::Off => mediacore::VideoQuality::Off,
    }
}

pub fn preference_of(settings: &mediacore::UpdateTrackSettings) -> LayerPreference {
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
    qualities: Vec<mediacore::SubscribedQuality>,
) -> signal_response::Message {
    signal_response::Message::SubscribedQualityUpdate(mediacore::SubscribedQualityUpdate {
        track_sid: track_sid.to_owned(),
        subscribed_qualities: qualities.clone(),
        subscribed_codecs: vec![mediacore::SubscribedCodec {
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
#[path = "simulcast_tests.rs"]
mod tests;
