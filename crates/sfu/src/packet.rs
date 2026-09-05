use std::sync::Arc;
use std::time::Instant;

use str0m::rtp::{ExtensionValues, SeqNo};

use crate::layer::SpatialLayer;

#[derive(Debug, Clone)]
pub struct InboundPacket {
    pub ssrc: u32,
    pub layer: SpatialLayer,
    pub seq: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub keyframe: bool,
    pub payload: Arc<[u8]>,
    pub ext: ExtensionValues,
    pub arrival: Instant,
}

impl InboundPacket {
    pub fn new(
        ssrc: u32,
        seq: u16,
        timestamp: u32,
        payload: impl Into<Arc<[u8]>>,
        arrival: Instant,
    ) -> Self {
        Self {
            ssrc,
            layer: SpatialLayer::BASE,
            seq,
            timestamp,
            marker: false,
            keyframe: false,
            payload: payload.into(),
            ext: ExtensionValues::default(),
            arrival,
        }
    }

    pub fn with_layer(mut self, layer: SpatialLayer) -> Self {
        self.layer = layer;
        self
    }

    pub fn with_marker(mut self, marker: bool) -> Self {
        self.marker = marker;
        self
    }

    pub fn with_keyframe(mut self, keyframe: bool) -> Self {
        self.keyframe = keyframe;
        self
    }

    pub fn with_ext(mut self, ext: ExtensionValues) -> Self {
        self.ext = ext;
        self
    }

    pub fn is_padding(&self) -> bool {
        self.payload.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ForwardedPacket {
    pub seq: SeqNo,
    pub timestamp: u32,
    pub ext_timestamp: u64,
    pub marker: bool,
    pub keyframe: bool,
    pub nackable: bool,
    pub switched: bool,
    pub payload: Arc<[u8]>,
    pub ext: ExtensionValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    PublisherMuted,
    SubscriberPaused,
    AwaitingKeyframe,
    WrongLayer,
    Duplicate,
    PaddingOnly,
    TooOld,
}

#[derive(Debug, Clone)]
pub enum Forwarding {
    Send(Box<ForwardedPacket>),
    Drop(DropReason),
}

impl Forwarding {
    pub fn sent(&self) -> Option<&ForwardedPacket> {
        match self {
            Self::Send(packet) => Some(packet),
            Self::Drop(_) => None,
        }
    }

    pub fn dropped(&self) -> Option<DropReason> {
        match self {
            Self::Send(_) => None,
            Self::Drop(reason) => Some(*reason),
        }
    }

    pub fn is_sent(&self) -> bool {
        matches!(self, Self::Send(_))
    }
}

pub(crate) fn subscriber_extensions(ext: &ExtensionValues) -> ExtensionValues {
    let mut ext = ext.clone();

    ext.mid = None;
    ext.rid = None;
    ext.rid_repair = None;
    ext.transport_cc = None;
    ext.abs_send_time = None;
    ext.tx_time_offs = None;

    ext
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::media::{Mid, Rid};

    #[test]
    fn publisher_routing_extensions_never_reach_a_subscriber() {
        let mut ext = ExtensionValues {
            audio_level: Some(-30),
            voice_activity: Some(true),
            ..Default::default()
        };
        ext.mid = Some(Mid::from("2"));
        ext.rid = Some(Rid::from("h"));
        ext.rid_repair = Some(Rid::from("h"));
        ext.transport_cc = Some(17);
        ext.tx_time_offs = Some(5);

        let out = subscriber_extensions(&ext);

        assert_eq!(out.mid, None);
        assert_eq!(out.rid, None);
        assert_eq!(out.rid_repair, None);
        assert_eq!(out.transport_cc, None);
        assert_eq!(out.abs_send_time, None);
        assert_eq!(out.tx_time_offs, None);
        assert_eq!(out.audio_level, Some(-30));
        assert_eq!(out.voice_activity, Some(true));
    }
}
