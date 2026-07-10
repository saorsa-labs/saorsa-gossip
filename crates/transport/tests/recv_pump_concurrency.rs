//! High-concurrency recv pump stress test.
//!
//! Tests whether the recv pump can misroute payloads or interleave buffers
//! under HIGH concurrent multi-peer, multi-topic, interleaved
//! PubSub+Membership+Bulk delivery.
//!
//! Star topology: 1 hub + N spokes. Each spoke sends interleaved messages
//! of all three stream types concurrently. The hub receives everything via
//! a single recv pump (exactly as production x0x does) and verifies:
//! 1. Every message goes to the correct channel (stream type matches)
//! 2. Every message's bytes are intact (position-dependent payload)
//!
//! If this passes, the recv pump is concurrency-safe under high load.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use tokio::time::timeout;

mod common;

const RECV_TIMEOUT: Duration = Duration::from_secs(30);

/// Position-dependent payload so any corruption/misrouting is detectable.
/// The first 4 bytes encode the stream_type byte + a seed for easy verification.
fn make_tagged_payload(stream_type: GossipStreamType, seed: u64) -> Vec<u8> {
    let st_byte = stream_type.to_byte();
    let mut buf = Vec::with_capacity(256);
    // 4-byte tag: [st_byte, seed_low, seed_mid, seed_high]
    buf.push(st_byte);
    buf.extend_from_slice(&seed.to_le_bytes()[..3]);
    // Position-dependent filler
    for i in 0..200 {
        let v = (i as u64).wrapping_add(seed).wrapping_add(st_byte as u64);
        buf.push((v % 251) as u8);
    }
    buf
}

/// Verify a received payload matches the expected stream_type + seed.
fn verify_tagged(payload: &[u8], expected_st: GossipStreamType, seed: u64) -> bool {
    let expected = make_tagged_payload(expected_st, seed);
    payload == expected.as_slice()
}

/// Star-topology concurrent multi-type stress: 4 spokes each send interleaved
/// PubSub + Membership + Bulk to the hub concurrently. Hub verifies every
/// message goes to the right channel with intact bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn star_concurrent_multi_type_no_misroute() {
    const SPOKES: usize = 4;
    const MSGS_PER_TYPE_PER_SPOKE: usize = 15;

    let mut star = common::loopback_star(1 + SPOKES).await;
    let nodes: Vec<_> = star.drain(..).map(|(n, _)| Arc::new(n)).collect();
    let hub = Arc::clone(&nodes[0]);
    let hub_peer = hub.peer_id();

    // Each spoke sends MSGS_PER_TYPE_PER_SPOKE messages of each type concurrently.
    let total_expected = SPOKES * 3 * MSGS_PER_TYPE_PER_SPOKE;
    let mut send_handles = Vec::new();

    for spoke_idx in 1..=SPOKES {
        let spoke = Arc::clone(&nodes[spoke_idx]);
        for i in 0..MSGS_PER_TYPE_PER_SPOKE {
            let seed = (spoke_idx as u64) * 10_000 + i as u64;
            let hub_peer = hub_peer;
            for &st in &[
                GossipStreamType::PubSub,
                GossipStreamType::Membership,
                GossipStreamType::Bulk,
            ] {
                let spoke = Arc::clone(&spoke);
                let payload = Bytes::from(make_tagged_payload(st, seed));
                send_handles.push(tokio::spawn(async move {
                    GossipTransport::send_to_peer(&spoke, hub_peer, st, payload)
                        .await
                        .expect("send");
                    (st, seed)
                }));
            }
        }
    }

    // Hub receives all messages and verifies channel routing + byte integrity.
    let mut recv_count = 0usize;
    let mut misroute_count = 0usize;
    let mut corrupt_count = 0usize;
    let mut seen: std::collections::HashSet<(u8, u64)> = std::collections::HashSet::new();

    for _ in 0..total_expected {
        let recv = timeout(RECV_TIMEOUT, GossipTransport::receive_message(&hub))
            .await
            .expect("recv timed out")
            .expect("recv failed");
        let (_sender, st, data) = recv;
        recv_count += 1;

        // Extract the tag from the payload to identify which message this is.
        if data.len() < 4 {
            corrupt_count += 1;
            eprintln!("TRUNCATED payload len={}", data.len());
            continue;
        }
        let tag_st = data[0];
        let tag_seed = u64::from_le_bytes({
            let mut b = [0u8; 8];
            b[..3].copy_from_slice(&data[1..4]);
            b
        });

        // Check: did the message arrive on the correct channel?
        if tag_st != st.to_byte() {
            misroute_count += 1;
            eprintln!(
                "MISROUTE: payload tag st={} but delivered on channel {:?} (seed={})",
                tag_st, st, tag_seed
            );
        }

        // Check: are the bytes intact?
        let expected_st = GossipStreamType::from_byte(tag_st).unwrap_or(GossipStreamType::Bulk);
        if !verify_tagged(&data, expected_st, tag_seed) {
            corrupt_count += 1;
            eprintln!(
                "CORRUPTION: st={:?} seed={} len={}",
                st,
                tag_seed,
                data.len()
            );
        }

        seen.insert((tag_st, tag_seed));
    }

    // Wait for all senders.
    for h in send_handles {
        h.await.expect("send task");
    }

    eprintln!(
        "star_concurrent: recv={} expected={} misroute={} corrupt={} unique={}",
        recv_count,
        total_expected,
        misroute_count,
        corrupt_count,
        seen.len()
    );

    assert_eq!(
        misroute_count, 0,
        "{misroute_count} messages misrouted to wrong channel"
    );
    assert_eq!(corrupt_count, 0, "{corrupt_count} messages corrupted");
    assert_eq!(
        recv_count, total_expected,
        "received {recv_count} but expected {total_expected}"
    );

    for n in &nodes {
        let _ = GossipTransport::close(n).await;
    }
}

