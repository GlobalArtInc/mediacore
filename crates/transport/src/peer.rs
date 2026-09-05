use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpApi, SdpOffer, SdpPendingOffer};
use str0m::format::PayloadParams;
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaAdded, MediaData, MediaKind, Mid, Pt,
    Rid, Simulcast,
};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{ExtensionValues, RtpPacket, RtpWrite, SeqNo, Ssrc};
use str0m::{Candidate, Event, IceConnectionState, IceCreds, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::crypto::TransportCrypto;
use crate::error::TransportError;
use crate::router::{Datagram, DatagramRx, SessionId, SessionRoute, UdpRouter};

pub const COMMAND_QUEUE_CAPACITY: usize = 64;
pub const MEDIA_QUEUE_CAPACITY: usize = 512;
pub const ICE_DISCONNECTED_GRACE: Duration = Duration::from_secs(20);

const CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    Publisher,
    Subscriber,
}

#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub role: PeerRole,
    pub advertise: Vec<SocketAddr>,
    pub ice_lite: bool,
    pub rtp_mode: bool,
    pub stats_interval: Option<Duration>,
    pub command_queue: usize,
    pub media_queue: usize,
    pub ice_disconnected_grace: Duration,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            role: PeerRole::Publisher,
            advertise: Vec::new(),
            ice_lite: true,
            rtp_mode: true,
            stats_interval: Some(Duration::from_secs(2)),
            command_queue: COMMAND_QUEUE_CAPACITY,
            media_queue: MEDIA_QUEUE_CAPACITY,
            ice_disconnected_grace: ICE_DISCONNECTED_GRACE,
        }
    }
}

impl PeerConfig {
    pub fn publisher() -> Self {
        Self::default()
    }

