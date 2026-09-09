use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use mediacore_config::RtcConfig;
use mediacore_sfu::{VideoCodec, is_keyframe};
use mediacore_transport::str0m::Candidate;
use mediacore_transport::str0m::format::Codec;
use mediacore_transport::{
    BweSettings, KeyframeProbe, PeerConfig, PeerRole, RouterConfig, SpawnedPeer, TransportCrypto,
    TransportError, UdpRouter, spawn_peer,
};

const ROUTE_PROBE_TARGET: &str = "192.0.2.1:9";

pub struct MediaEngine {
    crypto: TransportCrypto,
    udp: UdpRouter,
    advertise: Vec<SocketAddr>,
    candidates: Vec<String>,
    ice_disconnected_grace: std::time::Duration,
    negotiation_timeout: std::time::Duration,
    keyframe_probe: KeyframeProbe,
    subscriber_bwe: Option<BweSettings>,
}

impl MediaEngine {
    pub async fn start(
        config: &RtcConfig,
        subscriber_bwe: Option<BweSettings>,
    ) -> anyhow::Result<Self> {
        let crypto = TransportCrypto::new()?;
        let bind: IpAddr = config.bind_address.parse()?;
        let udp = bind_ice_socket(config, bind).await?;
        let advertise = resolve_advertise(config, udp.local_addr());

        anyhow::ensure!(
            !advertise.is_empty(),
            "no advertised ice address could be resolved, set rtc.advertise_addresses"
        );

        let candidates = advertise
            .iter()
            .filter_map(|addr| match Candidate::host(*addr, "udp") {
                Ok(candidate) => Some(candidate.to_sdp_string()),
                Err(error) => {
                    tracing::warn!(%addr, %error, "cannot build a host candidate");
                    None
                }
            })
            .collect();

        tracing::info!(
            local = %udp.local_addr(),
            ?advertise,
            fingerprint = %crypto.fingerprint(),
            ice_disconnected_timeout = config.ice_disconnected_timeout,
            negotiation_timeout = config.negotiation_timeout,
            bwe = subscriber_bwe.is_some(),
            "media transport ready"
        );

        udp.spawn_reader();

        Ok(Self {
            crypto,
            udp,
            advertise,
            candidates,
            ice_disconnected_grace: config.ice_disconnected_grace(),
            negotiation_timeout: config.negotiation_grace(),
            keyframe_probe: keyframe_probe(),
            subscriber_bwe,
        })
    }

    pub fn negotiation_timeout(&self) -> std::time::Duration {
        self.negotiation_timeout
    }

    pub fn spawn_publisher(&self) -> Result<SpawnedPeer, TransportError> {
        self.spawn(PeerRole::Publisher)
    }

    pub fn spawn_subscriber(&self) -> Result<SpawnedPeer, TransportError> {
        self.spawn(PeerRole::Subscriber)
    }

    fn spawn(&self, role: PeerRole) -> Result<SpawnedPeer, TransportError> {
        let base = match role {
            PeerRole::Publisher => PeerConfig::publisher(),
            PeerRole::Subscriber => PeerConfig::subscriber(self.subscriber_bwe),
        };

        let keyframe_probe = match role {
            PeerRole::Publisher => Some(self.keyframe_probe.clone()),
            PeerRole::Subscriber => None,
        };

        spawn_peer(
            &self.crypto,
            &self.udp,
            PeerConfig {
                advertise: self.advertise.clone(),
                ice_disconnected_grace: self.ice_disconnected_grace,
                negotiation_timeout: self.negotiation_timeout,
                keyframe_probe,
                ..base
            },
        )
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.udp.local_addr()
    }

    pub fn advertised(&self) -> &[SocketAddr] {
        &self.advertise
    }

    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    pub fn subscriber_bwe(&self) -> Option<BweSettings> {
        self.subscriber_bwe
    }
}

fn keyframe_probe() -> KeyframeProbe {
    KeyframeProbe::new(|codec: Codec, payload: &[u8]| {
        match VideoCodec::from_mime(&codec.to_string()) {
            Ok(codec) => is_keyframe(codec, payload).unwrap_or(false),
            Err(_) => false,
        }
    })
}

async fn bind_ice_socket(config: &RtcConfig, bind: IpAddr) -> anyhow::Result<UdpRouter> {
    let ports = config.candidate_ports();
    let mut last: Option<TransportError> = None;

    for port in &ports {
        let attempt = UdpRouter::bind(RouterConfig {
            bind: SocketAddr::new(bind, *port),
            ..RouterConfig::default()
        })
        .await;

        match attempt {
            Ok(udp) => return Ok(udp),
            Err(error) => last = Some(error),
        }
    }

    let first = ports.first().copied().unwrap_or_default();
    let last_port = ports.last().copied().unwrap_or_default();

    match last {
        Some(error) => Err(anyhow::Error::new(error).context(format!(
            "no free udp port in {first}..={last_port} on {bind}"
        ))),
        None => anyhow::bail!("rtc configuration produced no port to bind"),
    }
}

fn resolve_advertise(config: &RtcConfig, local: SocketAddr) -> Vec<SocketAddr> {
    if config.use_external_ip {
        tracing::warn!(
            "rtc.use_external_ip is not implemented, falling back to the configured addresses"
        );
    }

    let configured = config.advertise(local.port());
    if !configured.is_empty() {
        return configured;
    }

    if !config.node_ip.is_empty()
        && let Ok(ip) = config.node_ip.parse::<IpAddr>()
    {
        return vec![SocketAddr::new(ip, local.port())];
    }

    if config.use_socket_address && !local.ip().is_unspecified() {
        return vec![local];
    }

    let ip = primary_local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    tracing::info!(%ip, "advertising the primary local address");

    vec![SocketAddr::new(ip, local.port())]
}

fn primary_local_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(ROUTE_PROBE_TARGET).ok()?;

    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
