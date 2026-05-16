//! Large single-stream transfer integration tests.
//!
//! Sends one large `Bulk`-stream payload between two loopback nodes and
//! verifies every byte. Records wall-clock throughput per size to surface
//! regressions in the QUIC bulk path.
//!
//! ## Upstream limit (ant-quic 0.27.x)
//!
//! `Node::send` opens a fresh unidirectional QUIC stream and is capped per
//! call by ant-quic's `P2pConfig::max_message_size` (default **4 MiB**).
//! Saorsa-gossip's `NodeConfig` builder path does not currently expose this
//! knob, so any single message above the default fails on the receive path
//! with `Connection closed: ReaderExit`.
//!
//! Tracked upstream in <https://github.com/saorsa-labs/ant-quic/issues/191>;
//! once the knob is plumbed through `NodeConfigBuilder`, the >4 MiB tests
//! below become un-ignored.
//!
//! Sizes:
//! - 1 MiB and 3 MiB run on every invocation (both within the 4 MiB cap).
//! - 10 MiB / 100 MiB are `#[ignore]` until the cap is configurable; opt in
//!   with `cargo nextest run --test large_transfer --run-ignored all`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use tokio::time::timeout;

mod common;

const ONE_MIB: usize = 1024 * 1024;
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// Position-dependent bytes so any offset/framing bug surfaces as a value
/// mismatch rather than a length-only mismatch.
fn make_payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

async fn run_one(size_mib: usize) {
    let size = size_mib * ONE_MIB;
    let payload = make_payload(size);
    let bytes = Bytes::from(payload.clone());

    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1 = Arc::new(node1);
    let node2 = Arc::new(node2);
    let dest_peer = node1.peer_id();
    let sender_peer = node2.peer_id();

    // Receiver runs on its own task so a busy send cannot starve it.
    let recv_handle = {
        let node1 = Arc::clone(&node1);
        tokio::spawn(async move {
            timeout(RECV_TIMEOUT, GossipTransport::receive_message(&node1)).await
        })
    };

    let start = Instant::now();
    GossipTransport::send_to_peer(&node2, dest_peer, GossipStreamType::Bulk, bytes)
        .await
        .expect("send_to_peer");

    let recv = recv_handle
        .await
        .expect("recv task join")
        .expect("recv timed out")
        .expect("recv failed");
    let elapsed = start.elapsed();

    let (sender, stream_type, received) = recv;
    assert_eq!(sender, sender_peer, "sender mismatch");
    assert_eq!(stream_type, GossipStreamType::Bulk);
    assert_eq!(received.len(), size, "length mismatch");
    assert_eq!(received.as_ref(), payload.as_slice(), "byte mismatch");

    let mb_per_sec = (size as f64 / 1_048_576.0) / elapsed.as_secs_f64();
    eprintln!(
        "large_transfer {size_mib} MiB: round_trip={:?} throughput={:.2} MiB/s",
        elapsed, mb_per_sec,
    );

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_mib_round_trip() {
    run_one(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_mib_round_trip() {
    run_one(3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "blocked on ant-quic#191 — exceeds 4 MiB max_message_size default"]
async fn ten_mib_round_trip() {
    run_one(10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "blocked on ant-quic#191; also heavy. Set SAORSA_LARGE_TRANSFER_HEAVY=1 to attempt."]
async fn one_hundred_mib_round_trip() {
    if std::env::var("SAORSA_LARGE_TRANSFER_HEAVY").as_deref() != Ok("1") {
        eprintln!("skipping: SAORSA_LARGE_TRANSFER_HEAVY != 1");
        return;
    }
    run_one(100).await;
}