/// Sustained high-throughput multi-type flood: keep all 3 channels saturated
/// for 10 seconds, verify zero misrouting or corruption.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sustained_multi_type_flood() {
    const DURATION_SECS: u64 = 10;
    const SPOKES: usize = 3;

    let mut star = common::loopback_star(1 + SPOKES).await;
    let nodes: Vec<_> = star.drain(..).map(|(n, _)| Arc::new(n)).collect();
    let hub = Arc::clone(&nodes[0]);
    let hub_peer = hub.peer_id();

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let running = Arc::new(AtomicBool::new(true));
    let sent = Arc::new(AtomicU64::new(0));
    let misroute = Arc::new(AtomicU64::new(0));
    let corrupt = Arc::new(AtomicU64::new(0));
    let recv_total = Arc::new(AtomicU64::new(0));

    // Senders: each spoke continuously sends interleaved types.
    let mut send_handles = Vec::new();
    for spoke_idx in 1..=SPOKES {
        let spoke = Arc::clone(&nodes[spoke_idx]);
        let running = Arc::clone(&running);
        let sent = Arc::clone(&sent);
        let hub_peer = hub_peer;

        send_handles.push(tokio::spawn(async move {
            let mut seq = spoke_idx as u64 * 100_000;
            let types = [
                GossipStreamType::PubSub,
                GossipStreamType::Membership,
                GossipStreamType::Bulk,
            ];
            while running.load(Ordering::Relaxed) {
                let st = types[(seq % 3) as usize];
                let payload = Bytes::from(make_tagged_payload(st, seq));
                let _ = GossipTransport::send_to_peer(&spoke, hub_peer, st, payload).await;
                sent.fetch_add(1, Ordering::Relaxed);
                seq += 1;
            }
        }));
    }

    // Receiver: verify every message.
    let hub_recv = Arc::clone(&hub);
    let running_r = Arc::clone(&running);
    let misroute_r = Arc::clone(&misroute);
    let corrupt_r = Arc::clone(&corrupt);
    let recv_total_r = Arc::clone(&recv_total);

    let recv_handle = tokio::spawn(async move {
        while running_r.load(Ordering::Relaxed) {
            let recv = match timeout(
                Duration::from_millis(500),
                GossipTransport::receive_message(&hub_recv),
            )
            .await
            {
                Ok(Ok((_s, st, data))) => (_s, st, data),
                _ => continue,
            };
            let (_sender, st, data) = recv;
            recv_total_r.fetch_add(1, Ordering::Relaxed);

            if data.len() < 4 {
                corrupt_r.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let tag_st = data[0];
            if tag_st != st.to_byte() {
                misroute_r.fetch_add(1, Ordering::Relaxed);
            }
            let tag_seed = u64::from_le_bytes({
                let mut b = [0u8; 8];
                b[..3].copy_from_slice(&data[1..4]);
                b
            });
            let expected_st = GossipStreamType::from_byte(tag_st).unwrap_or(GossipStreamType::Bulk);
            if !verify_tagged(&data, expected_st, tag_seed) {
                corrupt_r.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    tokio::time::sleep(Duration::from_secs(DURATION_SECS)).await;
    running.store(false, Ordering::Relaxed);

    // Drain remaining messages.
    tokio::time::sleep(Duration::from_secs(2)).await;
    recv_handle.abort();
    for h in send_handles {
        h.abort();
    }

    let s = sent.load(Ordering::Relaxed);
    let r = recv_total.load(Ordering::Relaxed);
    let m = misroute.load(Ordering::Relaxed);
    let c = corrupt.load(Ordering::Relaxed);

    eprintln!(
        "sustained_multi_type_flood: sent={s} recv={r} misroute={m} corrupt={c} ({DURATION_SECS}s)"
    );

    assert_eq!(m, 0, "{m} messages misrouted");
    assert_eq!(c, 0, "{c} messages corrupted");

    for n in &nodes {
        let _ = GossipTransport::close(n).await;
    }
}
