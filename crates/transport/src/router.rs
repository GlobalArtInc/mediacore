use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use bytes::Bytes;
use str0m::IceCreds;
use str0m::crypto::CryptoProvider;
use str0m::ice::StunMessage;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::TransportError;

pub const DATAGRAM_QUEUE_CAPACITY: usize = 512;

const MAX_DATAGRAM: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "s{}", self.0)
    }
}

#[derive(Debug)]
pub struct Datagram {
    pub at: Instant,
    pub source: SocketAddr,
    pub data: Bytes,
}

pub type DatagramRx = mpsc::Receiver<Datagram>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Routed(SessionId),
    StunMalformed,
    StunWithoutUsername,
    StunUnauthenticated,
    UnknownUfrag,
    UnknownSource,
    QueueFull(SessionId),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RouterStats {
    pub received: u64,
    pub routed: u64,
    pub stun_malformed: u64,
    pub stun_without_username: u64,
    pub stun_unauthenticated: u64,
    pub unknown_ufrag: u64,
    pub unknown_source: u64,
    pub queue_full: u64,
}

#[derive(Debug, Default)]
struct Counters {
    received: AtomicU64,
    routed: AtomicU64,
    stun_malformed: AtomicU64,
    stun_without_username: AtomicU64,
    stun_unauthenticated: AtomicU64,
    unknown_ufrag: AtomicU64,
    unknown_source: AtomicU64,
    queue_full: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> RouterStats {
        RouterStats {
            received: self.received.load(Ordering::Relaxed),
            routed: self.routed.load(Ordering::Relaxed),
            stun_malformed: self.stun_malformed.load(Ordering::Relaxed),
            stun_without_username: self.stun_without_username.load(Ordering::Relaxed),
            stun_unauthenticated: self.stun_unauthenticated.load(Ordering::Relaxed),
            unknown_ufrag: self.unknown_ufrag.load(Ordering::Relaxed),
            unknown_source: self.unknown_source.load(Ordering::Relaxed),
            queue_full: self.queue_full.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
struct Entry {
    id: SessionId,
    tx: mpsc::Sender<Datagram>,
}

#[derive(Clone)]
struct IceEntry {
    entry: Entry,
    pass: Arc<str>,
}

#[derive(Default)]
struct Tables {
    by_ufrag: RwLock<HashMap<String, IceEntry>>,
    by_addr: RwLock<HashMap<SocketAddr, Entry>>,
}

pub struct Router {
    next_id: AtomicU64,
    tables: Tables,
    counters: Counters,
    crypto: CryptoProvider,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::default(),
            tables: Tables::default(),
            counters: Counters::default(),
            crypto: crypto_provider(),
        }
    }
}

impl Router {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(
        self: &Arc<Self>,
        creds: &IceCreds,
        queue_capacity: usize,
    ) -> Result<SessionRoute, TransportError> {
        let id = SessionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel(queue_capacity);

        {
            let mut by_ufrag = self.write_by_ufrag();
            if by_ufrag.contains_key(&creds.ufrag) {
                return Err(TransportError::DuplicateUfrag(creds.ufrag.clone()));
            }
            by_ufrag.insert(
                creds.ufrag.clone(),
                IceEntry {
                    entry: Entry { id, tx },
                    pass: Arc::from(creds.pass.as_str()),
                },
            );
        }

        tracing::debug!(%id, ufrag = %creds.ufrag, "registered ice session");

        Ok(SessionRoute {
            id,
            ufrag: creds.ufrag.clone(),
            router: Arc::clone(self),
            rx: Some(rx),
        })
    }

    fn rekey(&self, id: SessionId, previous_ufrag: &str, creds: &IceCreds) -> bool {
        let mut by_ufrag = self.write_by_ufrag();

        let Some(mut moved) = by_ufrag
            .get(previous_ufrag)
            .filter(|current| current.entry.id == id)
            .cloned()
        else {
            tracing::warn!(%id, previous_ufrag, "rekeying a session the router does not hold");
            return false;
        };

        if by_ufrag
            .get(&creds.ufrag)
            .is_some_and(|taken| taken.entry.id != id)
        {
            tracing::error!(
                %id,
                ufrag = %creds.ufrag,
                "refusing to rekey onto a ufrag another session holds"
            );
            return false;
        }

        by_ufrag.remove(previous_ufrag);
        moved.pass = Arc::from(creds.pass.as_str());
        by_ufrag.insert(creds.ufrag.clone(), moved);

        tracing::debug!(
            %id,
            previous_ufrag,
            ufrag = %creds.ufrag,
            "ice session rekeyed"
        );

        true
    }

