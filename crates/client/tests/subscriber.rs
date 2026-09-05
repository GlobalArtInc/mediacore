use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use mediacore_client::proto::{self as livekit, signal_request, signal_response};
use mediacore_client::{
    Client, ClientEvent, ClientOptions, SubscriptionRequest, TrackKind, TrackSettings,
};
use prost::Message as _;
use str0m::change::{SdpAnswer, SdpPendingOffer};
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
const BOB_TRACK: &str = "TR_bobmic";
const PAYLOAD_SIZE: usize = 80;

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PeerEvent {
    Answered { mid_count: usize },
    Connected,
    Trickled,
    Subscription { subscribe: bool, sids: Vec<String> },
    Settings { sids: Vec<String>, disabled: bool },
    Left,
}

struct Peer {
    addr: SocketAddr,
    events: mpsc::UnboundedReceiver<PeerEvent>,
    control: mpsc::UnboundedSender<signal_response::Message>,
}

impl Peer {
    async fn wait_for(
        &mut self,
        timeout: Duration,
        wanted: impl Fn(&PeerEvent) -> bool,
    ) -> PeerEvent {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(left, self.events.recv())
                .await
                .expect("the peer went quiet")
                .expect("the peer task stopped");

            if wanted(&event) {
                return event;
            }
        }
    }

    fn push(&self, message: signal_response::Message) {
        self.control.send(message).expect("the peer is listening");
    }
}

async fn spawn_peer() -> Peer {
    str0m::crypto::from_feature_flags().install_process_default();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, receiver) = mpsc::unbounded_channel();
    let (control, pushes) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("a signalling connection");
        let socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("a websocket handshake");
        run_peer(socket, events, pushes).await;
    });

    Peer {
        addr,
        events: receiver,
        control,
    }
}

struct PeerState {
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    mid: Option<Mid>,
    pt: Option<Pt>,
    seq: u64,
    ticks: u32,
    connected: bool,
}

