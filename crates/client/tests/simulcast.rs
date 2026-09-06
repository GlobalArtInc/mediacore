use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use mediacore_client::proto::{self as mediacore, signal_request, signal_response};
use mediacore_client::{
    Client, ClientOptions, LayerId, MediaProfile, PublishedTrack, SyntheticSource, TrackReport,
    TrackSpec, layer_tag,
};
use prost::Message as _;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpWrite, SeqNo};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

const ROOM: &str = "demo";
const IDENTITY: &str = "alice";
const BOB_SID: &str = "PA_bob";
const BOB_TRACK: &str = "TR_bobcam";
const POLL: Duration = Duration::from_millis(20);
const PATIENCE: Duration = Duration::from_secs(10);

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LayerCount {
    packets: u64,
    with_rid: u64,
    ssrc: u32,
}

#[derive(Debug, Clone, Default)]
struct Seen {
    published: BTreeMap<u8, LayerCount>,
    unlabelled: u64,
    settings: Vec<mediacore::UpdateTrackSettings>,
}

impl Seen {
    fn layer(&self, tag: u8) -> LayerCount {
        self.published.get(&tag).copied().unwrap_or_default()
    }

    fn quality_asked(&self) -> Option<mediacore::VideoQuality> {
        self.settings
            .last()
            .and_then(|settings| mediacore::VideoQuality::try_from(settings.quality).ok())
    }
}

enum Control {
    Push(Box<signal_response::Message>),
    SwitchTo(u8),
}

struct Peer {
    addr: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    control: mpsc::UnboundedSender<Control>,
}

impl Peer {
    fn seen(&self) -> Seen {
        self.seen
            .lock()
            .expect("the peer state is poisoned")
            .clone()
    }

    fn push(&self, message: signal_response::Message) {
        self.control
            .send(Control::Push(Box::new(message)))
            .expect("the peer is listening");
    }

    fn switch_to(&self, tag: u8) {
        self.control
            .send(Control::SwitchTo(tag))
            .expect("the peer is listening");
    }

    async fn wait_until(&self, timeout: Duration, wanted: impl Fn(&Seen) -> bool) -> Seen {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let seen = self.seen();
            if wanted(&seen) {
                return seen;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "the peer never saw what the test wanted: {seen:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

async fn spawn_peer() -> Peer {
    str0m::crypto::from_feature_flags().install_process_default();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (control, controls) = mpsc::unbounded_channel();

    let state = seen.clone();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("a signalling connection");
        let socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("a websocket handshake");
        run_peer(socket, state, controls).await;
    });

    Peer {
        addr,
        seen,
        control,
    }
}

struct Endpoint {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
}

impl Endpoint {
    async fn bind() -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let local = socket.local_addr().unwrap();

        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true)
            .enable_vp8(true)
            .build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap())
            .expect("a host candidate");

        Self { rtc, socket, local }
    }

    async fn poll(&mut self, seen: &Mutex<Seen>) -> Option<Instant> {
        loop {
            if !self.rtc.is_alive() {
                return None;
            }

            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return Some(at.max(Instant::now())),
                Ok(Output::Transmit(transmit)) => {
                    let _ = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await;
                }
                Ok(Output::Event(Event::RtpPacket(packet))) => {
                    let tag = layer_tag(&packet.payload);
                    let mut seen = seen.lock().expect("the peer state is poisoned");

                    match tag {
                        Some(tag) => {
                            let layer = seen.published.entry(tag).or_default();
                            layer.packets += 1;
                            layer.ssrc = *packet.header.ssrc;
                            if packet.header.ext_vals.rid.is_some() {
                                layer.with_rid += 1;
                            }
                        }
                        None => seen.unlabelled += 1,
                    }
                }
                Ok(Output::Event(_)) => {}
                Err(error) => panic!("peer transport failed: {error}"),
            }
        }
    }

    fn receive(&mut self, at: Instant, source: SocketAddr, data: &[u8]) {
        if let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, data) {
            let _ = self.rtc.handle_input(Input::Receive(at, receive));
        }
    }
}

struct Downstream {
    pending: Option<SdpPendingOffer>,
    mid: Option<Mid>,
    pt: Option<Pt>,
    source: SyntheticSource,
    tag: u8,
    seq: u64,
    ticks: u32,
    sending: bool,
}

impl Downstream {
    fn new() -> Self {
        Self {
            pending: None,
            mid: None,
            pt: None,
            source: SyntheticSource::new(layer_profile(b'q')),
            tag: b'q',
            seq: 0,
            ticks: 0,
            sending: false,
        }
    }

