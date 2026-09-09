use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mediacore_transport::str0m::change::{SdpAnswer, SdpPendingOffer};
use mediacore_transport::str0m::channel::ChannelId;
use mediacore_transport::str0m::net::{Protocol, Receive};
use mediacore_transport::str0m::{Candidate, Event, Input, Output, Rtc};
use mediacore_transport::{
    DataChannel, DataSettings, LOSSY_LABEL, OutgoingData, PeerConfig, PeerEvent, PeerEvents,
    PeerHandle, RELIABLE_LABEL, RouterConfig, SpawnedPeer, TransportCrypto, UdpRouter, spawn_peer,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PATIENCE: Duration = Duration::from_secs(15);

enum Command {
    Offer(Vec<String>, oneshot::Sender<Result<String, String>>),
    Answer(String, oneshot::Sender<Result<(), String>>),
    Write(String, Vec<u8>, oneshot::Sender<Result<(), String>>),
    Received(oneshot::Sender<Vec<(String, Vec<u8>)>>),
    WaitOpen(String, oneshot::Sender<()>),
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
            declared: Vec::new(),
            open: Vec::new(),
            received: Vec::new(),
            waiters: Vec::new(),
        };

        Self {
            commands,
            task: tokio::spawn(actor.run()),
        }
    }

    async fn offer_channels(&self, labels: &[&str]) -> String {
        let (reply, response) = oneshot::channel();
        let labels = labels.iter().map(|label| (*label).to_owned()).collect();
        self.commands
            .send(Command::Offer(labels, reply))
            .await
            .unwrap();
        response.await.unwrap().expect("the client offers channels")
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

    async fn write(&self, label: &str, payload: &[u8]) {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Write(label.to_owned(), payload.to_vec(), reply))
            .await
            .unwrap();
        response.await.unwrap().expect("the client writes");
    }

    async fn received(&self) -> Vec<(String, Vec<u8>)> {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Received(reply)).await.unwrap();
        response.await.unwrap()
    }

    async fn wait_open(&self, label: &str) {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::WaitOpen(label.to_owned(), reply))
            .await
            .unwrap();
        tokio::time::timeout(PATIENCE, response)
            .await
            .expect("the client sees its data channel open")
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
    pending: Option<SdpPendingOffer>,
    declared: Vec<(ChannelId, String)>,
    open: Vec<(ChannelId, String)>,
    received: Vec<(String, Vec<u8>)>,
    waiters: Vec<(String, oneshot::Sender<()>)>,
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
                Ok(Output::Event(Event::ChannelOpen(id, label))) => {
                    self.open.push((id, label.clone()));

                    let mut index = 0;
                    while index < self.waiters.len() {
                        if self.waiters[index].0 == label {
                            let (_, reply) = self.waiters.remove(index);
                            let _ = reply.send(());
                        } else {
                            index += 1;
                        }
                    }
                }
                Ok(Output::Event(Event::ChannelData(data))) => {
                    if let Some((_, label)) = self
                        .open
                        .iter()
                        .find(|(known, _)| *known == data.id)
                        .cloned()
                    {
                        self.received.push((label, data.data));
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

    fn on_datagram(&mut self, at: Instant, source: SocketAddr, data: &[u8]) {
        if let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, data) {
            let _ = self.rtc.handle_input(Input::Receive(at, receive));
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Offer(labels, reply) => {
                let mut api = self.rtc.sdp_api();
                let declared: Vec<(ChannelId, String)> = labels
                    .into_iter()
                    .map(|label| (api.add_channel(label.clone()), label))
                    .collect();

                match api.apply() {
                    Some((offer, pending)) => {
                        self.declared.extend(declared);
                        self.pending = Some(pending);
                        let _ = reply.send(Ok(offer.to_sdp_string()));
                    }
                    None => {
                        let _ = reply.send(Err("nothing to negotiate".to_owned()));
                    }
                }
            }
            Command::Answer(sdp, reply) => {
                let _ = reply.send(self.on_answer(&sdp));
            }
            Command::Write(label, payload, reply) => {
                let id = self
                    .open
                    .iter()
                    .chain(self.declared.iter())
                    .find(|(_, known)| *known == label)
                    .map(|(id, _)| *id);

                let answer = match id {
                    Some(id) => match self.rtc.channel(id) {
                        Some(mut channel) => channel
                            .write(true, &payload)
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                        None => Err(format!("channel {label} is not writable")),
                    },
                    None => Err(format!("channel {label} is unknown")),
                };

                let _ = reply.send(answer);
            }
            Command::Received(reply) => {
                let _ = reply.send(self.received.clone());
            }
            Command::WaitOpen(label, reply) => {
                if self.open.iter().any(|(_, known)| *known == label) {
                    let _ = reply.send(());
                } else {
                    self.waiters.push((label, reply));
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
            .map_err(|error| error.to_string())
    }
}

async fn server(data: Option<DataSettings>) -> (TransportCrypto, UdpRouter, SpawnedPeer) {
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
            data,
            ..PeerConfig::publisher()
        },
    )
    .expect("the peer starts");

    (crypto, udp, peer)
}

async fn wait_for<F>(events: &mut PeerEvents, label: &str, mut matches: F) -> PeerEvent
where
    F: FnMut(&PeerEvent) -> bool,
{
    let found = tokio::time::timeout(PATIENCE, async {
        while let Some(event) = events.recv().await {
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

async fn connected_client(
    crypto: &TransportCrypto,
    handle: &PeerHandle,
    events: &mut PeerEvents,
) -> TestClient {
    let client = TestClient::start(crypto).await;

    let offer = client.offer_channels(&[RELIABLE_LABEL, LOSSY_LABEL]).await;
    let answer = handle
        .accept_offer(offer)
        .await
        .expect("the server answers the offer");
    client.answer(answer).await;

    wait_for(events, "the transport to connect", |event| {
        matches!(event, PeerEvent::Connected)
    })
    .await;

    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_data_packet_reaches_the_server_and_comes_back() {
    let (crypto, _udp, peer) = server(Some(DataSettings::default())).await;
    let handle = peer.handle;
    let mut events = peer.events;
    let client = connected_client(&crypto, &handle, &mut events).await;

    wait_for(&mut events, "the reliable channel to open", |event| {
        matches!(event, PeerEvent::ChannelOpen(DataChannel::Reliable))
    })
    .await;

    client.wait_open(RELIABLE_LABEL).await;
    client.write(RELIABLE_LABEL, b"a line of chat").await;

    let event = wait_for(&mut events, "the packet to arrive", |event| {
        matches!(event, PeerEvent::Data(_))
    })
    .await;

    let PeerEvent::Data(incoming) = event else {
        panic!("the event is a data packet");
    };
    assert_eq!(incoming.channel, DataChannel::Reliable);
    assert_eq!(incoming.payload, b"a line of chat");

    handle
        .write_data(OutgoingData {
            channel: DataChannel::Reliable,
            binary: true,
            payload: Arc::from(b"and the answer".as_slice()),
        })
        .expect("the server writes back");

    let delivered = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(delivered) = client
                .received()
                .await
                .into_iter()
                .find(|(label, _)| label == RELIABLE_LABEL)
            {
                return delivered;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the client receives the answer");

    assert_eq!(delivered.1, b"and the answer");

    client.stop();
    handle.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_without_data_channels_reports_no_packets() {
    let (crypto, _udp, peer) = server(None).await;
    let handle = peer.handle;
    let mut events = peer.events;
    let client = connected_client(&crypto, &handle, &mut events).await;

    client.wait_open(RELIABLE_LABEL).await;
    client.write(RELIABLE_LABEL, b"nobody is listening").await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let error = handle
        .write_data(OutgoingData {
            channel: DataChannel::Reliable,
            binary: true,
            payload: Arc::from(b"nor answering".as_slice()),
        })
        .expect_err("a peer without data channels refuses to write");
    assert!(error.to_string().contains("without data channels"));

    client.stop();
    handle.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_packet_is_dropped_rather_than_forwarded() {
    let (crypto, _udp, peer) = server(Some(DataSettings { max_packet: 512 })).await;
    let handle = peer.handle;
    let mut events = peer.events;
    let client = connected_client(&crypto, &handle, &mut events).await;

    wait_for(&mut events, "the reliable channel to open", |event| {
        matches!(event, PeerEvent::ChannelOpen(DataChannel::Reliable))
    })
    .await;

    client.wait_open(RELIABLE_LABEL).await;
    client.write(RELIABLE_LABEL, &vec![7u8; 4096]).await;

    let counted = until("the oversized packet to be counted", || {
        let counted = handle.oversized_data_packets();
        (counted > 0).then_some(counted)
    })
    .await;
    assert_eq!(counted, 1);

    client.write(RELIABLE_LABEL, b"small enough").await;

    let event = wait_for(&mut events, "the small packet to arrive", |event| {
        matches!(event, PeerEvent::Data(_))
    })
    .await;

    let PeerEvent::Data(incoming) = event else {
        panic!("the event is a data packet");
    };
    assert_eq!(incoming.payload, b"small enough");

    client.stop();
    handle.close();
}
