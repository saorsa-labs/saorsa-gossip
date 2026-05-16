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
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
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

    // Act.
    node2.close().await.expect("close node2");

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
