use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mediacore_transport::str0m::change::{SdpAnswer, SdpOffer};
use mediacore_transport::str0m::format::Codec;
use mediacore_transport::str0m::media::{Direction, MediaKind, Mid, Pt, Rid};
use mediacore_transport::str0m::net::{Protocol, Receive};
use mediacore_transport::str0m::rtp::{ExtensionValues, SeqNo};
use mediacore_transport::str0m::{Candidate, Event, Input, Output, Rtc};
use mediacore_transport::{
    Bitrate, BweSettings, MediaSpec, Negotiation, OutgoingRtp, PeerConfig, PeerEvent, PeerEvents,
    PeerHandle, RouterConfig, SendQueueEntry, SendStream, SpawnedPeer, TransportCrypto, UdpRouter,
    spawn_peer,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PATIENCE: Duration = Duration::from_secs(20);
const INITIAL: Bitrate = Bitrate::kbps(600);
const VIDEO_TICKS: u32 = 3_000;
const AUDIO_TICKS: u32 = 960;
const FIRST_SEQ: u64 = 40_000;
const FIRST_TIME: u32 = 7_000_000;
const VIDEO_PAYLOAD: [u8; 1_100] = [0x9d; 1_100];
const AUDIO_PAYLOAD: [u8; 80] = [0x41; 80];
const BURST: u64 = 600;
const AUDIO_BURST: u64 = 40;
const PACED_AGE: Duration = Duration::from_millis(500);
const PACING_RATIO: u32 = 3;

#[derive(Debug, Default)]
struct Drain {
    oldest: Duration,
    delivered: bool,
    dropped_writes: u64,
}

enum Command {
    Answer(String, oneshot::Sender<Result<String, String>>),
    Received(oneshot::Sender<usize>),
    WaitConnected(oneshot::Sender<()>),
}

struct Client {
    commands: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl Client {
    async fn start(crypto: &TransportCrypto) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("client socket"));
        let local = socket.local_addr().expect("client address");

        let mut rtc = Rtc::builder()
            .set_ice_lite(false)
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .clear_codecs()
            .enable_opus(true)
            .enable_vp8(true)
            .build(Instant::now());

        rtc.add_local_candidate(Candidate::host(local, "udp").expect("client host candidate"));

        let (datagram_tx, datagrams) = mpsc::channel(4096);
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
        let actor = ClientActor {
            rtc,
            socket,
            local,
            datagrams,
            commands: inbox,
            received: 0,
            connected: false,
            waiters: Vec::new(),
        };

        Self {
            commands,
            task: tokio::spawn(actor.run()),
        }
    }

    async fn answer(&self, offer: String) -> String {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Answer(offer, reply))
            .await
            .unwrap();
        response
            .await
            .unwrap()
            .expect("the client answers our offer")
    }

    async fn received(&self) -> usize {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Received(reply)).await.unwrap();
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

