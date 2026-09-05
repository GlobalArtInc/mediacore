use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mediacore_transport::str0m::change::{SdpAnswer, SdpPendingOffer};
use mediacore_transport::str0m::format::Codec;
use mediacore_transport::str0m::media::{
    Direction, KeyframeRequestKind, MediaKind, Mid, Pt, Rid, Simulcast, SimulcastLayer,
};
use mediacore_transport::str0m::net::{Protocol, Receive};
use mediacore_transport::str0m::rtp::{RtpWrite, SeqNo};
use mediacore_transport::str0m::{Candidate, Event, Input, Output, Rtc};
use mediacore_transport::{
    KeyframeProbe, LayerStats, PeerConfig, PeerEvent, PeerEvents, PeerHandle, RouterConfig,
    SpawnedPeer, TransportCrypto, TransportError, UdpRouter, spawn_peer,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PATIENCE: Duration = Duration::from_secs(20);
const BURST: usize = 4;
const BURST_INTERVAL: Duration = Duration::from_millis(10);
const VIDEO_TICKS: u32 = 3_000;
const KEYFRAME_BYTE: u8 = 0x01;
const DELTA_BYTE: u8 = 0x00;
const LOW: &str = "q";
const MID_LAYER: &str = "h";
const HIGH: &str = "f";

fn layers() -> [(Rid, usize); 3] {
    [
        (Rid::from(LOW), 40),
        (Rid::from(MID_LAYER), 80),
        (Rid::from(HIGH), 160),
    ]
}

fn vp8_probe() -> KeyframeProbe {
    KeyframeProbe::new(|codec, payload: &[u8]| {
        codec == Codec::Vp8 && payload.first() == Some(&KEYFRAME_BYTE)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Asked {
    mid: Mid,
    rid: Option<Rid>,
    kind: KeyframeRequestKind,
}

enum Command {
    Publish(oneshot::Sender<Result<(Mid, String), String>>),
    Answer(String, oneshot::Sender<Result<(), String>>),
    Start(Rid),
    Stop(Rid),
    Sent(Rid, oneshot::Sender<u64>),
    Asked(oneshot::Sender<Vec<Asked>>),
    WaitConnected(oneshot::Sender<()>),
}

struct Publisher {
    commands: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl Publisher {
    async fn start(crypto: &TransportCrypto) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("client socket"));
        let local = socket.local_addr().expect("client address");

        let mut rtc = Rtc::builder()
            .set_ice_lite(false)
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .clear_codecs()
            .enable_vp8(true)
            .build(Instant::now());

        rtc.add_local_candidate(Candidate::host(local, "udp").expect("client host candidate"));

        let (datagram_tx, datagrams) = mpsc::channel(1024);
        let reader = Arc::clone(&socket);

        tokio::spawn(async move {
            let mut buffer = vec![0u8; 2048];

            loop {
                let Ok((size, source)) = reader.recv_from(&mut buffer).await else {
                    break;
                };
                let data = Bytes::copy_from_slice(&buffer[..size]);
                if datagram_tx
                    .send((Instant::now(), source, data))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let (commands, inbox) = mpsc::channel(16);
        let actor = PublisherActor {
            rtc,
            socket,
            local,
            datagrams,
            commands: inbox,
            pending: None,
            mid: None,
            senders: Vec::new(),
            asked: Vec::new(),
            connected: false,
            waiters: Vec::new(),
        };

        Self {
            commands,
            task: tokio::spawn(actor.run()),
        }
    }

    async fn publish(&self) -> (Mid, String) {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Publish(reply)).await.unwrap();
        response
            .await
            .unwrap()
            .expect("the client offers simulcast")
    }

    async fn answer(&self, sdp: String) {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Answer(sdp, reply))
            .await
            .unwrap();
        response
            .await
            .unwrap()
            .expect("the client applies the answer");
    }

    async fn start_sending(&self, rid: Rid) {
        self.commands.send(Command::Start(rid)).await.unwrap();
    }

    async fn stop_sending(&self, rid: Rid) {
        self.commands.send(Command::Stop(rid)).await.unwrap();
    }

    async fn sent(&self, rid: Rid) -> u64 {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Sent(rid, reply)).await.unwrap();
        response.await.unwrap()
    }

    async fn asked(&self) -> Vec<Asked> {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Asked(reply)).await.unwrap();
        response.await.unwrap()
    }

    async fn wait_connected(&self) {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::WaitConnected(reply))
            .await
            .unwrap();
        tokio::time::timeout(PATIENCE, response)
            .await
            .expect("the client transport connects")
            .expect("the client transport is alive");
    }

    fn stop(self) {
        self.task.abort();
    }
}

struct LayerSender {
    rid: Rid,
    payload: usize,
    seq: u64,
    ticks: u32,
    packets: u64,
    keyframe_due: bool,
    active: bool,
    next: Instant,
}

struct PublisherActor {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    datagrams: mpsc::Receiver<(Instant, SocketAddr, Bytes)>,
    commands: mpsc::Receiver<Command>,
    pending: Option<SdpPendingOffer>,
    mid: Option<Mid>,
    senders: Vec<LayerSender>,
    asked: Vec<Asked>,
    connected: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

impl PublisherActor {
    async fn run(mut self) {
        loop {
            let Some(deadline) = self.poll().await else {
                break;
            };

            let deadline = match self.media_deadline() {
                Some(media) => deadline.min(media),
                None => deadline,
            }
            .max(Instant::now());

            let sleep = tokio::time::sleep_until(deadline.into());
            tokio::pin!(sleep);

            tokio::select! {
                biased;

                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command),
                    None => break,
                },

                datagram = self.datagrams.recv() => match datagram {
                    Some((at, source, data)) => self.on_datagram(at, source, &data),
                    None => break,
                },

                _ = &mut sleep => {
                    let now = Instant::now();
                    self.write_due(now);
                    let _ = self.rtc.handle_input(Input::Timeout(now));
                }
            }
        }
    }

    async fn poll(&mut self) -> Option<Instant> {
        loop {
            if !self.rtc.is_alive() {
                return None;
            }

            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return Some(at),
                Ok(Output::Transmit(transmit)) => {
                    let _ = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await;
                }
                Ok(Output::Event(Event::Connected)) => {
                    self.connected = true;
                    let now = Instant::now();
                    for sender in &mut self.senders {
                        sender.next = now;
                    }
                    for waiter in self.waiters.drain(..) {
                        let _ = waiter.send(());
                    }
                }
                Ok(Output::Event(Event::KeyframeRequest(request))) => {
                    self.asked.push(Asked {
                        mid: request.mid,
                        rid: request.rid,
                        kind: request.kind,
                    });
                }
                Ok(Output::Event(_)) => {}
                Err(_) => {
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

    fn on_datagram(&mut self, at: Instant, source: SocketAddr, data: &[u8]) {
        if let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, data) {
            let _ = self.rtc.handle_input(Input::Receive(at, receive));
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Publish(reply) => {
                let mut simulcast = Simulcast::new();
                for (rid, _) in layers() {
                    simulcast.add_send_layer(SimulcastLayer::new(&rid.to_string()));
                }

                let mut api = self.rtc.sdp_api();
                let mid = api.add_media(
                    MediaKind::Video,
                    Direction::SendOnly,
                    Some("stream".to_owned()),
                    Some("camera".to_owned()),
                    Some(simulcast),
                );

                match api.apply() {
                    Some((offer, pending)) => {
                        self.pending = Some(pending);
                        self.mid = Some(mid);
                        let _ = reply.send(Ok((mid, offer.to_sdp_string())));
                    }
                    None => {
                        let _ = reply.send(Err("nothing to negotiate".to_owned()));
                    }
                }
            }
            Command::Answer(sdp, reply) => {
                let _ = reply.send(self.on_answer(&sdp));
            }
            Command::Start(rid) => {
                let now = Instant::now();
                if let Some(sender) = self.senders.iter_mut().find(|sender| sender.rid == rid) {
                    sender.active = true;
                    sender.keyframe_due = true;
                    sender.next = now;
                }
            }
            Command::Stop(rid) => {
                if let Some(sender) = self.senders.iter_mut().find(|sender| sender.rid == rid) {
                    sender.active = false;
                }
            }
            Command::Sent(rid, reply) => {
                let packets = self
                    .senders
                    .iter()
                    .find(|sender| sender.rid == rid)
                    .map(|sender| sender.packets)
                    .unwrap_or_default();
                let _ = reply.send(packets);
            }
            Command::Asked(reply) => {
                let _ = reply.send(self.asked.clone());
            }
            Command::WaitConnected(reply) => {
                if self.connected {
                    let _ = reply.send(());
                } else {
                    self.waiters.push(reply);
                }
            }
        }
    }

    fn on_answer(&mut self, sdp: &str) -> Result<(), String> {
        let pending = self.pending.take().ok_or("no offer is outstanding")?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|error| error.to_string())?;

        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|error| error.to_string())?;

        let mid = self.mid.ok_or("nothing was published")?;

        for (index, (rid, payload)) in layers().into_iter().enumerate() {
            let mut direct = self.rtc.direct_api();
            let stream = direct
                .stream_tx_by_mid(mid, Some(rid))
                .ok_or_else(|| format!("no send stream for rid {rid}"))?;
            stream.set_unpaced(true);

            self.senders.push(LayerSender {
                rid,
                payload,
                seq: 40_000 + index as u64 * 1_000,
                ticks: 7_000_000 + index as u32 * 90_000,
                packets: 0,
                keyframe_due: false,
                active: false,
                next: Instant::now(),
            });
        }

        Ok(())
    }

    fn vp8_pt(&self, mid: Mid) -> Option<Pt> {
        let media = self.rtc.media(mid)?;
        let remote = media.remote_pts();

        self.rtc
            .codec_config()
            .params()
            .iter()
            .find(|params| params.spec().codec == Codec::Vp8 && remote.contains(&params.pt()))
            .map(|params| params.pt())
    }

    fn write_due(&mut self, now: Instant) {
        if !self.connected {
            return;
        }

        let Some(mid) = self.mid else {
            return;
        };
        let Some(pt) = self.vp8_pt(mid) else {
            return;
        };

        for index in 0..self.senders.len() {
            if !self.senders[index].active || self.senders[index].next > now {
                continue;
            }

            for _ in 0..BURST {
                let sender = &mut self.senders[index];
                let rid = sender.rid;
                let seq: SeqNo = sender.seq.into();
                let time = sender.ticks;
                let keyframe = sender.keyframe_due;

                let mut payload = vec![DELTA_BYTE; sender.payload];
                if keyframe {
                    payload[0] = KEYFRAME_BYTE;
                }

                sender.seq += 1;
                sender.ticks = sender.ticks.wrapping_add(VIDEO_TICKS);
                sender.packets += 1;
                sender.keyframe_due = false;

                let mut direct = self.rtc.direct_api();
                let Some(stream) = direct.stream_tx_by_mid(mid, Some(rid)) else {
                    break;
                };

                stream.write_rtp(RtpWrite::new(pt, seq, time, now, payload).marker(true));
            }

            self.senders[index].next = now + BURST_INTERVAL;
        }
    }
}

