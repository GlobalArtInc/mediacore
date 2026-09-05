use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use mediacore_transport::str0m::change::{SdpAnswer, SdpOffer};
use mediacore_transport::str0m::format::Codec;
use mediacore_transport::str0m::media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt, Rid};
use mediacore_transport::str0m::net::{Protocol, Receive};
use mediacore_transport::str0m::rtp::{ExtensionValues, SeqNo, Ssrc, VideoOrientation};
use mediacore_transport::str0m::{Candidate, Event, Input, Output, Rtc};
use mediacore_transport::{
    DeclaredStream, MediaSpec, Negotiation, OutgoingRtp, PeerConfig, PeerEvent, PeerEvents,
    PeerHandle, RouterConfig, SendStream, SpawnedPeer, TransportCrypto, UdpRouter, spawn_peer,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PATIENCE: Duration = Duration::from_secs(20);
const NO_DROP: i64 = -1;
const VIDEO_TICKS: u32 = 3_000;
const FIRST_SEQ: u64 = 40_000;
const FIRST_TIME: u32 = 7_000_000;
const VP8_PAYLOAD: [u8; 160] = [0x9d; 160];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Received {
    ssrc: u32,
    seq: u64,
    time: u32,
    pt: u8,
    marker: bool,
    len: usize,
    mid: Option<Mid>,
    abs_send_time: bool,
    video_orientation: Option<VideoOrientation>,
}

enum Command {
    Answer(String, oneshot::Sender<Result<String, String>>),
    Received(oneshot::Sender<Vec<Received>>),
    RequestKeyframe(Ssrc, KeyframeRequestKind),
    WaitConnected(oneshot::Sender<()>),
}

struct Subscriber {
    commands: mpsc::Sender<Command>,
    drop_at: Arc<AtomicI64>,
    task: tokio::task::JoinHandle<()>,
}

impl Subscriber {
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

        let (datagram_tx, datagrams) = mpsc::channel(1024);
        let reader = Arc::clone(&socket);
        let drop_at = Arc::new(AtomicI64::new(NO_DROP));
        let dropper = Arc::clone(&drop_at);

        tokio::spawn(async move {
            let mut buffer = vec![0u8; 2048];
            let mut seen: i64 = 0;

            loop {
                let Ok((size, source)) = reader.recv_from(&mut buffer).await else {
                    break;
                };

                let data = &buffer[..size];

                if is_rtp(data) {
                    let index = seen;
                    seen += 1;

                    if dropper.load(Ordering::Relaxed) == index {
                        dropper.store(NO_DROP, Ordering::Relaxed);
                        continue;
                    }
                }

                let data = Bytes::copy_from_slice(data);
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
        let actor = SubscriberActor {
            rtc,
            socket,
            local,
            datagrams,
            commands: inbox,
            received: Vec::new(),
            connected: false,
            waiters: Vec::new(),
        };

        Self {
            commands,
            drop_at,
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

    async fn received(&self) -> Vec<Received> {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Received(reply)).await.unwrap();
        response.await.unwrap()
    }

    async fn request_keyframe(&self, ssrc: Ssrc, kind: KeyframeRequestKind) {
        self.commands
            .send(Command::RequestKeyframe(ssrc, kind))
            .await
            .unwrap();
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

    fn drop_rtp_datagram(&self, index: i64) {
        self.drop_at.store(index, Ordering::Relaxed);
    }

    fn armed(&self) -> bool {
        self.drop_at.load(Ordering::Relaxed) != NO_DROP
    }

    fn stop(self) {
        self.task.abort();
    }
}

fn is_rtp(data: &[u8]) -> bool {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return false;
    }

    !(200..=206).contains(&(data[1] & 0x7f))
}

struct SubscriberActor {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    datagrams: mpsc::Receiver<(Instant, SocketAddr, Bytes)>,
    commands: mpsc::Receiver<Command>,
    received: Vec<Received>,
    connected: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

impl SubscriberActor {
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
                Ok(Output::Event(Event::RtpPacket(packet))) => {
                    self.received.push(Received {
                        ssrc: *packet.header.ssrc,
                        seq: *packet.seq_no,
                        time: packet.header.timestamp,
                        pt: *packet.header.payload_type,
                        marker: packet.header.marker,
                        len: packet.payload.len(),
                        mid: packet.header.ext_vals.mid,
                        abs_send_time: packet.header.ext_vals.abs_send_time.is_some(),
                        video_orientation: packet.header.ext_vals.video_orientation,
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
                let _ = reply.send(self.received.clone());
            }
            Command::RequestKeyframe(ssrc, kind) => {
                if let Some(stream) = self.rtc.direct_api().stream_rx(&ssrc) {
                    stream.request_keyframe(kind);
                }
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

async fn subscriber_server() -> (TransportCrypto, UdpRouter, SpawnedPeer) {
    let crypto = TransportCrypto::new().expect("crypto");
    let udp = UdpRouter::bind(RouterConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..RouterConfig::default()
    })
    .await
    .expect("the ice socket binds");
    udp.spawn_reader();

    let peer = spawn_peer(&crypto, &udp, PeerConfig::subscriber()).expect("the peer starts");

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

async fn subscribe(
    peer: &PeerHandle,
    client: &Subscriber,
    kind: MediaKind,
    track: &str,
) -> (Mid, DeclaredStream, Pt) {
    let local = peer
        .negotiate(outgoing(kind, track))
        .await
        .expect("the server offers the new subscription");
    let mid = local.mids[0];

    let answer = client.answer(local.sdp).await;
    peer.accept_answer(answer)
        .await
        .expect("the server applies the client answer");

    let declared = peer
        .declare_send_stream(SendStream::new(mid))
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

    (mid, declared, pt)
}

fn write_burst(peer: &PeerHandle, mid: Mid, pt: Pt, first_seq: u64, first_time: u32, count: u64) {
    for index in 0..count {
        let ext_vals = ExtensionValues {
            video_orientation: Some(VideoOrientation::Deg90),
            ..ExtensionValues::default()
        };

        let packet = OutgoingRtp {
            mid,
            rid: None::<Rid>,
            pt,
            seq_no: SeqNo::from(first_seq + index),
            time: first_time.wrapping_add(VIDEO_TICKS * index as u32),
            wallclock: Instant::now(),
            marker: true,
            nackable: true,
            ext_vals,
            payload: Arc::from(&VP8_PAYLOAD[..]),
        };

        peer.write_rtp(packet).expect("the peer is alive");
    }
}

async fn wait_for_control<F>(events: &mut PeerEvents, label: &str, mut matches: F)
where
    F: FnMut(&PeerEvent) -> bool,
{
    let found = tokio::time::timeout(PATIENCE, async {
        while let Some(event) = events.recv_control().await {
            if matches(&event) {
                return true;
            }
        }
        false
    })
    .await;

    assert!(
        matches!(found, Ok(true)),
        "timed out waiting for {label} on the control channel"
    );
}

async fn until<T, F, Fut>(label: &str, mut poll: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_written_packet_arrives_with_the_sequence_number_and_timestamp_we_chose() {
    let (crypto, _udp, peer) = subscriber_server().await;
    let mut events = peer.events;
    let handle = peer.handle;
    let client = Subscriber::start(&crypto).await;

    let (mid, declared, pt) = subscribe(&handle, &client, MediaKind::Video, "camera").await;

    wait_for_control(
        &mut events,
        "the subscriber transport to connect",
        |event| matches!(event, PeerEvent::Connected),
    )
    .await;
    client.wait_connected().await;

    const COUNT: u64 = 24;
    write_burst(&handle, mid, pt, FIRST_SEQ, FIRST_TIME, COUNT);

    let received = until("the written packets to reach the subscriber", || async {
        let received = client.received().await;
        (received.len() as u64 >= COUNT).then_some(received)
    })
    .await;

    let ours: Vec<Received> = received
        .into_iter()
        .filter(|packet| packet.ssrc == *declared.ssrc)
        .collect();

    assert_eq!(ours.len() as u64, COUNT);

    for (index, packet) in ours.iter().enumerate() {
        let index = index as u64;

        assert_eq!(
            packet.seq,
            FIRST_SEQ + index,
            "the sequence number must be the one the caller chose"
        );
        assert_eq!(
            packet.time,
            FIRST_TIME + VIDEO_TICKS * index as u32,
            "the rtp timestamp must be the one the caller chose"
        );
        assert_eq!(packet.pt, *pt);
        assert!(packet.marker);
        assert_eq!(packet.len, VP8_PAYLOAD.len());
        assert_eq!(
            packet.video_orientation,
            Some(VideoOrientation::Deg90),
            "header extension values supplied by the caller must reach the wire"
        );
    }

    assert!(
        ours.iter().any(|packet| packet.mid == Some(mid)),
        "str0m fills in the mid extension for us"
    );
    assert!(
        ours.iter().all(|packet| packet.abs_send_time),
        "str0m fills in absolute send time for us"
    );

    assert_eq!(handle.dropped_rtp_writes(), 0);
    assert_eq!(handle.unroutable_rtp_writes(), 0);

    client
        .request_keyframe(declared.ssrc, KeyframeRequestKind::Pli)
        .await;

    wait_for_control(
        &mut events,
        "a keyframe request from the subscriber",
        |event| {
            matches!(
                event,
                PeerEvent::KeyframeRequested(request)
                    if request.mid == mid && request.kind == KeyframeRequestKind::Pli
            )
        },
    )
    .await;

    client.stop();
    handle.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_packet_is_resent_from_the_str0m_cache_without_our_involvement() {
    let (crypto, _udp, peer) = subscriber_server().await;
    let mut events = peer.events;
    let handle = peer.handle;
    let client = Subscriber::start(&crypto).await;

    let (mid, declared, pt) = subscribe(&handle, &client, MediaKind::Video, "camera").await;
    assert!(declared.rtx.is_some(), "resends need an rtx ssrc");

    wait_for_control(
        &mut events,
        "the subscriber transport to connect",
        |event| matches!(event, PeerEvent::Connected),
    )
    .await;
    client.wait_connected().await;

    const COUNT: u64 = 60;
    const LOST: u64 = 8;

    client.drop_rtp_datagram(LOST as i64);

    for index in 0..COUNT {
        write_burst(
            &handle,
            mid,
            pt,
            FIRST_SEQ + index,
            FIRST_TIME.wrapping_add(VIDEO_TICKS * index as u32),
            1,
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(!client.armed(), "the test never dropped a media datagram");

    let received = until("the lost packet to be resent", || async {
        let received = client.received().await;
        received
            .iter()
            .any(|packet| packet.seq == FIRST_SEQ + LOST)
            .then_some(received)
    })
    .await;

    let resent = received
        .iter()
        .find(|packet| packet.seq == FIRST_SEQ + LOST)
        .expect("the resent packet");

    assert_eq!(
        resent.ssrc, *declared.ssrc,
        "a resend is unwrapped back onto the main ssrc"
    );
    assert_eq!(
        resent.time,
        FIRST_TIME.wrapping_add(VIDEO_TICKS * LOST as u32),
        "the resend carries the timestamp we originally wrote"
    );
    assert_eq!(resent.len, VP8_PAYLOAD.len());

    let nacked = tokio::time::timeout(PATIENCE, async {
        while let Some(event) = events.recv_control().await {
            if let PeerEvent::EgressReport(report) = event
                && report.mid == mid
                && report.nacks > 0
            {
                return true;
            }
        }
        false
    })
    .await;

    assert!(
        matches!(nacked, Ok(true)),
        "the egress report must show the nack that str0m answered for us"
    );

    client.stop();
    handle.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_subscription_renegotiates_without_interrupting_the_first() {
    let (crypto, _udp, peer) = subscriber_server().await;
    let mut events = peer.events;
    let handle = peer.handle;
    let client = Subscriber::start(&crypto).await;

    let (first_mid, first, first_pt) =
        subscribe(&handle, &client, MediaKind::Video, "camera").await;

    wait_for_control(
        &mut events,
        "the subscriber transport to connect",
        |event| matches!(event, PeerEvent::Connected),
    )
    .await;
    client.wait_connected().await;

    const BEFORE: u64 = 20;
    const AFTER: u64 = 20;

    write_burst(&handle, first_mid, first_pt, FIRST_SEQ, FIRST_TIME, BEFORE);

    until("the first subscription to flow", || async {
        let received = client.received().await;
        (received
            .iter()
            .filter(|packet| packet.ssrc == *first.ssrc)
            .count() as u64
            >= BEFORE)
            .then_some(())
    })
    .await;

    let (second_mid, second, second_pt) =
        subscribe(&handle, &client, MediaKind::Audio, "microphone").await;

    assert_ne!(second_mid, first_mid);
    assert_ne!(second.ssrc, first.ssrc);

    write_burst(
        &handle,
        first_mid,
        first_pt,
        FIRST_SEQ + BEFORE,
        FIRST_TIME.wrapping_add(VIDEO_TICKS * BEFORE as u32),
        AFTER,
    );
    write_burst(&handle, second_mid, second_pt, FIRST_SEQ, FIRST_TIME, AFTER);

    let received = until("both subscriptions to flow", || async {
        let received = client.received().await;
        let first_count = received
            .iter()
            .filter(|packet| packet.ssrc == *first.ssrc)
            .count() as u64;
        let second_count = received
            .iter()
            .filter(|packet| packet.ssrc == *second.ssrc)
            .count() as u64;

        (first_count >= BEFORE + AFTER && second_count >= AFTER).then_some(received)
    })
    .await;

    let first_seqs: Vec<u64> = received
        .iter()
        .filter(|packet| packet.ssrc == *first.ssrc)
        .map(|packet| packet.seq)
        .collect();

    assert_eq!(
        first_seqs,
        (FIRST_SEQ..FIRST_SEQ + BEFORE + AFTER).collect::<Vec<u64>>(),
        "the stream that was already flowing must not lose or reorder packets"
    );

    assert_eq!(handle.dropped_rtp_writes(), 0);
    assert_eq!(handle.unroutable_rtp_writes(), 0);

    client.stop();
    handle.close();
}
