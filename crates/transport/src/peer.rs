use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpApi, SdpOffer, SdpPendingOffer};
use str0m::format::{Codec, PayloadParams};
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaAdded, MediaChanged, MediaData,
    MediaKind, Mid, Pt, Rid,
};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpPacket, RtpWrite, SeqNo, Ssrc};
use str0m::stats::MediaEgressStats;
use str0m::{Candidate, Event, IceConnectionState, IceCreds, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::crypto::TransportCrypto;
use crate::error::TransportError;
use crate::negotiation::{
    LocalOffer, NEGOTIATION_BACKLOG, NEGOTIATION_TIMEOUT, Negotiation, Queued, Renegotiation,
};
use crate::router::{Datagram, DatagramRx, SessionId, SessionRoute, UdpRouter};
use crate::send::{DeclaredStream, OutgoingRtp, RtxCache, SendStream};
use crate::simulcast::{
    IngressReport, KeyframeProbe, KeyframeTarget, LAYER_SILENCE_THRESHOLD, LayerActivity, LayerSet,
    LayerStats, LayerTable,
};

pub const COMMAND_QUEUE_CAPACITY: usize = 64;
pub const MEDIA_QUEUE_CAPACITY: usize = 512;
pub const WRITE_QUEUE_CAPACITY: usize = 1024;
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
    pub write_queue: usize,
    pub ice_disconnected_grace: Duration,
    pub negotiation_timeout: Duration,
    pub negotiation_backlog: usize,
    pub layer_silence: Duration,
    pub keyframe_probe: Option<KeyframeProbe>,
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
            write_queue: WRITE_QUEUE_CAPACITY,
            ice_disconnected_grace: ICE_DISCONNECTED_GRACE,
            negotiation_timeout: NEGOTIATION_TIMEOUT,
            negotiation_backlog: NEGOTIATION_BACKLOG,
            layer_silence: LAYER_SILENCE_THRESHOLD,
            keyframe_probe: None,
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
pub struct MediaInfo {
    pub mid: Mid,
    pub kind: MediaKind,
    pub direction: Direction,
    pub rids: Vec<Rid>,
    pub receive_layers: LayerSet,
    pub send_layers: LayerSet,
    pub params: Vec<PayloadParams>,
}

impl MediaInfo {
    pub fn is_simulcast(&self) -> bool {
        self.rids.len() > 1
    }
}

#[derive(Debug)]
pub struct IncomingRtp {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub carried_mid_extension: bool,
    pub packet: RtpPacket,
}

#[derive(Debug, Clone, Copy)]
pub struct RemoteIngress {
    pub jitter: u32,
    pub packets_lost: u64,
    pub max_seq_no: SeqNo,
}

#[derive(Debug, Clone, Copy)]
pub struct EgressReport {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub rtt: Option<Duration>,
    pub loss: Option<f32>,
    pub packets: u64,
    pub bytes: u64,
    pub plis: u64,
    pub firs: u64,
    pub nacks: u64,
    pub remote: Option<RemoteIngress>,
    pub at: Instant,
}

impl From<MediaEgressStats> for EgressReport {
    fn from(stats: MediaEgressStats) -> Self {
        Self {
            mid: stats.mid,
            rid: stats.rid,
            rtt: stats.rtt,
            loss: stats.loss,
            packets: stats.packets,
            bytes: stats.bytes,
            plis: stats.plis,
            firs: stats.firs,
            nacks: stats.nacks,
            remote: stats.remote.map(|remote| RemoteIngress {
                jitter: remote.jitter,
                packets_lost: remote.packets_lost,
                max_seq_no: remote.maximum_sequence_number,
            }),
            at: stats.timestamp,
        }
    }
}

#[derive(Debug)]
pub enum PeerEvent {
    IceConnectionState(IceConnectionState),
    Connected,
    MediaAdded(MediaInfo),
    MediaChanged(MediaInfo),
    NegotiationFailed,
    Rtp(Box<IncomingRtp>),
    Media(Box<MediaData>),
    KeyframeRequested(KeyframeRequest),
    EgressReport(EgressReport),
    IngressReport(IngressReport),
    LayerActivity(LayerActivity),
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
        target: KeyframeTarget,
        kind: KeyframeRequestKind,
    },
    SetLayerWanted {
        mid: Mid,
        rid: Option<Rid>,
        wanted: bool,
        reply: oneshot::Sender<Result<bool, TransportError>>,
    },
    LayerQuery {
        mid: Mid,
        reply: oneshot::Sender<Vec<LayerStats>>,
    },
    DeclareSendStream {
        stream: Box<SendStream>,
        reply: oneshot::Sender<Result<DeclaredStream, TransportError>>,
    },
    RemoveSendStream {
        mid: Mid,
        rid: Option<Rid>,
        reply: oneshot::Sender<bool>,
    },
    MediaQuery {
        mid: Mid,
        reply: oneshot::Sender<Option<MediaInfo>>,
    },
    Close,
}