async fn run_peer(
    socket: WebSocketStream<TcpStream>,
    events: mpsc::UnboundedSender<PeerEvent>,
    mut pushes: mpsc::UnboundedReceiver<signal_response::Message>,
) {
    let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let local = udp.local_addr().unwrap();

    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_opus(true)
        .enable_vp8(true)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap())
        .expect("a host candidate");

    let mut state = PeerState {
        rtc,
        pending: None,
        mid: None,
        pt: None,
        seq: 0,
        ticks: 0,
        connected: false,
    };

    let (mut sink, mut stream) = socket.split();
    send(&mut sink, signal_response::Message::Join(join_response())).await;

    let (datagram_tx, mut datagrams) = mpsc::channel::<(Instant, SocketAddr, Bytes)>(256);
    let reader = udp.clone();

    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 2048];
        while let Ok((size, source)) = reader.recv_from(&mut buffer).await {
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

    let mut media = tokio::time::interval(Duration::from_millis(10));

    loop {
        let deadline = loop {
            if !state.rtc.is_alive() {
                return;
            }

            match state.rtc.poll_output() {
                Ok(Output::Timeout(at)) => break at.max(Instant::now()),
                Ok(Output::Transmit(transmit)) => {
                    let _ = udp.send_to(&transmit.contents, transmit.destination).await;
                }
                Ok(Output::Event(Event::Connected)) => {
                    state.connected = true;
                    let _ = events.send(PeerEvent::Connected);
                }
                Ok(Output::Event(_)) => {}
                Err(error) => panic!("peer transport failed: {error}"),
            }
        };

        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);

        tokio::select! {
            frame = stream.next() => {
                let Some(Ok(Message::Binary(data))) = frame else { return };
                let Ok(request) = livekit::SignalRequest::decode(data.as_ref()) else { continue };
                let Some(message) = request.message else { continue };
                if !on_request(&mut state, &mut sink, &events, message).await {
                    return;
                }
            }
            push = pushes.recv() => {
                let Some(message) = push else { return };
                send(&mut sink, message).await;
            }
            datagram = datagrams.recv() => {
                let Some((at, source, data)) = datagram else { return };
                if let Ok(receive) = Receive::new(Protocol::Udp, source, local, &data) {
                    let _ = state.rtc.handle_input(Input::Receive(at, receive));
                }
            }
            _ = media.tick() => {
                write_media(&mut state);
            }
            _ = &mut sleep => {
                let _ = state.rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
    }
}

fn write_media(state: &mut PeerState) {
    if !state.connected {
        return;
    }

    let (Some(mid), Some(pt)) = (state.mid, state.pt) else {
        return;
    };

    let seq: SeqNo = state.seq.into();
    let time = state.ticks;
    let payload = payload(state.seq);

    state.seq += 1;
    state.ticks = state.ticks.wrapping_add(960);

    let mut direct = state.rtc.direct_api();
    let Some(stream) = direct.stream_tx_by_mid(mid, None) else {
        return;
    };

    stream.write_rtp(RtpWrite::new(pt, seq, time, Instant::now(), payload).marker(true));
}

fn payload(index: u64) -> Vec<u8> {
    let mut payload = vec![0x78_u8; PAYLOAD_SIZE];
    payload[1] = (index % 251) as u8;
    payload
}

async fn on_request(
    state: &mut PeerState,
    sink: &mut Sink,
    events: &mpsc::UnboundedSender<PeerEvent>,
    message: signal_request::Message,
) -> bool {
    match message {
        signal_request::Message::Subscription(update) => {
            let _ = events.send(PeerEvent::Subscription {
                subscribe: update.subscribe,
                sids: update.track_sids.clone(),
            });

            if update.subscribe && state.mid.is_none() && state.pending.is_none() {
                offer_track(state, sink).await;
            }

            for sid in update.track_sids {
                let response =
                    signal_response::Message::SubscriptionResponse(livekit::SubscriptionResponse {
                        track_sid: sid,
                        err: livekit::SubscriptionError::SeUnknown as i32,
                    });
                send(sink, response).await;
            }
        }
        signal_request::Message::TrackSetting(settings) => {
            let _ = events.send(PeerEvent::Settings {
                sids: settings.track_sids,
                disabled: settings.disabled,
            });
        }
        signal_request::Message::Answer(answer) => {
            let mid_count = answer.sdp.matches("a=mid:").count();
            let parsed = SdpAnswer::from_sdp_string(&answer.sdp).expect("a parseable answer");
            let pending = state.pending.take().expect("an outstanding offer");
            state
                .rtc
                .sdp_api()
                .accept_answer(pending, parsed)
                .expect("the answer applies");

            let mid = state.mid.expect("a negotiated mid");
            state.pt = Some(opus_pt(&state.rtc, mid));

            let _ = events.send(PeerEvent::Answered { mid_count });
        }
        signal_request::Message::Trickle(trickle) => {
            if trickle.candidate_init.is_empty() {
                return true;
            }

            let _ = events.send(PeerEvent::Trickled);

            #[derive(serde::Deserialize)]
            struct CandidateInit {
                candidate: String,
            }

            if let Ok(init) = serde_json::from_str::<CandidateInit>(&trickle.candidate_init)
                && let Ok(candidate) = Candidate::from_sdp_string(&init.candidate)
            {
                state.rtc.add_remote_candidate(candidate);
            }
        }
        signal_request::Message::PingReq(ping) => {
            let pong = signal_response::Message::PongResp(livekit::Pong {
                last_ping_timestamp: ping.timestamp,
                timestamp: 0,
            });
            send(sink, pong).await;
        }
        signal_request::Message::Leave(_) => {
            let _ = events.send(PeerEvent::Left);
            return false;
        }
        _ => {}
    }

    true
}

async fn offer_track(state: &mut PeerState, sink: &mut Sink) {
    let mut api = state.rtc.sdp_api();
    let mid = api.add_media(
        MediaKind::Audio,
        Direction::SendOnly,
        Some("ST_bob".to_owned()),
        Some(BOB_TRACK.to_owned()),
        None,
    );
    let (offer, pending) = api.apply().expect("an offer");

    state.mid = Some(mid);
    state.pending = Some(pending);

    let offer = signal_response::Message::Offer(livekit::SessionDescription {
        r#type: "offer".to_owned(),
        sdp: offer.to_sdp_string(),
        id: 7,
        mid_to_track_id: [(mid.to_string(), BOB_TRACK.to_owned())]
            .into_iter()
            .collect(),
    });
    send(sink, offer).await;
}

fn opus_pt(rtc: &Rtc, mid: Mid) -> Pt {
    let media = rtc.media(mid).expect("a negotiated media");

    rtc.codec_config()
        .params()
        .iter()
        .find(|params| {
            params.spec().codec == Codec::Opus && media.remote_pts().contains(&params.pt())
        })
        .map(|params| params.pt())
        .expect("an opus payload type")
}

async fn send(sink: &mut Sink, message: signal_response::Message) {
    let response = livekit::SignalResponse {
        message: Some(message),
    };
    let _ = sink
        .send(Message::Binary(Bytes::from(response.encode_to_vec())))
        .await;
}

fn bob(tracks: Vec<livekit::TrackInfo>) -> livekit::ParticipantInfo {
    livekit::ParticipantInfo {
        sid: BOB_SID.to_owned(),
        identity: "bob".to_owned(),
        tracks,
        ..livekit::ParticipantInfo::default()
    }
}

fn bob_track() -> livekit::TrackInfo {
    livekit::TrackInfo {
        sid: BOB_TRACK.to_owned(),
        name: "mic".to_owned(),
        r#type: livekit::TrackType::Audio as i32,
        source: livekit::TrackSource::Microphone as i32,
        mime_type: "audio/opus".to_owned(),
        ..livekit::TrackInfo::default()
    }
}

fn join_response() -> livekit::JoinResponse {
    livekit::JoinResponse {
        room: Some(livekit::Room {
            sid: "RM_test".to_owned(),
            name: ROOM.to_owned(),
            ..livekit::Room::default()
        }),
        participant: Some(livekit::ParticipantInfo {
            sid: "PA_alice".to_owned(),
            identity: IDENTITY.to_owned(),
            ..livekit::ParticipantInfo::default()
        }),
        other_participants: vec![bob(vec![bob_track()])],
        subscriber_primary: true,
        ping_interval: 15,
        ping_timeout: 60,
        ..livekit::JoinResponse::default()
    }
}

fn options() -> ClientOptions {
    ClientOptions {
        response_timeout: Duration::from_secs(10),
        ..ClientOptions::default()
    }
}

async fn connect(peer: &Peer) -> Client {
    Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client")
}

#[tokio::test]
async fn the_tracks_announced_at_join_are_listed() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let tracks = client.remote_tracks();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].sid(), BOB_TRACK);
    assert_eq!(tracks[0].participant_sid, BOB_SID);
    assert_eq!(tracks[0].participant_identity, "bob");
    assert_eq!(tracks[0].track_type(), livekit::TrackType::Audio);
}