struct ClientActor {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    datagrams: mpsc::Receiver<(Instant, SocketAddr, Bytes)>,
    commands: mpsc::Receiver<Command>,
    received: usize,
    connected: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

impl ClientActor {
    async fn run(mut self) {
        loop {
            let Some(deadline) = self.poll().await else {
                break;
            };

            let deadline = deadline.max(Instant::now());
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
                    let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
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
                    for waiter in self.waiters.drain(..) {
                        let _ = waiter.send(());
                    }
                }
                Ok(Output::Event(Event::RtpPacket(_))) => self.received += 1,
                Ok(Output::Event(_)) => {}
                Err(_) => {
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    fn on_datagram(&mut self, at: Instant, source: SocketAddr, data: &[u8]) {
        if let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, data) {
            let _ = self.rtc.handle_input(Input::Receive(at, receive));
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Answer(offer, reply) => {
                let _ = reply.send(self.on_offer(&offer));
            }
            Command::Received(reply) => {
                let _ = reply.send(self.received);
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

    fn on_offer(&mut self, offer: &str) -> Result<String, String> {
        let offer = SdpOffer::from_sdp_string(offer).map_err(|error| error.to_string())?;
        let answer: SdpAnswer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|error| error.to_string())?;

        Ok(answer.to_sdp_string())
    }
}

async fn server(bwe: Option<BweSettings>) -> (TransportCrypto, UdpRouter, SpawnedPeer) {
    spawn(PeerConfig::subscriber(bwe)).await
}

async fn spawn(config: PeerConfig) -> (TransportCrypto, UdpRouter, SpawnedPeer) {
    let crypto = TransportCrypto::new().expect("crypto");
    let udp = UdpRouter::bind(RouterConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..RouterConfig::default()
    })
    .await
    .expect("the ice socket binds");
    udp.spawn_reader();

    let peer = spawn_peer(&crypto, &udp, config).expect("the peer starts");

    (crypto, udp, peer)
}

fn outgoing(kind: MediaKind, track: &str) -> Negotiation {
    Negotiation::add(MediaSpec {
        kind,
        direction: Direction::SendOnly,
        stream_id: Some(format!("stream-{track}")),
        track_id: Some(track.to_owned()),
        simulcast: None,
    })
}

async fn subscribe(peer: &PeerHandle, client: &Client, kind: MediaKind, track: &str) -> (Mid, Pt) {
    let local = peer
        .negotiate(outgoing(kind, track))
        .await
        .expect("the server offers the new subscription");
    let mid = local.mids[0];

    let answer = client.answer(local.sdp).await;
    peer.accept_answer(answer)
        .await
        .expect("the server applies the client answer");

    peer.declare_send_stream(SendStream::new(mid))
        .await
        .expect("the negotiated mid accepts a send stream");

    let media = peer.media(mid).await.expect("the media is known");
    let codec = if kind.is_audio() {
        Codec::Opus
    } else {
        Codec::Vp8
    };
    let pt = media
        .params
        .iter()
        .find(|params| params.spec().codec == codec)
        .map(|params| params.pt())
        .expect("the client accepted our codec");

    (mid, pt)
}

fn write_burst(peer: &PeerHandle, mid: Mid, pt: Pt, kind: MediaKind, count: u64) {
    let (payload, ticks): (&[u8], u32) = if kind.is_audio() {
        (&AUDIO_PAYLOAD[..], AUDIO_TICKS)
    } else {
        (&VIDEO_PAYLOAD[..], VIDEO_TICKS)
    };

    for index in 0..count {
        let packet = OutgoingRtp {
            mid,
            rid: None::<Rid>,
            pt,
            seq_no: SeqNo::from(FIRST_SEQ + index),
            time: FIRST_TIME.wrapping_add(ticks * index as u32),
            wallclock: Instant::now(),
            marker: true,
            nackable: true,
            ext_vals: ExtensionValues::default(),
            payload: Arc::from(payload),
        };

        peer.write_rtp(packet).expect("the peer is alive");
    }
}

async fn wait_for_control<T, F>(events: &mut PeerEvents, label: &str, mut pick: F) -> T
where
    F: FnMut(PeerEvent) -> Option<T>,
{
    let found = tokio::time::timeout(PATIENCE, async {
        while let Some(event) = events.recv_control().await {
            if let Some(found) = pick(event) {
                return Some(found);
            }
        }
        None
    })
    .await;

    match found {
        Ok(Some(found)) => found,
        _ => panic!("timed out waiting for {label} on the control channel"),
    }
}

fn queue_for(queues: &[SendQueueEntry], mid: Mid) -> Option<&SendQueueEntry> {
    queues.iter().find(|queue| queue.mid == mid)
}

#[tokio::test]
async fn a_subscriber_with_bwe_reports_the_estimate_as_a_typed_event() {
    let (_crypto, _udp, peer) = server(Some(BweSettings::new(INITIAL))).await;
    let SpawnedPeer {
        handle,
        mut events,
        task: _task,
    } = peer;

    assert!(handle.estimates_bandwidth());

    let bitrate = wait_for_control(&mut events, "a bitrate estimate", |event| match event {
        PeerEvent::BitrateEstimate { bitrate, .. } => Some(bitrate),
        _ => None,
    })
    .await;

    assert_eq!(
        bitrate, INITIAL,
        "the first estimate str0m emits is the one we built the peer with"
    );

    let report = wait_for_control(&mut events, "a peer report", |event| match event {
        PeerEvent::PeerReport(report) => Some(report),
        _ => None,
    })
    .await;

    assert_eq!(
        report.bwe_tx,
        Some(INITIAL),
        "the unsmoothed estimate is the congestion trigger and must reach the caller"
    );
    assert!(report.queues.is_empty());
}

#[tokio::test]
async fn a_publisher_reports_statistics_but_never_an_estimate() {
    let (_crypto, _udp, peer) = spawn(PeerConfig::publisher()).await;
    let SpawnedPeer {
        handle,
        mut events,
        task: _task,
    } = peer;

    assert!(!handle.estimates_bandwidth());

    let mut estimates = 0usize;

    let report = wait_for_control(&mut events, "a peer report", |event| match event {
        PeerEvent::PeerReport(report) => Some(report),
        PeerEvent::BitrateEstimate { .. } => {
            estimates += 1;
            None
        }
        _ => None,
    })
    .await;

    assert_eq!(estimates, 0, "a publisher peer keeps the null pacer");
    assert_eq!(
        report.bwe_tx, None,
        "str0m has no estimator on this peer to report from"
    );
}

async fn drain_a_burst(bwe: Option<BweSettings>) -> Drain {
    let (crypto, _udp, peer) = server(bwe).await;
    let client = Client::start(&crypto).await;
    let (mid, pt) = subscribe(&peer.handle, &client, MediaKind::Video, "video").await;
    client.wait_connected().await;

    let started = Instant::now();
    write_burst(&peer.handle, mid, pt, MediaKind::Video, BURST);

    let mut drain = Drain::default();
    let deadline = started + PATIENCE;

    while Instant::now() < deadline {
        let now = Instant::now();
        let queues = peer.handle.send_queues().await.expect("the peer is alive");

        if let Some(queue) = queue_for(&queues, mid) {
            drain.oldest = drain.oldest.max(queue.age(now));
        }

        if client.received().await as u64 >= BURST {
            drain.delivered = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    drain.dropped_writes = peer.handle.dropped_rtp_writes();

    client.stop();
    drain
}

#[tokio::test]
async fn the_leaky_bucket_holds_a_burst_the_null_pacer_lets_straight_through() {
    let paced = drain_a_burst(Some(BweSettings::new(INITIAL))).await;

    assert!(
        paced.delivered,
        "pacing delays packets, it must never drop them"
    );
    assert_eq!(paced.dropped_writes, 0);
    assert!(
        paced.oldest >= PACED_AGE,
        "the leaky bucket drains a {BURST} packet burst over its own two second queue limit, \
         the oldest packet only ever waited {:?}",
        paced.oldest
    );

    let unpaced = drain_a_burst(None).await;

    assert!(unpaced.delivered);
    assert!(
        unpaced.oldest * PACING_RATIO < paced.oldest,
        "the null pacer drains as fast as the actor loop spins, it must not hold a packet \
         anywhere near as long as the leaky bucket: {:?} against {:?}",
        unpaced.oldest,
        paced.oldest
    );
}

#[tokio::test]
async fn audio_stays_unpaced_and_jumps_the_video_queue() {
    let (crypto, _udp, peer) = server(Some(BweSettings::new(INITIAL))).await;
    let client = Client::start(&crypto).await;

    let (video, video_pt) = subscribe(&peer.handle, &client, MediaKind::Video, "video").await;
    let (audio, audio_pt) = subscribe(&peer.handle, &client, MediaKind::Audio, "audio").await;
    client.wait_connected().await;

    write_burst(&peer.handle, video, video_pt, MediaKind::Video, BURST);
    write_burst(&peer.handle, audio, audio_pt, MediaKind::Audio, AUDIO_BURST);

    let deadline = Instant::now() + PATIENCE;
    let mut video_age = Duration::ZERO;
    let mut audio_age = Duration::ZERO;
    let mut delivered = false;

    while Instant::now() < deadline {
        let now = Instant::now();
        let queues = peer.handle.send_queues().await.expect("the peer is alive");

        if let Some(queue) = queue_for(&queues, video) {
            video_age = video_age.max(queue.age(now));
        }

        if let Some(queue) = queue_for(&queues, audio) {
            audio_age = audio_age.max(queue.age(now));
        }

        if client.received().await as u64 >= BURST + AUDIO_BURST {
            delivered = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(delivered, "both bursts arrive in full");
    assert!(
        video_age >= PACED_AGE,
        "the video burst must queue behind the leaky bucket for this test to mean anything, \
         its oldest packet waited only {video_age:?}"
    );
    assert!(
        audio_age * PACING_RATIO < video_age,
        "audio is declared unpaced and is emitted ahead of a paced queue, it must not wait \
         {audio_age:?} while video waits {video_age:?}"
    );

    client.stop();
}