    pub fn stats(&self) -> RouterStats {
        self.counters.snapshot()
    }

    pub fn session_count(&self) -> usize {
        self.read_by_ufrag().len()
    }

    pub fn address_count(&self) -> usize {
        self.read_by_addr().len()
    }

    pub fn session_for_address(&self, addr: &SocketAddr) -> Option<SessionId> {
        self.read_by_addr().get(addr).map(|entry| entry.id)
    }

    pub fn session_for_ufrag(&self, ufrag: &str) -> Option<SessionId> {
        self.read_by_ufrag().get(ufrag).map(|ice| ice.entry.id)
    }

    pub fn dispatch(&self, at: Instant, source: SocketAddr, buf: &[u8]) -> Dispatch {
        self.counters.received.fetch_add(1, Ordering::Relaxed);

        let entry = if looks_like_stun(buf) {
            match self.route_stun(source, buf) {
                Ok(entry) => entry,
                Err(dispatch) => return dispatch,
            }
        } else {
            match self.read_by_addr().get(&source).cloned() {
                Some(entry) => entry,
                None => {
                    self.counters.unknown_source.fetch_add(1, Ordering::Relaxed);
                    return Dispatch::UnknownSource;
                }
            }
        };

        let datagram = Datagram {
            at,
            source,
            data: Bytes::copy_from_slice(buf),
        };

        match entry.tx.try_send(datagram) {
            Ok(()) => {
                self.counters.routed.fetch_add(1, Ordering::Relaxed);
                Dispatch::Routed(entry.id)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counters.queue_full.fetch_add(1, Ordering::Relaxed);
                Dispatch::QueueFull(entry.id)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.counters.unknown_source.fetch_add(1, Ordering::Relaxed);
                Dispatch::UnknownSource
            }
        }
    }

    fn route_stun(&self, source: SocketAddr, buf: &[u8]) -> Result<Entry, Dispatch> {
        let message = match StunMessage::parse(buf) {
            Ok(message) => message,
            Err(err) => {
                tracing::trace!(%source, ?err, "malformed stun datagram");
                self.counters.stun_malformed.fetch_add(1, Ordering::Relaxed);
                return Err(Dispatch::StunMalformed);
            }
        };

        let Some((local_ufrag, _remote_ufrag)) = message.split_username() else {
            if let Some(entry) = self.read_by_addr().get(&source).cloned() {
                return Ok(entry);
            }

            tracing::trace!(%source, "stun datagram without a usable username");
            self.counters
                .stun_without_username
                .fetch_add(1, Ordering::Relaxed);
            return Err(Dispatch::StunWithoutUsername);
        };

        let Some(ice) = self.read_by_ufrag().get(local_ufrag).cloned() else {
            tracing::trace!(%source, local_ufrag, "stun datagram for an unknown ufrag");
            self.counters.unknown_ufrag.fetch_add(1, Ordering::Relaxed);
            return Err(Dispatch::UnknownUfrag);
        };

        if !message.verify(ice.pass.as_bytes(), |key, payloads| {
            self.crypto.sha1_hmac_provider.sha1_hmac(key, payloads)
        }) {
            tracing::debug!(
                %source,
                local_ufrag,
                id = %ice.entry.id,
                "stun datagram failed the message integrity check"
            );
            self.counters
                .stun_unauthenticated
                .fetch_add(1, Ordering::Relaxed);
            return Err(Dispatch::StunUnauthenticated);
        }

        self.learn_address(source, &ice.entry);

        Ok(ice.entry)
    }

    fn learn_address(&self, source: SocketAddr, entry: &Entry) {
        if self.read_by_addr().get(&source).map(|e| e.id) == Some(entry.id) {
            return;
        }

        let previous = self.write_by_addr().insert(source, entry.clone());

        match previous {
            Some(previous) => tracing::debug!(
                %source,
                from = %previous.id,
                to = %entry.id,
                "ice source address taken over"
            ),
            None => tracing::debug!(%source, id = %entry.id, "learned ice source address"),
        }
    }