#[tokio::test]
async fn the_subscriber_and_publisher_bind_different_ports() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    assert_ne!(client.local_addr(), client.subscriber_addr());
}

#[tokio::test]
async fn a_subscription_request_reaches_the_server() {
    let mut peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscribe request");

    let event = peer
        .wait_for(Duration::from_secs(5), |event| {
            matches!(event, PeerEvent::Subscription { .. })
        })
        .await;

    assert_eq!(
        event,
        PeerEvent::Subscription {
            subscribe: true,
            sids: vec![BOB_TRACK.to_owned()],
        }
    );
}

#[tokio::test]
async fn unsubscribing_reaches_the_server_too() {
    let mut peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscribe request");
    peer.wait_for(Duration::from_secs(5), |event| {
        matches!(
            event,
            PeerEvent::Subscription {
                subscribe: true,
                ..
            }
        )
    })
    .await;

    client
        .unsubscribe([BOB_TRACK])
        .expect("an unsubscribe request");

    let event = peer
        .wait_for(Duration::from_secs(5), |event| {
            matches!(
                event,
                PeerEvent::Subscription {
                    subscribe: false,
                    ..
                }
            )
        })
        .await;

    assert_eq!(
        event,
        PeerEvent::Subscription {
            subscribe: false,
            sids: vec![BOB_TRACK.to_owned()],
        }
    );
}

#[tokio::test]
async fn a_subscription_scoped_to_one_participant_carries_both_lists() {
    let mut peer = spawn_peer().await;
    let client = connect(&peer).await;

    let request =
        SubscriptionRequest::subscribe(Vec::<String>::new()).from_participant(BOB_SID, [BOB_TRACK]);
    client
        .update_subscription(&request)
        .expect("a subscribe request");

    let event = peer
        .wait_for(Duration::from_secs(5), |event| {
            matches!(event, PeerEvent::Subscription { .. })
        })
        .await;

    assert_eq!(
        event,
        PeerEvent::Subscription {
            subscribe: true,
            sids: vec![BOB_TRACK.to_owned()],
        }
    );
}

