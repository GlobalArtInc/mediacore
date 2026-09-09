
use super::*;

fn config(body: &str) -> RtcConfig {
    mediacore_config::Config::parse(&format!("keys:\n  k: s\nrtc:\n{body}"))
        .expect("config parses")
        .rtc
}

#[test]
fn explicit_addresses_win_over_everything_else() {
    let rtc = config("  advertise_addresses:\n    - 203.0.113.7\n  node_ip: 198.51.100.4\n");
    let local = SocketAddr::from(([0, 0, 0, 0], 7882));

    assert_eq!(
        resolve_advertise(&rtc, local),
        vec![SocketAddr::from(([203, 0, 113, 7], 7882))]
    );
}

#[test]
fn the_node_ip_is_used_when_no_address_is_listed() {
    let rtc = config("  node_ip: 198.51.100.4\n");
    let local = SocketAddr::from(([0, 0, 0, 0], 7882));

    assert_eq!(
        resolve_advertise(&rtc, local),
        vec![SocketAddr::from(([198, 51, 100, 4], 7882))]
    );
}

#[test]
fn a_bound_address_is_advertised_as_it_is() {
    let rtc = config("  use_socket_address: true\n");
    let local = SocketAddr::from(([127, 0, 0, 1], 40000));

    assert_eq!(resolve_advertise(&rtc, local), vec![local]);
}

#[test]
fn a_wildcard_bind_never_advertises_the_wildcard() {
    let rtc = config("  use_socket_address: true\n");
    let local = SocketAddr::from(([0, 0, 0, 0], 7882));

    let advertised = resolve_advertise(&rtc, local);

    assert_eq!(advertised.len(), 1);
    assert!(!advertised[0].ip().is_unspecified());
    assert_eq!(advertised[0].port(), 7882);
}

#[tokio::test]
async fn the_engine_binds_a_socket_and_offers_a_host_candidate() {
    let rtc = config(
        "  udp_port: 0\n  port_range_start: 0\n  port_range_end: 0\n  bind_address: 127.0.0.1\n",
    );
    let engine = MediaEngine::start(&rtc, None).await.unwrap();

    assert_eq!(engine.local_addr().ip().to_string(), "127.0.0.1");
    assert_eq!(engine.advertised(), [engine.local_addr()]);
    assert!(engine.candidates()[0].contains("typ host"));

    let peer = engine.spawn_publisher().unwrap();
    peer.handle.close();
}
