use std::collections::HashMap;
use std::sync::Arc;

use mediacore_proto::livekit::{self, signal_response};
use mediacore_room::Joined;
use mediacore_transport::str0m::format::{Codec, PayloadParams};
use mediacore_transport::str0m::media::MediaKind;
use mediacore_transport::{MediaInfo, PeerEvent, PeerEvents, PeerHandle};
use tokio::task::JoinHandle;

use crate::publisher::PublisherStats;
use crate::state::AppState;

pub struct PublisherSession {
    stats: Arc<PublisherStats>,
    handle: Option<PeerHandle>,
    events: Option<PeerEvents>,
    task: Option<JoinHandle<()>>,
    queued_candidates: Vec<String>,
    cid_by_mid: HashMap<String, String>,
    sid_by_mid: HashMap<String, String>,
}

impl PublisherSession {
    pub fn new(state: &AppState, participant_sid: &str) -> Self {
        Self {
            stats: state.publishers.register(participant_sid),
            handle: None,
            events: None,
            task: None,
            queued_candidates: Vec::new(),
            cid_by_mid: HashMap::new(),
            sid_by_mid: HashMap::new(),
        }
    }

    pub async fn next_event(&mut self) -> Option<PeerEvent> {
        match self.events.as_mut() {
            Some(events) => events.recv().await,
            None => std::future::pending().await,
        }
    }

    pub async fn on_offer(
        &mut self,
        state: &AppState,
        joined: &Joined,
        offer: livekit::SessionDescription,
    ) {
        let handle = match self.ensure_peer(state, joined).await {
            Some(handle) => handle,
            None => return,
        };

        let answer = match handle.accept_offer(offer.sdp.clone()).await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(
                    identity = %joined.participant.identity,
                    %error,
                    "cannot answer the publisher offer"
                );
                return;
            }
        };

        let bundle = bundle_mid(&answer);

        joined.participant.send(signal_response::Message::Answer(
            livekit::SessionDescription {
                r#type: "answer".to_owned(),
                sdp: answer,
                ..Default::default()
            },
        ));

        self.send_local_candidates(state, joined, &bundle);
        self.bind_offered_tracks(joined, &offer);
    }

    pub async fn on_trickle(&mut self, joined: &Joined, request: livekit::TrickleRequest) {
        if request.target != livekit::SignalTarget::Publisher as i32 {
            tracing::debug!(
                identity = %joined.participant.identity,
                target = request.target,
                "ignoring a candidate for a transport that does not exist yet"
            );
            return;
        }

        let Some(candidate) = candidate_from_init(&request.candidate_init) else {
            return;
        };

        let Some(handle) = self.handle.clone() else {
            tracing::debug!(
                identity = %joined.participant.identity,
                "holding a candidate until the publisher offer arrives"
            );
            self.queued_candidates.push(candidate);
            return;
        };

        if let Err(error) = handle.add_remote_candidate(candidate).await {
            tracing::debug!(
                identity = %joined.participant.identity,
                %error,
                "rejected a remote candidate"
            );
        }
    }

    pub fn on_event(&mut self, joined: &Joined, event: PeerEvent) -> bool {
        match event {
            PeerEvent::Connected => {
                self.stats.mark_connected();
                tracing::info!(
                    identity = %joined.participant.identity,
                    "publisher transport connected"
                );
            }
            PeerEvent::IceConnectionState(state) => {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    ?state,
                    "publisher ice state"
                );
            }
            PeerEvent::MediaAdded(info) => self.on_media_added(joined, info),
            PeerEvent::Rtp(rtp) => {
                let track_sid = self.sid_by_mid.get(&*rtp.mid).map(String::as_str);
                self.stats.count_rtp(
                    track_sid,
                    rtp.packet.payload.len(),
                    rtp.carried_mid_extension,
                );
            }
            PeerEvent::Media(_) | PeerEvent::KeyframeRequested(_) => {}
            PeerEvent::Disconnected => {
                self.stats.mark_disconnected();
                tracing::info!(
                    identity = %joined.participant.identity,
                    "publisher transport disconnected"
                );
                return false;
            }
        }

        true
    }

    pub fn close(&mut self, state: &AppState, participant_sid: &str) {
        if let Some(handle) = self.handle.take() {
            handle.close();
        }
        self.events = None;
        self.task = None;
        state.publishers.remove(participant_sid);
    }

    async fn ensure_peer(&mut self, state: &AppState, joined: &Joined) -> Option<PeerHandle> {
        if let Some(handle) = self.handle.clone() {
            return Some(handle);
        }

        let peer = match state.media.spawn_publisher() {
            Ok(peer) => peer,
            Err(error) => {
                tracing::error!(
                    identity = %joined.participant.identity,
                    %error,
                    "cannot start the publisher transport"
                );
                joined
                    .participant
                    .disconnect(livekit::DisconnectReason::UnknownReason);
                return None;
            }
        };

        tracing::info!(
            identity = %joined.participant.identity,
            id = %peer.handle.id(),
            ufrag = %peer.handle.ice_creds().ufrag,
            "publisher transport started"
        );

        self.handle = Some(peer.handle.clone());
        self.events = Some(peer.events);
        self.task = Some(peer.task);

        for candidate in std::mem::take(&mut self.queued_candidates) {
            if let Err(error) = peer.handle.add_remote_candidate(candidate).await {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    %error,
                    "rejected a queued remote candidate"
                );
            }
        }

        Some(peer.handle)
    }

    fn send_local_candidates(&self, state: &AppState, joined: &Joined, bundle_mid: &str) {
        let Some((last, rest)) = state.media.candidates().split_last() else {
            tracing::warn!(
                identity = %joined.participant.identity,
                "no local ice candidate to trickle"
            );
            return;
        };

        for candidate in rest {
            joined
                .participant
                .send(trickle(candidate_init(candidate, bundle_mid), false));
        }

        joined
            .participant
            .send(trickle(candidate_init(last, bundle_mid), true));
    }

    fn bind_offered_tracks(&mut self, joined: &Joined, offer: &livekit::SessionDescription) {
        let mut bound = false;

        for (mid, cid) in offered_tracks(offer) {
            self.cid_by_mid.insert(mid.clone(), cid.clone());

            let Some(track) = joined.participant.track_by_cid(&cid) else {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    %mid,
                    %cid,
                    "the offer carries media for a track that was never added"
                );
                continue;
            };

            self.sid_by_mid.insert(mid.clone(), track.sid.clone());

            if track.mid.as_deref() == Some(mid.as_str()) {
                continue;
            }

            if joined.participant.set_track_mid(&track.sid, &mid).is_some() {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    track = %track.sid,
                    %mid,
                    "bound a published track to its media section"
                );
                self.stats.count_bound_track();
                bound = true;
            }
        }

        if bound {
            announce(joined);
        }
    }

    fn on_media_added(&mut self, joined: &Joined, info: MediaInfo) {
        let mid = info.mid.to_string();

        let Some(cid) = self.cid_by_mid.get(&mid) else {
            tracing::warn!(
                identity = %joined.participant.identity,
                %mid,
                "media arrived on a section no track was offered on"
            );
            return;
        };

        let Some(track) = joined.participant.track_by_cid(cid) else {
            tracing::warn!(
                identity = %joined.participant.identity,
                %mid,
                %cid,
                "media arrived for a track that is no longer published"
            );
            return;
        };

        let Some(mime_type) = mime_type(info.kind, &info.params) else {
            tracing::warn!(
                identity = %joined.participant.identity,
                %mid,
                "media was negotiated without a usable codec"
            );
            return;
        };

        joined
            .participant
            .set_track_mime_type(&track.sid, &mime_type);
        self.sid_by_mid.insert(mid.clone(), track.sid.clone());

        tracing::info!(
            identity = %joined.participant.identity,
            track = %track.sid,
            %mid,
            %mime_type,
            rids = info.rids.len(),
            "publisher media is live"
        );

        announce(joined);
    }
}

