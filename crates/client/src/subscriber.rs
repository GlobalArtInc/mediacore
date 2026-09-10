use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use mediacore_proto::mediacore::{self, signal_request};
use str0m::change::SdpOffer;
use str0m::media::MediaKind;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::error::ClientError;
use crate::receiver::{ReceivedPacket, TrackReceiver, TrackReport};
use crate::signal::SignalHandle;
use crate::track::TrackKind;
use crate::transport::dtls_cert;
use crate::udp::{self, Datagram};

pub(crate) enum SubCommand {
    Offer(Box<mediacore::SessionDescription>),
    RemoteCandidate(String),
    Connected(oneshot::Sender<()>),
    Reports(oneshot::Sender<Vec<TrackReport>>),
    Close(oneshot::Sender<()>),
}

pub(crate) struct Subscriber {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    candidate: Candidate,
    signal: SignalHandle,
    commands: mpsc::Receiver<SubCommand>,
    datagrams: mpsc::Receiver<Datagram>,
    tracks: BTreeMap<String, TrackReceiver>,
    window: usize,
    connected: bool,
    connect_waiters: Vec<oneshot::Sender<()>>,
    trickled: bool,
}

pub(crate) async fn start(
    bind: IpAddr,
    window: usize,
    commands: mpsc::Receiver<SubCommand>,
    signal: SignalHandle,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), ClientError> {
    let endpoint = udp::bind(bind).await?;
    let local = endpoint.local;

    let mut rtc = subscriber_rtc()?;

    let candidate = Candidate::host(local, "udp")
        .map_err(|error| ClientError::HostCandidate(local, error.to_string()))?;

    if rtc.add_local_candidate(candidate.clone()).is_none() {
        tracing::warn!(%local, "str0m rejected our subscriber host candidate");
    }

    let subscriber = Subscriber {
        rtc,
        socket: endpoint.socket,
        local,
        candidate,
        signal,
        commands,
        datagrams: endpoint.datagrams,
        tracks: BTreeMap::new(),
        window: window.max(1),
        connected: false,
        connect_waiters: Vec::new(),
        trickled: false,
    };

    Ok((local, tokio::spawn(subscriber.run())))
}

pub(crate) fn subscriber_rtc() -> Result<Rtc, ClientError> {
    Ok(Rtc::builder()
        .set_ice_lite(false)
        .set_rtp_mode(true)
        .set_dtls_cert(dtls_cert()?)
        .clear_codecs()
        .enable_opus(true)
        .enable_vp8(true)
        .build(Instant::now()))
}

pub(crate) fn answer_offer(rtc: &mut Rtc, sdp: &str) -> Result<String, ClientError> {
    let offer = SdpOffer::from_sdp_string(sdp).map_err(str0m::RtcError::Sdp)?;
    let answer = rtc.sdp_api().accept_offer(offer)?;
    Ok(answer.to_sdp_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MediaSection {
    pub(crate) mid: String,
    pub(crate) kind: Option<TrackKind>,
    pub(crate) stream_id: Option<String>,
    pub(crate) track_id: Option<String>,
    pub(crate) direction: Option<String>,
}

pub(crate) fn media_sections(sdp: &str) -> Vec<MediaSection> {
    let mut sections: Vec<MediaSection> = Vec::new();
    let mut current: Option<MediaSection> = None;

    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');

        if let Some(rest) = line.strip_prefix("m=") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let kind = match rest.split_whitespace().next() {
                Some("audio") => Some(TrackKind::Audio),
                Some("video") => Some(TrackKind::Video),
                _ => None,
            };
            current = Some(MediaSection {
                kind,
                ..MediaSection::default()
            });
            continue;
        }

        let Some(section) = current.as_mut() else {
            continue;
        };

        if let Some(mid) = line.strip_prefix("a=mid:") {
            section.mid = mid.trim().to_owned();
        } else if let Some(msid) = line.strip_prefix("a=msid:") {
            apply_msid(section, msid);
        } else if let Some(ssrc) = line.strip_prefix("a=ssrc:") {
            if let Some((_, msid)) = ssrc.split_once(" msid:") {
                apply_msid(section, msid);
            }
        } else if matches!(
            line,
            "a=sendonly" | "a=recvonly" | "a=sendrecv" | "a=inactive"
        ) {
            section.direction = Some(line[2..].to_owned());
        }
    }

    if let Some(section) = current.take() {
        sections.push(section);
    }

    sections.retain(|section| !section.mid.is_empty());
    sections
}

