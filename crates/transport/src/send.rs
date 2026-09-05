use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::media::{MediaKind, Mid, Pt, Rid};
use str0m::rtp::{ExtensionValues, SeqNo, Ssrc};

pub const AUDIO_RTX_CACHE_PACKETS: usize = 256;
pub const VIDEO_RTX_CACHE_PACKETS: usize = 2048;
pub const RTX_CACHE_AGE: Duration = Duration::from_secs(3);
pub const RTX_RATIO_CAP: Option<f32> = Some(0.15);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtxCache {
    pub packets: usize,
    pub age: Duration,
    pub ratio_cap: Option<f32>,
}

impl RtxCache {
    pub fn for_kind(kind: MediaKind) -> Self {
        let packets = if kind.is_audio() {
            AUDIO_RTX_CACHE_PACKETS
        } else {
            VIDEO_RTX_CACHE_PACKETS
        };

        Self {
            packets,
            age: RTX_CACHE_AGE,
            ratio_cap: RTX_RATIO_CAP,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SendStream {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Option<Ssrc>,
    pub rtx: Option<Ssrc>,
    pub rtx_cache: Option<RtxCache>,
    pub unpaced: Option<bool>,
}

impl SendStream {
    pub fn new(mid: Mid) -> Self {
        Self {
            mid,
            rid: None,
            ssrc: None,
            rtx: None,
            rtx_cache: None,
            unpaced: None,
        }
    }

    pub fn with_rid(mut self, rid: Rid) -> Self {
        self.rid = Some(rid);
        self
    }

    pub fn with_ssrc(mut self, ssrc: Ssrc, rtx: Option<Ssrc>) -> Self {
        self.ssrc = Some(ssrc);
        self.rtx = rtx;
        self
    }

    pub fn with_rtx_cache(mut self, cache: RtxCache) -> Self {
        self.rtx_cache = Some(cache);
        self
    }

    pub fn with_unpaced(mut self, unpaced: bool) -> Self {
        self.unpaced = Some(unpaced);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredStream {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub rtx: Option<Ssrc>,
    pub negotiated: bool,
}

#[derive(Debug)]
pub struct OutgoingRtp {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub pt: Pt,
    pub seq_no: SeqNo,
    pub time: u32,
    pub wallclock: Instant,
    pub marker: bool,
    pub nackable: bool,
    pub ext_vals: ExtensionValues,
    pub payload: Arc<[u8]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rtx_cache_is_sized_per_media_kind() {
        let audio = RtxCache::for_kind(MediaKind::Audio);
        let video = RtxCache::for_kind(MediaKind::Video);

        assert_eq!(audio.packets, AUDIO_RTX_CACHE_PACKETS);
        assert_eq!(video.packets, VIDEO_RTX_CACHE_PACKETS);
        assert!(
            video.packets > audio.packets,
            "video needs a deeper resend window than audio"
        );
        assert_eq!(audio.age, RTX_CACHE_AGE);
        assert_eq!(video.ratio_cap, RTX_RATIO_CAP);
    }
}
