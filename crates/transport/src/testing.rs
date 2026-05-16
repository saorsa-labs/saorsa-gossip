//! Test-only helpers for spinning up loopback transport pairs.
//!
//! Available when the `test-helpers` feature is enabled. Used by integration
//! tests across the workspace (transport, pubsub, membership, runtime, …)
//! to avoid duplicating the connection-setup boilerplate.
//!
//! Tests are allowed to use `unwrap`/`expect`/`panic`; production code
//! paths are not. This module is only compiled into the build when the
//! `test-helpers` feature is enabled, so the relaxed lints stay scoped
//! to test infrastructure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Once;
use std::time::{Duration, Instant};

use saorsa_gossip_types::PeerId;
use tokio::time::sleep;

use crate::UdpTransportAdapter;

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

/// Force a SocketAddr to use the IPv4 loopback address while keeping its
/// port. ant-quic's `local_addr()` can report the wildcard (`::` or
/// `0.0.0.0`) for a port-0 bind, which other ant-quic configs reject as a
/// known-peer entry.
pub fn loopback_from(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
}

/// Spin up two transports on loopback using OS-assigned ephemeral ports
/// and wait until each direction is connected. Returns the pair plus their
/// bound addresses.
///
/// Using port 0 avoids cross-process collisions entirely — the kernel
/// guarantees each caller gets a free port.
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

    wait_until_connected(&node1, node2.peer_id(), Duration::from_secs(5)).await;
    wait_until_connected(&node2, node1.peer_id(), Duration::from_secs(5)).await;

    (node1, addr1, node2, addr2)
}

/// Spin up `n` transports on loopback in a star topology around the first
/// node. The first node is bootstrap; nodes 2..n connect to it via their
/// `known_peers` list. Returns the nodes paired with their loopback addrs.
///
/// Each non-bootstrap node is verified to be connected to the bootstrap.
/// The full mesh (every-to-every) is *not* established by this helper.
pub async fn loopback_star(n: usize) -> Vec<(UdpTransportAdapter, SocketAddr)> {
    assert!(n >= 1, "need at least one node");
    init_tracing();

    let any: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    let bootstrap = UdpTransportAdapter::new(any, vec![])
        .await
        .expect("bootstrap startup");
    let bootstrap_addr =
        loopback_from(bootstrap.node().local_addr().expect("bootstrap local_addr"));

    let mut nodes = Vec::with_capacity(n);
    nodes.push((bootstrap, bootstrap_addr));

    for i in 1..n {
        let t = UdpTransportAdapter::new(any, vec![bootstrap_addr])
            .await
            .unwrap_or_else(|e| panic!("node {i} startup: {e}"));
        let addr = loopback_from(t.node().local_addr().expect("local_addr"));
        nodes.push((t, addr));
    }

    let bootstrap_peer = nodes[0].0.peer_id();
    for (i, (node, _)) in nodes.iter().enumerate().skip(1) {
        wait_until_connected(node, bootstrap_peer, Duration::from_secs(5)).await;
        wait_until_connected(&nodes[0].0, node.peer_id(), Duration::from_secs(5)).await;
        tracing::debug!(target: "saorsa_gossip_transport::testing", "node {i} connected to bootstrap");
    }

    nodes
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
