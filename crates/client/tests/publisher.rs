use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use mediacore_client::proto::{self as mediacore, signal_request, signal_response};
use mediacore_client::{Client, ClientOptions, TrackSpec};
use prost::Message as _;
use str0m::change::SdpOffer;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

const ROOM: &str = "demo";
const IDENTITY: &str = "alice";

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PeerEvent {
    Connected,
    Trickled,
    Left,
    Rtp { mid: String, payload: usize },
}

struct Peer {
    addr: SocketAddr,
    events: mpsc::UnboundedReceiver<PeerEvent>,
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
}

async fn spawn_peer() -> Peer {
    str0m::crypto::from_feature_flags().install_process_default();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, receiver) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("a signalling connection");
        let socket = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("a websocket handshake");
        run_peer(socket, events).await;
    });

    Peer {
        addr,
        events: receiver,
    }
}

async fn run_peer(socket: WebSocketStream<TcpStream>, events: mpsc::UnboundedSender<PeerEvent>) {
    let udp = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let local = udp.local_addr().unwrap();

    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap())
        .expect("a host candidate");

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

    loop {
        let deadline = loop {
            if !rtc.is_alive() {
                return;
            }

            match rtc.poll_output() {
                Ok(Output::Timeout(at)) => break at.max(Instant::now()),
                Ok(Output::Transmit(transmit)) => {
                    let _ = udp.send_to(&transmit.contents, transmit.destination).await;
                }
                Ok(Output::Event(Event::Connected)) => {
                    let _ = events.send(PeerEvent::Connected);
                }
                Ok(Output::Event(Event::RtpPacket(packet))) => {
                    if let Some(mid) = packet.header.ext_vals.mid {
                        let _ = events.send(PeerEvent::Rtp {
                            mid: mid.to_string(),
                            payload: packet.payload.len(),
                        });
                    }
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
                let Ok(request) = mediacore::SignalRequest::decode(data.as_ref()) else { continue };
                let Some(message) = request.message else { continue };
                if !on_request(&mut rtc, &mut sink, &events, message).await {
                    return;
                }
            }
            datagram = datagrams.recv() => {
                let Some((at, source, data)) = datagram else { return };
                if let Ok(receive) = Receive::new(Protocol::Udp, source, local, &data) {
                    let _ = rtc.handle_input(Input::Receive(at, receive));
                }
            }
            _ = &mut sleep => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
    }
}

async fn on_request(
    rtc: &mut Rtc,
    sink: &mut Sink,
    events: &mpsc::UnboundedSender<PeerEvent>,
    message: signal_request::Message,
) -> bool {
    match message {
        signal_request::Message::AddTrack(add) => {
            let published =
                signal_response::Message::TrackPublished(mediacore::TrackPublishedResponse {
                    cid: add.cid.clone(),
                    track: Some(mediacore::TrackInfo {
                        sid: format!("TR_{}", add.cid),
                        r#type: add.r#type,
                        name: add.name,
                        source: add.source,
                        width: add.width,
                        height: add.height,
                        mime_type: "audio/opus".to_owned(),
                        ..mediacore::TrackInfo::default()
                    }),
                });
            send(sink, published).await;
        }
        signal_request::Message::Offer(offer) => {
            let offer_id = offer.id;
            let offer = SdpOffer::from_sdp_string(&offer.sdp).expect("a parseable offer");
            let answer = rtc.sdp_api().accept_offer(offer).expect("an answer");
            let answer = signal_response::Message::Answer(mediacore::SessionDescription {
                r#type: "answer".to_owned(),
                sdp: answer.to_sdp_string(),
                id: offer_id,
                ..mediacore::SessionDescription::default()
            });
            send(sink, answer).await;
        }
        signal_request::Message::Trickle(trickle) => {
            if !trickle.candidate_init.is_empty() {
                let _ = events.send(PeerEvent::Trickled);
            }
        }
        signal_request::Message::PingReq(ping) => {
            let pong = signal_response::Message::PongResp(mediacore::Pong {
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
            sid: "PA_other".to_owned(),
            identity: "bob".to_owned(),
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
        response_timeout: Duration::from_secs(10),
        ..ClientOptions::default()
    }
}

#[tokio::test]
async fn the_join_response_is_available_after_connecting() {
    let peer = spawn_peer().await;
    let client = Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client");

    assert_eq!(client.join().room.as_ref().unwrap().name, ROOM);
    assert_eq!(client.identity(), IDENTITY);
    assert_eq!(client.sid(), "PA_test");

    let participants = client.participants();
    assert_eq!(participants.len(), 1);
    assert_eq!(participants[0].identity, "bob");
}

#[tokio::test]
async fn publishing_negotiates_a_publisher_transport() {
    let mut peer = spawn_peer().await;
    let client = Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client");

    let track = client
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("a published track");

    assert_eq!(track.sid(), format!("TR_{}", track.cid()));
    assert_eq!(track.info().unwrap().name, "mic");
    assert!(!track.mid().is_empty());

    peer.wait_for(Duration::from_secs(5), |event| {
        matches!(event, PeerEvent::Trickled)
    })
    .await;

    client
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");

    peer.wait_for(Duration::from_secs(5), |event| {
        matches!(event, PeerEvent::Connected)
    })
    .await;

    client.leave().await.expect("a clean leave");

    peer.wait_for(Duration::from_secs(5), |event| {
        matches!(event, PeerEvent::Left)
    })
    .await;
}

#[tokio::test]
async fn a_published_audio_track_sends_rtp_the_peer_accepts() {
    let mut peer = spawn_peer().await;
    let client = Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client");

    let track = client
        .publish(TrackSpec::audio("mic"))
        .await
        .expect("a published track");

    client
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");

    track.start().await.expect("the track starts sending");

    let mid = track.mid();
    let received = peer
        .wait_for(
            Duration::from_secs(10),
            |event| matches!(event, PeerEvent::Rtp { mid: seen, .. } if *seen == mid),
        )
        .await;

    match received {
        PeerEvent::Rtp { payload, .. } => assert_eq!(payload, 80),
        other => panic!("expected an rtp packet, got {other:?}"),
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let stats = track.stats().await.expect("track statistics");
    assert!(stats.sending);
    assert!(stats.packets >= 5, "only sent {} packets", stats.packets);
    assert_eq!(stats.payload_bytes, stats.packets * 80);

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn a_published_video_track_sends_rtp_the_peer_accepts() {
    let mut peer = spawn_peer().await;
    let client = Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client");

    let track = client
        .publish(TrackSpec::video("cam"))
        .await
        .expect("a published track");

    client
        .wait_connected(Duration::from_secs(10))
        .await
        .expect("the publisher transport connects");

    track.start().await.expect("the track starts sending");

    let mid = track.mid();
    let received = peer
        .wait_for(
            Duration::from_secs(10),
            |event| matches!(event, PeerEvent::Rtp { mid: seen, .. } if *seen == mid),
        )
        .await;

    match received {
        PeerEvent::Rtp { payload, .. } => assert_eq!(payload, 900),
        other => panic!("expected an rtp packet, got {other:?}"),
    }

    client.leave().await.expect("a clean leave");
}

#[tokio::test]
async fn two_clients_in_one_test_do_not_share_state() {
    let first_peer = spawn_peer().await;
    let second_peer = spawn_peer().await;

    let first = Client::connect_with(&format!("ws://{}", first_peer.addr), "token", options())
        .await
        .expect("the first client");
    let second = Client::connect_with(&format!("ws://{}", second_peer.addr), "token", options())
        .await
        .expect("the second client");

    assert_ne!(first.local_addr(), second.local_addr());

    let one = first.publish(TrackSpec::audio("mic")).await.unwrap();
    let two = second.publish(TrackSpec::video("cam")).await.unwrap();

    assert_ne!(one.cid(), two.cid());
    assert!(!one.mid().is_empty());
    assert!(!two.mid().is_empty());
}

#[tokio::test]
async fn two_tracks_on_one_client_are_negotiated_in_turn() {
    let peer = spawn_peer().await;
    let client = Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client");

    let audio = client.publish(TrackSpec::audio("mic")).await.unwrap();
    let video = client.publish(TrackSpec::video("cam")).await.unwrap();

    assert_ne!(audio.mid(), video.mid());
    assert_eq!(audio.spec().kind, mediacore_client::TrackKind::Audio);
    assert_eq!(video.spec().kind, mediacore_client::TrackKind::Video);
}

#[tokio::test]
async fn a_ping_is_answered_with_a_pong() {
    let peer = spawn_peer().await;
    let client = Client::connect_with(&format!("ws://{}", peer.addr), "token", options())
        .await
        .expect("a connected client");

    let mut responses = client.responses();
    client
        .send(signal_request::Message::PingReq(mediacore::Ping {
            timestamp: 1,
            rtt: 0,
        }))
        .expect("a ping goes out");

    let pong = responses
        .wait_for(Duration::from_secs(5), |message| match message {
            signal_response::Message::PongResp(pong) => Some(*pong),
            _ => None,
        })
        .await
        .expect("a pong");

    assert_eq!(pong.last_ping_timestamp, 1);
}

#[tokio::test]
async fn an_unreachable_server_fails_to_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let error = Client::connect_with(&format!("ws://{addr}"), "token", options())
        .await
        .expect_err("the connection cannot succeed");

    assert!(
        matches!(error, mediacore_client::ClientError::Socket(_)),
        "{error}"
    );
}