#[tokio::test]
async fn track_settings_reach_the_server() {
    let mut peer = spawn_peer().await;
    let client = connect(&peer).await;

    client
        .update_track_settings(&TrackSettings::for_tracks([BOB_TRACK]).disabled(true))
        .expect("a settings request");

    let event = peer
        .wait_for(Duration::from_secs(5), |event| {
            matches!(event, PeerEvent::Settings { .. })
        })
        .await;

    assert_eq!(
        event,
        PeerEvent::Settings {
            sids: vec![BOB_TRACK.to_owned()],
            disabled: true,
        }
    );
}

#[tokio::test]
async fn a_server_offer_is_answered_and_the_subscriber_connects() {
    let mut peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscribe request");

    let answered = peer
        .wait_for(Duration::from_secs(10), |event| {
            matches!(event, PeerEvent::Answered { .. })
        })
        .await;
    assert_eq!(answered, PeerEvent::Answered { mid_count: 1 });

    peer.wait_for(Duration::from_secs(5), |event| {
        matches!(event, PeerEvent::Trickled)
    })
    .await;

    client
        .wait_subscribed(Duration::from_secs(10))
        .await
        .expect("the subscriber transport connects");

    peer.wait_for(Duration::from_secs(10), |event| {
        matches!(event, PeerEvent::Connected)
    })
    .await;

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn subscribed_media_is_accounted_per_track() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    client.subscribe([BOB_TRACK]).expect("a subscribe request");
    client
        .wait_subscribed(Duration::from_secs(10))
        .await
        .expect("the subscriber transport connects");

    let report = client
        .wait_for_packets(BOB_TRACK, 10, Duration::from_secs(10))
        .await
        .expect("media on the subscribed track");

    assert_eq!(report.track_sid, BOB_TRACK);
    assert_eq!(report.kind, Some(TrackKind::Audio));
    assert!(report.packets >= 10, "only {} packets", report.packets);
    assert_eq!(report.payload_bytes, report.packets * PAYLOAD_SIZE as u64);
    assert_eq!(report.payload_types.len(), 1);
    assert_eq!(report.duplicates, 0);
    assert_eq!(report.ssrcs.len(), 1);

    let sequence = report.sequence.expect("a sequence span");
    assert_eq!(sequence.lowest, 0);
    assert!(sequence.highest >= 9);

    let timestamp = report.timestamp.expect("a timestamp span");
    assert_eq!(timestamp.first, 0);
    assert!(timestamp.highest >= 9 * 960);

    for packet in &report.window {
        assert_eq!(packet.payload.len(), PAYLOAD_SIZE);
        assert_eq!(packet.payload[0], 0x78);
        assert_eq!(packet.payload[1], (packet.sequence % 251) as u8);
        assert!(packet.marker);
    }

    let all = client.received().await.expect("the received tracks");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].track_sid, BOB_TRACK);

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn nothing_is_reported_for_a_track_we_never_subscribed_to() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    assert!(client.received().await.expect("a report").is_empty());
    assert!(
        client
            .received_track(BOB_TRACK)
            .await
            .expect("a report")
            .is_none()
    );
}

#[tokio::test]
async fn subscription_responses_are_surfaced() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    peer.push(signal_response::Message::SubscriptionResponse(
        livekit::SubscriptionResponse {
            track_sid: BOB_TRACK.to_owned(),
            err: livekit::SubscriptionError::SeTrackNotfound as i32,
        },
    ));

    let error = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::SubscriptionResponse { track_sid, error } if track_sid == BOB_TRACK => {
                Some(*error)
            }
            _ => None,
        })
        .await
        .expect("a subscription response");

    assert_eq!(error, Some(livekit::SubscriptionError::SeTrackNotfound));
}

#[tokio::test]
async fn a_successful_subscription_response_carries_no_error() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    client.subscribe([BOB_TRACK]).expect("a subscribe request");

    let error = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::SubscriptionResponse { track_sid, error } if track_sid == BOB_TRACK => {
                Some(*error)
            }
            _ => None,
        })
        .await
        .expect("a subscription response");

    assert_eq!(error, None);
}

