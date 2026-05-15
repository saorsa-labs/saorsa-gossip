//! Shared fixtures for transport integration tests.
//!
//! Test code is allowed to use `unwrap`/`expect`; production crates are not.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Once;
use std::time::{Duration, Instant};

use saorsa_gossip_transport::UdpTransportAdapter;
use saorsa_gossip_types::PeerId;
use tokio::time::sleep;

/// Force a SocketAddr to use the IPv4 loopback address while keeping its
/// port. ant-quic's `local_addr()` can report the wildcard (`::` or
/// `0.0.0.0`) for a port-0 bind, which other ant-quic configs reject as a
/// known-peer entry.
fn loopback_from(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
}

/// Initialise tracing exactly once across the test process.
pub fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });
}

/// Spin up two transports on loopback using OS-assigned ephemeral ports.
///
/// Returns the pair and each adapter's actual bound address. Using port 0
/// avoids cross-process collisions entirely — the kernel guarantees each
/// caller gets a free port.
pub async fn connected_pair() -> (
    UdpTransportAdapter,
    SocketAddr,
    UdpTransportAdapter,
    SocketAddr,
) {
    init_tracing();

    let any: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    let node1 = UdpTransportAdapter::new(any, vec![])
        .await
        .expect("node1 startup");
    let addr1 = loopback_from(node1.node().local_addr().expect("node1 local_addr"));

    let node2 = UdpTransportAdapter::new(any, vec![addr1])
        .await
        .expect("node2 startup");
    let addr2 = loopback_from(node2.node().local_addr().expect("node2 local_addr"));

    // node2 has node1 in `known_peers`; the adapter establishes the
    // connection in the background after `new()`. Poll until both
    // directions report each other rather than dialing explicitly — the
    // explicit dial can race with the auto-connect and produce two
    // half-open connections that confuse subsequent sends.
    wait_until_connected(&node1, node2.peer_id(), Duration::from_secs(5)).await;
    wait_until_connected(&node2, node1.peer_id(), Duration::from_secs(5)).await;

    (node1, addr1, node2, addr2)
}

/// Poll `node`'s connected-peer set until `peer` appears or the timeout fires.
pub async fn wait_until_connected(node: &UdpTransportAdapter, peer: PeerId, deadline: Duration) {
    let start = Instant::now();
    loop {
        let peers = node.connected_peers().await;
        if peers.iter().any(|(p, _)| *p == peer) {
            return;
        }
        if start.elapsed() > deadline {
            panic!(
                "timed out after {:?} waiting for peer {:?}; current peers: {:?}",
                deadline, peer, peers
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}