#[derive(Debug, Clone)]
pub struct PeerHandle {
    id: SessionId,
    role: PeerRole,
    ice_creds: Arc<RwLock<IceCreds>>,
    dropped_media: Arc<AtomicU64>,
    dropped_writes: Arc<AtomicU64>,
    unroutable_writes: Arc<AtomicU64>,
    missed_keyframes: Arc<AtomicU64>,
    keyframes_without_stream: Arc<AtomicU64>,
    tx: mpsc::Sender<PeerCommand>,
    rtp: mpsc::Sender<Box<OutgoingRtp>>,
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

    pub fn dropped_rtp_writes(&self) -> u64 {
        self.dropped_writes.load(Ordering::Relaxed)
    }

    pub fn unroutable_rtp_writes(&self) -> u64 {
        self.unroutable_writes.load(Ordering::Relaxed)
    }

    pub fn missed_keyframe_requests(&self) -> u64 {
        self.missed_keyframes.load(Ordering::Relaxed)
    }

    pub fn keyframe_requests_without_stream(&self) -> u64 {
        self.keyframes_without_stream.load(Ordering::Relaxed)
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
        self.notify(PeerCommand::RequestKeyframe {
            mid,
            target: KeyframeTarget::Layer(rid),
            kind,
        })
    }

    pub fn request_keyframe_on_every_layer(
        &self,
        mid: Mid,
        kind: KeyframeRequestKind,
    ) -> Result<(), TransportError> {
        self.notify(PeerCommand::RequestKeyframe {
            mid,
            target: KeyframeTarget::EveryLayer,
            kind,
        })
    }

    pub async fn set_layer_wanted(
        &self,
        mid: Mid,
        rid: Option<Rid>,
        wanted: bool,
    ) -> Result<bool, TransportError> {
        self.request(|reply| PeerCommand::SetLayerWanted {
            mid,
            rid,
            wanted,
            reply,
        })
        .await?
    }

    pub async fn pause_layer(&self, mid: Mid, rid: Option<Rid>) -> Result<bool, TransportError> {
        self.set_layer_wanted(mid, rid, false).await
    }

    pub async fn resume_layer(&self, mid: Mid, rid: Option<Rid>) -> Result<bool, TransportError> {
        self.set_layer_wanted(mid, rid, true).await
    }

    pub async fn layers(&self, mid: Mid) -> Result<Vec<LayerStats>, TransportError> {
        self.request(|reply| PeerCommand::LayerQuery { mid, reply })
            .await
    }

    pub async fn layer(
        &self,
        mid: Mid,
        rid: Option<Rid>,
    ) -> Result<Option<LayerStats>, TransportError> {
        Ok(self
            .layers(mid)
            .await?
            .into_iter()
            .find(|layer| layer.rid == rid))
    }

    pub async fn declare_send_stream(
        &self,
        stream: SendStream,
    ) -> Result<DeclaredStream, TransportError> {
        self.request(|reply| PeerCommand::DeclareSendStream {
            stream: Box::new(stream),
            reply,
        })
        .await?
    }

    pub async fn remove_send_stream(
        &self,
        mid: Mid,
        rid: Option<Rid>,
    ) -> Result<bool, TransportError> {
        self.request(|reply| PeerCommand::RemoveSendStream { mid, rid, reply })
            .await
    }

    pub async fn media(&self, mid: Mid) -> Result<MediaInfo, TransportError> {
        self.request(|reply| PeerCommand::MediaQuery { mid, reply })
            .await?
            .ok_or(TransportError::UnknownMid(mid))
    }