fn announce(joined: &Joined) {
    if joined.participant.permission.hidden {
        return;
    }

    let info = joined
        .participant
        .to_proto(livekit::participant_info::State::Active);

    joined.room.broadcast(
        mediacore_room::participant_update(vec![info]),
        Some(&joined.participant.sid),
    );
}

fn trickle(candidate_init: String, final_candidate: bool) -> signal_response::Message {
    signal_response::Message::Trickle(livekit::TrickleRequest {
        candidate_init,
        target: livekit::SignalTarget::Publisher as i32,
        r#final: final_candidate,
    })
}

fn candidate_init(candidate: &str, mid: &str) -> String {
    serde_json::json!({
        "candidate": candidate,
        "sdpMid": mid,
        "sdpMLineIndex": 0,
    })
    .to_string()
}

pub fn bundle_mid(sdp: &str) -> String {
    sdp.lines()
        .find_map(|line| line.trim().strip_prefix("a=mid:"))
        .map(|mid| mid.trim().to_owned())
        .unwrap_or_else(|| "0".to_owned())
}

fn candidate_from_init(candidate_init: &str) -> Option<String> {
    if candidate_init.trim().is_empty() {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct CandidateInit {
        #[serde(default)]
        candidate: String,
    }

    let parsed = match serde_json::from_str::<CandidateInit>(candidate_init) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::debug!(%error, "undecodable trickle candidate");
            return None;
        }
    };

    (!parsed.candidate.trim().is_empty()).then_some(parsed.candidate)
}

pub fn offered_tracks(offer: &livekit::SessionDescription) -> Vec<(String, String)> {
    if !offer.mid_to_track_id.is_empty() {
        let mut pairs: Vec<(String, String)> = offer
            .mid_to_track_id
            .iter()
            .map(|(mid, cid)| (mid.clone(), cid.clone()))
            .collect();
        pairs.sort();
        return pairs;
    }

    media_sections(&offer.sdp)
}