#[tokio::test]
async fn a_track_published_after_join_is_surfaced() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    let second = livekit::TrackInfo {
        sid: "TR_bobcam".to_owned(),
        name: "cam".to_owned(),
        r#type: livekit::TrackType::Video as i32,
        ..livekit::TrackInfo::default()
    };

    peer.push(signal_response::Message::Update(
        livekit::ParticipantUpdate {
            participants: vec![bob(vec![bob_track(), second])],
        },
    ));

    let published = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::TrackPublished(track) => Some(track.clone()),
            _ => None,
        })
        .await
        .expect("a published track");

    assert_eq!(published.sid(), "TR_bobcam");
    assert_eq!(published.participant_identity, "bob");
    assert_eq!(client.remote_tracks().len(), 2);
}

#[tokio::test]
async fn a_track_that_disappears_from_an_update_is_unpublished() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    peer.push(signal_response::Message::Update(
        livekit::ParticipantUpdate {
            participants: vec![bob(Vec::new())],
        },
    ));

    let unpublished = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::TrackUnpublished { track_sid, .. } => Some(track_sid.clone()),
            _ => None,
        })
        .await
        .expect("an unpublished track");

    assert_eq!(unpublished, BOB_TRACK);
    assert!(client.remote_tracks().is_empty());
}

#[tokio::test]
async fn a_participant_that_leaves_takes_its_tracks_with_it() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    peer.push(signal_response::Message::Update(
        livekit::ParticipantUpdate {
            participants: vec![livekit::ParticipantInfo {
                state: livekit::participant_info::State::Disconnected as i32,
                ..bob(vec![bob_track()])
            }],
        },
    ));

    let left = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::ParticipantLeft {
                participant_sid, ..
            } => Some(participant_sid.clone()),
            _ => None,
        })
        .await
        .expect("a departure");

    assert_eq!(left, BOB_SID);
    assert!(client.remote_tracks().is_empty());
}

#[tokio::test]
async fn stream_state_and_permission_updates_are_surfaced() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    peer.push(signal_response::Message::StreamStateUpdate(
        livekit::StreamStateUpdate {
            stream_states: vec![livekit::StreamStateInfo {
                participant_sid: BOB_SID.to_owned(),
                track_sid: BOB_TRACK.to_owned(),
                state: livekit::StreamState::Paused as i32,
            }],
        },
    ));

    let states = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::StreamStateChanged(states) => Some(states.clone()),
            _ => None,
        })
        .await
        .expect("a stream state update");

    assert_eq!(states.len(), 1);
    assert_eq!(states[0].track_sid, BOB_TRACK);
    assert_eq!(states[0].state, livekit::StreamState::Paused as i32);

    peer.push(signal_response::Message::SubscriptionPermissionUpdate(
        livekit::SubscriptionPermissionUpdate {
            participant_sid: BOB_SID.to_owned(),
            track_sid: BOB_TRACK.to_owned(),
            allowed: false,
        },
    ));

    let allowed = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::SubscriptionPermissionChanged { allowed, .. } => Some(*allowed),
            _ => None,
        })
        .await
        .expect("a permission update");

    assert!(!allowed);
}

#[tokio::test]
async fn a_server_side_unpublish_of_our_own_track_is_surfaced() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    peer.push(signal_response::Message::TrackUnpublished(
        livekit::TrackUnpublishedResponse {
            track_sid: "TR_mine".to_owned(),
        },
    ));

    let track_sid = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::LocalTrackUnpublished { track_sid } => Some(track_sid.clone()),
            _ => None,
        })
        .await
        .expect("a local unpublish");

    assert_eq!(track_sid, "TR_mine");
}

#[tokio::test]
async fn the_first_subscriber_of_our_track_is_surfaced() {
    let peer = spawn_peer().await;
    let client = connect(&peer).await;

    let mut events = client.events();
    peer.push(signal_response::Message::TrackSubscribed(
        livekit::TrackSubscribed {
            track_sid: "TR_mine".to_owned(),
        },
    ));

    let track_sid = events
        .wait_for(Duration::from_secs(5), |event| match event {
            ClientEvent::TrackSubscribed { track_sid } => Some(track_sid.clone()),
            _ => None,
        })
        .await
        .expect("a track subscribed notice");

    assert_eq!(track_sid, "TR_mine");
}