    pub fn write_rtp(&self, packet: OutgoingRtp) -> Result<(), TransportError> {
        match self.rtp.try_send(Box::new(packet)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::PeerGone),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_writes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
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
    let (rtp_tx, writes) = mpsc::channel(config.write_queue.max(1));
    let (control_tx, control) = mpsc::unbounded_channel();
    let (media_tx, media) = mpsc::channel(config.media_queue.max(1));

    let creds = Arc::new(RwLock::new(ice_creds));
    let dropped_media = Arc::new(AtomicU64::new(0));
    let dropped_writes = Arc::new(AtomicU64::new(0));
    let unroutable_writes = Arc::new(AtomicU64::new(0));
    let missed_keyframes = Arc::new(AtomicU64::new(0));
    let keyframes_without_stream = Arc::new(AtomicU64::new(0));

    let handle = PeerHandle {
        id: route.id(),
        role: config.role,
        ice_creds: Arc::clone(&creds),
        dropped_media: Arc::clone(&dropped_media),
        dropped_writes,
        unroutable_writes: Arc::clone(&unroutable_writes),
        missed_keyframes: Arc::clone(&missed_keyframes),
        keyframes_without_stream: Arc::clone(&keyframes_without_stream),
        tx,
        rtp: rtp_tx,
    };

    let actor = PeerActor {
        rtc,
        route,
        socket: udp.socket(),
        local_addr: receiving_addr(udp.local_addr(), &advertise),
        datagrams,
        commands,
        writes,
        unroutable_writes,
        sink: EventSink {
            control: control_tx,
            media: media_tx,
            dropped: dropped_media,
            reported: 0,
            reported_at: None,
        },
        creds,
        ice: IceWatch::new(config.ice_disconnected_grace),
        renegotiation: Renegotiation::new(config.negotiation_timeout, config.negotiation_backlog),
        layers: LayerTable::default(),
        codecs: HashMap::new(),
        probe: config.keyframe_probe.clone(),
        layer_silence: config.layer_silence,
        missed_keyframes,
        keyframes_without_stream,
        drain_deadline: None,
        commands_closed: false,
        writes_closed: false,
    };

    let events = PeerEvents {
        control,
        media,
        control_open: true,
        media_open: true,
    };

    Ok((handle, actor, events))
}

fn receiving_addr(bound: SocketAddr, advertise: &[SocketAddr]) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return bound;
    }

    advertise
        .iter()
        .find(|addr| !addr.ip().is_unspecified())
        .copied()
        .unwrap_or(bound)
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
    writes: mpsc::Receiver<Box<OutgoingRtp>>,
    unroutable_writes: Arc<AtomicU64>,
    sink: EventSink,
    creds: Arc<RwLock<IceCreds>>,
    ice: IceWatch,
    renegotiation: Renegotiation,
    layers: LayerTable,
    codecs: HashMap<Pt, Codec>,
    probe: Option<KeyframeProbe>,
    layer_silence: Duration,
    missed_keyframes: Arc<AtomicU64>,
    keyframes_without_stream: Arc<AtomicU64>,
    drain_deadline: Option<Instant>,
    commands_closed: bool,
    writes_closed: bool,
}

