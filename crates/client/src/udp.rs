use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::error::ClientError;

const DATAGRAM_BUFFER: usize = 2048;
const DATAGRAM_QUEUE: usize = 256;

pub(crate) type Datagram = (Instant, SocketAddr, Bytes);

pub(crate) struct Endpoint {
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) local: SocketAddr,
    pub(crate) datagrams: mpsc::Receiver<Datagram>,
}

pub(crate) async fn bind(address: IpAddr) -> Result<Endpoint, ClientError> {
    let socket = Arc::new(UdpSocket::bind(SocketAddr::new(address, 0)).await?);
    let local = socket.local_addr()?;

    let (sender, datagrams) = mpsc::channel(DATAGRAM_QUEUE);
    let reader = socket.clone();

    tokio::spawn(async move {
        let mut buffer = vec![0_u8; DATAGRAM_BUFFER];

        loop {
            let Ok((size, source)) = reader.recv_from(&mut buffer).await else {
                break;
            };
            let at = Instant::now();
            let data = Bytes::copy_from_slice(&buffer[..size]);
            if sender.send((at, source, data)).await.is_err() {
                break;
            }
        }
    });

    Ok(Endpoint {
        socket,
        local,
        datagrams,
    })
}