fn media_sections(sdp: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut mid = None;
    let mut cid = None;

    for line in sdp.lines() {
        let line = line.trim();

        if line.starts_with("m=") {
            push_section(&mut sections, mid.take(), cid.take());
        } else if let Some(value) = line.strip_prefix("a=mid:") {
            mid = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("a=msid:") {
            cid = track_id_from_msid(value);
        } else if let Some(value) = line.strip_prefix("a=ssrc:")
            && cid.is_none()
        {
            cid = value
                .split_once(" msid:")
                .and_then(|(_, msid)| track_id_from_msid(msid));
        }
    }

    push_section(&mut sections, mid, cid);
    sections
}

fn push_section(sections: &mut Vec<(String, String)>, mid: Option<String>, cid: Option<String>) {
    if let (Some(mid), Some(cid)) = (mid, cid) {
        sections.push((mid, cid));
    }
}

fn track_id_from_msid(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let _stream_id = parts.next()?;

    parts.next().map(str::to_owned)
}

fn mime_type(kind: MediaKind, params: &[PayloadParams]) -> Option<String> {
    let prefix = match kind {
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
    };

    params
        .iter()
        .map(|params| params.spec().codec)
        .find(|codec| !matches!(codec, Codec::Rtx | Codec::Null | Codec::Unknown))
        .map(|codec| format!("{prefix}/{codec}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use mediacore_transport::str0m::format::{CodecSpec, FormatParams};
    use mediacore_transport::str0m::media::Frequency;

    const OFFER: &str = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:aBc\r\n\
a=sendonly\r\n\
a=msid:stream-1 cid-audio\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:dEf\r\n\
a=sendonly\r\n\
a=ssrc:42 cname:x\r\n\
a=ssrc:42 msid:stream-1 cid-video\r\n";

    fn offer(sdp: &str) -> livekit::SessionDescription {
        livekit::SessionDescription {
            r#type: "offer".to_owned(),
            sdp: sdp.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn tracks_are_read_from_the_msid_of_every_media_section() {
        assert_eq!(
            offered_tracks(&offer(OFFER)),
            vec![
                ("aBc".to_owned(), "cid-audio".to_owned()),
                ("dEf".to_owned(), "cid-video".to_owned()),
            ]
        );
    }

    #[test]
    fn a_declared_mid_to_track_map_wins_over_the_sdp() {
        let mut description = offer(OFFER);
        description
            .mid_to_track_id
            .insert("aBc".to_owned(), "declared".to_owned());

        assert_eq!(
            offered_tracks(&description),
            vec![("aBc".to_owned(), "declared".to_owned())]
        );
    }

    #[test]
    fn a_media_section_without_a_track_is_skipped() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";

        assert!(offered_tracks(&offer(sdp)).is_empty());
    }

    #[test]
    fn a_candidate_is_unwrapped_from_its_json_envelope() {
        let init = candidate_init("candidate:1 1 udp 100 127.0.0.1 40000 typ host", "aBc");

        assert_eq!(
            candidate_from_init(&init).as_deref(),
            Some("candidate:1 1 udp 100 127.0.0.1 40000 typ host")
        );
        assert_eq!(candidate_from_init(""), None);
        assert_eq!(candidate_from_init("{\"candidate\":\"\"}"), None);
        assert_eq!(candidate_from_init("not json"), None);
    }

    #[test]
    fn a_trickled_candidate_names_the_bundled_media_section() {
        let init = candidate_init("candidate:1 1 udp 100 127.0.0.1 40000 typ host", "aBc");
        let parsed: serde_json::Value = serde_json::from_str(&init).expect("the envelope is json");

        assert_eq!(parsed["sdpMid"], serde_json::json!("aBc"));
        assert_eq!(parsed["sdpMLineIndex"], serde_json::json!(0));
        assert_eq!(
            parsed["candidate"],
            serde_json::json!("candidate:1 1 udp 100 127.0.0.1 40000 typ host")
        );
    }

    #[test]
    fn the_bundle_mid_is_the_first_media_section_of_the_answer() {
        assert_eq!(bundle_mid(OFFER), "aBc");
        assert_eq!(bundle_mid("v=0\r\nm=audio 9 RTP/AVP 0\r\n"), "0");
    }

    fn params(pt: u8, codec: Codec, clock_rate: Frequency, channels: Option<u8>) -> PayloadParams {
        PayloadParams::new(
            pt.into(),
            None,
            CodecSpec {
                codec,
                clock_rate,
                channels,
                format: FormatParams::default(),
            },
        )
    }

    #[test]
    fn the_mime_type_follows_the_negotiated_codec() {
        let opus = params(111, Codec::Opus, Frequency::FORTY_EIGHT_KHZ, Some(2));
        let vp8 = params(96, Codec::Vp8, Frequency::NINETY_KHZ, None);

        assert_eq!(
            mime_type(MediaKind::Audio, &[opus]).as_deref(),
            Some("audio/opus")
        );
        assert_eq!(
            mime_type(MediaKind::Video, &[vp8]).as_deref(),
            Some("video/VP8")
        );
        assert_eq!(mime_type(MediaKind::Video, &[]), None);
    }
}
