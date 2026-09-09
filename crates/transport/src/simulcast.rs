use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::format::Codec;
use str0m::media::{KeyframeRequestKind, Mid, Rid, Rids};
use str0m::rtp::Ssrc;
use str0m::stats::MediaIngressStats;

pub const LAYER_SILENCE_THRESHOLD: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LayerSet {
    #[default]
    None,
    Any,
    Specific(Vec<Rid>),
}

impl LayerSet {
    pub fn rids(&self) -> &[Rid] {
        match self {
            Self::Specific(rids) => rids,
            Self::None | Self::Any => &[],
        }
    }

    pub fn contains(&self, rid: Rid) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Specific(rids) => rids.contains(&rid),
        }
    }

    pub fn is_specific(&self) -> bool {
        matches!(self, Self::Specific(_))
    }

    pub fn len(&self) -> usize {
        self.rids().len()
    }

    pub fn is_empty(&self) -> bool {
        self.rids().is_empty()
    }
}

impl From<&Rids> for LayerSet {
    fn from(rids: &Rids) -> Self {
        match rids {
            Rids::None => Self::None,
            Rids::Any => Self::Any,
            Rids::Specific(rids) => Self::Specific(rids.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeTarget {
    Layer(Option<Rid>),
    EveryLayer,
}

type Probe = dyn Fn(Codec, &[u8]) -> bool + Send + Sync;

#[derive(Clone)]
pub struct KeyframeProbe(Arc<Probe>);

impl KeyframeProbe {
    pub fn new<F>(probe: F) -> Self
    where
        F: Fn(Codec, &[u8]) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(probe))
    }

    pub fn probe(&self, codec: Codec, payload: &[u8]) -> bool {
        (self.0)(codec, payload)
    }
}

impl fmt::Debug for KeyframeProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyframeProbe")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerActivity {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IngressReport {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub packets: u64,
    pub bytes: u64,
    pub plis: u64,
    pub firs: u64,
    pub nacks: u64,
    pub jitter: u32,
    pub rtt: Option<Duration>,
    pub loss: Option<f32>,
    pub at: Instant,
}

impl From<MediaIngressStats> for IngressReport {
    fn from(stats: MediaIngressStats) -> Self {
        Self {
            mid: stats.mid,
            rid: stats.rid,
            packets: stats.packets,
            bytes: stats.bytes,
            plis: stats.plis,
            firs: stats.firs,
            nacks: stats.nacks,
            jitter: stats.jitter,
            rtt: stats.rtt,
            loss: stats.loss,
            at: stats.timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerStats {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Option<Ssrc>,
    pub packets: u64,
    pub bytes: u64,
    pub keyframes: u64,
    pub plis: u64,
    pub firs: u64,
    pub nacks: u64,
    pub jitter: Option<u32>,
    pub rtt: Option<Duration>,
    pub loss: Option<f32>,
    pub first_arrival: Option<Instant>,
    pub last_arrival: Option<Instant>,
    pub last_keyframe: Option<Instant>,
    pub silent_since: Option<Instant>,
    pub active: bool,
    pub wanted: bool,
    pub nack_suppressed: bool,
}

impl LayerStats {
    fn new(mid: Mid, rid: Option<Rid>) -> Self {
        Self {
            mid,
            rid,
            ssrc: None,
            packets: 0,
            bytes: 0,
            keyframes: 0,
            plis: 0,
            firs: 0,
            nacks: 0,
            jitter: None,
            rtt: None,
            loss: None,
            first_arrival: None,
            last_arrival: None,
            last_keyframe: None,
            silent_since: None,
            active: false,
            wanted: true,
            nack_suppressed: false,
        }
    }

    pub fn seen(&self) -> bool {
        self.packets > 0
    }

    pub fn idle_for(&self, now: Instant) -> Option<Duration> {
        self.last_arrival
            .map(|arrival| now.saturating_duration_since(arrival))
    }
}

#[derive(Debug, Default)]
pub(crate) struct LayerTable {
    mids: HashMap<Mid, Vec<LayerStats>>,
}

impl LayerTable {
    fn slot(&mut self, mid: Mid, rid: Option<Rid>) -> &mut LayerStats {
        let layers = self.mids.entry(mid).or_default();

        let index = match layers.iter().position(|layer| layer.rid == rid) {
            Some(index) => index,
            None => {
                layers.push(LayerStats::new(mid, rid));
                layers.len() - 1
            }
        };

        &mut layers[index]
    }

    fn find(&self, mid: Mid, rid: Option<Rid>) -> Option<&LayerStats> {
        self.mids.get(&mid)?.iter().find(|layer| layer.rid == rid)
    }

    pub fn observe(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: Ssrc,
        bytes: u64,
        keyframe: bool,
        now: Instant,
    ) -> bool {
        let layer = self.slot(mid, rid);
        let fresh = layer.ssrc != Some(ssrc);

        layer.ssrc = Some(ssrc);
        layer.packets += 1;
        layer.bytes += bytes;
        layer.last_arrival = Some(now);
        layer.first_arrival.get_or_insert(now);
        layer.active = true;
        layer.silent_since = None;

        if keyframe {
            layer.keyframes += 1;
            layer.last_keyframe = Some(now);
        }

        fresh
    }

    pub fn note_keyframe_request(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        let layer = self.slot(mid, rid);

        match kind {
            KeyframeRequestKind::Pli => layer.plis += 1,
            KeyframeRequestKind::Fir => layer.firs += 1,
        }
    }

    pub fn note_activity(&mut self, activity: LayerActivity, now: Instant) {
        let layer = self.slot(activity.mid, activity.rid);

        layer.ssrc = Some(activity.ssrc);
        layer.active = activity.active;
        layer.silent_since = if activity.active { None } else { Some(now) };
    }

    pub fn merge_ingress(&mut self, report: &IngressReport) {
        let layer = self.slot(report.mid, report.rid);

        layer.nacks = report.nacks;
        layer.jitter = Some(report.jitter);
        layer.rtt = report.rtt;
        layer.loss = report.loss;
    }

    pub fn set_wanted(&mut self, mid: Mid, rid: Option<Rid>, wanted: bool) -> bool {
        let layer = self.slot(mid, rid);
        let changed = layer.wanted != wanted;

        layer.wanted = wanted;

        changed
    }

    pub fn note_nack_suppressed(&mut self, mid: Mid, rid: Option<Rid>, suppressed: bool) {
        self.slot(mid, rid).nack_suppressed = suppressed;
    }

    pub fn wanted(&self, mid: Mid, rid: Option<Rid>) -> bool {
        self.find(mid, rid)
            .map(|layer| layer.wanted)
            .unwrap_or(true)
    }

    pub fn snapshot(&self, mid: Mid) -> Vec<LayerStats> {
        self.mids.get(&mid).cloned().unwrap_or_default()
    }

    pub fn forget(&mut self, mid: Mid) {
        self.mids.remove(&mid);
    }
}

#[cfg(test)]
#[path = "simulcast_tests.rs"]
mod tests;