async fn server(layer_silence: Duration) -> (TransportCrypto, UdpRouter, SpawnedPeer) {
    let crypto = TransportCrypto::new().expect("crypto");
    let udp = UdpRouter::bind(RouterConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..RouterConfig::default()
    })
    .await
    .expect("the ice socket binds");
    udp.spawn_reader();

    let peer = spawn_peer(
        &crypto,
        &udp,
        PeerConfig {
            layer_silence,
            keyframe_probe: Some(vp8_probe()),
            ..PeerConfig::publisher()
        },
    )
    .expect("the peer starts");

    (crypto, udp, peer)
}

struct Watch {
    control: mpsc::UnboundedReceiver<PeerEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl Watch {
    fn spawn(mut events: PeerEvents) -> Self {
        let (tx, control) = mpsc::unbounded_channel();

        let task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if matches!(event, PeerEvent::Rtp(_) | PeerEvent::Media(_)) {
                    continue;
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
        });

        Self { control, task }
    }

    async fn wait<F>(&mut self, label: &str, mut matches: F) -> PeerEvent
    where
        F: FnMut(&PeerEvent) -> bool,
    {
        let found = tokio::time::timeout(PATIENCE, async {
            while let Some(event) = self.control.recv().await {
                if matches(&event) {
                    return Some(event);
                }
            }
            None
        })
        .await;

        match found {
            Ok(Some(event)) => event,
            _ => panic!("timed out waiting for {label}"),
        }
    }

