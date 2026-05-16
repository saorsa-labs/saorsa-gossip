//! Regression test for issue #14.
//!
//! `UdpTransportAdapter::close()` must tear down the underlying ant-quic
//! connections, not just clear the adapter's local `connected_peers` map.
//!
//! Before the fix, `close()` only cleared local tracking and the
//! underlying `ant_quic::Node` retained every live connection. That left
//! peers reachable on the wire, broke graceful-shutdown semantics, and
//! forced PR #11's SWIM failure-detection test to abort the dispatcher
//! task instead of calling `close()`.
//!
//! After the fix, `close()` iterates `node.connected_peers()` and calls
//! `node.disconnect()` for every entry, so the ant-quic node-level peer
//! registry must be empty post-close. Asserting that on the *closed*
//! node is deterministic — no propagation delay, no NAT-traversal
//! reconnect races, no loopback-specific timing surprises. The
//! cross-node propagation that PR #11's test actually depends on is
//! still verified by the existing SWIM integration test (which is what
//! the issue ultimately unblocks).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use bytes::Bytes;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport, TransportAdapter};
use tokio::time::timeout;

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_disconnects_peers_at_ant_quic_node_layer() {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1_peer = node1.peer_id();
    let node2_peer = node2.peer_id();

    // Sanity: baseline send must succeed so a post-close failure cannot
    // be blamed on missing test setup.
    node1
        .send_to_peer(
            node2_peer,
            GossipStreamType::Membership,
            Bytes::from_static(b"hello"),
        )
        .await
        .expect("baseline send before close");
    let baseline = timeout(Duration::from_secs(5), node2.receive_message())
        .await
        .expect("baseline recv timed out")
        .expect("baseline recv error");
    assert_eq!(baseline.0, node1_peer);
    assert_eq!(baseline.1, GossipStreamType::Membership);

    // Pre-condition: both nodes report each other connected at the
    // ant-quic Node layer (this is what we expect close() to act on —
    // the live ant-quic registry, not just the adapter's map).
    assert!(
        node2
            .connected_peers()
            .await
            .iter()
            .any(|(p, _)| *p == node1_peer),
        "pre-condition: node2 must see node1 connected before close()"
    );

    // Act via the GossipTransport trait specifically — two `close()`
    // methods exist on UdpTransportAdapter (one per trait), so an
    // unqualified `node2.close()` is ambiguous.
    GossipTransport::close(&node2)
        .await
        .expect("GossipTransport::close node2");

    // Post-condition: node2's own ant-quic registry no longer lists any
    // peers. Before the fix this assertion would fail because
    // `node.connected_peers()` would still report node1 as connected.
    let post = node2.connected_peers().await;
    assert!(
        post.is_empty(),
        "close() failed to disconnect ant-quic peers; node2 still reports: {:?}",
        post
    );
}

/// `TransportAdapter::close` is a *separate* impl on the same type, used
/// by the older `TransportAdapter` trait. It had the same bug shape —
/// iterating the lazy adapter-local map instead of the live ant-quic
/// registry — so it gets the same assertion.
///
/// To reproduce the *original* failure mode (rather than a happy-path
/// fixture in which the lazy map happens to be populated by the
/// `wait_until_connected` helper), the test explicitly clears
/// `connected_peers` immediately before calling `close()`. This models
/// production reality on the accept side: `ant_quic::Node` accepts a
/// connection, the application-layer adapter never observes it via
/// `connected_peers()`, but the connection is still live in ant-quic.
/// Pre-fix, `close()` walks an empty local map and disconnects nothing.
/// Post-fix, `close()` walks `node.connected_peers()` and tears down
/// the real wire connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_adapter_close_disconnects_even_when_lazy_cache_is_cold() {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1_peer = node1.peer_id();

    // Baseline so the post-close assertion is unambiguous about cause.
    node1
        .send_to_peer(
            node2.peer_id(),
            GossipStreamType::Membership,
            Bytes::from_static(b"hello"),
        )
        .await
        .expect("baseline send before close");
    let _ = timeout(Duration::from_secs(5), node2.receive_message())
        .await
        .expect("baseline recv timed out")
        .expect("baseline recv error");

    // Pre-condition: node1 IS connected at the ant-quic node layer of
    // node2 (we read through to node2.node().connected_peers()).
    let pre = node2.connected_peers().await;
    assert!(
        pre.iter().any(|(p, _)| *p == node1_peer),
        "pre-condition: node2 must see node1 connected before close()"
    );

    // Simulate the accept-side-not-yet-observed scenario: clear the
    // adapter's lazy local map. The underlying node still holds the
    // live connection; only the adapter's cache is empty. This is
    // exactly the production gap the user flagged in PR #16 review.
    node2.clear_lazy_peer_cache_for_test().await;

    // Act via the TransportAdapter trait specifically — separate code
    // path from GossipTransport::close, separate bug.
    TransportAdapter::close(&node2)
        .await
        .expect("TransportAdapter::close node2");

    let post = node2.connected_peers().await;
    assert!(
        post.is_empty(),
        "TransportAdapter::close() with cold lazy cache failed to \
         disconnect ant-quic peers; node2 still reports: {:?}",
        post
    );
}
