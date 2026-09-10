use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpApi, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
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

use crate::bwe::{BweSettings, PeerReport, SendQueueEntry, clamp_desired};
use crate::crypto::TransportCrypto;
use crate::data::{
    DATA_QUEUE_CAPACITY, DataChannel, DataChannels, DataSettings, IncomingData, OutgoingData,
};
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
pub const PUBLISHER_STATS_INTERVAL: Duration = Duration::from_secs(2);
pub const SUBSCRIBER_STATS_INTERVAL: Duration = Duration::from_secs(1);

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
    pub data_queue: usize,
    pub ice_disconnected_grace: Duration,
    pub negotiation_timeout: Duration,
    pub negotiation_backlog: usize,
    pub layer_silence: Duration,
    pub keyframe_probe: Option<KeyframeProbe>,
    pub bwe: Option<BweSettings>,
    pub data: Option<DataSettings>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            role: PeerRole::Publisher,
            advertise: Vec::new(),
            ice_lite: true,
            rtp_mode: true,
            stats_interval: Some(PUBLISHER_STATS_INTERVAL),
            command_queue: COMMAND_QUEUE_CAPACITY,
            media_queue: MEDIA_QUEUE_CAPACITY,
            write_queue: WRITE_QUEUE_CAPACITY,
            data_queue: DATA_QUEUE_CAPACITY,
            ice_disconnected_grace: ICE_DISCONNECTED_GRACE,
            negotiation_timeout: NEGOTIATION_TIMEOUT,
            negotiation_backlog: NEGOTIATION_BACKLOG,
            layer_silence: LAYER_SILENCE_THRESHOLD,
            keyframe_probe: None,
            bwe: None,
            data: None,
        }
    }
}

impl PeerConfig {
    pub fn publisher() -> Self {
        Self::default()
    }