impl PeerActor {
    async fn run(mut self) {
        tracing::debug!(ufrag = self.route.ufrag(), "peer connection started");

        loop {
            self.expire_negotiation(Instant::now());

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

            let deadline = match self.renegotiation.deadline() {
                Some(offer) => deadline.min(offer),
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

                packet = self.writes.recv(), if !self.writes_closed => match packet {
                    Some(packet) => self.write_rtp(*packet),
                    None => self.writes_closed = true,
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
                self.advance_negotiation();
            }
            PeerCommand::Negotiate { request, reply } => {
                self.renegotiation.submit(request, reply);
                self.advance_negotiation();
            }
            PeerCommand::AcceptAnswer { sdp, reply } => {
                let _ = reply.send(self.accept_answer(&sdp));
                self.advance_negotiation();
            }
            PeerCommand::AddRemoteCandidate { candidate, reply } => {
                let _ = reply.send(self.add_remote_candidate(&candidate));
            }
            PeerCommand::RequestKeyframe { mid, target, kind } => {
                self.request_keyframe(mid, target, kind);
            }
            PeerCommand::SetLayerWanted {
                mid,
                rid,
                wanted,
                reply,
            } => {
                let _ = reply.send(self.set_layer_wanted(mid, rid, wanted));
            }
            PeerCommand::LayerQuery { mid, reply } => {
                let _ = reply.send(self.layers.snapshot(mid));
            }
            PeerCommand::DeclareSendStream { stream, reply } => {
                let _ = reply.send(self.declare_send_stream(*stream));
            }
            PeerCommand::RemoveSendStream { mid, rid, reply } => {
                let _ = reply.send(self.remove_send_stream(mid, rid));
            }
            PeerCommand::MediaQuery { mid, reply } => {
                let _ = reply.send(self.media_by_mid(mid));
            }
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

        self.renegotiation
            .abandon("the remote offered while our own offer was outstanding");

        let answer: SdpAnswer = self.rtc.sdp_api().accept_offer(offer)?;
        self.sync_ice_credentials()?;

        Ok(answer.to_sdp_string())
    }

    fn advance_negotiation(&mut self) {
        while let Some(Queued { request, reply }) = self.renegotiation.next() {
            match self.create_offer(*request) {
                Ok((offer, pending)) => {
                    self.renegotiation.hold(pending, Instant::now());
                    let _ = reply.send(Ok(offer));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
    }

    fn expire_negotiation(&mut self, now: Instant) {
        if self.renegotiation.has_failed() || !self.renegotiation.expired(now) {
            return;
        }

        if self
            .renegotiation
            .fail("the remote never answered our offer")
        {
            self.sink.emit_control(PeerEvent::NegotiationFailed);
        }
    }

    fn create_offer(
        &mut self,
        request: Negotiation,
    ) -> Result<(LocalOffer, SdpPendingOffer), TransportError> {
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

        Ok((
            LocalOffer {
                mids,
                sdp: offer.to_sdp_string(),
            },
            pending,
        ))
    }

    fn accept_answer(&mut self, sdp: &str) -> Result<(), TransportError> {
        if !self.renegotiation.is_pending() {
            return Err(TransportError::NoPendingOffer);
        }

        let answer = SdpAnswer::from_sdp_string(sdp).map_err(str0m::RtcError::from)?;
        let pending = self
            .renegotiation
            .take()
            .ok_or(TransportError::NoPendingOffer)?;

        self.rtc.sdp_api().accept_answer(pending, answer)?;
        self.sync_ice_credentials()?;

        Ok(())
    }

    fn declare_send_stream(&mut self, spec: SendStream) -> Result<DeclaredStream, TransportError> {
        let kind = self
            .rtc
            .media(spec.mid)
            .map(|media| media.kind())
            .ok_or(TransportError::UnknownMid(spec.mid))?;

        let cache = spec.rtx_cache.unwrap_or_else(|| RtxCache::for_kind(kind));
        let unpaced = spec.unpaced.unwrap_or(kind.is_audio());

        let mut direct = self.rtc.direct_api();

        let negotiated = direct
            .stream_tx_by_mid(spec.mid, spec.rid)
            .map(|stream| (stream.ssrc(), stream.rtx()));

        let (ssrc, rtx) = match negotiated {
            Some(pair) => pair,
            None => {
                let ssrc = spec.ssrc.unwrap_or_else(|| direct.new_ssrc());
                let rtx = match spec.rtx {
                    Some(rtx) => Some(rtx),
                    None if kind.is_video() => Some(direct.new_ssrc()),
                    None => None,
                };

                (ssrc, rtx)
            }
        };

        let stream = direct.declare_stream_tx(ssrc, rtx, spec.mid, spec.rid);
        stream.set_rtx_cache(cache.packets, cache.age, cache.ratio_cap);
        stream.set_unpaced(unpaced);

        let declared = DeclaredStream {
            mid: spec.mid,
            rid: spec.rid,
            ssrc,
            rtx,
            negotiated: negotiated.is_some(),
        };

        tracing::debug!(
            mid = %declared.mid,
            rid = ?declared.rid,
            ssrc = ?declared.ssrc,
            rtx = ?declared.rtx,
            negotiated = declared.negotiated,
            unpaced,
            "declared a send stream"
        );

        Ok(declared)
    }

    fn remove_send_stream(&mut self, mid: Mid, rid: Option<Rid>) -> bool {
        let mut direct = self.rtc.direct_api();

        let Some(ssrc) = direct
            .stream_tx_by_mid(mid, rid)
            .map(|stream| stream.ssrc())
        else {
            return false;
        };

        direct.remove_stream_tx(ssrc)
    }

    fn media_by_mid(&self, mid: Mid) -> Option<MediaInfo> {
        let media = self.rtc.media(mid)?;
        let direction = media.direction();

        Some(self.media_layers(mid, media.kind(), direction))
    }

    fn media_layers(&self, mid: Mid, kind: MediaKind, direction: Direction) -> MediaInfo {
        let (receive_layers, send_layers) = match self.rtc.media(mid) {
            Some(media) => (
                LayerSet::from(media.rids_rx()),
                LayerSet::from(media.rids_tx()),
            ),
            None => (LayerSet::None, LayerSet::None),
        };

        let carried = if direction.is_receiving() {
            &receive_layers
        } else {
            &send_layers
        };

        MediaInfo {
            mid,
            kind,
            direction,
            rids: carried.rids().to_vec(),
            receive_layers,
            send_layers,
            params: self.negotiated_params(mid),
        }
    }

    fn receive_layers(&self, mid: Mid) -> LayerSet {
        self.rtc
            .media(mid)
            .map(|media| LayerSet::from(media.rids_rx()))
            .unwrap_or_default()
    }

    fn negotiated_params(&self, mid: Mid) -> Vec<PayloadParams> {
        let Some(media) = self.rtc.media(mid) else {
            return Vec::new();
        };

        let config = self.rtc.codec_config();

        media
            .remote_pts()
            .iter()
            .filter_map(|pt| config.params().iter().find(|params| params.pt() == *pt))
            .cloned()
            .collect()
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

    fn resolve_layer(&self, mid: Mid, rid: Option<Rid>) -> Result<Option<Rid>, TransportError> {
        if self.rtc.media(mid).is_none() {
            return Err(TransportError::UnknownMid(mid));
        }

        let negotiated = self.receive_layers(mid);

        match rid {
            Some(rid) if negotiated.is_specific() && !negotiated.contains(rid) => {
                Err(TransportError::UnknownLayer { mid, rid })
            }
            Some(rid) => Ok(Some(rid)),
            None => match negotiated.rids() {
                [] => Ok(None),
                [only] => Ok(Some(*only)),
                _ => Err(TransportError::AmbiguousLayer(mid)),
            },
        }
    }

    fn keyframe_layers(
        &self,
        mid: Mid,
        target: KeyframeTarget,
    ) -> Result<Vec<Option<Rid>>, TransportError> {
        match target {
            KeyframeTarget::Layer(rid) => Ok(vec![self.resolve_layer(mid, rid)?]),
            KeyframeTarget::EveryLayer => {
                if self.rtc.media(mid).is_none() {
                    return Err(TransportError::UnknownMid(mid));
                }

                match self.receive_layers(mid).rids() {
                    [] => Ok(vec![None]),
                    rids => Ok(rids.iter().copied().map(Some).collect()),
                }
            }
        }
    }

    fn request_keyframe(&mut self, mid: Mid, target: KeyframeTarget, kind: KeyframeRequestKind) {
        let layers = match self.keyframe_layers(mid, target) {
            Ok(layers) => layers,
            Err(error) => {
                self.missed_keyframes.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%mid, ?target, %error, "refusing an unaimed keyframe request");
                return;
            }
        };

        for rid in layers {
            self.request_keyframe_on(mid, rid, kind);
        }
    }

    fn request_keyframe_on(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        if !self.layers.wanted(mid, rid) {
            tracing::debug!(%mid, ?rid, "not asking a layer we paused for a keyframe");
            return;
        }

        let asked = match self.rtc.direct_api().stream_rx_by_mid(mid, rid) {
            Some(stream) if rid.is_none() || stream.rid() == rid => {
                stream.request_keyframe(kind);
                true
            }
            _ => false,
        };

        if !asked {
            self.keyframes_without_stream
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%mid, ?rid, "no receive stream to request a keyframe on yet");
            return;
        }

        self.layers.note_keyframe_request(mid, rid, kind);
    }

    fn set_layer_wanted(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        wanted: bool,
    ) -> Result<bool, TransportError> {
        let rid = self.resolve_layer(mid, rid)?;
        let changed = self.layers.set_wanted(mid, rid, wanted);

        self.configure_layer(mid, rid);

        if changed {
            tracing::info!(%mid, ?rid, wanted, "layer demand changed");
        }

        if wanted && changed {
            self.request_keyframe_on(mid, rid, KeyframeRequestKind::Pli);
        }

        Ok(changed)
    }

    fn configure_layer(&mut self, mid: Mid, rid: Option<Rid>) {
        let suppress = !self.layers.wanted(mid, rid);
        let silence = self.layer_silence;

        let applied = match self.rtc.direct_api().stream_rx_by_mid(mid, rid) {
            Some(stream) if rid.is_none() || stream.rid() == rid => {
                stream.set_pause_threshold(silence);
                stream.suppress_nack(suppress);
                true
            }
            _ => false,
        };

        if applied {
            self.layers.note_nack_suppressed(mid, rid, suppress);
        }
    }

    fn codec_for(&mut self, pt: Pt) -> Option<Codec> {
        if let Some(codec) = self.codecs.get(&pt) {
            return Some(*codec);
        }

        let codec = self
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|params| params.pt() == pt)
            .map(|params| params.spec().codec)?;

        self.codecs.insert(pt, codec);

        Some(codec)
    }

    fn carries_keyframe(&mut self, packet: &RtpPacket) -> bool {
        if self.probe.is_none() {
            return false;
        }

        let Some(codec) = self.codec_for(packet.header.payload_type) else {
            return false;
        };

        self.probe
            .as_ref()
            .is_some_and(|probe| probe.probe(codec, &packet.payload))
    }

    fn note_intake(&mut self, mid: Mid, rid: Option<Rid>, ssrc: Ssrc, packet: &RtpPacket) {
        let keyframe = self.carries_keyframe(packet);
        let bytes = packet.payload.len() as u64;

        let fresh = self
            .layers
            .observe(mid, rid, ssrc, bytes, keyframe, packet.timestamp);

        if fresh {
            tracing::debug!(%mid, ?rid, ?ssrc, "a layer started arriving");
            self.configure_layer(mid, rid);
        }
    }

    fn write_rtp(&mut self, packet: OutgoingRtp) {
        let mut direct = self.rtc.direct_api();

        let Some(stream) = direct.stream_tx_by_mid(packet.mid, packet.rid) else {
            self.unroutable_writes.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(mid = %packet.mid, rid = ?packet.rid, "no send stream declared");
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
            Event::MediaChanged(changed) => {
                if changed.direction == Direction::Inactive {
                    self.layers.forget(changed.mid);
                }

                match self.media_changed(changed) {
                    Some(info) => self.sink.emit_control(PeerEvent::MediaChanged(info)),
                    None => true,
                }
            }
            Event::RtpPacket(packet) => {
                let ssrc = packet.header.ssrc;
                let carried_mid_extension = packet.header.ext_vals.mid.is_some();

                let Some((mid, rid)) = self.stream_midrid(&packet) else {
                    tracing::trace!(?ssrc, "rtp packet on an unmapped stream");
                    return true;
                };

                self.note_intake(mid, rid, ssrc, &packet);

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
            Event::MediaEgressStats(stats) => self
                .sink
                .emit_control(PeerEvent::EgressReport(stats.into())),
            Event::MediaIngressStats(stats) => {
                let report = IngressReport::from(stats);
                self.layers.merge_ingress(&report);
                self.sink.emit_control(PeerEvent::IngressReport(report))
            }
            Event::StreamPaused(paused) => {
                let activity = LayerActivity {
                    mid: paused.mid,
                    rid: paused.rid,
                    ssrc: paused.ssrc,
                    active: !paused.paused,
                };

                self.layers.note_activity(activity, Instant::now());

                tracing::debug!(
                    mid = %activity.mid,
                    rid = ?activity.rid,
                    ssrc = ?activity.ssrc,
                    active = activity.active,
                    "layer activity changed"
                );

                self.sink.emit_control(PeerEvent::LayerActivity(activity))
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

    fn media_changed(&self, changed: MediaChanged) -> Option<MediaInfo> {
        let mut info = self.media_by_mid(changed.mid)?;
        info.direction = changed.direction;

        Some(info)
    }

    fn media_info(&self, added: MediaAdded) -> MediaInfo {
        self.media_layers(added.mid, added.kind, added.direction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::negotiation::MediaSpec;
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

    #[test]
    fn a_wildcard_socket_reports_the_advertised_address_to_the_session() {
        let bound = SocketAddr::from(([0, 0, 0, 0], 7882));
        let advertised = SocketAddr::from(([203, 0, 113, 7], 7882));

        assert_eq!(
            receiving_addr(bound, &[advertised]),
            advertised,
            "a packet that arrived on a wildcard socket must be attributed to the candidate we \
             advertised, or ice never finds a pair and stays in checking for ever"
        );

        let explicit = SocketAddr::from(([127, 0, 0, 1], 7882));
        assert_eq!(receiving_addr(explicit, &[advertised]), explicit);
        assert_eq!(receiving_addr(bound, &[]), bound);
        assert_eq!(
            receiving_addr(bound, &[SocketAddr::from(([0, 0, 0, 0], 7882)), advertised]),
            advertised
        );
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

    fn answering_client(crypto: &TransportCrypto, port: u16) -> Rtc {
        let mut client = Rtc::builder()
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .build(Instant::now());

        client.add_local_candidate(
            Candidate::host(SocketAddr::from(([127, 0, 0, 1], port)), "udp")
                .expect("client host candidate"),
        );

        client
    }

    fn outgoing(kind: MediaKind, track: &str) -> Negotiation {
        Negotiation::add(MediaSpec {
            kind,
            direction: Direction::SendOnly,
            stream_id: Some("stream".to_owned()),
            track_id: Some(track.to_owned()),
            simulcast: None,
        })
    }

    #[tokio::test]
    async fn declaring_a_send_stream_for_an_unknown_mid_is_refused() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber()).unwrap();

        let missing = Mid::from("nosuch");
        let error = peer
            .handle
            .declare_send_stream(SendStream::new(missing))
            .await
            .expect_err("a mid the session never had cannot carry a stream");

        assert!(matches!(error, TransportError::UnknownMid(mid) if mid == missing));

        assert!(matches!(
            peer.handle.media(missing).await,
            Err(TransportError::UnknownMid(_))
        ));
        assert!(!peer.handle.remove_send_stream(missing, None).await.unwrap());

        peer.handle
            .add_remote_candidate(String::new())
            .await
            .expect("the peer survives the refusal and keeps answering commands");
    }

    #[tokio::test]
    async fn a_declared_stream_keeps_the_ssrc_that_was_offered() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber()).unwrap();

        let local = peer
            .handle
            .negotiate(outgoing(MediaKind::Video, "video"))
            .await
            .unwrap();
        let mid = local.mids[0];

        let mut client = answering_client(&crypto, 40100);
        let answer = client
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&local.sdp).expect("our offer parses"))
            .expect("the client answers");
        peer.handle
            .accept_answer(answer.to_sdp_string())
            .await
            .unwrap();

        let declared = peer
            .handle
            .declare_send_stream(SendStream::new(mid))
            .await
            .expect("the negotiated mid accepts a send stream");

        assert_eq!(declared.mid, mid);
        assert!(declared.negotiated, "the sdp already allocated the ssrc");
        assert!(declared.rtx.is_some(), "video needs an rtx ssrc to resend");
        assert!(
            local.sdp.contains(&format!("a=ssrc:{}", *declared.ssrc)),
            "the declared ssrc must be the one we put in the offer"
        );
        assert!(
            local
                .sdp
                .contains(&format!("a=ssrc:{}", *declared.rtx.unwrap()))
        );

        let media = peer.handle.media(mid).await.unwrap();
        assert_eq!(media.kind, MediaKind::Video);
        assert_eq!(media.direction, Direction::SendOnly);
        assert!(
            !media.params.is_empty(),
            "the answer must leave us at least one payload type"
        );

        assert!(peer.handle.remove_send_stream(mid, None).await.unwrap());
        assert!(!peer.handle.remove_send_stream(mid, None).await.unwrap());
    }

    #[tokio::test]
    async fn a_second_negotiation_waits_for_the_answer_to_the_first() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, _events) =
            peer_actor(&crypto, &udp, PeerConfig::subscriber()).await;

        let (reply, first) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "video")),
            reply,
        });
        let first = first.await.unwrap().expect("the first offer is created");

        let (reply, mut second) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Audio, "audio")),
            reply,
        });

        assert!(
            second.try_recv().is_err(),
            "a second offer must not be created while the first is unanswered"
        );
        assert_eq!(actor.renegotiation.queued(), 1);
        assert!(actor.renegotiation.is_pending());

        let mut client = answering_client(&crypto, 40101);
        let answer = client
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&first.sdp).expect("our offer parses"))
            .expect("the client answers the first offer");

        let (reply, applied) = oneshot::channel();
        actor.on_command(PeerCommand::AcceptAnswer {
            sdp: answer.to_sdp_string(),
            reply,
        });
        applied.await.unwrap().expect("the answer applies");

        let second = second.await.unwrap().expect("the queued offer runs next");

        assert_ne!(second.mids[0], first.mids[0]);
        assert_eq!(actor.renegotiation.queued(), 0);
        assert!(actor.renegotiation.is_pending());
        assert!(
            actor
                .declare_send_stream(SendStream::new(first.mids[0]))
                .is_ok(),
            "the stream from the first negotiation stays declarable"
        );
    }

    #[tokio::test]
    async fn an_unanswered_offer_is_held_rather_than_rolled_back() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, mut events) = peer_actor(
            &crypto,
            &udp,
            PeerConfig {
                negotiation_timeout: Duration::from_millis(10),
                ..PeerConfig::subscriber()
            },
        )
        .await;

        let (reply, first) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "video")),
            reply,
        });
        let first = first.await.unwrap().expect("the first offer is created");

        let (reply, second) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Audio, "audio")),
            reply,
        });

        actor.expire_negotiation(Instant::now() + Duration::from_millis(11));

        assert!(
            matches!(
                second.await.unwrap(),
                Err(TransportError::NegotiationFailed)
            ),
            "a queued offer must never take the place of one the remote may have applied"
        );
        assert!(
            actor.renegotiation.is_pending(),
            "the unanswered offer must stay held so str0m never releases mid {}",
            first.mids[0]
        );
        assert!(actor.renegotiation.has_failed());
        assert_eq!(
            actor.renegotiation.deadline(),
            None,
            "a failed negotiation must not keep waking the peer up"
        );

        let (reply, later) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "later")),
            reply,
        });
        assert!(matches!(
            later.await.unwrap(),
            Err(TransportError::NegotiationFailed)
        ));

        assert!(
            matches!(
                events.recv_control().await,
                Some(PeerEvent::NegotiationFailed)
            ),
            "the session must be told its negotiation is beyond repair"
        );
    }

    #[tokio::test]
    async fn a_late_answer_puts_the_session_back_in_business() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, _events) = peer_actor(
            &crypto,
            &udp,
            PeerConfig {
                negotiation_timeout: Duration::from_millis(10),
                ..PeerConfig::subscriber()
            },
        )
        .await;

        let (reply, first) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "video")),
            reply,
        });
        let first = first.await.unwrap().expect("the first offer is created");

        actor.expire_negotiation(Instant::now() + Duration::from_millis(11));
        assert!(actor.renegotiation.has_failed());

        let mut client = answering_client(&crypto, 40102);
        let answer = client
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&first.sdp).expect("our offer parses"))
            .expect("the client answers late");

        let (reply, applied) = oneshot::channel();
        actor.on_command(PeerCommand::AcceptAnswer {
            sdp: answer.to_sdp_string(),
            reply,
        });
        applied
            .await
            .unwrap()
            .expect("the late answer still applies");

        assert!(!actor.renegotiation.has_failed());
        assert!(
            actor
                .declare_send_stream(SendStream::new(first.mids[0]))
                .is_ok(),
            "the media section the remote answered must be the one we offered"
        );
    }

    #[tokio::test]
    async fn a_backlog_of_renegotiations_is_refused_rather_than_queued_forever() {
        let crypto = TransportCrypto::new().unwrap();
        let udp = UdpRouter::bind(loopback_router_config()).await.unwrap();
        let (_handle, mut actor, _events) = peer_actor(
            &crypto,
            &udp,
            PeerConfig {
                negotiation_backlog: 1,
                ..PeerConfig::subscriber()
            },
        )
        .await;

        let (reply, first) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "one")),
            reply,
        });
        let _first = first.await.unwrap().expect("the first offer is created");

        let (reply, _queued) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "two")),
            reply,
        });

        let (reply, refused) = oneshot::channel();
        actor.on_command(PeerCommand::Negotiate {
            request: Box::new(outgoing(MediaKind::Video, "three")),
            reply,
        });

        assert!(matches!(
            refused.await.unwrap(),
            Err(TransportError::NegotiationBacklog)
        ));
    }
}