fn apply_msid(section: &mut MediaSection, msid: &str) {
    let mut parts = msid.split_whitespace();

    if let Some(stream_id) = parts.next()
        && section.stream_id.is_none()
    {
        section.stream_id = Some(stream_id.to_owned());
    }

    if let Some(track_id) = parts.next()
        && section.track_id.is_none()
    {
        section.track_id = Some(track_id.to_owned());
    }
}

impl Subscriber {
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
                    if let Err(error) = self.rtc.handle_input(Input::Timeout(now)) {
                        tracing::debug!(%error, "subscriber timeout input failed");
                        self.rtc.disconnect();
                    }
                }
            }
        }

        self.connect_waiters.clear();
    }

    async fn poll(&mut self) -> Option<Instant> {
        loop {
            if !self.rtc.is_alive() {
                return None;
            }

            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return Some(at.max(Instant::now())),
                Ok(Output::Transmit(transmit)) => {
                    let sent = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await;
                    if let Err(error) = sent {
                        tracing::debug!(
                            %error,
                            destination = %transmit.destination,
                            "subscriber udp send failed"
                        );
                    }
                }
                Ok(Output::Event(event)) => self.on_event(event),
                Err(error) => {
                    tracing::warn!(%error, "subscriber transport failed");
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected => {
                tracing::debug!(local = %self.local, "subscriber transport connected");
                self.connected = true;
                for waiter in self.connect_waiters.drain(..) {
                    let _ = waiter.send(());
                }
            }
            Event::MediaAdded(added) => {
                let kind = match added.kind {
                    MediaKind::Audio => TrackKind::Audio,
                    MediaKind::Video => TrackKind::Video,
                };
                let window = self.window;
                self.tracks
                    .entry(added.mid.to_string())
                    .or_insert_with(|| {
                        TrackReceiver::new(added.mid.to_string(), window, Instant::now())
                    })
                    .describe(None, Some(kind));
            }
            Event::RtpPacket(packet) => self.on_rtp(packet),
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                tracing::debug!("subscriber ice connection went to disconnected");
                self.rtc.disconnect();
            }
            Event::Closed => self.rtc.disconnect(),
            _ => {}
        }
    }

    fn on_rtp(&mut self, packet: str0m::rtp::RtpPacket) {
        let mid = packet
            .header
            .ext_vals
            .mid
            .map(|mid| mid.to_string())
            .or_else(|| {
                self.rtc
                    .direct_api()
                    .stream_rx(&packet.header.ssrc)
                    .map(|stream| stream.mid().to_string())
            });

        let Some(mid) = mid else {
            tracing::trace!(ssrc = *packet.header.ssrc, "rtp for an unknown stream");
            return;
        };

        let window = self.window;
        let track = self
            .tracks
            .entry(mid.clone())
            .or_insert_with(|| TrackReceiver::new(mid, window, Instant::now()));

        track.accept(ReceivedPacket {
            sequence: *packet.seq_no,
            timestamp: packet.header.timestamp,
            payload_type: *packet.header.payload_type,
            ssrc: *packet.header.ssrc,
            marker: packet.header.marker,
            rid: packet.header.ext_vals.rid.map(|rid| rid.to_string()),
            payload: packet.payload.to_vec(),
            // str0m's own stamp, taken when the datagram was read off the
            // socket, rather than one taken here: everything between the two is
            // decryption and this task's turn to run, and a jitter estimate that
            // included our own scheduling would be measuring the bot.
            arrival: Some(packet.timestamp),
        });
    }

    fn on_datagram(&mut self, at: Instant, source: SocketAddr, data: &[u8]) {
        match Receive::new(Protocol::Udp, source, self.local, data) {
            Ok(receive) => {
                if let Err(error) = self.rtc.handle_input(Input::Receive(at, receive)) {
                    tracing::debug!(%error, "subscriber receive input failed");
                }
            }
            Err(error) => tracing::trace!(%error, "undecodable subscriber datagram"),
        }
    }

    fn on_command(&mut self, command: SubCommand) -> bool {
        match command {
            SubCommand::Offer(offer) => self.on_offer(&offer),
            SubCommand::RemoteCandidate(init) => self.on_remote_candidate(&init),
            SubCommand::Connected(reply) => {
                if self.connected {
                    let _ = reply.send(());
                } else {
                    self.connect_waiters.push(reply);
                }
            }
            SubCommand::Reports(reply) => {
                let reports = self.tracks.values().map(TrackReceiver::report).collect();
                let _ = reply.send(reports);
            }
            SubCommand::Close(reply) => {
                let _ = self.rtc.close();
                let _ = reply.send(());
                return false;
            }
        }

        true
    }

    fn on_offer(&mut self, offer: &mediacore::SessionDescription) {
        let sdp = match answer_offer(&mut self.rtc, &offer.sdp) {
            Ok(sdp) => sdp,
            Err(error) => {
                tracing::warn!(%error, "cannot answer the subscriber offer");
                return;
            }
        };

        let answer = signal_request::Message::Answer(mediacore::SessionDescription {
            r#type: "answer".to_owned(),
            sdp,
            id: offer.id,
            ..mediacore::SessionDescription::default()
        });

        if let Err(error) = self.signal.send(answer) {
            tracing::debug!(%error, "cannot send the subscriber answer");
            return;
        }

        self.describe_tracks(offer);

        if !self.trickled {
            self.trickled = true;
            self.trickle_local_candidate();
        }
    }

    fn describe_tracks(&mut self, offer: &mediacore::SessionDescription) {
        let window = self.window;

        for section in media_sections(&offer.sdp) {
            let track_sid = offer
                .mid_to_track_id
                .get(&section.mid)
                .cloned()
                .or(section.track_id);

            // The offer is the moment this client learns it will be given the
            // track, and so where the clock on time-to-first-packet starts.
            let opened = Instant::now();

            self.tracks
                .entry(section.mid.clone())
                .or_insert_with(|| TrackReceiver::new(section.mid, window, opened))
                .describe(track_sid, section.kind);
        }
    }

    fn trickle_local_candidate(&self) {
        let init = serde_json::json!({
            "candidate": self.candidate.to_sdp_string(),
            "sdpMid": "0",
            "sdpMLineIndex": 0,
        });

        let candidate = signal_request::Message::Trickle(mediacore::TrickleRequest {
            candidate_init: init.to_string(),
            target: mediacore::SignalTarget::Subscriber as i32,
            r#final: false,
        });

        if let Err(error) = self.signal.send(candidate) {
            tracing::debug!(%error, "cannot trickle the subscriber candidate");
            return;
        }

        let end = signal_request::Message::Trickle(mediacore::TrickleRequest {
            candidate_init: String::new(),
            target: mediacore::SignalTarget::Subscriber as i32,
            r#final: true,
        });

        if let Err(error) = self.signal.send(end) {
            tracing::debug!(%error, "cannot signal the end of subscriber candidates");
        }
    }

    fn on_remote_candidate(&mut self, init: &str) {
        #[derive(serde::Deserialize)]
        struct CandidateInit {
            #[serde(default)]
            candidate: String,
        }

        let Ok(parsed) = serde_json::from_str::<CandidateInit>(init) else {
            tracing::debug!("undecodable subscriber trickle candidate");
            return;
        };

        if parsed.candidate.is_empty() {
            return;
        }

        match Candidate::from_sdp_string(&parsed.candidate) {
            Ok(candidate) => self.rtc.add_remote_candidate(candidate),
            Err(error) => tracing::debug!(%error, "malformed remote subscriber candidate"),
        }
    }
}

#[cfg(test)]
#[path = "subscriber_tests.rs"]
mod tests;