    pub fn subscriber(bwe: Option<BweSettings>) -> Self {
        Self {
            role: PeerRole::Subscriber,
            stats_interval: Some(SUBSCRIBER_STATS_INTERVAL),
            bwe,
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
    PeerReport(Box<PeerReport>),
    BitrateEstimate { bitrate: Bitrate, at: Instant },
    RembRequested { mid: Mid, bitrate: Bitrate },
    LayerActivity(LayerActivity),
    ChannelOpen(DataChannel),
    Data(IncomingData),
    ChannelClose(DataChannel),
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
    SetDesiredBitrate(Bitrate),
    ResetBwe(Bitrate),
    QueueQuery {
        reply: oneshot::Sender<Vec<SendQueueEntry>>,
    },
    Close,
}

#[derive(Debug, Clone)]
pub struct PeerHandle {
    id: SessionId,
    role: PeerRole,
    bwe: Option<BweSettings>,
    data: Option<DataSettings>,
    ice_creds: Arc<RwLock<IceCreds>>,
    dropped_media: Arc<AtomicU64>,
    dropped_writes: Arc<AtomicU64>,
    unroutable_writes: Arc<AtomicU64>,
    missed_keyframes: Arc<AtomicU64>,
    keyframes_without_stream: Arc<AtomicU64>,
    dropped_data: Arc<AtomicU64>,
    incoming_data_dropped: Arc<AtomicU64>,
    unroutable_data: Arc<AtomicU64>,
    refused_data: Arc<AtomicU64>,
    oversized_data: Arc<AtomicU64>,
    tx: mpsc::Sender<PeerCommand>,
    rtp: mpsc::Sender<Box<OutgoingRtp>>,
    data_tx: mpsc::Sender<Box<OutgoingData>>,
}

impl PeerHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn role(&self) -> PeerRole {
        self.role
    }

    pub fn bwe(&self) -> Option<BweSettings> {
        self.bwe
    }

    pub fn estimates_bandwidth(&self) -> bool {
        self.bwe.is_some()
    }

    pub fn data(&self) -> Option<DataSettings> {
        self.data
    }

    pub fn carries_data(&self) -> bool {
        self.data.is_some()
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

    pub fn dropped_data_writes(&self) -> u64 {
        self.dropped_data.load(Ordering::Relaxed)
    }

    pub fn dropped_incoming_data(&self) -> u64 {
        self.incoming_data_dropped.load(Ordering::Relaxed)
    }

    pub fn unroutable_data_writes(&self) -> u64 {
        self.unroutable_data.load(Ordering::Relaxed)
    }

    pub fn refused_data_writes(&self) -> u64 {
        self.refused_data.load(Ordering::Relaxed)
    }

    pub fn oversized_data_packets(&self) -> u64 {
        self.oversized_data.load(Ordering::Relaxed)
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

    pub fn set_desired_bitrate(&self, desired: Bitrate) -> Result<(), TransportError> {
        if self.bwe.is_none() {
            return Err(TransportError::BweDisabled);
        }

        self.notify(PeerCommand::SetDesiredBitrate(clamp_desired(desired)))
    }

    pub fn reset_bwe(&self, initial: Bitrate) -> Result<(), TransportError> {
        if self.bwe.is_none() {
            return Err(TransportError::BweDisabled);
        }

        self.notify(PeerCommand::ResetBwe(initial))
    }

    pub async fn send_queues(&self) -> Result<Vec<SendQueueEntry>, TransportError> {
        self.request(|reply| PeerCommand::QueueQuery { reply })
            .await
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

    pub fn write_data(&self, packet: OutgoingData) -> Result<(), TransportError> {
        let Some(settings) = self.data else {
            return Err(TransportError::DataDisabled);
        };

        if packet.payload.len() > settings.max_packet {
            self.oversized_data.fetch_add(1, Ordering::Relaxed);
            return Err(TransportError::DataPacketTooLarge {
                bytes: packet.payload.len(),
                limit: settings.max_packet,
            });
        }

        match self.data_tx.try_send(Box::new(packet)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::PeerGone),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_data.fetch_add(1, Ordering::Relaxed);
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
    data: mpsc::Receiver<PeerEvent>,
    control_open: bool,
    media_open: bool,
    data_open: bool,
}

impl PeerEvents {
    pub async fn recv(&mut self) -> Option<PeerEvent> {
        loop {
            if !self.control_open && !self.media_open && !self.data_open {
                return None;
            }

            tokio::select! {
                biased;

                control = self.control.recv(), if self.control_open => match control {
                    Some(event) => return Some(event),
                    None => self.control_open = false,
                },

                data = self.data.recv(), if self.data_open => match data {
                    Some(event) => return Some(event),
                    None => self.data_open = false,
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
            .field("data_open", &self.data_open)
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
    if config.bwe.is_some() && config.role != PeerRole::Subscriber {
        return Err(TransportError::BweOnPublisher);
    }

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
    let (data_tx, data_writes) = mpsc::channel(config.data_queue.max(1));
    let (incoming_data_tx, incoming_data) = mpsc::channel(config.data_queue.max(1));

    let creds = Arc::new(RwLock::new(ice_creds));
    let dropped_media = Arc::new(AtomicU64::new(0));
    let dropped_writes = Arc::new(AtomicU64::new(0));
    let unroutable_writes = Arc::new(AtomicU64::new(0));
    let missed_keyframes = Arc::new(AtomicU64::new(0));
    let keyframes_without_stream = Arc::new(AtomicU64::new(0));
    let dropped_data = Arc::new(AtomicU64::new(0));
    let incoming_data_dropped = Arc::new(AtomicU64::new(0));
    let unroutable_data = Arc::new(AtomicU64::new(0));
    let refused_data = Arc::new(AtomicU64::new(0));
    let oversized_data = Arc::new(AtomicU64::new(0));

    let handle = PeerHandle {
        id: route.id(),
        role: config.role,
        bwe: config.bwe,
        data: config.data,
        ice_creds: Arc::clone(&creds),
        dropped_media: Arc::clone(&dropped_media),
        dropped_writes,
        unroutable_writes: Arc::clone(&unroutable_writes),
        missed_keyframes: Arc::clone(&missed_keyframes),
        keyframes_without_stream: Arc::clone(&keyframes_without_stream),
        dropped_data,
        incoming_data_dropped: Arc::clone(&incoming_data_dropped),
        unroutable_data: Arc::clone(&unroutable_data),
        refused_data: Arc::clone(&refused_data),
        oversized_data: Arc::clone(&oversized_data),
        tx,
        rtp: rtp_tx,
        data_tx,
    };

    let actor = PeerActor {
        rtc,
        route,
        socket: udp.socket(),
        local_addr: receiving_addr(udp.local_addr(), &advertise),
        datagrams,
        commands,
        writes,
        data_writes,
        unroutable_writes,
        sink: EventSink {
            control: control_tx,
            media: media_tx,
            data: incoming_data_tx,
            dropped: dropped_media,
            dropped_data: incoming_data_dropped,
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
        bwe: config.bwe,
        data: config.data,
        channels: DataChannels::default(),
        send_streams: Vec::new(),
        missed_keyframes,
        keyframes_without_stream,
        unroutable_data,
        refused_data,
        oversized_data,
        drain_deadline: None,
        commands_closed: false,
        writes_closed: false,
        data_writes_closed: false,
    };

    let events = PeerEvents {
        control,
        media,
        data: incoming_data,
        control_open: true,
        media_open: true,
        data_open: true,
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
    let builder = Rtc::builder()
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
        .enable_av1(true);

    let builder = match config.bwe {
        Some(bwe) => builder.enable_bwe(Some(bwe.initial)),
        None => builder,
    };

    let mut rtc = builder.build(Instant::now());

    if let Some(bwe) = config.bwe {
        rtc.bwe().set_desired_bitrate(bwe.desired);

        tracing::info!(
            initial = bwe.initial.as_u64(),
            desired = bwe.desired.as_u64(),
            "bandwidth estimation and the leaky bucket pacer are on for this peer"
        );
    }

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
    data: mpsc::Sender<PeerEvent>,
    dropped: Arc<AtomicU64>,
    dropped_data: Arc<AtomicU64>,
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

    fn emit_data(&mut self, event: PeerEvent) -> bool {
        match self.data.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let total = self.dropped_data.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(total, "data event queue is full, dropping a data packet");
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
    data_writes: mpsc::Receiver<Box<OutgoingData>>,
    unroutable_writes: Arc<AtomicU64>,
    sink: EventSink,
    creds: Arc<RwLock<IceCreds>>,
    ice: IceWatch,
    renegotiation: Renegotiation,
    layers: LayerTable,
    codecs: HashMap<Pt, Codec>,
    probe: Option<KeyframeProbe>,
    layer_silence: Duration,
    bwe: Option<BweSettings>,
    data: Option<DataSettings>,
    channels: DataChannels,
    send_streams: Vec<(Mid, Option<Rid>)>,
    missed_keyframes: Arc<AtomicU64>,
    keyframes_without_stream: Arc<AtomicU64>,
    unroutable_data: Arc<AtomicU64>,
    refused_data: Arc<AtomicU64>,
    oversized_data: Arc<AtomicU64>,
    drain_deadline: Option<Instant>,
    commands_closed: bool,
    writes_closed: bool,
    data_writes_closed: bool,
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

                packet = self.data_writes.recv(), if !self.data_writes_closed => match packet {
                    Some(packet) => self.write_data(*packet),
                    None => self.data_writes_closed = true,
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
            PeerCommand::SetDesiredBitrate(desired) => self.set_desired_bitrate(desired),
            PeerCommand::ResetBwe(initial) => self.reset_bwe(initial),
            PeerCommand::QueueQuery { reply } => {
                let _ = reply.send(self.send_queues());
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

        Ok(self.silence_idle_sections(&answer.to_sdp_string()))
    }

    /// Answer `inactive` on the sections we have nothing to send on.
    ///
    /// The direction in an answer is the offer's, inverted, and nothing in that
    /// arithmetic knows whether there is anything to put on the section. So a
    /// client that pre-allocates receive slots -- `@livekit/rtc-node` offers
    /// three audio and three video `a=recvonly` sections before it has
    /// subscribed to anything -- is answered `a=sendonly` on every one of them,
    /// with the `a=msid` it sent echoed back at it. The answer then names
    /// streams belonging to the client and promises six tracks that do not
    /// exist, and the client holds those slots open for media that is never
    /// coming.
    ///
    /// `send_streams` is the list of what this transport actually writes to, so
    /// a section whose mid is not in it is one we have nothing for -- which is
    /// every section on a publisher transport, and the pre-allocated ones on
    /// any other. A section we do send on is left exactly as str0m wrote it,
    /// simulcast rids included, because the mid is what both are keyed by.
    ///
    /// Only sections that were going to say `sendonly` are touched, and only
    /// their own `a=msid` goes with them: `recvonly` is a track arriving, and
    /// none of this is about those.
    fn silence_idle_sections(&self, answer: &str) -> String {
        let mut out = String::with_capacity(answer.len());
        let mut section: Vec<&str> = Vec::new();
        let mut session = true;

        for line in answer.split_inclusive('\n') {
            if bare(line).starts_with("m=") {
                match session {
                    true => session = false,
                    false => self.flush_section(&mut section, &mut out),
                }
            }

            match session {
                true => out.push_str(line),
                false => section.push(line),
            }
        }

        self.flush_section(&mut section, &mut out);

        out
    }

    fn flush_section(&self, section: &mut Vec<&str>, out: &mut String) {
        let mid = section
            .iter()
            .find_map(|line| bare(line).strip_prefix("a=mid:"));
        let sends = mid.is_some_and(|mid| {
            self.send_streams
                .iter()
                .any(|(declared, _)| declared.to_string() == mid)
        });
        let idle = !sends && section.iter().any(|line| bare(line) == "a=sendonly");

        for line in section.drain(..) {
            if idle && bare(line) == "a=sendonly" {
                out.push_str("a=inactive");
                out.push_str(&line[bare(line).len()..]);
                continue;
            }
            if idle && bare(line).starts_with("a=msid:") {
                continue;
            }

            out.push_str(line);
        }
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
        let resendable = self.carries_rtx_payload_type(spec.mid);

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
                    None if kind.is_video() || resendable => Some(direct.new_ssrc()),
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
            paced = self.bwe.is_some() && !unpaced,
            "declared a send stream"
        );

        let key = (declared.mid, declared.rid);
        if !self.send_streams.contains(&key) {
            self.send_streams.push(key);
        }

        Ok(declared)
    }

    fn carries_rtx_payload_type(&self, mid: Mid) -> bool {
        self.negotiated_params(mid)
            .iter()
            .any(|params| params.resend().is_some())
    }

    fn remove_send_stream(&mut self, mid: Mid, rid: Option<Rid>) -> bool {
        self.send_streams.retain(|declared| *declared != (mid, rid));

        let mut direct = self.rtc.direct_api();

        let Some(ssrc) = direct
            .stream_tx_by_mid(mid, rid)
            .map(|stream| stream.ssrc())
        else {
            return false;
        };

        direct.remove_stream_tx(ssrc)
    }

    fn set_desired_bitrate(&mut self, desired: Bitrate) {
        if self.bwe.is_none() {
            tracing::warn!(
                desired = desired.as_u64(),
                "ignoring a desired bitrate on a peer without bandwidth estimation"
            );
            return;
        }

        tracing::debug!(desired = desired.as_u64(), "desired bitrate set");

        self.rtc.bwe().set_desired_bitrate(desired);
    }

    fn reset_bwe(&mut self, initial: Bitrate) {
        if self.bwe.is_none() {
            tracing::warn!(
                initial = initial.as_u64(),
                "ignoring a bandwidth estimator reset on a peer without one"
            );
            return;
        }

        tracing::info!(initial = initial.as_u64(), "bandwidth estimator reset");

        self.rtc.bwe().reset(initial);
    }

    fn send_queues(&mut self) -> Vec<SendQueueEntry> {
        let keys = self.send_streams.clone();
        let mut queues = Vec::with_capacity(keys.len());
        let mut stale = Vec::new();

        for (mid, rid) in keys {
            let mut direct = self.rtc.direct_api();

            let Some(stream) = direct.stream_tx_by_mid(mid, rid) else {
                stale.push((mid, rid));
                continue;
            };

            let entry = match stream.queue_info() {
                Some(info) => SendQueueEntry {
                    mid,
                    rid,
                    bytes: info.byte_size(),
                    packets: info.packet_count(),
                    oldest_unsent: info.first_unsent(),
                    sampled_at: Some(info.created_at()),
                },
                None => SendQueueEntry::empty(mid, rid),
            };

            queues.push(entry);
        }

        if !stale.is_empty() {
            self.send_streams.retain(|key| !stale.contains(key));
        }

        queues
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

    fn open_channel(&mut self, id: ChannelId, label: &str) -> Option<DataChannel> {
        self.data?;

        let channel = self.channels.opened(id, label)?;

        tracing::debug!(?channel, label, "a data channel opened");

        Some(channel)
    }

    fn incoming_data(&mut self, data: ChannelData) -> Option<IncomingData> {
        let settings = self.data?;
        let channel = self.channels.kind(data.id)?;

        if data.data.len() > settings.max_packet {
            self.oversized_data.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                ?channel,
                bytes = data.data.len(),
                limit = settings.max_packet,
                "dropping an oversized data packet"
            );
            return None;
        }

        Some(IncomingData {
            channel,
            binary: data.binary,
            payload: data.data,
        })
    }

    fn write_data(&mut self, packet: OutgoingData) {
        let Some(id) = self.channels.id(packet.channel) else {
            self.unroutable_data.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(channel = ?packet.channel, "no data channel is open");
            return;
        };

        let Some(mut channel) = self.rtc.channel(id) else {
            self.unroutable_data.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(channel = ?packet.channel, "the data channel is not writable");
            return;
        };

        match channel.write(packet.binary, &packet.payload) {
            Ok(true) => {}
            Ok(false) => {
                self.refused_data.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    channel = ?packet.channel,
                    bytes = packet.payload.len(),
                    "the sctp send buffer refused a data packet"
                );
            }
            Err(error) => {
                self.unroutable_data.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(channel = ?packet.channel, %error, "writing to a data channel failed");
            }
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
            Event::PeerStats(stats) => {
                let queues = self.send_queues();
                let report = PeerReport::from_stats(stats, queues);

                self.sink
                    .emit_control(PeerEvent::PeerReport(Box::new(report)))
            }
            Event::EgressBitrateEstimate(BweKind::Twcc(bitrate)) => {
                self.sink.emit_control(PeerEvent::BitrateEstimate {
                    bitrate,
                    at: Instant::now(),
                })
            }
            Event::EgressBitrateEstimate(BweKind::Remb(mid, bitrate)) => self
                .sink
                .emit_control(PeerEvent::RembRequested { mid, bitrate }),
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
            Event::ChannelOpen(id, label) => match self.open_channel(id, &label) {
                Some(channel) => self.sink.emit_control(PeerEvent::ChannelOpen(channel)),
                None => true,
            },
            Event::ChannelData(data) => match self.incoming_data(data) {
                Some(incoming) => self.sink.emit_data(PeerEvent::Data(incoming)),
                None => true,
            },
            Event::ChannelClose(id) => match self.channels.closed(id) {
                Some(channel) => {
                    tracing::debug!(?channel, "a data channel closed");
                    self.sink.emit_control(PeerEvent::ChannelClose(channel))
                }
                None => true,
            },
            Event::ChannelBufferedAmountLow(_)
            | Event::SenderFeedback(_)
            | Event::AppSpecificFeedback(_)
            | Event::RawPacket(_) => true,
            other => {
                tracing::trace!(?other, "an unhandled str0m event");
                true
            }
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


/// An sdp line without the line ending it arrived with.
fn bare(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
#[path = "peer_tests.rs"]
mod tests;
