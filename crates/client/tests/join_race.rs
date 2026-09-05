use std::time::Duration;

use bytes::Bytes;
use futures_util::SinkExt;
use mediacore_client::proto::{self as livekit, signal_response};
use mediacore_client::{Client, ClientOptions};
use prost::Message as _;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const ATTEMPTS: usize = 100;

fn join_frame() -> Message {
    let join = livekit::JoinResponse {
        room: Some(livekit::Room {
            sid: "RM_race".to_owned(),
            name: "demo".to_owned(),
            ..Default::default()
        }),
        participant: Some(livekit::ParticipantInfo {
            sid: "PA_race".to_owned(),
            identity: "alice".to_owned(),
            ..Default::default()
        }),
        subscriber_primary: true,
        ping_timeout: 60,
        ping_interval: 15,
        server_info: Some(livekit::ServerInfo {
            protocol: 17,
            ..Default::default()
        }),
        ..Default::default()
    };

    let response = livekit::SignalResponse {
        message: Some(signal_response::Message::Join(join)),
    };

    Message::Binary(Bytes::from(response.encode_to_vec()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_join_response_is_never_missed_by_a_late_subscriber() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };

            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };

                if socket.send(join_frame()).await.is_err() {
                    return;
                }

                while socket.next_frame().await {}
            });
        }
    });

    let mut load = Vec::new();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    for _ in 0..64 {
        let stop = stop.clone();
        load.push(tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
        }));
    }

    let options = ClientOptions {
        connect_timeout: Duration::from_secs(5),
        response_timeout: Duration::from_millis(500),
        ..ClientOptions::default()
    };

    for attempt in 0..ATTEMPTS {
        let client = Client::connect_with(&format!("ws://{addr}"), "token", options.clone())
            .await
            .unwrap_or_else(|error| {
                panic!("attempt {attempt} lost the join response to the reader task: {error}")
            });

        assert_eq!(client.join().participant.as_ref().unwrap().identity, "alice");
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for task in load {
        let _ = task.await;
    }
}

trait KeepOpen {
    async fn next_frame(&mut self) -> bool;
}

impl<S> KeepOpen for tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    async fn next_frame(&mut self) -> bool {
        use futures_util::StreamExt;
        self.next().await.is_some()
    }
}
