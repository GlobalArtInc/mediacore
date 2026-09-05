use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use mediacore_proto::livekit::{self, signal_request};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::config::DtlsCert;
use str0m::crypto::CryptoProvider;
use str0m::media::{Direction, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpWrite, SeqNo};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::error::ClientError;
use crate::media::{Codec, MediaProfile, SyntheticSource};
use crate::signal::SignalHandle;
use crate::udp::{self, Datagram};

const MAX_CATCH_UP_PACKETS: u32 = 4;

pub(crate) enum Command {
    Publish {
        kind: MediaKind,
        stream_id: String,
        track_id: String,
        profile: MediaProfile,
        reply: oneshot::Sender<Result<Mid, ClientError>>,
    },
    Unpublish {
        mid: Mid,
        reply: oneshot::Sender<Result<(), ClientError>>,
    },
    Answer(String),
    RemoteCandidate(String),
    Start(Mid),
    Stop(Mid),
    Connected(oneshot::Sender<()>),
    Stats(Mid, oneshot::Sender<Option<TrackStats>>),
    Close(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrackStats {
    pub packets: u64,
    pub payload_bytes: u64,
    pub sending: bool,
}

struct Pending {
    offer: SdpPendingOffer,
    awaiting: Awaiting,
}

enum Awaiting {
    Publish {
        mid: Mid,
        profile: MediaProfile,
        reply: oneshot::Sender<Result<Mid, ClientError>>,
    },
    Unpublish {
        mid: Mid,
        reply: oneshot::Sender<Result<(), ClientError>>,
    },
}

impl Awaiting {
    fn fail(self, error: ClientError) {
        match self {
            Self::Publish { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Unpublish { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

enum Queued {
    Publish {
        kind: MediaKind,
        stream_id: String,
        track_id: String,
        profile: MediaProfile,
        reply: oneshot::Sender<Result<Mid, ClientError>>,
    },
    Unpublish {
        mid: Mid,
        reply: oneshot::Sender<Result<(), ClientError>>,
    },
}

impl Queued {
    fn fail(self, error: ClientError) {
        match self {
            Self::Publish { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Unpublish { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

struct TrackSender {
    mid: Mid,
    pt: Pt,
    source: SyntheticSource,
    interval: Duration,
    next: Instant,
    seq: u64,
    ticks: u32,
    packets: u64,
    payload_bytes: u64,
    active: bool,
}

pub(crate) struct Transport {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    candidate: Candidate,
    signal: SignalHandle,
    commands: mpsc::Receiver<Command>,
    datagrams: mpsc::Receiver<Datagram>,
    senders: Vec<TrackSender>,
    pending: Option<Pending>,
    queue: VecDeque<Queued>,
    connected: bool,
    connect_waiters: Vec<oneshot::Sender<()>>,
}

pub(crate) async fn start(
    bind: IpAddr,
    commands: mpsc::Receiver<Command>,
    signal: SignalHandle,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), ClientError> {
    let endpoint = udp::bind(bind).await?;
    let local = endpoint.local;

    let mut rtc = publisher_rtc()?;

    let candidate = Candidate::host(local, "udp")
        .map_err(|error| ClientError::HostCandidate(local, error.to_string()))?;

    if rtc.add_local_candidate(candidate.clone()).is_none() {
        tracing::warn!(%local, "str0m rejected our host candidate");
    }

    let transport = Transport {
        rtc,
        socket: endpoint.socket,
        local,
        candidate,
        signal,
        commands,
        datagrams: endpoint.datagrams,
        senders: Vec::new(),
        pending: None,
        queue: VecDeque::new(),
        connected: false,
        connect_waiters: Vec::new(),
    };

    Ok((local, tokio::spawn(transport.run())))
}

pub(crate) fn publisher_rtc() -> Result<Rtc, ClientError> {
    Ok(Rtc::builder()
        .set_ice_lite(false)
        .set_rtp_mode(true)
        .set_dtls_cert(dtls_cert()?)
        .clear_codecs()
        .enable_opus(true)
        .enable_vp8(true)
        .build(Instant::now()))
}

pub(crate) fn dtls_cert() -> Result<DtlsCert, ClientError> {
    static INSTALLED: OnceLock<()> = OnceLock::new();

    INSTALLED.get_or_init(|| {
        if CryptoProvider::get_default().is_none() {
            str0m::crypto::from_feature_flags().install_process_default();
        }
    });

    CryptoProvider::get_default()
        .and_then(|provider| provider.dtls_provider.generate_certificate())
        .ok_or(ClientError::NoDtlsCertificate)
}

pub(crate) fn negotiated_pt(rtc: &Rtc, mid: Mid, profile: MediaProfile) -> Result<Pt, ClientError> {
    let missing = || ClientError::CodecNotNegotiated {
        codec: profile.codec.name(),
        mid: mid.to_string(),
    };

    let wanted = profile.codec.str0m();
    let media = rtc.media(mid).ok_or_else(missing)?;

    rtc.codec_config()
        .params()
        .iter()
        .find(|params| params.spec().codec == wanted && media.remote_pts().contains(&params.pt()))
        .map(|params| params.pt())
        .ok_or_else(missing)
}

impl Transport {
    async fn run(mut self) {
        loop {
            let Some(deadline) = self.poll().await else {
                break;
            };

            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            tokio::pin!(sleep);

            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    let Some(command) = command else { break };
                    if !self.on_command(command) {
                        break;
                    }
                }
                datagram = self.datagrams.recv() => {
                    let Some((at, source, data)) = datagram else { break };
                    self.on_datagram(at, source, &data);
                }
                _ = &mut sleep => {
                    let now = Instant::now();
                    self.write_due_media(now);
                    if let Err(error) = self.rtc.handle_input(Input::Timeout(now)) {
                        tracing::debug!(%error, "timeout input failed");
                        self.rtc.disconnect();
                    }
                }
            }
        }

        self.fail_waiters();
    }

    async fn poll(&mut self) -> Option<Instant> {
        loop {
            if !self.rtc.is_alive() {
                return None;
            }

            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => {
                    let at = match self.media_deadline() {
                        Some(media) => at.min(media),
                        None => at,
                    };
                    return Some(at.max(Instant::now()));
                }
                Ok(Output::Transmit(transmit)) => {
                    let sent = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await;
                    if let Err(error) = sent {
                        tracing::debug!(
                            %error,
                            destination = %transmit.destination,
                            "udp send failed"
                        );
                    }
                }
                Ok(Output::Event(event)) => self.on_event(event),
                Err(error) => {
                    tracing::warn!(%error, "publisher transport failed");
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    fn media_deadline(&self) -> Option<Instant> {
        if !self.connected {
            return None;
        }

        self.senders
            .iter()
            .filter(|sender| sender.active)
            .map(|sender| sender.next)
            .min()
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected => {
                tracing::debug!(local = %self.local, "publisher transport connected");
                self.connected = true;
                let now = Instant::now();
                for sender in &mut self.senders {
                    sender.next = now;
                }
                for waiter in self.connect_waiters.drain(..) {
                    let _ = waiter.send(());
                }
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                tracing::debug!("ice connection went to disconnected");
                self.rtc.disconnect();
            }
            Event::KeyframeRequest(request) => {
                let sender = self
                    .senders
                    .iter_mut()
                    .find(|sender| sender.mid == request.mid);
                if let Some(sender) = sender {
                    sender.source.request_keyframe();
                }
            }
            Event::Closed => self.rtc.disconnect(),
            _ => {}
        }
    }

    fn on_datagram(&mut self, at: Instant, source: SocketAddr, data: &[u8]) {
        match Receive::new(Protocol::Udp, source, self.local, data) {
            Ok(receive) => {
                if let Err(error) = self.rtc.handle_input(Input::Receive(at, receive)) {
                    tracing::debug!(%error, "receive input failed");
                }
            }
            Err(error) => tracing::trace!(%error, "undecodable datagram"),
        }
    }

    fn on_command(&mut self, command: Command) -> bool {
        match command {
            Command::Publish {
                kind,
                stream_id,
                track_id,
                profile,
                reply,
            } => {
                self.queue.push_back(Queued::Publish {
                    kind,
                    stream_id,
                    track_id,
                    profile,
                    reply,
                });
                self.negotiate();
            }
            Command::Unpublish { mid, reply } => {
                self.queue.push_back(Queued::Unpublish { mid, reply });
                self.negotiate();
            }
            Command::Answer(sdp) => self.on_answer(&sdp),
            Command::RemoteCandidate(init) => self.on_remote_candidate(&init),
            Command::Start(mid) => {
                let now = Instant::now();
                if let Some(sender) = self.sender_mut(mid) {
                    sender.active = true;
                    sender.next = now;
                }
            }
            Command::Stop(mid) => {
                if let Some(sender) = self.sender_mut(mid) {
                    sender.active = false;
                }
            }
            Command::Connected(reply) => {
                if self.connected {
                    let _ = reply.send(());
                } else {
                    self.connect_waiters.push(reply);
                }
            }
            Command::Stats(mid, reply) => {
                let stats = self
                    .senders
                    .iter()
                    .find(|s| s.mid == mid)
                    .map(|sender| TrackStats {
                        packets: sender.packets,
                        payload_bytes: sender.payload_bytes,
                        sending: sender.active,
                    });
                let _ = reply.send(stats);
            }
            Command::Close(reply) => {
                let _ = self.rtc.close();
                let _ = reply.send(());
                return false;
            }
        }

        true
    }

    fn sender_mut(&mut self, mid: Mid) -> Option<&mut TrackSender> {
        self.senders.iter_mut().find(|sender| sender.mid == mid)
    }

    fn negotiate(&mut self) {
        if self.pending.is_some() {
            return;
        }

        let Some(queued) = self.queue.pop_front() else {
            return;
        };

        let mut api = self.rtc.sdp_api();

        let awaiting = match queued {
            Queued::Publish {
                kind,
                stream_id,
                track_id,
                profile,
                reply,
            } => {
                let mid = api.add_media(
                    kind,
                    Direction::SendOnly,
                    Some(stream_id),
                    Some(track_id),
                    None,
                );

                Awaiting::Publish {
                    mid,
                    profile,
                    reply,
                }
            }
            Queued::Unpublish { mid, reply } => {
                api.set_direction(mid, Direction::Inactive);

                Awaiting::Unpublish { mid, reply }
            }
        };

        let Some((offer, pending)) = api.apply() else {
            awaiting.fail(ClientError::NothingToNegotiate);
            return;
        };

        let request = signal_request::Message::Offer(livekit::SessionDescription {
            r#type: "offer".to_owned(),
            sdp: offer.to_sdp_string(),
            ..livekit::SessionDescription::default()
        });

        if let Err(error) = self.signal.send(request) {
            awaiting.fail(error);
            return;
        }

        self.trickle_local_candidate();

        self.pending = Some(Pending {
            offer: pending,
            awaiting,
        });
    }

    fn trickle_local_candidate(&self) {
        let init = serde_json::json!({
            "candidate": self.candidate.to_sdp_string(),
            "sdpMid": "0",
            "sdpMLineIndex": 0,
        });

        let candidate = signal_request::Message::Trickle(livekit::TrickleRequest {
            candidate_init: init.to_string(),
            target: livekit::SignalTarget::Publisher as i32,
            r#final: false,
        });

        if let Err(error) = self.signal.send(candidate) {
            tracing::debug!(%error, "cannot trickle the local candidate");
            return;
        }

        let end = signal_request::Message::Trickle(livekit::TrickleRequest {
            candidate_init: String::new(),
            target: livekit::SignalTarget::Publisher as i32,
            r#final: true,
        });

        if let Err(error) = self.signal.send(end) {
            tracing::debug!(%error, "cannot signal the end of candidates");
        }
    }

    fn on_answer(&mut self, sdp: &str) {
        let Some(pending) = self.pending.take() else {
            tracing::debug!("an answer arrived with no offer outstanding");
            return;
        };

        let answer = match SdpAnswer::from_sdp_string(sdp) {
            Ok(answer) => answer,
            Err(error) => {
                pending
                    .awaiting
                    .fail(ClientError::Rtc(str0m::RtcError::Sdp(error)));
                self.negotiate();
                return;
            }
        };

        if let Err(error) = self.rtc.sdp_api().accept_answer(pending.offer, answer) {
            pending.awaiting.fail(ClientError::Rtc(error));
            self.negotiate();
            return;
        }

        match pending.awaiting {
            Awaiting::Publish {
                mid,
                profile,
                reply,
            } => {
                let outcome = self.add_sender(mid, profile).map(|()| mid);
                let _ = reply.send(outcome);
            }
            Awaiting::Unpublish { mid, reply } => {
                self.senders.retain(|sender| sender.mid != mid);
                let _ = reply.send(Ok(()));
            }
        }

        self.negotiate();
    }

    fn add_sender(&mut self, mid: Mid, profile: MediaProfile) -> Result<(), ClientError> {
        let pt = negotiated_pt(&self.rtc, mid, profile)?;

        if profile.codec == Codec::Opus {
            let mut direct = self.rtc.direct_api();
            if let Some(stream) = direct.stream_tx_by_mid(mid, None) {
                stream.set_unpaced(true);
            }
        }

        self.senders.push(TrackSender {
            mid,
            pt,
            source: SyntheticSource::new(profile),
            interval: profile.packet_interval,
            next: Instant::now(),
            seq: 0,
            ticks: 0,
            packets: 0,
            payload_bytes: 0,
            active: false,
        });

        Ok(())
    }

    fn on_remote_candidate(&mut self, init: &str) {
        #[derive(serde::Deserialize)]
        struct CandidateInit {
            #[serde(default)]
            candidate: String,
        }

        let Ok(parsed) = serde_json::from_str::<CandidateInit>(init) else {
            tracing::debug!("undecodable trickle candidate");
            return;
        };

        if parsed.candidate.is_empty() {
            return;
        }

        match Candidate::from_sdp_string(&parsed.candidate) {
            Ok(candidate) => self.rtc.add_remote_candidate(candidate),
            Err(error) => tracing::debug!(%error, "malformed remote candidate"),
        }
    }

    fn write_due_media(&mut self, now: Instant) {
        if !self.connected {
            return;
        }

        for index in 0..self.senders.len() {
            for _ in 0..MAX_CATCH_UP_PACKETS {
                let sender = &mut self.senders[index];
                if !sender.active || sender.next > now {
                    break;
                }

                let packet = sender.source.next();
                let mid = sender.mid;
                let pt = sender.pt;
                let seq: SeqNo = sender.seq.into();
                let time = sender.ticks;
                let marker = packet.marker;

                sender.seq += 1;
                sender.ticks = sender.ticks.wrapping_add(packet.ticks);
                sender.packets += 1;
                sender.payload_bytes += packet.payload.len() as u64;
                sender.next += sender.interval;

                let behind = sender.interval * MAX_CATCH_UP_PACKETS;
                if sender.next + behind < now {
                    sender.next = now + sender.interval;
                }

                let mut direct = self.rtc.direct_api();
                let Some(stream) = direct.stream_tx_by_mid(mid, None) else {
                    tracing::debug!(%mid, "no send stream for a published track");
                    break;
                };

                stream.write_rtp(RtpWrite::new(pt, seq, time, now, packet.payload).marker(marker));
            }
        }
    }

    fn fail_waiters(&mut self) {
        self.connect_waiters.clear();

        if let Some(pending) = self.pending.take() {
            pending.awaiting.fail(ClientError::TransportGone);
        }

        while let Some(queued) = self.queue.pop_front() {
            queued.fail(ClientError::TransportGone);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::change::SdpOffer;

    fn host(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn offering_rtc(port: u16) -> Rtc {
        let mut rtc = publisher_rtc().expect("a publisher rtc");
        let candidate = Candidate::host(host(port), "udp").expect("a host candidate");
        assert!(rtc.add_local_candidate(candidate).is_some());
        rtc
    }

    fn answering_rtc(port: u16) -> Rtc {
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .set_dtls_cert(dtls_cert().expect("a dtls certificate"))
            .build(Instant::now());
        let candidate = Candidate::host(host(port), "udp").expect("a host candidate");
        assert!(rtc.add_local_candidate(candidate).is_some());
        rtc
    }

    #[test]
    fn the_offer_describes_a_send_only_track_with_ice_and_dtls() {
        let mut rtc = offering_rtc(40001);

        let mut api = rtc.sdp_api();
        let mid = api.add_media(
            MediaKind::Audio,
            Direction::SendOnly,
            Some("stream".to_owned()),
            Some("TR_test".to_owned()),
            None,
        );
        let (offer, _pending) = api.apply().expect("an offer");
        let sdp = offer.to_sdp_string();

        assert!(sdp.contains("m=audio"), "{sdp}");
        assert!(sdp.contains("a=sendonly"), "{sdp}");
        assert!(sdp.contains(&format!("a=mid:{mid}")), "{sdp}");
        assert!(sdp.contains("a=ice-ufrag:"), "{sdp}");
        assert!(sdp.contains("a=ice-pwd:"), "{sdp}");
        assert!(sdp.contains("a=fingerprint:"), "{sdp}");
        assert!(sdp.contains("a=candidate:"), "{sdp}");
        assert!(sdp.contains("127.0.0.1 40001"), "{sdp}");
        assert!(sdp.contains("opus/48000/2"), "{sdp}");
        assert!(sdp.contains("a=msid:stream TR_test"), "{sdp}");
        assert!(!sdp.contains("a=ice-lite"), "{sdp}");
    }

    #[test]
    fn a_video_offer_advertises_vp8_and_nothing_we_cannot_send() {
        let mut rtc = offering_rtc(40002);

        let mut api = rtc.sdp_api();
        api.add_media(
            MediaKind::Video,
            Direction::SendOnly,
            Some("stream".to_owned()),
            Some("TR_video".to_owned()),
            None,
        );
        let (offer, _pending) = api.apply().expect("an offer");
        let sdp = offer.to_sdp_string().to_lowercase();

        assert!(sdp.contains("m=video"), "{sdp}");
        assert!(sdp.contains("vp8/90000"), "{sdp}");
        assert!(!sdp.contains("h264"), "{sdp}");
        assert!(!sdp.contains("vp9"), "{sdp}");
    }

    #[test]
    fn a_peer_accepts_the_offer_and_we_learn_the_payload_type() {
        let mut ours = offering_rtc(40003);
        let mut theirs = answering_rtc(40004);

        let mut api = ours.sdp_api();
        let mid = api.add_media(
            MediaKind::Audio,
            Direction::SendOnly,
            Some("stream".to_owned()),
            Some("TR_test".to_owned()),
            None,
        );
        let (offer, pending) = api.apply().expect("an offer");

        let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("a parseable offer");
        let answer = theirs.sdp_api().accept_offer(offer).expect("an answer");

        let answer =
            SdpAnswer::from_sdp_string(&answer.to_sdp_string()).expect("a parseable answer");
        ours.sdp_api()
            .accept_answer(pending, answer)
            .expect("the answer applies");

        let pt = negotiated_pt(&ours, mid, MediaProfile::opus()).expect("an opus payload type");
        assert_eq!(
            ours.codec_config()
                .params()
                .iter()
                .find(|params| params.pt() == pt)
                .map(|params| params.spec().codec),
            Some(str0m::format::Codec::Opus)
        );

        assert!(ours.direct_api().stream_tx_by_mid(mid, None).is_some());
    }

    #[test]
    fn asking_for_a_codec_the_peer_did_not_accept_is_an_error() {
        let mut ours = offering_rtc(40005);
        let mut theirs = answering_rtc(40006);

        let mut api = ours.sdp_api();
        let mid = api.add_media(
            MediaKind::Audio,
            Direction::SendOnly,
            Some("stream".to_owned()),
            Some("TR_test".to_owned()),
            None,
        );
        let (offer, pending) = api.apply().expect("an offer");

        let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("a parseable offer");
        let answer = theirs.sdp_api().accept_offer(offer).expect("an answer");
        let answer =
            SdpAnswer::from_sdp_string(&answer.to_sdp_string()).expect("a parseable answer");
        ours.sdp_api()
            .accept_answer(pending, answer)
            .expect("the answer applies");

        let error = negotiated_pt(&ours, mid, MediaProfile::vp8()).unwrap_err();
        assert!(matches!(error, ClientError::CodecNotNegotiated { .. }));
    }
}