    fn switch_to(&mut self, tag: u8) {
        if self.tag == tag {
            return;
        }

        self.tag = tag;
        self.source = SyntheticSource::new(layer_profile(tag));
    }
}

fn layer_profile(tag: u8) -> MediaProfile {
    let size = match tag {
        b'q' => 300,
        b'h' => 600,
        _ => 900,
    };

    MediaProfile::vp8()
        .with_payload_size(size)
        .with_layer_tag(tag)
}

async fn run_peer(
    socket: WebSocketStream<TcpStream>,
    seen: Arc<Mutex<Seen>>,
    mut controls: mpsc::UnboundedReceiver<Control>,
) {
    let mut up = Endpoint::bind().await;
    let mut down = Endpoint::bind().await;
    let mut stream_out = Downstream::new();

    let (mut sink, mut stream) = socket.split();
    send(&mut sink, signal_response::Message::Join(join_response())).await;

    let mut datagrams = mpsc::channel::<(Instant, SocketAddr, Bytes, bool)>(256);
    read_datagrams(up.socket.clone(), datagrams.0.clone(), true);
    read_datagrams(down.socket.clone(), datagrams.0.clone(), false);

    let mut media = tokio::time::interval(Duration::from_millis(10));

    loop {
        let deadlines = [up.poll(&seen).await, down.poll(&seen).await];
        let deadline = deadlines.into_iter().flatten().min();
        let Some(deadline) = deadline else {
            return;
        };

        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);

        tokio::select! {
            frame = stream.next() => {
                let Some(Ok(Message::Binary(data))) = frame else { return };
                let Ok(request) = mediacore::SignalRequest::decode(data.as_ref()) else { continue };
                let Some(message) = request.message else { continue };
                if !on_request(&mut up, &mut down, &mut stream_out, &seen, &mut sink, message).await
                {
                    return;
                }
            }
            control = controls.recv() => {
                match control {
                    Some(Control::Push(message)) => send(&mut sink, *message).await,
                    Some(Control::SwitchTo(tag)) => stream_out.switch_to(tag),
                    None => return,
                }
            }
            datagram = datagrams.1.recv() => {
                let Some((at, source, data, upstream)) = datagram else { return };
                if upstream {
                    up.receive(at, source, &data);
                } else {
                    down.receive(at, source, &data);
                }
            }
            _ = media.tick() => write_media(&mut down, &mut stream_out),
            _ = &mut sleep => {
                let now = Instant::now();
                let _ = up.rtc.handle_input(Input::Timeout(now));
                let _ = down.rtc.handle_input(Input::Timeout(now));
            }
        }
    }
}