    pub fn subscriber() -> Self {
        Self {
            role: PeerRole::Subscriber,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaSpec {
    pub kind: MediaKind,
    pub direction: Direction,
    pub stream_id: Option<String>,
    pub track_id: Option<String>,
    pub simulcast: Option<Simulcast>,
}

#[derive(Debug, Clone, Default)]
pub struct Negotiation {
    pub add_media: Vec<MediaSpec>,
    pub set_direction: Vec<(Mid, Direction)>,
    pub stop_media: Vec<Mid>,
}

impl Negotiation {
    pub fn add(spec: MediaSpec) -> Self {
        Self {
            add_media: vec![spec],
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalOffer {
    pub mids: Vec<Mid>,
    pub sdp: String,
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

#[derive(Debug)]
pub struct MediaInfo {
    pub mid: Mid,
    pub kind: MediaKind,
    pub direction: Direction,
    pub rids: Vec<Rid>,
    pub params: Vec<PayloadParams>,
}

#[derive(Debug)]
pub struct IncomingRtp {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub carried_mid_extension: bool,
    pub packet: RtpPacket,
}

#[derive(Debug)]
pub enum PeerEvent {
    IceConnectionState(IceConnectionState),
    Connected,
    MediaAdded(MediaInfo),
    Rtp(Box<IncomingRtp>),
    Media(Box<MediaData>),
    KeyframeRequested(KeyframeRequest),
    Disconnected,
}

#[derive(Debug)]
enum PeerCommand {
    AcceptOffer {
        sdp: String,
        reply: oneshot::Sender<Result<String, TransportError>>,
    },
    Negotiate {
        request: Box<Negotiation>,
        reply: oneshot::Sender<Result<LocalOffer, TransportError>>,
    },
    AcceptAnswer {
        sdp: String,
        reply: oneshot::Sender<Result<(), TransportError>>,
    },
    AddRemoteCandidate {
        candidate: String,
        reply: oneshot::Sender<Result<(), TransportError>>,
    },
    RequestKeyframe {
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    },
    WriteRtp(Box<OutgoingRtp>),
    Close,
}

#[derive(Debug, Clone)]
pub struct PeerHandle {
    id: SessionId,
    role: PeerRole,
    ice_creds: Arc<RwLock<IceCreds>>,
    dropped_media: Arc<AtomicU64>,
    tx: mpsc::Sender<PeerCommand>,
}

impl PeerHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn role(&self) -> PeerRole {
        self.role
    }

    pub fn ice_creds(&self) -> IceCreds {
        self.ice_creds
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn dropped_media_events(&self) -> u64 {
        self.dropped_media.load(Ordering::Relaxed)
    }

    pub async fn accept_offer(&self, sdp: String) -> Result<String, TransportError> {
        self.request(|reply| PeerCommand::AcceptOffer { sdp, reply })
            .await?
    }

    pub async fn negotiate(&self, request: Negotiation) -> Result<LocalOffer, TransportError> {
        self.request(|reply| PeerCommand::Negotiate {
            request: Box::new(request),
            reply,
        })
        .await?
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<(), TransportError> {
        self.request(|reply| PeerCommand::AcceptAnswer { sdp, reply })
            .await?
    }

    pub async fn add_remote_candidate(&self, candidate: String) -> Result<(), TransportError> {
        self.request(|reply| PeerCommand::AddRemoteCandidate { candidate, reply })
            .await?
    }

    pub fn request_keyframe(
        &self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<(), TransportError> {
        self.notify(PeerCommand::RequestKeyframe { mid, rid, kind })
    }

    pub fn write_rtp(&self, packet: OutgoingRtp) -> Result<(), TransportError> {
        self.notify(PeerCommand::WriteRtp(Box::new(packet)))
    }

    pub fn close(&self) {
        let _ = self.notify(PeerCommand::Close);
    }

    fn notify(&self, command: PeerCommand) -> Result<(), TransportError> {
        match self.tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::PeerGone),
            Err(mpsc::error::TrySendError::Full(command)) => {
                tracing::warn!(id = %self.id, ?command, "peer command queue is full");
                Ok(())
            }
        }
    }

    async fn request<T, F>(&self, build: F) -> Result<T, TransportError>
    where
        F: FnOnce(oneshot::Sender<T>) -> PeerCommand,
    {
        let (reply, response) = oneshot::channel();

        self.tx
            .send(build(reply))
            .await
            .map_err(|_| TransportError::PeerGone)?;

        response.await.map_err(|_| TransportError::PeerGone)
    }
}

pub struct PeerEvents {
    control: mpsc::UnboundedReceiver<PeerEvent>,
    media: mpsc::Receiver<PeerEvent>,
    control_open: bool,
    media_open: bool,
}

impl PeerEvents {
    pub async fn recv(&mut self) -> Option<PeerEvent> {
        loop {
            if !self.control_open && !self.media_open {
                return None;
            }

            tokio::select! {
                biased;

                control = self.control.recv(), if self.control_open => match control {
                    Some(event) => return Some(event),
                    None => self.control_open = false,
                },

                media = self.media.recv(), if self.media_open => match media {
                    Some(event) => return Some(event),
                    None => self.media_open = false,
                },
            }
        }
    }

    pub async fn recv_control(&mut self) -> Option<PeerEvent> {
        self.control.recv().await
    }
}

impl std::fmt::Debug for PeerEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerEvents")
            .field("control_open", &self.control_open)
            .field("media_open", &self.media_open)
            .finish()
    }
}

#[derive(Debug)]
pub struct SpawnedPeer {
    pub handle: PeerHandle,
    pub events: PeerEvents,
    pub task: JoinHandle<()>,
}

pub fn spawn_peer(
    crypto: &TransportCrypto,
    udp: &UdpRouter,
    config: PeerConfig,
) -> Result<SpawnedPeer, TransportError> {
    let (handle, actor, events) = build_peer(crypto, udp, config)?;
    let span = tracing::info_span!("peer", id = %handle.id(), role = ?handle.role());
    let task = tokio::spawn(actor.run().instrument(span));

    Ok(SpawnedPeer {
        handle,
        events,
        task,
    })
}

fn build_peer(
    crypto: &TransportCrypto,
    udp: &UdpRouter,
    config: PeerConfig,
) -> Result<(PeerHandle, PeerActor, PeerEvents), TransportError> {
    let ice_creds = IceCreds::new();
    let mut route = udp.register(&ice_creds)?;
    let datagrams = route
        .take_receiver()
        .ok_or(TransportError::PeerGone)
        .inspect_err(|_| tracing::error!("router handed out a route without a receiver"))?;

    let advertise: Vec<SocketAddr> = if config.advertise.is_empty() {
        udp.advertised().to_vec()
    } else {
        config.advertise.clone()
    };

    let rtc = build_rtc(crypto, &ice_creds, &config, &advertise)?;

    let (tx, commands) = mpsc::channel(config.command_queue.max(1));
    let (control_tx, control) = mpsc::unbounded_channel();
    let (media_tx, media) = mpsc::channel(config.media_queue.max(1));

    let creds = Arc::new(RwLock::new(ice_creds));
    let dropped_media = Arc::new(AtomicU64::new(0));

    let handle = PeerHandle {
        id: route.id(),
        role: config.role,
        ice_creds: Arc::clone(&creds),
        dropped_media: Arc::clone(&dropped_media),
        tx,
    };

    let actor = PeerActor {
        rtc,
        route,
        socket: udp.socket(),
        local_addr: udp.local_addr(),
        datagrams,
        commands,
        sink: EventSink {
            control: control_tx,
            media: media_tx,
            dropped: dropped_media,
            reported: 0,
            reported_at: None,
        },
        creds,
        ice: IceWatch::new(config.ice_disconnected_grace),
        pending_offer: None,
        drain_deadline: None,
        commands_closed: false,
    };

    let events = PeerEvents {
        control,
        media,
        control_open: true,
        media_open: true,
    };

    Ok((handle, actor, events))
}

fn build_rtc(
    crypto: &TransportCrypto,
    ice_creds: &IceCreds,
    config: &PeerConfig,
    advertise: &[SocketAddr],
) -> Result<Rtc, TransportError> {
    let mut rtc = Rtc::builder()
        .set_ice_lite(config.ice_lite)
        .set_rtp_mode(config.rtp_mode)
        .set_local_ice_credentials(ice_creds.clone())
        .set_dtls_cert(crypto.dtls_cert())
        .set_stats_interval(config.stats_interval)
        .clear_codecs()
        .enable_opus(true)
        .enable_vp8(true)
        .enable_h264(true)
        .enable_vp9(true)
        .enable_av1(true)
        .build(Instant::now());

    let mut accepted = 0usize;

    for addr in advertise {
        match Candidate::host(*addr, "udp") {
            Ok(candidate) => {
                if rtc.add_local_candidate(candidate).is_some() {
                    accepted += 1;
                } else {
                    tracing::warn!(%addr, "local candidate rejected by the ice agent");
                }
            }
            Err(err) => tracing::warn!(%addr, ?err, "cannot build a host candidate"),
        }
    }

    if accepted == 0 {
        return Err(TransportError::NoLocalCandidate);
    }

    Ok(rtc)
}

#[derive(Debug)]
struct EventSink {
    control: mpsc::UnboundedSender<PeerEvent>,
    media: mpsc::Sender<PeerEvent>,
    dropped: Arc<AtomicU64>,
    reported: u64,
    reported_at: Option<Instant>,
}

impl EventSink {
    fn emit_control(&self, event: PeerEvent) -> bool {
        self.control.send(event).is_ok()
    }

    fn emit_media(&mut self, event: PeerEvent) -> bool {
        match self.media.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.note_drop(Instant::now());
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn note_drop(&mut self, now: Instant) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;

        let due = match self.reported_at {
            Some(at) => now.saturating_duration_since(at) >= DROP_REPORT_INTERVAL,
            None => true,
        };

        if !due {
            return;
        }

        tracing::warn!(
            dropped = total - self.reported,
            total,
            "media event queue is full, dropping media events"
        );

        self.reported = total;
        self.reported_at = Some(now);
    }
}

#[derive(Debug)]
struct IceWatch {
    grace: Duration,
    disconnected_since: Option<Instant>,
}

impl IceWatch {
    fn new(grace: Duration) -> Self {
        Self {
            grace,
            disconnected_since: None,
        }
    }

    fn observe(&mut self, state: IceConnectionState, now: Instant) {
        match state {
            IceConnectionState::Connected | IceConnectionState::Completed => {
                self.disconnected_since = None;
            }
            IceConnectionState::Disconnected => {
                self.disconnected_since.get_or_insert(now);
            }
            _ => {}
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.disconnected_since.map(|since| since + self.grace)
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| deadline <= now)
    }
}

struct PeerActor {
    rtc: Rtc,
    route: SessionRoute,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    datagrams: DatagramRx,
    commands: mpsc::Receiver<PeerCommand>,
    sink: EventSink,
    creds: Arc<RwLock<IceCreds>>,
    ice: IceWatch,
    pending_offer: Option<SdpPendingOffer>,
    drain_deadline: Option<Instant>,
    commands_closed: bool,
}

impl PeerActor {
    async fn run(mut self) {
        tracing::debug!(ufrag = self.route.ufrag(), "peer connection started");

        loop {
            let Some(deadline) = self.drive().await else {
                break;
            };

            let now = Instant::now();

            if self.ice.expired(now) {
                tracing::info!(
                    grace = ?self.ice.grace,
                    "ice stayed disconnected past the grace period, tearing the session down"
                );
                self.rtc.disconnect();
                break;
            }

            let deadline = match self.drain_deadline {
                Some(drain) if drain <= now => {
                    tracing::debug!("close drain timed out, disconnecting");
                    self.rtc.disconnect();
                    break;
                }
                Some(drain) => deadline.min(drain),
                None => deadline,
            };

            let deadline = match self.ice.deadline() {
                Some(ice) => deadline.min(ice),
                None => deadline,
            };

            let deadline = deadline.max(now);
            let sleep = tokio::time::sleep_until(deadline.into());
            tokio::pin!(sleep);

            tokio::select! {
                datagram = self.datagrams.recv() => match datagram {
                    Some(datagram) => self.on_datagram(datagram),
                    None => break,
                },

                command = self.commands.recv(), if !self.commands_closed => match command {
                    Some(command) => self.on_command(command),
                    None => {
                        self.commands_closed = true;
                        self.begin_close();
                    }
                },

                _ = &mut sleep => {
                    self.handle_timeout();
                }
            }
        }

        self.sink.emit_control(PeerEvent::Disconnected);

        tracing::debug!("peer connection stopped");
    }

    async fn drive(&mut self) -> Option<Instant> {
        loop {
            if !self.rtc.is_alive() {
                return None;
            }

            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return Some(at),
                Ok(Output::Transmit(transmit)) => {
                    if let Err(err) = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await
                    {
                        tracing::debug!(dst = %transmit.destination, ?err, "udp send failed");
                    }
                }
                Ok(Output::Event(event)) => {
                    if !self.on_event(event) {
                        return None;
                    }
                }
                Err(err) => {
                    tracing::warn!(?err, "poll_output failed, disconnecting");
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    fn on_datagram(&mut self, datagram: Datagram) {
        let receive = match Receive::new(
            Protocol::Udp,
            datagram.source,
            self.local_addr,
            &datagram.data,
        ) {
            Ok(receive) => receive,
            Err(err) => {
                tracing::trace!(source = %datagram.source, ?err, "undecodable datagram");
                return;
            }
        };

        if let Err(err) = self.rtc.handle_input(Input::Receive(datagram.at, receive)) {
            tracing::debug!(source = %datagram.source, ?err, "rejected inbound datagram");
        }
    }

    fn handle_timeout(&mut self) {
        if let Err(err) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
            tracing::warn!(?err, "timeout handling failed, disconnecting");
            self.rtc.disconnect();
        }
    }

    fn on_command(&mut self, command: PeerCommand) {
        match command {
            PeerCommand::AcceptOffer { sdp, reply } => {
                let _ = reply.send(self.accept_offer(&sdp));
            }
            PeerCommand::Negotiate { request, reply } => {
                let _ = reply.send(self.negotiate(*request));
            }
            PeerCommand::AcceptAnswer { sdp, reply } => {
                let _ = reply.send(self.accept_answer(&sdp));
            }
            PeerCommand::AddRemoteCandidate { candidate, reply } => {
                let _ = reply.send(self.add_remote_candidate(&candidate));
            }
            PeerCommand::RequestKeyframe { mid, rid, kind } => {
                self.request_keyframe(mid, rid, kind);
            }
            PeerCommand::WriteRtp(packet) => self.write_rtp(*packet),
            PeerCommand::Close => self.begin_close(),
        }
    }

    fn begin_close(&mut self) {
        if self.drain_deadline.is_some() {
            return;
        }

        self.drain_deadline = Some(Instant::now() + CLOSE_DRAIN_TIMEOUT);

        if !self.rtc.is_connected() {
            self.rtc.disconnect();
            return;
        }

        if let Err(err) = self.rtc.close() {
            tracing::debug!(?err, "graceful close failed, disconnecting");
            self.rtc.disconnect();
        }
    }

    fn accept_offer(&mut self, sdp: &str) -> Result<String, TransportError> {
        let offer = SdpOffer::from_sdp_string(sdp).map_err(str0m::RtcError::from)?;
        let answer: SdpAnswer = self.rtc.sdp_api().accept_offer(offer)?;
        self.pending_offer = None;
        self.sync_ice_credentials()?;

        Ok(answer.to_sdp_string())
    }

    fn negotiate(&mut self, request: Negotiation) -> Result<LocalOffer, TransportError> {
        let mut api: SdpApi<'_> = self.rtc.sdp_api();
        let mut mids = Vec::with_capacity(request.add_media.len());

        for spec in request.add_media {
            mids.push(api.add_media(
                spec.kind,
                spec.direction,
                spec.stream_id,
                spec.track_id,
                spec.simulcast,
            ));
        }

        for (mid, direction) in request.set_direction {
            api.set_direction(mid, direction);
        }

        for mid in request.stop_media {
            api.stop_media(mid);
        }

        let (offer, pending) = api.apply().ok_or(TransportError::NothingToNegotiate)?;
        self.pending_offer = Some(pending);

        Ok(LocalOffer {
            mids,
            sdp: offer.to_sdp_string(),
        })
    }

    fn accept_answer(&mut self, sdp: &str) -> Result<(), TransportError> {
        let pending = self
            .pending_offer
            .take()
            .ok_or(TransportError::NoPendingOffer)?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(str0m::RtcError::from)?;

        self.rtc.sdp_api().accept_answer(pending, answer)?;
        self.sync_ice_credentials()?;

        Ok(())
    }

    fn sync_ice_credentials(&mut self) -> Result<(), TransportError> {
        let current = self.rtc.direct_api().local_ice_credentials();

        {
            let known = self.creds.read().unwrap_or_else(|error| error.into_inner());
            if *known == current {
                return Ok(());
            }
        }

        tracing::info!(
            previous = self.route.ufrag(),
            ufrag = %current.ufrag,
            "local ice credentials rotated, rekeying the route"
        );

        if !self.route.rekey(&current) {
            tracing::error!(
                previous = self.route.ufrag(),
                ufrag = %current.ufrag,
                "the router refused the rekey, the session can no longer receive packets"
            );
            self.rtc.disconnect();
            return Err(TransportError::RouteRekey(current.ufrag));
        }

        *self
            .creds
            .write()
            .unwrap_or_else(|error| error.into_inner()) = current;

        Ok(())
    }

    fn add_remote_candidate(&mut self, candidate: &str) -> Result<(), TransportError> {
        let trimmed = candidate.trim();

        if trimmed.is_empty() {
            return Ok(());
        }

        let candidate = Candidate::from_sdp_string(trimmed)
            .map_err(|err| TransportError::Candidate(err.to_string()))?;

        self.rtc.add_remote_candidate(candidate);

        Ok(())
    }

    fn request_keyframe(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        match self.rtc.direct_api().stream_rx_by_mid(mid, rid) {
            Some(stream) => stream.request_keyframe(kind),
            None => tracing::debug!(%mid, ?rid, "no receive stream to request a keyframe on"),
        }
    }

    fn write_rtp(&mut self, packet: OutgoingRtp) {
        let mut direct = self.rtc.direct_api();

        let Some(stream) = direct.stream_tx_by_mid(packet.mid, packet.rid) else {
            tracing::debug!(mid = %packet.mid, rid = ?packet.rid, "no send stream declared");
            return;
        };

        stream.write_rtp(
            RtpWrite::new(
                packet.pt,
                packet.seq_no,
                packet.time,
                packet.wallclock,
                packet.payload,
            )
            .marker(packet.marker)
            .nackable(packet.nackable)
            .ext_vals(packet.ext_vals),
        );
    }

    fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Connected => self.sink.emit_control(PeerEvent::Connected),
            Event::IceConnectionStateChange(state) => {
                self.ice.observe(state, Instant::now());

                if state == IceConnectionState::Disconnected {
                    tracing::info!(
                        grace = ?self.ice.grace,
                        "ice disconnected, holding the session open until it recovers"
                    );
                }

                self.sink.emit_control(PeerEvent::IceConnectionState(state))
            }
            Event::MediaAdded(added) => {
                let info = self.media_info(added);
                self.sink.emit_control(PeerEvent::MediaAdded(info))
            }
            Event::RtpPacket(packet) => {
                let ssrc = packet.header.ssrc;
                let carried_mid_extension = packet.header.ext_vals.mid.is_some();

                let Some((mid, rid)) = self.stream_midrid(&packet) else {
                    tracing::trace!(?ssrc, "rtp packet on an unmapped stream");
                    return true;
                };

                let incoming = IncomingRtp {
                    mid,
                    rid,
                    ssrc,
                    carried_mid_extension,
                    packet,
                };
                self.sink.emit_media(PeerEvent::Rtp(Box::new(incoming)))
            }
            Event::MediaData(data) => self.sink.emit_media(PeerEvent::Media(Box::new(data))),
            Event::KeyframeRequest(request) => self
                .sink
                .emit_control(PeerEvent::KeyframeRequested(request)),
            Event::StreamPaused(paused) => {
                tracing::debug!(ssrc = ?paused.ssrc, paused = paused.paused, "stream pause changed");
                true
            }
            Event::Closed => {
                self.rtc.disconnect();
                false
            }
            _ => true,
        }
    }

    fn stream_midrid(&mut self, packet: &RtpPacket) -> Option<(Mid, Option<Rid>)> {
        if let Some(stream) = self.rtc.direct_api().stream_rx(&packet.header.ssrc) {
            return Some((stream.mid(), stream.rid()));
        }

        packet
            .header
            .ext_vals
            .mid
            .map(|mid| (mid, packet.header.ext_vals.rid))
    }

    fn media_info(&self, added: MediaAdded) -> MediaInfo {
        let rids = added
            .simulcast
            .as_ref()
            .map(|simulcast| simulcast.send.iter().map(|layer| layer.rid).collect())
            .unwrap_or_default();

        let params = match self.rtc.media(added.mid) {
            Some(media) => {
                let remote_pts = media.remote_pts();
                self.rtc
                    .codec_config()
                    .params()
                    .iter()
                    .filter(|params| remote_pts.contains(&params.pt()))
                    .cloned()
                    .collect()
            }
            None => Vec::new(),
        };

        MediaInfo {
            mid: added.mid,
            kind: added.kind,
            direction: added.direction,
            rids,
            params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::router::RouterConfig;

    fn loopback_router_config() -> RouterConfig {
        RouterConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..RouterConfig::default()
        }
    }

    fn client_offer(crypto: &TransportCrypto) -> String {
        let mut rtc = Rtc::builder()
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .build(Instant::now());

        rtc.add_local_candidate(
            Candidate::host(SocketAddr::from(([127, 0, 0, 1], 40000)), "udp")
                .expect("client host candidate"),
        );

        let mut api = rtc.sdp_api();
        api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);

        let (offer, _pending) = api.apply().expect("client offer");

        offer.to_sdp_string()
    }

    fn sdp_attribute(sdp: &str, name: &str) -> Option<String> {
        sdp.lines()
            .find_map(|line| line.trim().strip_prefix(name))
            .map(str::to_owned)
    }

    async fn peer_actor(
        crypto: &TransportCrypto,
        udp: &UdpRouter,
        config: PeerConfig,
    ) -> (PeerHandle, PeerActor, PeerEvents) {
        build_peer(crypto, udp, config).expect("a peer is built")
    }

    #[tokio::test]
    async fn answers_a_client_offer_and_advertises_a_host_candidate() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();

        assert_eq!(udp.router().session_count(), 1);
        assert_eq!(
            udp.router()
                .session_for_ufrag(&peer.handle.ice_creds().ufrag),
            Some(peer.handle.id())
        );

        let answer = peer
            .handle
            .accept_offer(client_offer(&crypto))
            .await
            .unwrap();

        assert!(answer.contains(&format!("a=ice-ufrag:{}", peer.handle.ice_creds().ufrag)));
        assert!(answer.contains("a=ice-lite"));
        assert!(answer.contains("typ host"));
        assert!(answer.contains(&udp.local_addr().port().to_string()));
    }

    #[tokio::test]
    async fn an_ice_restart_rekeys_the_route_to_the_new_credentials() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();

        let mut client = Rtc::builder()
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .build(Instant::now());
        client.add_local_candidate(
            Candidate::host(SocketAddr::from(([127, 0, 0, 1], 40010)), "udp")
                .expect("client host candidate"),
        );

        let mut api = client.sdp_api();
        api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        let (offer, pending) = api.apply().expect("client offer");

        let answer = peer
            .handle
            .accept_offer(offer.to_sdp_string())
            .await
            .unwrap();
        client
            .sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap())
            .expect("the client applies our answer");

        let first = peer.handle.ice_creds();
        assert_eq!(
            udp.router().session_for_ufrag(&first.ufrag),
            Some(peer.handle.id())
        );

        let mut api = client.sdp_api();
        api.ice_restart(true);
        let (restart, _pending) = api.apply().expect("an ice restart offer");

        let answer = peer
            .handle
            .accept_offer(restart.to_sdp_string())
            .await
            .unwrap();

        let restarted = peer.handle.ice_creds();
        assert_ne!(restarted.ufrag, first.ufrag);
        assert_eq!(
            sdp_attribute(&answer, "a=ice-ufrag:").as_deref(),
            Some(restarted.ufrag.as_str())
        );
        assert_eq!(
            sdp_attribute(&answer, "a=ice-pwd:").as_deref(),
            Some(restarted.pass.as_str())
        );

        assert_eq!(udp.router().session_for_ufrag(&first.ufrag), None);
        assert_eq!(
            udp.router().session_for_ufrag(&restarted.ufrag),
            Some(peer.handle.id())
        );
        assert_eq!(udp.router().session_count(), 1);
    }

    #[tokio::test]
    async fn offers_outgoing_media_and_applies_the_client_answer() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber()).unwrap();

        assert!(matches!(
            peer.handle.negotiate(Negotiation::default()).await,
            Err(TransportError::NothingToNegotiate)
        ));
        assert!(matches!(
            peer.handle.accept_answer(String::new()).await,
            Err(TransportError::NoPendingOffer)
        ));

        let local = peer
            .handle
            .negotiate(Negotiation::add(MediaSpec {
                kind: MediaKind::Video,
                direction: Direction::SendOnly,
                stream_id: Some("stream".to_owned()),
                track_id: Some("track".to_owned()),
                simulcast: None,
            }))
            .await
            .unwrap();

        let mid = local.mids[0];
        let offer = local.sdp;
        assert!(offer.contains(&format!("a=mid:{mid}")));

        let mut client = Rtc::builder()
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .build(Instant::now());
        client.add_local_candidate(
            Candidate::host(SocketAddr::from(([127, 0, 0, 1], 40001)), "udp")
                .expect("client host candidate"),
        );

        let answer = client
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer).expect("our offer parses"))
            .expect("client answers our offer");

        peer.handle
            .accept_answer(answer.to_sdp_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn closing_a_peer_deregisters_it_from_the_router() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();
        let ufrag = peer.handle.ice_creds().ufrag;

        assert_eq!(udp.router().session_count(), 1);

        peer.handle.close();
        peer.task.await.unwrap();

        assert_eq!(udp.router().session_count(), 0);
        assert_eq!(udp.router().address_count(), 0);
        assert_eq!(udp.router().session_for_ufrag(&ufrag), None);
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_the_peer_and_frees_the_ufrag() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();
        let ufrag = peer.handle.ice_creds().ufrag;

        let SpawnedPeer {
            handle,
            events: _events,
            task,
        } = peer;
        drop(handle);
        task.await.unwrap();

        assert_eq!(udp.router().session_for_ufrag(&ufrag), None);
    }

    #[tokio::test]
    async fn refuses_to_start_without_a_usable_advertised_address() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(RouterConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            advertise: vec![SocketAddr::from(([0, 0, 0, 0], 7881))],
            ..RouterConfig::default()
        })
        .await
        .unwrap();

        let error = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap_err();

        assert!(matches!(error, TransportError::NoLocalCandidate));
        assert_eq!(udp.router().session_count(), 0);
    }

    #[tokio::test]
    async fn an_empty_trickle_candidate_is_ignored() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::publisher()).unwrap();

        peer.handle
            .add_remote_candidate(String::new())
            .await
            .unwrap();

        let error = peer
            .handle
            .add_remote_candidate("not a candidate".to_owned())
            .await
            .unwrap_err();

        assert!(matches!(error, TransportError::Candidate(_)));
    }

    #[tokio::test]
    async fn a_flapping_ice_connection_still_expires() {
        let mut ice = IceWatch::new(Duration::from_secs(10));
        let start = Instant::now();

        ice.observe(IceConnectionState::Disconnected, start);
        ice.observe(IceConnectionState::Checking, start + Duration::from_secs(1));
        ice.observe(
            IceConnectionState::Disconnected,
            start + Duration::from_secs(2),
        );

        assert!(
            ice.expired(start + Duration::from_secs(11)),
            "a peer that keeps flapping must still be reaped"
        );

        ice.observe(
            IceConnectionState::Connected,
            start + Duration::from_secs(12),
        );
        assert_eq!(ice.deadline(), None);
    }

    #[tokio::test]
    async fn a_refused_rekey_closes_the_session() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, _events) =
            peer_actor(&crypto, &udp, PeerConfig::publisher()).await;
        let (occupier, _occupier_actor, _occupier_events) =
            peer_actor(&crypto, &udp, PeerConfig::publisher()).await;

        let taken = occupier.ice_creds();
        actor
            .rtc
            .direct_api()
            .set_local_ice_credentials(taken.clone());

        let error = actor
            .sync_ice_credentials()
            .expect_err("the rekey is refused");

        assert!(matches!(error, TransportError::RouteRekey(ufrag) if ufrag == taken.ufrag));
        assert!(
            !actor.rtc.is_alive(),
            "a session the router cannot reach must not stay alive"
        );
    }

    #[tokio::test]
    async fn a_transient_ice_disconnect_keeps_the_session_alive() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, mut events) = peer_actor(
            &crypto,
            &udp,
            PeerConfig {
                ice_disconnected_grace: Duration::from_secs(30),
                ..PeerConfig::publisher()
            },
        )
        .await;

        let start = Instant::now();

        assert!(actor.on_event(Event::IceConnectionStateChange(
            IceConnectionState::Disconnected
        )));
        assert!(
            actor.rtc.is_alive(),
            "a transient ice disconnect must not close the rtc session"
        );
        assert!(actor.ice.deadline().is_some());
        assert!(!actor.ice.expired(start + Duration::from_secs(29)));

        assert!(matches!(
            events.recv_control().await,
            Some(PeerEvent::IceConnectionState(
                IceConnectionState::Disconnected
            ))
        ));

        assert!(actor.on_event(Event::IceConnectionStateChange(
            IceConnectionState::Connected
        )));
        assert!(actor.rtc.is_alive());
        assert_eq!(actor.ice.deadline(), None);
        assert!(!actor.ice.expired(start + Duration::from_secs(600)));
    }

    #[tokio::test]
    async fn an_ice_disconnect_that_never_recovers_expires() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, _events) = peer_actor(
            &crypto,
            &udp,
            PeerConfig {
                ice_disconnected_grace: Duration::from_millis(50),
                ..PeerConfig::publisher()
            },
        )
        .await;

        let start = Instant::now();
        actor.on_event(Event::IceConnectionStateChange(
            IceConnectionState::Disconnected,
        ));

        assert!(!actor.ice.expired(start));
        assert!(actor.ice.expired(start + Duration::from_millis(51)));
        assert!(actor.rtc.is_alive());
    }

    #[tokio::test]
    async fn the_ice_grace_period_survives_repeated_disconnected_reports() {
        let mut ice = IceWatch::new(Duration::from_secs(10));
        let start = Instant::now();

        ice.observe(IceConnectionState::Disconnected, start);
        ice.observe(
            IceConnectionState::Disconnected,
            start + Duration::from_secs(9),
        );

        assert_eq!(ice.deadline(), Some(start + Duration::from_secs(10)));
        assert!(ice.expired(start + Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn a_full_media_queue_drops_media_and_never_blocks_control() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (handle, mut actor, mut events) = peer_actor(
            &crypto,
            &udp,
            PeerConfig {
                media_queue: 4,
                ..PeerConfig::publisher()
            },
        )
        .await;

        let filled = tokio::time::timeout(Duration::from_secs(1), async {
            for _ in 0..64 {
                assert!(actor.sink.emit_media(PeerEvent::Connected));
            }
        })
        .await;
        assert!(filled.is_ok(), "emitting media must never block the actor");

        assert_eq!(handle.dropped_media_events(), 60);

        let control = tokio::time::timeout(Duration::from_secs(1), async {
            assert!(actor.sink.emit_control(PeerEvent::Connected));
            events.recv_control().await
        })
        .await
        .expect("control events flow while the media queue is full");

        assert!(matches!(control, Some(PeerEvent::Connected)));
    }
}
