//! Two-node loopback integration test.
//!
//! Canonical "does the transport actually work" smoke test. Spins up two
//! `UdpTransportAdapter`s on loopback, exchanges one message per
//! `GossipStreamType`, and verifies bytes round-trip with the correct sender
//! and stream classification.
//!
//! This is the template pattern for future transport integration tests
//! (large_transfer, message_volume, nat_loopback, etc.).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use bytes::Bytes;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use tokio::time::timeout;

mod common;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_exchange_one_message_per_stream_type() {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;

    let node1_peer_id = node1.peer_id();
    let node2_peer_id = node2.peer_id();

    let payloads: &[(GossipStreamType, &[u8])] = &[
        (GossipStreamType::Membership, b"swim-ping"),
        (GossipStreamType::PubSub, b"plumtree-have"),
        (GossipStreamType::Bulk, b"crdt-delta"),
    ];

    for (stream_type, body) in payloads {
        let payload = Bytes::copy_from_slice(body);

        node2
            .send_to_peer(node1_peer_id, *stream_type, payload.clone())
            .await
            .expect("send from node2 to node1");

        let recv = timeout(RECV_TIMEOUT, node1.receive_message())
            .await
            .expect("recv timed out")
            .expect("recv failed");

        let (sender, st, data) = recv;
        assert_eq!(sender, node2_peer_id, "sender peer id mismatch");
        assert_eq!(st, *stream_type, "stream type mismatch");
        assert_eq!(data.as_ref(), *body, "payload bytes mismatch");
    }

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_payload_round_trip_integrity() {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;

    // 64 KiB of recognisable bytes — small enough to be quick, large enough
    // to exercise more than one QUIC frame.
    const SIZE: usize = 64 * 1024;
    let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let bytes = Bytes::from(payload.clone());

    node2
        .send_to_peer(node1.peer_id(), GossipStreamType::Bulk, bytes)
        .await
        .expect("send bulk");

    let (_sender, stream_type, received) = timeout(RECV_TIMEOUT, node1.receive_message())
        .await
        .expect("recv timed out")
        .expect("recv failed");

    assert_eq!(stream_type, GossipStreamType::Bulk);
    assert_eq!(received.len(), SIZE);
    assert_eq!(received.as_ref(), payload.as_slice());

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}