fn read_datagrams(
    socket: Arc<UdpSocket>,
    sender: mpsc::Sender<(Instant, SocketAddr, Bytes, bool)>,
    upstream: bool,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 2048];
        while let Ok((size, source)) = socket.recv_from(&mut buffer).await {
            let data = Bytes::copy_from_slice(&buffer[..size]);
            if sender
                .send((Instant::now(), source, data, upstream))
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

fn write_media(down: &mut Endpoint, state: &mut Downstream) {
    if !state.sending {
        return;
    }

    let (Some(mid), Some(pt)) = (state.mid, state.pt) else {
        return;
    };

    let packet = state.source.next_packet();
    let seq: SeqNo = state.seq.into();
    let time = state.ticks;

    state.seq += 1;
    state.ticks = state.ticks.wrapping_add(packet.ticks);

    let mut direct = down.rtc.direct_api();
    let Some(stream) = direct.stream_tx_by_mid(mid, None) else {
        return;
    };

    stream.set_unpaced(true);
    stream.write_rtp(
        RtpWrite::new(pt, seq, time, Instant::now(), packet.payload).marker(packet.marker),
    );
}

async fn on_request(
    up: &mut Endpoint,
    down: &mut Endpoint,
    state: &mut Downstream,
    seen: &Mutex<Seen>,
    sink: &mut Sink,
    message: signal_request::Message,
) -> bool {
    match message {
        signal_request::Message::AddTrack(add) => {
            let published =
                signal_response::Message::TrackPublished(mediacore::TrackPublishedResponse {
                    cid: add.cid.clone(),
                    track: Some(published_track(add)),
                });
            send(sink, published).await;
        }
        signal_request::Message::Offer(offer) => {
            let offer_id = offer.id;
            let offer = SdpOffer::from_sdp_string(&offer.sdp).expect("a parseable offer");
            let answer = up.rtc.sdp_api().accept_offer(offer).expect("an answer");
            let answer = signal_response::Message::Answer(mediacore::SessionDescription {
                r#type: "answer".to_owned(),
                sdp: answer.to_sdp_string(),
                id: offer_id,
                ..mediacore::SessionDescription::default()
            });
            send(sink, answer).await;
        }
        signal_request::Message::Subscription(update) => {
            if update.subscribe && state.mid.is_none() && state.pending.is_none() {
                offer_track(down, state, sink).await;
            }
        }
        signal_request::Message::TrackSetting(settings) => {
            seen.lock()
                .expect("the peer state is poisoned")
                .settings
                .push(settings);
        }
        signal_request::Message::Answer(answer) => {
            let parsed = SdpAnswer::from_sdp_string(&answer.sdp).expect("a parseable answer");
            let pending = state.pending.take().expect("an outstanding offer");
            down.rtc
                .sdp_api()
                .accept_answer(pending, parsed)
                .expect("the answer applies");

            let mid = state.mid.expect("a negotiated mid");
            state.pt = Some(vp8_pt(&down.rtc, mid));
            state.sending = true;
        }
        signal_request::Message::Trickle(trickle) => {
            if trickle.candidate_init.is_empty() {
                return true;
            }

            #[derive(serde::Deserialize)]
            struct CandidateInit {
                candidate: String,
            }

            let Ok(init) = serde_json::from_str::<CandidateInit>(&trickle.candidate_init) else {
                return true;
            };
            let Ok(candidate) = Candidate::from_sdp_string(&init.candidate) else {
                return true;
            };

            if trickle.target == mediacore::SignalTarget::Subscriber as i32 {
                down.rtc.add_remote_candidate(candidate);
            } else {
                up.rtc.add_remote_candidate(candidate);
            }
        }
        signal_request::Message::PingReq(ping) => {
            let pong = signal_response::Message::PongResp(mediacore::Pong {
                last_ping_timestamp: ping.timestamp,
                timestamp: 0,
            });
            send(sink, pong).await;
        }
        signal_request::Message::Leave(_) => return false,
        _ => {}
    }

    true
}

#[allow(deprecated)]
fn published_track(add: mediacore::AddTrackRequest) -> mediacore::TrackInfo {
    mediacore::TrackInfo {
        sid: format!("TR_{}", add.cid),
        r#type: add.r#type,
        name: add.name,
        source: add.source,
        width: add.width,
        height: add.height,
        mime_type: "video/vp8".to_owned(),
        simulcast: add.layers.len() > 1,
        layers: add.layers,
        ..mediacore::TrackInfo::default()
    }
}

#[allow(deprecated)]
fn bob_camera() -> mediacore::TrackInfo {
    mediacore::TrackInfo {
        sid: BOB_TRACK.to_owned(),
        name: "cam".to_owned(),
        r#type: mediacore::TrackType::Video as i32,
        simulcast: true,
        ..mediacore::TrackInfo::default()
    }
}

async fn offer_track(down: &mut Endpoint, state: &mut Downstream, sink: &mut Sink) {
    let mut api = down.rtc.sdp_api();
    let mid = api.add_media(
        MediaKind::Video,
        Direction::SendOnly,
        Some("ST_bob".to_owned()),
        Some(BOB_TRACK.to_owned()),
        None,
    );
    let (offer, pending) = api.apply().expect("an offer");

    state.mid = Some(mid);
    state.pending = Some(pending);

    let offer = signal_response::Message::Offer(mediacore::SessionDescription {
        r#type: "offer".to_owned(),
        sdp: offer.to_sdp_string(),
        id: 11,
        mid_to_track_id: [(mid.to_string(), BOB_TRACK.to_owned())]
            .into_iter()
            .collect(),
    });
    send(sink, offer).await;
}

fn vp8_pt(rtc: &Rtc, mid: Mid) -> Pt {
    let media = rtc.media(mid).expect("a negotiated media");

    rtc.codec_config()
        .params()
        .iter()
        .find(|params| {
            params.spec().codec == Codec::Vp8 && media.remote_pts().contains(&params.pt())
        })
        .map(|params| params.pt())
        .expect("a vp8 payload type")
}

async fn send(sink: &mut Sink, message: signal_response::Message) {
    let response = mediacore::SignalResponse {
        message: Some(message),
    };
    let _ = sink
        .send(Message::Binary(Bytes::from(response.encode_to_vec())))
        .await;
}

fn join_response() -> mediacore::JoinResponse {
    mediacore::JoinResponse {
        room: Some(mediacore::Room {
            sid: "RM_test".to_owned(),
            name: ROOM.to_owned(),
            ..mediacore::Room::default()
        }),
        participant: Some(mediacore::ParticipantInfo {
            sid: "PA_test".to_owned(),
            identity: IDENTITY.to_owned(),
            ..mediacore::ParticipantInfo::default()
        }),
        other_participants: vec![mediacore::ParticipantInfo {
            sid: BOB_SID.to_owned(),
            identity: "bob".to_owned(),
            tracks: vec![bob_camera()],
            ..mediacore::ParticipantInfo::default()
        }],
        subscriber_primary: true,
        ping_interval: 15,
        ping_timeout: 60,
        ..mediacore::JoinResponse::default()
    }
}

fn options() -> ClientOptions {
    ClientOptions {
        response_timeout: PATIENCE,
        ..ClientOptions::default()
    }
}

async fn connect(peer: &Peer) -> Client {
    Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client")
}

async fn publish_simulcast(client: &Client) -> PublishedTrack {
    let track = client
        .publish(TrackSpec::simulcast_video("cam"))
        .await
        .expect("a published track");

    client
        .wait_connected(PATIENCE)
        .await
        .expect("the publisher transport connects");

    track
}

async fn subscribed_report(client: &Client, packets: u64) -> TrackReport {
    client
        .wait_for_packets(BOB_TRACK, packets, PATIENCE)
        .await
        .expect("media on the subscribed track")
}

#[tokio::test]
async fn three_encodings_are_published_as_three_tagged_streams() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;
    let track = publish_simulcast(&client).await;

    assert_eq!(track.rids(), vec!["q", "h", "f"]);
    track.start().await.expect("every layer starts");

    let seen = peer
        .wait_until(PATIENCE, |seen| {
            (*b"qhf")
                .into_iter()
                .all(|tag| seen.layer(tag).packets >= 5)
        })
        .await;

    let ssrcs: Vec<u32> = (*b"qhf")
        .into_iter()
        .map(|tag| seen.layer(tag).ssrc)
        .collect();
    assert_eq!(
        ssrcs
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "each layer must arrive on its own ssrc: {ssrcs:?}"
    );

    for tag in *b"qhf" {
        assert!(
            seen.layer(tag).with_rid > 0,
            "layer {} arrived without ever naming its rid",
            char::from(tag)
        );
    }
    assert_eq!(seen.unlabelled, 0);

    let stats = track.layer_stats().await.expect("per layer statistics");
    assert_eq!(stats.len(), 3);
    assert!(stats.iter().all(|layer| layer.stats.sending));
    assert_eq!(
        stats
            .iter()
            .filter_map(|layer| layer.rid.clone())
            .collect::<Vec<_>>(),
        vec!["q", "h", "f"]
    );

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn stopping_one_layer_leaves_the_others_running() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;
    let track = publish_simulcast(&client).await;

    track.start().await.expect("every layer starts");
    peer.wait_until(PATIENCE, |seen| {
        (*b"qhf")
            .into_iter()
            .all(|tag| seen.layer(tag).packets >= 5)
    })
    .await;

    track.stop_layer("f").await.expect("the top layer stops");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stopped = peer.seen();
    let after = peer
        .wait_until(PATIENCE, |seen| {
            seen.layer(b'q').packets >= stopped.layer(b'q').packets + 5
                && seen.layer(b'h').packets >= stopped.layer(b'h').packets + 5
        })
        .await;

    assert_eq!(
        after.layer(b'f').packets,
        stopped.layer(b'f').packets,
        "the stopped layer kept sending"
    );

    let stats = track.layer_stats().await.expect("per layer statistics");
    let sending: Vec<(String, bool)> = stats
        .iter()
        .map(|layer| (layer.rid.clone().unwrap_or_default(), layer.stats.sending))
        .collect();
    assert_eq!(
        sending,
        vec![
            ("q".to_owned(), true),
            ("h".to_owned(), true),
            ("f".to_owned(), false)
        ]
    );

    let top = track.stats_of("f").await.expect("the top layer statistics");
    assert!(!top.sending);
    assert!(top.packets > 0);

    track.start_layer("f").await.expect("the top layer resumes");
    let resumed = peer
        .wait_until(PATIENCE, |seen| {
            seen.layer(b'f').packets >= after.layer(b'f').packets + 5
        })
        .await;
    assert!(resumed.layer(b'f').packets > after.layer(b'f').packets);

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn a_layer_that_does_not_exist_is_refused() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;
    let track = publish_simulcast(&client).await;

    let error = track.start_layer("nope").await.unwrap_err();
    assert!(
        matches!(&error, mediacore_client::ClientError::UnknownLayer(rid) if rid == "nope"),
        "{error}"
    );

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn the_subscriber_reports_the_layer_it_is_receiving() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscription");
    client
        .wait_subscribed(PATIENCE)
        .await
        .expect("the subscriber transport connects");

    let report = subscribed_report(&client, 10).await;

    assert_eq!(report.current_layer(), Some(LayerId::Tag(b'q')));
    assert_eq!(report.tags(), vec![b'q']);
    assert_eq!(report.switch_count, 0);

    let low = report
        .layer(&LayerId::Tag(b'q'))
        .expect("the low layer arrived");
    assert!(low.packets >= 10);
    assert_eq!(low.payload_bytes, low.packets * 300);

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn asking_for_a_higher_quality_switches_the_incoming_stream() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscription");
    client
        .wait_subscribed(PATIENCE)
        .await
        .expect("the subscriber transport connects");

    let before = subscribed_report(&client, 10).await;
    assert_eq!(before.current_layer(), Some(LayerId::Tag(b'q')));

    client
        .request_quality([BOB_TRACK], mediacore::VideoQuality::High)
        .expect("a quality request");

    peer.wait_until(PATIENCE, |seen| {
        seen.quality_asked() == Some(mediacore::VideoQuality::High)
    })
    .await;
    peer.switch_to(b'f');

    let after = subscribed_report(&client, before.packets + 20).await;

    assert_eq!(after.current_layer(), Some(LayerId::Tag(b'f')));
    assert_eq!(after.tags(), vec![b'q', b'f']);
    assert_eq!(after.switch_count, 1);

    let switch = after.last_switch().expect("a recorded switch");
    assert_eq!(switch.from, LayerId::Tag(b'q'));
    assert_eq!(switch.to, LayerId::Tag(b'f'));
    assert_eq!(switch.sequence_step(), 1);
    assert!(switch.timestamp_step() > 0);
    assert!(switch.is_seamless(), "{switch:?}");

    let high = after
        .layer(&LayerId::Tag(b'f'))
        .expect("the high layer arrived");
    assert_eq!(high.payload_bytes, high.packets * 900);
    assert_eq!(
        high.sequence.expect("a sequence span").first,
        switch.sequence
    );

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn a_size_request_names_the_track_it_is_about() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscription");
    client
        .request_video_size([BOB_TRACK], 640, 360)
        .expect("a size request");

    let seen = peer
        .wait_until(PATIENCE, |seen| !seen.settings.is_empty())
        .await;
    let settings = seen.settings.last().expect("the settings arrived");

    assert_eq!(settings.track_sids, vec![BOB_TRACK]);
    assert_eq!(settings.width, 640);
    assert_eq!(settings.height, 360);
    assert!(!settings.disabled);

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn a_subscribed_quality_update_reaches_the_application() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;
    let mut events = client.events();

    peer.push(signal_response::Message::SubscribedQualityUpdate(
        mediacore::SubscribedQualityUpdate {
            track_sid: BOB_TRACK.to_owned(),
            subscribed_codecs: vec![mediacore::SubscribedCodec {
                codec: "vp8".to_owned(),
                qualities: vec![
                    mediacore::SubscribedQuality {
                        quality: mediacore::VideoQuality::Low as i32,
                        enabled: true,
                    },
                    mediacore::SubscribedQuality {
                        quality: mediacore::VideoQuality::High as i32,
                        enabled: false,
                    },
                ],
            }],
            ..mediacore::SubscribedQualityUpdate::default()
        },
    ));

    let codecs = events
        .wait_for(PATIENCE, |event| match event {
            mediacore_client::ClientEvent::SubscribedQualityChanged {
                track_sid, codecs, ..
            } if track_sid == BOB_TRACK => Some(codecs.clone()),
            _ => None,
        })
        .await
        .expect("a subscribed quality update");

    assert_eq!(codecs.len(), 1);
    assert_eq!(codecs[0].codec, "vp8");
    assert!(codecs[0].qualities[0].enabled);
    assert!(!codecs[0].qualities[1].enabled);

    client.leave().await.expect("a clean leave");
}
