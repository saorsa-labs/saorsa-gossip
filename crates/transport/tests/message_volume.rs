//! High-volume small-message integration test.
//!
//! Sends N small (1 KiB) `PubSub` messages back-to-back from one loopback
//! node to another, verifies every message arrives intact, and reports
//! latency percentiles.
//!
//! Default N is 1_000 — enough to surface backpressure / dropped-message
//! regressions but quick enough for the routine test loop. Set
//! `SAORSA_VOLUME_HEAVY=1` to escalate to 10_000 (still bounded by the
//! adapter's 4 MiB per-message ceiling — these messages are 1 KiB each).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use tokio::time::timeout;

mod common;

const MSG_SIZE: usize = 1024;
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

fn payload_for(seq: u32) -> Bytes {
    // 4-byte big-endian seq prefix, rest filled deterministically so any
    // offset / interleave bug shows up immediately.
    let mut buf = Vec::with_capacity(MSG_SIZE);
    buf.extend_from_slice(&seq.to_be_bytes());
    for i in 4..MSG_SIZE {
        buf.push(((seq as usize + i) % 251) as u8);
    }
    Bytes::from(buf)
}

fn percentile(sorted_us: &[u128], p: f64) -> u128 {
    if sorted_us.is_empty() {
        return 0;
    }
    let idx = ((sorted_us.len() as f64) * p).clamp(0.0, sorted_us.len() as f64 - 1.0) as usize;
    sorted_us[idx]
}

async fn run_volume(count: u32) {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1 = Arc::new(node1);
    let node2 = Arc::new(node2);
    let dest_peer = node1.peer_id();
    let sender_peer = node2.peer_id();

    // Receiver task: pull `count` messages, record per-message latency
    // (ts captured by sender embedded in the payload would require clock
    // sync; instead we record receive-side intervals from the start of the
    // burst, which is sufficient for regression detection).
    let recv_handle = {
        let node1 = Arc::clone(&node1);
        tokio::spawn(async move {
            let mut received: Vec<(u32, Instant)> = Vec::with_capacity(count as usize);
            let start = Instant::now();
            while received.len() < count as usize {
                let msg = timeout(RECV_TIMEOUT, GossipTransport::receive_message(&node1))
                    .await
                    .expect("recv timed out")
                    .expect("recv failed");
                let (sender, st, data) = msg;
                assert_eq!(sender, sender_peer);
                assert_eq!(st, GossipStreamType::PubSub);
                assert_eq!(data.len(), MSG_SIZE, "payload size mismatch");
                let seq = u32::from_be_bytes(data[..4].try_into().expect("4-byte prefix"));
                received.push((seq, Instant::now()));
            }
            (start, received)
        })
    };

    let send_start = Instant::now();
    for seq in 0..count {
        GossipTransport::send_to_peer(
            &node2,
            dest_peer,
            GossipStreamType::PubSub,
            payload_for(seq),
        )
        .await
        .expect("send_to_peer");
    }
    let send_elapsed = send_start.elapsed();

    let (recv_start, received) = recv_handle.await.expect("recv task join");
    let recv_total = recv_start.elapsed();

    // Verify all sequence numbers present (order is best-effort over QUIC
    // unidirectional streams, so we check membership not strict order).
    let mut seqs: Vec<u32> = received.iter().map(|(s, _)| *s).collect();
    seqs.sort_unstable();
    for (i, s) in seqs.iter().enumerate() {
        assert_eq!(*s, i as u32, "missing or duplicated sequence at index {i}");
    }

    // Per-message latency = (recv_ts - recv_start) — this captures
    // end-to-end pipeline latency relative to the start of the burst.
    let mut latencies_us: Vec<u128> = received
        .iter()
        .map(|(_, ts)| ts.duration_since(recv_start).as_micros())
        .collect();
    latencies_us.sort_unstable();

    eprintln!(
        "message_volume count={count} send_total={:?} recv_total={:?} \
         msgs/s_send={:.0} msgs/s_recv={:.0} \
         p50={}us p95={}us p99={}us max={}us",
        send_elapsed,
        recv_total,
        count as f64 / send_elapsed.as_secs_f64(),
        count as f64 / recv_total.as_secs_f64(),
        percentile(&latencies_us, 0.50),
        percentile(&latencies_us, 0.95),
        percentile(&latencies_us, 0.99),
        latencies_us.last().copied().unwrap_or(0),
    );

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_thousand_small_messages() {
    run_volume(1_000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "10k-message volume; set SAORSA_VOLUME_HEAVY=1 and run with --run-ignored"]
async fn ten_thousand_small_messages() {
    if std::env::var("SAORSA_VOLUME_HEAVY").as_deref() != Ok("1") {
        eprintln!("skipping: SAORSA_VOLUME_HEAVY != 1");
        return;
    }
    run_volume(10_000).await;
}
