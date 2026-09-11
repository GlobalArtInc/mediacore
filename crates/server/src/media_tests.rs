use mediacore_config::DataConfig;

use super::*;
use crate::state::publisher_data;

fn config(body: &str) -> RtcConfig {
    // A key long enough to sign with. Nothing here looks at it, but a config
    // without one no longer parses: an HS256 secret under 32 bytes is refused,
    // because the front end refuses it too and the pair would disagree about
    // which tokens are valid.
    mediacore_config::Config::parse(&format!(
        "keys:\n  k: 0123456789abcdef0123456789abcdef\nrtc:\n{body}"
    ))
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
    let engine = MediaEngine::start(&rtc, None, None).await.unwrap();

    assert_eq!(engine.local_addr().ip().to_string(), "127.0.0.1");
    assert_eq!(engine.advertised(), [engine.local_addr()]);
    assert!(engine.candidates()[0].contains("typ host"));

    let peer = engine.spawn_publisher().unwrap();
    peer.handle.close();
}

#[tokio::test]
async fn the_publisher_transport_carries_the_data_channels_the_configuration_asked_for() {
    let rtc = config(
        "  udp_port: 0\n  port_range_start: 0\n  port_range_end: 0\n  bind_address: 127.0.0.1\n",
    );

    let carrying = MediaEngine::start(&rtc, None, publisher_data(&DataConfig::default()))
        .await
        .unwrap();
    let publisher = carrying.spawn_publisher().unwrap();

    assert!(
        publisher.handle.carries_data(),
        "a transport built without data settings ignores the channels a client opens, and the \
         room goes quiet with every test still passing"
    );
    assert_eq!(
        publisher.handle.data().map(|data| data.max_packet),
        Some(DataConfig::default().max_packet_bytes as usize)
    );

    let subscriber = carrying.spawn_subscriber().unwrap();
    assert!(
        !subscriber.handle.carries_data(),
        "a client opens its channels on the publisher connection, so a subscriber that accepted \
         data would be accepting it where nothing arrives"
    );

    let silent = MediaEngine::start(
        &rtc,
        None,
        publisher_data(&DataConfig {
            enabled: false,
            ..DataConfig::default()
        }),
    )
    .await
    .unwrap();
    let refused = silent.spawn_publisher().unwrap();
    assert!(!refused.handle.carries_data());

    publisher.handle.close();
    subscriber.handle.close();
    refused.handle.close();
}