    fn stop(self) {
        self.task.abort();
    }
}

async fn until<T, F, Fut>(label: &str, mut poll: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + PATIENCE;

    loop {
        if let Some(found) = poll().await {
            return found;
        }

        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn layer(stats: &[LayerStats], rid: &str) -> LayerStats {
    stats
        .iter()
        .find(|layer| layer.rid == Some(Rid::from(rid)))
        .unwrap_or_else(|| panic!("the transport tracks a layer for rid {rid}"))
        .clone()
}

async fn connected_publisher(
    layer_silence: Duration,
) -> (Publisher, PeerHandle, Watch, Mid, UdpRouter) {
    let (crypto, udp, peer) = server(layer_silence).await;
    let SpawnedPeer {
        handle,
        events,
        task: _task,
    } = peer;
    let mut watch = Watch::spawn(events);
    let client = Publisher::start(&crypto).await;

    let (mid, offer) = client.publish().await;
    let answer = handle
        .accept_offer(offer)
        .await
        .expect("the server answers the simulcast offer");
    client.answer(answer).await;

    watch
        .wait("the transport to connect", |event| {
            matches!(event, PeerEvent::Connected)
        })
        .await;
    client.wait_connected().await;

    let added = watch
        .wait("the simulcast track to be added", |event| {
            matches!(event, PeerEvent::MediaAdded(_))
        })
        .await;

    let PeerEvent::MediaAdded(info) = added else {
        unreachable!("the matcher only lets MediaAdded through")
    };

    assert_eq!(info.mid, mid);
    assert_eq!(info.kind, MediaKind::Video);
    assert!(
        info.is_simulcast(),
        "the negotiated rid set must survive into the media description"
    );
    assert_eq!(
        info.rids,
        layers().map(|(rid, _)| rid).to_vec(),
        "every offered rid must be exposed, in the order the publisher offered them"
    );
    assert!(info.receive_layers.is_specific());
    assert!(info.send_layers.is_empty());

    (client, handle, watch, mid, udp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn packets_of_three_rids_arrive_tagged_and_counted_separately() {
    let (client, peer, watch, mid, _udp) = connected_publisher(Duration::from_secs(30)).await;

    for (rid, _) in layers() {
        client.start_sending(rid).await;
    }

    let stats = until("all three layers to arrive", || async {
        let stats = peer.layers(mid).await.expect("the peer is alive");
        let seen = layers()
            .into_iter()
            .all(|(rid, _)| stats.iter().any(|l| l.rid == Some(rid) && l.packets >= 8));

        seen.then_some(stats)
    })
    .await;

    assert_eq!(stats.len(), 3, "a layer must never be folded into another");

    let low = layer(&stats, LOW);
    let mid_layer = layer(&stats, MID_LAYER);
    let high = layer(&stats, HIGH);

    for observed in [&low, &mid_layer, &high] {
        assert_eq!(observed.mid, mid);
        assert!(observed.active);
        assert!(observed.wanted);
        assert!(observed.last_arrival.is_some());
        assert!(observed.first_arrival.is_some());
        assert_eq!(
            observed.keyframes, 1,
            "each layer must be credited with its own keyframe"
        );
        assert!(observed.last_keyframe.is_some());
    }

    let ssrcs = [low.ssrc, mid_layer.ssrc, high.ssrc];
    assert!(
        ssrcs.iter().all(Option::is_some),
        "every layer must carry the ssrc it arrived on"
    );
    assert_ne!(low.ssrc, mid_layer.ssrc);
    assert_ne!(mid_layer.ssrc, high.ssrc);
    assert_ne!(low.ssrc, high.ssrc);

    assert_eq!(low.bytes, low.packets * 40);
    assert_eq!(mid_layer.bytes, mid_layer.packets * 80);
    assert_eq!(high.bytes, high.packets * 160);

    for (rid, _) in layers() {
        assert!(client.sent(rid).await >= 8);
    }

    watch.stop();
    client.stop();
    peer.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keyframe_request_for_one_rid_does_not_disturb_the_others() {
    let (client, peer, watch, mid, _udp) = connected_publisher(Duration::from_secs(30)).await;

    for (rid, _) in layers() {
        client.start_sending(rid).await;
    }

    until("all three layers to arrive", || async {
        let stats = peer.layers(mid).await.expect("the peer is alive");
        layers()
            .into_iter()
            .all(|(rid, _)| stats.iter().any(|l| l.rid == Some(rid) && l.packets > 0))
            .then_some(())
    })
    .await;

    let wanted = Rid::from(MID_LAYER);
    peer.request_keyframe(mid, Some(wanted), KeyframeRequestKind::Pli)
        .expect("the peer takes the request");

    let asked = until("the publisher to be asked for a keyframe", || async {
        let asked = client.asked().await;
        (!asked.is_empty()).then_some(asked)
    })
    .await;

    assert!(
        asked.iter().all(|ask| ask.rid == Some(wanted)),
        "a keyframe asked for one rid reached another layer: {asked:?}"
    );
    assert!(asked.iter().all(|ask| ask.mid == mid));
    assert!(asked.iter().all(|ask| ask.kind == KeyframeRequestKind::Pli));

    let stats = peer.layers(mid).await.expect("the peer is alive");
    assert_eq!(layer(&stats, MID_LAYER).plis, 1);
    assert_eq!(layer(&stats, LOW).plis, 0);
    assert_eq!(layer(&stats, HIGH).plis, 0);

    let missed = peer.missed_keyframe_requests();
    peer.request_keyframe(mid, None, KeyframeRequestKind::Pli)
        .expect("the peer takes the request");

    until("the unaimed request to be refused", || async {
        (peer.missed_keyframe_requests() > missed).then_some(())
    })
    .await;

    let elsewhere = Mid::from("nosuch");
    assert!(matches!(
        peer.set_layer_wanted(elsewhere, None, false).await,
        Err(TransportError::UnknownMid(refused)) if refused == elsewhere
    ));

    let unknown = Rid::from("nosuch");
    assert!(matches!(
        peer.set_layer_wanted(mid, Some(unknown), false).await,
        Err(TransportError::UnknownLayer { rid, .. }) if rid == unknown
    ));
    assert!(matches!(
        peer.set_layer_wanted(mid, None, false).await,
        Err(TransportError::AmbiguousLayer(refused)) if refused == mid
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;

    let asked = client.asked().await;
    assert!(
        asked.iter().all(|ask| ask.rid == Some(wanted)),
        "an ambiguous request must reach no layer at all: {asked:?}"
    );

    let stats = peer.layers(mid).await.expect("the peer is alive");
    assert_eq!(layer(&stats, LOW).plis, 0);
    assert_eq!(layer(&stats, HIGH).plis, 0);

    watch.stop();
    client.stop();
    peer.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_layer_statistics_survive_a_layer_going_silent_and_coming_back() {
    let (client, peer, mut watch, mid, _udp) =
        connected_publisher(Duration::from_millis(300)).await;

    for (rid, _) in layers() {
        client.start_sending(rid).await;
    }

    let silent = Rid::from(MID_LAYER);

    until("all three layers to arrive", || async {
        let stats = peer.layers(mid).await.expect("the peer is alive");
        layers()
            .into_iter()
            .all(|(rid, _)| stats.iter().any(|l| l.rid == Some(rid) && l.packets >= 8))
            .then_some(())
    })
    .await;

    let before = peer.layers(mid).await.expect("the peer is alive");
    let before = layer(&before, MID_LAYER);

    client.stop_sending(silent).await;

    watch
        .wait("the middle layer to fall silent", |event| {
            matches!(
                event,
                PeerEvent::LayerActivity(activity)
                    if activity.rid == Some(silent) && !activity.active
            )
        })
        .await;

    let stats = peer.layers(mid).await.expect("the peer is alive");
    let quiet = layer(&stats, MID_LAYER);

    assert!(!quiet.active);
    assert!(quiet.silent_since.is_some());
    assert_eq!(
        quiet.packets, before.packets,
        "a silent layer must keep what it counted"
    );
    assert_eq!(quiet.bytes, before.bytes);
    assert_eq!(quiet.keyframes, before.keyframes);
    assert_eq!(quiet.first_arrival, before.first_arrival);
    assert_eq!(quiet.ssrc, before.ssrc);

    for rid in [LOW, HIGH] {
        let neighbour = layer(&stats, rid);
        assert!(
            neighbour.active,
            "the layers that kept sending must stay active while {MID_LAYER} is quiet"
        );
    }

    let neighbours: Vec<u64> = [LOW, HIGH]
        .into_iter()
        .map(|rid| layer(&stats, rid).packets)
        .collect();

    until("the other layers to keep flowing", || async {
        let stats = peer.layers(mid).await.expect("the peer is alive");
        [LOW, HIGH]
            .into_iter()
            .zip(neighbours.iter())
            .all(|(rid, packets)| layer(&stats, rid).packets > *packets)
            .then_some(())
    })
    .await;

    client.start_sending(silent).await;

    watch
        .wait("the middle layer to come back", |event| {
            matches!(
                event,
                PeerEvent::LayerActivity(activity)
                    if activity.rid == Some(silent) && activity.active
            )
        })
        .await;

    let stats = peer.layers(mid).await.expect("the peer is alive");
    let back = layer(&stats, MID_LAYER);

    assert!(back.active);
    assert_eq!(back.silent_since, None);
    assert_eq!(
        back.ssrc, before.ssrc,
        "a layer that comes back is the same layer"
    );
    assert_eq!(
        back.first_arrival, before.first_arrival,
        "coming back must not restart the layer's history"
    );
    assert!(back.packets > before.packets);
    assert!(back.bytes > before.bytes);
    assert_eq!(
        back.keyframes,
        before.keyframes + 1,
        "the keyframe that restarted the layer belongs to that layer"
    );
    assert_eq!(stats.len(), 3);

    watch.stop();
    client.stop();
    peer.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_a_layer_stops_nacking_it_and_resuming_asks_it_for_a_keyframe() {
    let (client, peer, watch, mid, _udp) = connected_publisher(Duration::from_secs(30)).await;

    for (rid, _) in layers() {
        client.start_sending(rid).await;
    }

    let paused = Rid::from(HIGH);

    until("all three layers to arrive", || async {
        let stats = peer.layers(mid).await.expect("the peer is alive");
        layers()
            .into_iter()
            .all(|(rid, _)| stats.iter().any(|l| l.rid == Some(rid) && l.packets > 0))
            .then_some(())
    })
    .await;

    assert!(
        peer.pause_layer(mid, Some(paused))
            .await
            .expect("the peer is alive")
    );
    assert!(
        !peer
            .pause_layer(mid, Some(paused))
            .await
            .expect("the peer is alive"),
        "pausing an already paused layer changes nothing"
    );

    let stats = peer.layers(mid).await.expect("the peer is alive");
    let quiet = layer(&stats, HIGH);
    assert!(!quiet.wanted);
    assert!(
        quiet.nack_suppressed,
        "a layer we asked the publisher to stop must not be nacked"
    );

    for rid in [LOW, MID_LAYER] {
        let neighbour = layer(&stats, rid);
        assert!(neighbour.wanted);
        assert!(!neighbour.nack_suppressed);
    }

    peer.request_keyframe(mid, Some(paused), KeyframeRequestKind::Pli)
        .expect("the peer takes the request");

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        client.asked().await.is_empty(),
        "a paused layer must never be asked for a keyframe"
    );

    assert!(
        peer.resume_layer(mid, Some(paused))
            .await
            .expect("the peer is alive")
    );

    let asked = until("the resumed layer to be asked for a keyframe", || async {
        let asked = client.asked().await;
        (!asked.is_empty()).then_some(asked)
    })
    .await;

    assert!(asked.iter().all(|ask| ask.rid == Some(paused)));

    let stats = peer.layers(mid).await.expect("the peer is alive");
    let resumed = layer(&stats, HIGH);
    assert!(resumed.wanted);
    assert!(!resumed.nack_suppressed);
    assert_eq!(resumed.plis, 1);
    assert_eq!(layer(&stats, LOW).plis, 0);
    assert_eq!(layer(&stats, MID_LAYER).plis, 0);

    watch.stop();
    client.stop();
    peer.close();
}
