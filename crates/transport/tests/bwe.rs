use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mediacore_transport::str0m::change::{SdpAnswer, SdpOffer};
use mediacore_transport::str0m::format::Codec;
use mediacore_transport::str0m::media::{Direction, MediaKind, Mid, Pt, Rid};
use mediacore_transport::str0m::net::{Protocol, Receive};
use mediacore_transport::str0m::rtp::{ExtensionValues, RtpPacket, SeqNo, Ssrc};
use mediacore_transport::str0m::{Candidate, Event, Input, Output, Rtc};
use mediacore_transport::{
    Bitrate, BweSettings, MediaSpec, Negotiation, OutgoingRtp, PeerConfig, PeerEvent, PeerEvents,
    PeerHandle, RouterConfig, SendQueueEntry, SendStream, SpawnedPeer, TransportCrypto, UdpRouter,
    spawn_peer,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PATIENCE: Duration = Duration::from_secs(20);
const SAMPLE: Duration = Duration::from_millis(5);
const INITIAL: Bitrate = Bitrate::kbps(600);
const VIDEO_TICKS: u32 = 3_000;
const AUDIO_TICKS: u32 = 960;
const FIRST_SEQ: u64 = 40_000;
const FIRST_TIME: u32 = 7_000_000;
const VIDEO_PAYLOAD: [u8; 1_100] = [0x9d; 1_100];
const AUDIO_PAYLOAD: [u8; 80] = [0x41; 80];
const BURST: u64 = 600;
const AUDIO_BURST: u64 = 40;
// The burst's bodies as a byte total, and a statement about the pacer only.
// str0m adds a packet's body to `bytes_tx` inside `poll_rtp`, which is before
// the datagram is handed back to us and therefore before anything of ours could
// send it or fail to, and it adds a retransmitted body again on top. So this
// says the pacer let go of about this much; it counts no packets and it says
// nothing about what left the host. What left the host is `Arrivals`.
const BURST_PAYLOAD: u64 = BURST * VIDEO_PAYLOAD.len() as u64;
const PACED_AGE: Duration = Duration::from_millis(500);
const PACING_RATIO: u32 = 3;

#[derive(Debug, Default)]
struct Drain {
    /// The longest a queued packet was still waiting when str0m last looked --
    /// the one number that separates a pacer holding a burst from one that is
    /// not. See `waited`.
    waited: Duration,
    /// The deepest backlog seen, reported and never asserted on: how many writes
    /// the peer's actor drained before the pacer next ran is scheduling and not
    /// pacing, and on one commit and one machine the null pacer has been caught
    /// holding nothing at all, a couple of hundred packets, and the whole burst.
    peak: usize,
    /// What the far end took off its own socket, which is the half of this that
    /// our own send path cannot fake. See `drain_a_burst`.
    arrivals: Arrivals,
    /// Payload bytes the pacer has let go of, as str0m counts them -- see
    /// `BURST_PAYLOAD`. Held beside `arrivals` because between the two of them a
    /// missing packet has a side of the send path it went missing on.
    handed_over: u64,
    dropped_writes: u64,
}

/// What the far end has taken off its own socket.
///
/// The client is a str0m of its own, so this is a receiver's account of our
/// sending: after SRTP, after RTX has been unwrapped back into the stream it
/// repairs, and after str0m has dropped the packets it had already seen.
#[derive(Debug, Default, Clone, Copy)]
struct Arrivals {
    /// Distinct packets delivered to the client. A resend of a packet that got
    /// there the first time is a duplicate str0m drops before we see it, so
    /// nothing in here is counted twice.
    packets: usize,
    /// Packets that arrived behind a higher sequence number on their own stream.
    ///
    /// Nothing reorders on the loopback: one socket, one queue, and the reader
    /// hands datagrams to str0m in the order it read them off it. So a packet
    /// out of order is a packet that did not arrive when it was first sent and
    /// was resent afterwards -- which is how a send path that swallows a
    /// datagram shows up at the far end even in the runs where RTX goes on to
    /// make the count of packets whole.
    out_of_order: usize,
}

enum Command {
    Answer(String, oneshot::Sender<Result<String, String>>),
    Arrived(oneshot::Sender<Arrivals>),
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
            arrivals: Arrivals::default(),
            latest: HashMap::new(),
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

    async fn arrivals(&self) -> Arrivals {
        let (reply, response) = oneshot::channel();
        self.commands.send(Command::Arrived(reply)).await.unwrap();
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
    arrivals: Arrivals,
    /// The highest sequence number seen on each stream so far, which is all it
    /// takes to notice a packet that came late.
    latest: HashMap<Ssrc, SeqNo>,
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
                Ok(Output::Event(Event::RtpPacket(packet))) => self.on_rtp(&packet),
                Ok(Output::Event(_)) => {}
                Err(_) => {
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    /// One packet the client's str0m has accepted off the socket.
    ///
    /// Keyed by SSRC and not by mid, because a resend arrives on the RTX stream
    /// and str0m rewrites it back onto the SSRC and the sequence number of the
    /// stream it repairs before handing it over -- so the header we are given is
    /// already the right place to file it.
    fn on_rtp(&mut self, packet: &RtpPacket) {
        self.arrivals.packets += 1;

        let latest = self
            .latest
            .entry(packet.header.ssrc)
            .or_insert(packet.seq_no);

        if packet.seq_no > *latest {
            *latest = packet.seq_no;
        } else if packet.seq_no < *latest {
            self.arrivals.out_of_order += 1;
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
            Command::Arrived(reply) => {
                let _ = reply.send(self.arrivals);
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

/// How long the pacer has made the oldest queued packet wait, on str0m's clock.
///
/// `queue_info` behind `send_queues` is a cache the pacer refreshes when it runs
/// and not a value computed on read, which is why the entry carries the instant
/// it was taken as well as the instant the packet was enqueued. The distance
/// between those two is the wait the pacer imposed. Measuring against
/// `Instant::now()` instead adds however long ago the pacer last ran plus
/// however long our own reply took to come back, and a test binary holding more
/// threads than the machine has cores can be descheduled for longer than
/// anything a pacer would do -- that number says nothing about pacing.
fn waited(queue: &SendQueueEntry) -> Duration {
    queue
        .sampled_at
        .map(|sampled_at| queue.age(sampled_at))
        .unwrap_or_default()
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

/// Put a burst through a peer, watch its send queue drain, and ask the far end
/// what it got.
///
/// The far end is asked because nothing on the sending side can answer for our
/// own send path. str0m counts a packet's bytes in `poll_rtp`, before it hands
/// the datagram back to `PeerActor::drive` for us to put on the socket, so a
/// datagram that we then lose -- a `send_to` that fails and is logged and
/// forgotten, or any future batching that discards a `Transmit` -- is already
/// counted as sent. Only the receiver can tell the difference between a pacer
/// that delayed a packet and a send path that ate it, and one of those two is a
/// subscriber losing media under load.
///
/// Which is also why only the paced half of this can be asked. A burst the null
/// pacer releases is 660 kB pushed into one socket in a couple of hundred
/// milliseconds, and the reader is a task on the same single-threaded runtime,
/// so whatever it has not picked up yet is sitting in the kernel's receive
/// buffer -- which defaults to 768 kB on macOS and 208 kB on Linux. The blast
/// does not fit in the smaller one, so its tail is dropped there any time the
/// reader is not scheduled promptly, and the better the null pacer is at its job
/// the tighter the blast and the more of it goes: counting arrivals there would
/// make the pacer working correctly a reason to fail. The leaky bucket does none
/// of that -- it hands a few packets over per poll across the several seconds it
/// takes to drain, which is never a receive buffer's worth in flight -- so the
/// burst that has to arrive in full is the paced one, and a send path that loses
/// datagrams fails there. The null pacer's run is watched for its queue and for
/// what str0m let go of, and is asked nothing about arrivals.
async fn drain_a_burst(bwe: Option<BweSettings>) -> Drain {
    let (crypto, _udp, peer) = server(bwe).await;
    let SpawnedPeer {
        handle,
        mut events,
        task: _task,
    } = peer;
    let client = Client::start(&crypto).await;
    let (mid, pt) = subscribe(&handle, &client, MediaKind::Video, "video").await;
    client.wait_connected().await;

    // The leaky bucket is what comes with the estimator, so this is the same
    // question as which pacer the peer was built with -- and it decides which of
    // the two finish lines this run gets.
    let paced = handle.estimates_bandwidth();
    let deadline = Instant::now() + PATIENCE;
    write_burst(&handle, mid, pt, MediaKind::Video, BURST);

    let mut drain = Drain::default();

    while Instant::now() < deadline {
        let queues = handle.send_queues().await.expect("the peer is alive");

        if let Some(queue) = queue_for(&queues, mid) {
            drain.waited = drain.waited.max(waited(queue));
            drain.peak = drain.peak.max(queue.packets);
        }

        drain.arrivals = client.arrivals().await;

        let finished = if paced {
            drain.arrivals.packets as u64 >= BURST
        } else {
            drain.handed_over >= BURST_PAYLOAD
        };

        if finished {
            break;
        }

        // Blocking on the report is also the pause between samples. The control
        // channel is unbounded and a burst is long enough that leaving it unread
        // only grows a queue behind us.
        if let Ok(Some(PeerEvent::PeerReport(report))) =
            tokio::time::timeout(SAMPLE, events.recv_control()).await
        {
            drain.handed_over = drain.handed_over.max(report.bytes_tx);
        }
    }

    drain.dropped_writes = handle.dropped_rtp_writes();

    client.stop();
    drain
}

#[tokio::test]
async fn the_leaky_bucket_holds_a_burst_the_null_pacer_lets_straight_through() {
    let paced = drain_a_burst(Some(BweSettings::new(INITIAL))).await;

    assert!(
        paced.arrivals.packets as u64 >= BURST,
        "pacing delays packets, it must never swallow them: the far end took {} of the {BURST} \
         packet burst off its own socket, while the pacer had let go of {} of {BURST_PAYLOAD} \
         payload bytes",
        paced.arrivals.packets,
        paced.handed_over
    );
    assert_eq!(
        paced.arrivals.out_of_order, 0,
        "the loopback delivers in order, so a packet arriving behind a later one is a packet our \
         send path lost and str0m resent: {} of the burst got there that way, and RTX making the \
         count above whole is not the same thing as the burst having been sent",
        paced.arrivals.out_of_order
    );
    assert_eq!(paced.dropped_writes, 0);
    assert!(
        paced.waited >= PACED_AGE,
        "the leaky bucket drains a {BURST} packet burst over its own two second queue limit, \
         the oldest packet only ever waited {:?}",
        paced.waited
    );

    let unpaced = drain_a_burst(None).await;

    assert!(
        unpaced.handed_over >= BURST_PAYLOAD,
        "the null pacer has no reason to hold anything back, and this one let go of only {} of \
         the burst's {BURST_PAYLOAD} payload bytes",
        unpaced.handed_over
    );
    assert_eq!(unpaced.dropped_writes, 0);
    assert!(
        unpaced.waited * PACING_RATIO < paced.waited,
        "the null pacer sends whatever it is holding the moment it is polled, so a packet is \
         never waiting when str0m looks -- {} of them were queued at once and the oldest had \
         waited {:?}, against {:?} behind the leaky bucket",
        unpaced.peak,
        unpaced.waited,
        paced.waited
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
    let mut video_waited = Duration::ZERO;
    let mut audio_waited = Duration::ZERO;
    let mut video_behind_audio = None;
    let mut arrivals = Arrivals::default();

    while Instant::now() < deadline {
        let queues = peer.handle.send_queues().await.expect("the peer is alive");
        let video_queue = queue_for(&queues, video);

        if let Some(queue) = video_queue {
            video_waited = video_waited.max(waited(queue));
        }

        // Paired with the video backlog out of the same snapshot: how much of
        // the paced burst was still in hand while audio was still queued is the
        // queue-jumping itself, and it is a count rather than a duration, so a
        // machine that runs this half as fast reports the same thing.
        if let Some(queue) = queue_for(&queues, audio).filter(|queue| queue.packets > 0) {
            audio_waited = audio_waited.max(waited(queue));
            video_behind_audio = video_queue.map(|video| video.packets);
        }

        // Both bursts here are paced or small, which is what makes the far end
        // answerable: the video leaves at the pacer's rate rather than all at
        // once and the audio beside it is forty small packets, so there is never
        // enough in flight at any moment to overrun a receive buffer. See
        // `drain_a_burst`, which can only ask this of its paced half.
        arrivals = client.arrivals().await;

        if arrivals.packets as u64 >= BURST + AUDIO_BURST {
            break;
        }

        tokio::time::sleep(SAMPLE).await;
    }

    assert!(
        arrivals.packets as u64 >= BURST + AUDIO_BURST,
        "both bursts arrive in full, and the far end has {} of {}",
        arrivals.packets,
        BURST + AUDIO_BURST
    );
    assert_eq!(
        arrivals.out_of_order, 0,
        "the loopback delivers in order, so a packet arriving behind a later one is a packet our \
         send path lost and str0m resent: {} of the two bursts got there that way",
        arrivals.out_of_order
    );
    assert!(
        video_waited >= PACED_AGE,
        "the video burst must queue behind the leaky bucket for this test to mean anything, \
         its oldest packet waited only {video_waited:?}"
    );
    assert!(
        audio_waited * PACING_RATIO < video_waited,
        "audio is declared unpaced and is emitted ahead of a paced queue, it must not wait \
         {audio_waited:?} while video waits {video_waited:?}"
    );

    // Never catching the audio queue with a packet in it is the same statement
    // made more strongly -- it went out between two of our looks -- so there is
    // nothing to check in that case.
    if let Some(backlog) = video_behind_audio {
        assert!(
            backlog as u64 * 2 >= BURST,
            "the audio queue must clear while the leaky bucket still has the video burst in \
             hand, and it was down to {backlog} of {BURST} packets"
        );
    }

    client.stop();
}
