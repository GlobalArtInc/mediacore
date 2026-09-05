use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use mediacore_transport::str0m::change::{SdpAnswer, SdpPendingOffer};
use mediacore_transport::str0m::format::Codec;
use mediacore_transport::str0m::media::{Direction, MediaKind, Mid, Pt};
use mediacore_transport::str0m::net::{Protocol, Receive};
use mediacore_transport::str0m::rtp::{RtpWrite, SeqNo};
use mediacore_transport::str0m::{Candidate, Event, Input, Output, Rtc};
use mediacore_transport::{
    PeerConfig, PeerEvent, PeerEvents, RouterConfig, SpawnedPeer, TransportCrypto, UdpRouter,
    spawn_peer,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PATIENCE: Duration = Duration::from_secs(15);
const BURST: usize = 24;
const BURST_INTERVAL: Duration = Duration::from_millis(1);
const OPUS_TICKS: u32 = 960;
const OPUS_PAYLOAD: [u8; 80] = [0x78; 80];

enum Command {
    Publish(oneshot::Sender<Result<(Mid, String), String>>),
    Answer(String, oneshot::Sender<Result<(), String>>),
    IceRestart(oneshot::Sender<Result<String, String>>),
    Start(Mid),
    Sent(Mid, oneshot::Sender<u64>),
    WaitConnected(oneshot::Sender<()>),
}

struct TestClient {
    commands: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl TestClient {
    async fn start(crypto: &TransportCrypto) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("client socket"));
        let local = socket.local_addr().expect("client address");

        let mut rtc = Rtc::builder()
            .set_ice_lite(false)
            .set_rtp_mode(true)
            .set_dtls_cert(crypto.dtls_cert())
            .clear_codecs()
            .enable_opus(true)
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
        let actor = ClientActor {
            rtc,
            socket,
            local,
            datagrams,
            commands: inbox,
            pending: None,
            senders: Vec::new(),
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
        response.await.unwrap().expect("the client offers media")
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

    async fn ice_restart(&self) -> String {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::IceRestart(reply))
            .await
            .unwrap();
        response.await.unwrap().expect("the client restarts ice")
    }

    async fn start_sending(&self, mid: Mid) {
        self.commands.send(Command::Start(mid)).await.unwrap();
    }

    async fn sent(&self, mid: Mid) -> u64 {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Sent(mid, reply)).await.unwrap();
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

struct Sender {
    mid: Mid,
    pt: Pt,
    seq: u64,
    ticks: u32,
    packets: u64,
    active: bool,
    next: Instant,
}

struct ClientActor {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    datagrams: mpsc::Receiver<(Instant, SocketAddr, Bytes)>,
    commands: mpsc::Receiver<Command>,
    pending: Option<(SdpPendingOffer, Option<Mid>)>,
    senders: Vec<Sender>,
    connected: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

impl ClientActor {
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
                let mut api = self.rtc.sdp_api();
                let mid = api.add_media(
                    MediaKind::Audio,
                    Direction::SendOnly,
                    Some(format!("stream-{}", self.senders.len())),
                    Some(format!("track-{}", self.senders.len())),
                    None,
                );

                match api.apply() {
                    Some((offer, pending)) => {
                        self.pending = Some((pending, Some(mid)));
                        let _ = reply.send(Ok((mid, offer.to_sdp_string())));
                    }
                    None => {
                        let _ = reply.send(Err("nothing to negotiate".to_owned()));
                    }
                }
            }
            Command::IceRestart(reply) => {
                let mut api = self.rtc.sdp_api();
                api.ice_restart(true);

                match api.apply() {
                    Some((offer, pending)) => {
                        self.pending = Some((pending, None));
                        let _ = reply.send(Ok(offer.to_sdp_string()));
                    }
                    None => {
                        let _ = reply.send(Err("nothing to restart".to_owned()));
                    }
                }
            }
            Command::Answer(sdp, reply) => {
                let _ = reply.send(self.on_answer(&sdp));
            }
            Command::Start(mid) => {
                let now = Instant::now();
                if let Some(sender) = self.senders.iter_mut().find(|sender| sender.mid == mid) {
                    sender.active = true;
                    sender.next = now;
                }
            }
            Command::Sent(mid, reply) => {
                let packets = self
                    .senders
                    .iter()
                    .find(|sender| sender.mid == mid)
                    .map(|sender| sender.packets)
                    .unwrap_or_default();
                let _ = reply.send(packets);
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
        let (pending, fresh) = self.pending.take().ok_or("no offer is outstanding")?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|error| error.to_string())?;

        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|error| error.to_string())?;

        if let Some(mid) = fresh {
            let pt = self.opus_pt(mid).ok_or("opus was not negotiated")?;

            let mut direct = self.rtc.direct_api();
            if let Some(stream) = direct.stream_tx_by_mid(mid, None) {
                stream.set_unpaced(true);
            }

            self.senders.push(Sender {
                mid,
                pt,
                seq: 0,
                ticks: 0,
                packets: 0,
                active: false,
                next: Instant::now(),
            });
        }

        Ok(())
    }

    fn opus_pt(&self, mid: Mid) -> Option<Pt> {
        let media = self.rtc.media(mid)?;
        let remote = media.remote_pts();

        self.rtc
            .codec_config()
            .params()
            .iter()
            .find(|params| params.spec().codec == Codec::Opus && remote.contains(&params.pt()))
            .map(|params| params.pt())
    }

    fn write_due(&mut self, now: Instant) {
        if !self.connected {
            return;
        }

        for index in 0..self.senders.len() {
            if !self.senders[index].active || self.senders[index].next > now {
                continue;
            }

            for _ in 0..BURST {
                let sender = &mut self.senders[index];
                let mid = sender.mid;
                let pt = sender.pt;
                let seq: SeqNo = sender.seq.into();
                let time = sender.ticks;

                sender.seq += 1;
                sender.ticks = sender.ticks.wrapping_add(OPUS_TICKS);
                sender.packets += 1;

                let mut direct = self.rtc.direct_api();
                let Some(stream) = direct.stream_tx_by_mid(mid, None) else {
                    break;
                };

                stream.write_rtp(
                    RtpWrite::new(pt, seq, time, now, OPUS_PAYLOAD.to_vec()).marker(true),
                );
            }

            self.senders[index].next = now + BURST_INTERVAL;
        }
    }
}