    fn unregister(&self, id: SessionId, ufrag: &str) {
        {
            let mut by_ufrag = self.write_by_ufrag();
            if by_ufrag.get(ufrag).map(|ice| ice.entry.id) == Some(id) {
                by_ufrag.remove(ufrag);
            }
        }

        self.write_by_addr().retain(|_, entry| entry.id != id);

        tracing::debug!(%id, ufrag, "deregistered ice session");
    }

    fn read_by_ufrag(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, IceEntry>> {
        self.tables
            .by_ufrag
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn write_by_ufrag(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, IceEntry>> {
        self.tables
            .by_ufrag
            .write()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn read_by_addr(&self) -> std::sync::RwLockReadGuard<'_, HashMap<SocketAddr, Entry>> {
        self.tables
            .by_addr
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn write_by_addr(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<SocketAddr, Entry>> {
        self.tables
            .by_addr
            .write()
            .unwrap_or_else(|e| e.into_inner())
    }
}

pub struct SessionRoute {
    id: SessionId,
    ufrag: String,
    router: Arc<Router>,
    rx: Option<DatagramRx>,
}

impl SessionRoute {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn ufrag(&self) -> &str {
        &self.ufrag
    }

    pub fn take_receiver(&mut self) -> Option<DatagramRx> {
        self.rx.take()
    }

    pub fn rekey(&mut self, creds: &IceCreds) -> bool {
        if !self.router.rekey(self.id, &self.ufrag, creds) {
            return false;
        }

        self.ufrag = creds.ufrag.clone();

        true
    }
}

impl Drop for SessionRoute {
    fn drop(&mut self) {
        self.router.unregister(self.id, &self.ufrag);
    }
}

#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub bind: SocketAddr,
    pub advertise: Vec<SocketAddr>,
    pub queue_capacity: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 0)),
            advertise: Vec::new(),
            queue_capacity: DATAGRAM_QUEUE_CAPACITY,
        }
    }
}

#[derive(Clone)]
pub struct UdpRouter {
    router: Arc<Router>,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    advertise: Arc<Vec<SocketAddr>>,
    queue_capacity: usize,
}

impl UdpRouter {
    pub async fn bind(config: RouterConfig) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(config.bind)
            .await
            .map_err(|source| TransportError::Bind {
                addr: config.bind,
                source,
            })?;

        let local_addr = socket.local_addr().map_err(TransportError::LocalAddr)?;

        let advertise = if config.advertise.is_empty() {
            vec![local_addr]
        } else {
            config.advertise
        };

        tracing::info!(%local_addr, ?advertise, "ice socket bound");

        Ok(Self {
            router: Router::new(),
            socket: Arc::new(socket),
            local_addr,
            advertise: Arc::new(advertise),
            queue_capacity: config.queue_capacity.max(1),
        })
    }

    pub fn router(&self) -> &Arc<Router> {
        &self.router
    }

    pub fn socket(&self) -> Arc<UdpSocket> {
        Arc::clone(&self.socket)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn advertised(&self) -> &[SocketAddr] {
        &self.advertise
    }

    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    pub fn register(&self, creds: &IceCreds) -> Result<SessionRoute, TransportError> {
        self.router.register(creds, self.queue_capacity)
    }

    pub fn spawn_reader(&self) -> JoinHandle<()> {
        let socket = Arc::clone(&self.socket);
        let router = Arc::clone(&self.router);
        let local_addr = self.local_addr;

        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];

            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, source)) => {
                        router.dispatch(Instant::now(), source, &buf[..len]);
                    }
                    Err(err) if is_transient(&err) => {
                        tracing::trace!(?err, "transient udp receive error");
                    }
                    Err(err) => {
                        tracing::error!(%local_addr, ?err, "ice socket reader stopped");
                        return;
                    }
                }
            }
        })
    }
}

fn is_transient(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    )
}

pub fn looks_like_stun(buf: &[u8]) -> bool {
    buf.len() >= 20 && buf[0] < 2
}

fn crypto_provider() -> CryptoProvider {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(str0m::crypto::from_feature_flags)
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