async fn server(media_queue: usize) -> (TransportCrypto, UdpRouter, SpawnedPeer) {
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
            media_queue,
            ..PeerConfig::publisher()
        },
    )
    .expect("the peer starts");

    (crypto, udp, peer)
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

async fn until<T>(label: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;

    loop {
        if let Some(found) = poll() {
            return found;
        }

        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flooded_media_queue_never_stalls_control_events_or_commands() {
    let (crypto, _udp, peer) = server(32).await;
    let mut events = peer.events;
    let client = TestClient::start(&crypto).await;

    let (mid, offer) = client.publish().await;
    let answer = peer
        .handle
        .accept_offer(offer)
        .await
        .expect("the server answers the first offer");
    client.answer(answer).await;

    wait_for_control(&mut events, "the transport to connect", |event| {
        matches!(event, PeerEvent::Connected)
    })
    .await;
    client.wait_connected().await;

    wait_for_control(&mut events, "the first track to be added", |event| {
        matches!(event, PeerEvent::MediaAdded(_))
    })
    .await;

    client.start_sending(mid).await;

    let dropped = until("the media queue to overflow", || {
        let dropped = peer.handle.dropped_media_events();
        (dropped > 0).then_some(dropped)
    })
    .await;
    assert!(dropped > 0);

    let (_second_mid, second_offer) = client.publish().await;
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        peer.handle.accept_offer(second_offer),
    )
    .await
    .expect("the actor answers a command while media events are being dropped")
    .expect("the server answers the renegotiation offer");
    client.answer(answer).await;

    wait_for_control(&mut events, "the second track to be added", |event| {
        matches!(event, PeerEvent::MediaAdded(_))
    })
    .await;

    tokio::time::timeout(
        Duration::from_secs(5),
        peer.handle.add_remote_candidate(String::new()),
    )
    .await
    .expect("the actor still answers commands")
    .expect("an empty candidate is accepted");

    assert!(
        peer.handle.dropped_media_events() >= dropped,
        "the drop counter must keep counting"
    );

    client.stop();
    peer.handle.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_survives_an_ice_restart() {
    let (crypto, udp, peer) = server(512).await;
    let mut events = peer.events;
    let client = TestClient::start(&crypto).await;

    let (mid, offer) = client.publish().await;
    let answer = peer
        .handle
        .accept_offer(offer)
        .await
        .expect("the server answers the offer");
    client.answer(answer).await;

    wait_for_control(&mut events, "the transport to connect", |event| {
        matches!(event, PeerEvent::Connected)
    })
    .await;
    client.wait_connected().await;

    let received = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&received);
    let drain = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(event, PeerEvent::Rtp(_)) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    client.start_sending(mid).await;

    until("rtp to reach the server", || {
        (received.load(Ordering::Relaxed) > 0).then_some(())
    })
    .await;

    let before = peer.handle.ice_creds();
    let restart = client.ice_restart().await;
    let answer = peer
        .handle
        .accept_offer(restart)
        .await
        .expect("the server answers the ice restart");
    client.answer(answer).await;

    let after = peer.handle.ice_creds();
    assert_ne!(after.ufrag, before.ufrag, "str0m rotated the local ufrag");
    assert_eq!(udp.router().session_for_ufrag(&before.ufrag), None);
    assert_eq!(
        udp.router().session_for_ufrag(&after.ufrag),
        Some(peer.handle.id())
    );

    let checkpoint = received.load(Ordering::Relaxed);
    let sent = client.sent(mid).await;

    until("rtp to keep flowing after the ice restart", || {
        (received.load(Ordering::Relaxed) > checkpoint + 50).then_some(())
    })
    .await;

    assert!(client.sent(mid).await > sent);

    drain.abort();
    client.stop();
    peer.handle.close();
}
