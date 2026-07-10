//! End-to-end repro harness for the signed-payload drop / corruption bug.
//!
//! A signed ML-DSA-65 IdentityAnnouncement (or pubsub beacon) is ~5.3 KB on the
//! wire — multi-packet at 1448 B QUIC MSS, forcing reassembly. On the live
//! 6-node testnet (ant-quic 0.27.30 build, post-#195/#200 read_to_end fix) the
//! receiver still reports "ML-DSA-65 signature verification failed" at ~10 per
//! 3 min per node. Sig-verify only fails on corrupted bytes, so some recv path
//! still delivers corruption.
//!
//! ## Path under test (identical to x0x's production path)
//!
//! ```text
//! SEND: send_to_peer → node.send() → connection.open_uni() → write_all → finish
//! RECV: reader task → connection.accept_uni() → read_to_end(4 MiB) → node.recv()
//! ```
//!
//! No application-layer chunking exists in saorsa-gossip; the ONLY multi-packet
//! reassembly is ant-quic's `read_to_end` on unidirectional streams.
//!
//! ## What this harness does
//!
//! 1. **single_signed_envelope_round_trip** — sends one ~5.3 KB signed-shaped
//!    payload (v2 envelope format) and byte-compares. Establishes the baseline.
//!
//! 2. **concurrent_signed_envelope_stress** — sends N concurrent ~5.3 KB
//!    payloads with position-dependent bytes and a per-payload SHA-256 digest
//!    embedded in the envelope. Stresses the assembler with many concurrent
//!    uni streams. Any byte mismatch is reported with exact offset/diff.
//!
//! 3. **mixed_size_burst** — sends a burst of payloads of varying sizes
//!    (512 B … 16 KB) concurrently, mixing PubSub and Bulk stream types,
//!    to surface any stream-type or size-dependent corruption.
//!
//! If any test fails, the corruption is at the ant-quic transport boundary
//! (read_to_end / assembler). If all pass on loopback, the bug requires WAN
//! conditions (packet reordering / connection replacement) to trigger —
//! evidence that narrows the search to the assembler's out-of-order path.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

mod common;

const RECV_TIMEOUT: Duration = Duration::from_secs(30);

/// Wire size of a signed ML-DSA-65 v2 envelope (~5550 bytes: 1+32+2+1952+2+3309+2+topic+payload).
/// Spans 4+ QUIC packets at 1448 B MSS, forcing multi-packet reassembly.
const SIGNED_ENVELOPE_SIZE: usize = 5550;

/// Position-dependent bytes so any offset/framing bug surfaces as a value
/// mismatch rather than a length-only mismatch. Uses a 251-byte prime modulus
/// so the pattern doesn't alias with common zero-fill artefacts.
fn make_positional_bytes(size: usize, seed: u64) -> Vec<u8> {
    (0..size)
        .map(|i| {
            let v = (i as u64).wrapping_add(seed);
            ((v % 251) as u8) ^ ((v >> 8) as u8 & 0x0F)
        })
        .collect()
}

/// Construct a v2-shaped envelope matching x0x's wire format:
/// `[0x02 | agent_id(32) | pk_len(u16be) | pk(1952) | sig_len(u16be) | sig(3309)
///   | topic_len(u16be) | topic | digest(32) | filler]`
///
/// The digest is SHA-256 of everything BEFORE the digest field, so the receiver
/// can detect ANY byte corruption in the envelope (not just the filler).
fn make_signed_envelope(seed: u64) -> Vec<u8> {
    let topic = b"x0x.presence.global";
    let pk = vec![0xA1u8; 1952]; // ML-DSA-65 public key placeholder
    let sig = vec![0xB2u8; 3309]; // ML-DSA-65 signature placeholder
    let seed_le = seed.to_le_bytes();
    let mut agent_id = [0u8; 32];
    for (i, b) in seed_le.iter().cycle().take(32).enumerate() {
        agent_id[i] = *b;
    }

    // Filler to reach SIGNED_ENVELOPE_SIZE.
    let overhead = 1 + 32 + 2 + pk.len() + 2 + sig.len() + 2 + topic.len() + 32; // +32 for digest
    let filler_len = SIGNED_ENVELOPE_SIZE.saturating_sub(overhead);
    let filler = make_positional_bytes(filler_len, seed);

    // Build everything up to (but not including) the digest.
    let mut buf = Vec::with_capacity(SIGNED_ENVELOPE_SIZE);
    buf.push(0x02); // VERSION_V2
    buf.extend_from_slice(&agent_id);
    buf.extend_from_slice(&(pk.len() as u16).to_be_bytes());
    buf.extend_from_slice(&pk);
    buf.extend_from_slice(&(sig.len() as u16).to_be_bytes());
    buf.extend_from_slice(&sig);
    buf.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    buf.extend_from_slice(topic);

    // SHA-256 digest of everything so far.
    let digest = Sha256::digest(&buf);
    buf.extend_from_slice(&digest);
    buf.extend_from_slice(&filler);
    buf
}

/// Verify a received envelope: recompute the digest and compare.
/// Returns Ok(()) if bytes are intact, Err with diff details if corrupted.
fn verify_envelope(received: &[u8], seed: u64) -> Result<(), String> {
    let expected = make_signed_envelope(seed);

    if received.len() != expected.len() {
        // Find where the divergence starts by comparing field-by-field.
        return Err(format!(
            "LENGTH MISMATCH: expected {} bytes, got {} bytes (seed={})",
            expected.len(),
            received.len(),
            seed
        ));
    }

    // First, check the embedded digest. The digest covers bytes [0..digest_offset).
    // digest_offset = 1 + 32 + 2 + 1952 + 2 + 3309 + 2 + topic_len
    let topic_len = 19usize; // "x0x.presence.global"
    let digest_offset = 1 + 32 + 2 + 1952 + 2 + 3309 + 2 + topic_len;

    if received.len() < digest_offset + 32 {
        return Err(format!(
            "TRUNCATED: received {} bytes, need at least {} for digest (seed={})",
            received.len(),
            digest_offset + 32,
            seed
        ));
    }

    let recomputed = Sha256::digest(&received[..digest_offset]);
    let embedded = &received[digest_offset..digest_offset + 32];
    if recomputed.as_slice() != embedded {
        // The digest check failed — corruption is in the envelope header/sig/pk fields.
        return Err(format!(
            "DIGEST MISMATCH: embedded digest does not match recomputed digest \
             (seed={}). Corruption is in envelope bytes [0..{}].",
            seed, digest_offset
        ));
    }

    // Full byte-by-byte comparison for exhaustive diff.
    let mut first_diff = None;
    let mut diff_count = 0usize;
    for (i, (a, b)) in expected.iter().zip(received.iter()).enumerate() {
        if a != b {
            if first_diff.is_none() {
                first_diff = Some(i);
            }
            diff_count += 1;
            if diff_count <= 10 {
                eprintln!(
                    "  byte diff @ offset {i}: expected 0x{a:02X}, got 0x{b:02X} (seed={seed})"
                );
            }
        }
    }

    if diff_count > 0 {
        let last = first_diff.unwrap();
        Err(format!(
            "BYTE MISMATCH: {diff_count} bytes differ, first at offset {last} (seed={seed})"
        ))
    } else {
        Ok(())
    }
}

/// Single ~5.3 KB signed-shaped envelope round-trip — establishes baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_signed_envelope_round_trip() {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1 = Arc::new(node1);
    let node2 = Arc::new(node2);
    let dest_peer = node1.peer_id();

    let payload = make_signed_envelope(42);
    let bytes = Bytes::from(payload.clone());

    // Receiver on its own task so a busy send cannot starve it.
    let recv_handle = {
        let node1 = Arc::clone(&node1);
        tokio::spawn(async move {
            timeout(RECV_TIMEOUT, GossipTransport::receive_message(&node1)).await
        })
    };

    GossipTransport::send_to_peer(&node2, dest_peer, GossipStreamType::PubSub, bytes)
        .await
        .expect("send_to_peer");

    let recv = recv_handle
        .await
        .expect("recv task join")
        .expect("recv timed out")
        .expect("recv failed");

    let (_sender, stream_type, received) = recv;
    assert_eq!(
        stream_type,
        GossipStreamType::PubSub,
        "stream type mismatch"
    );

    verify_envelope(&received, 42).unwrap_or_else(|e| panic!("single envelope corrupted: {e}"));

    eprintln!(
        "single_signed_envelope_round_trip: {} bytes OK",
        received.len()
    );

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}

/// Concurrent stress: send N signed-shaped envelopes simultaneously, each with a
/// unique seed, and verify every byte of every one. This exercises many
/// concurrent uni streams + multi-packet reassembly per stream simultaneously.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_signed_envelope_stress() {
    const N: usize = 50;

    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1 = Arc::new(node1);
    let node2 = Arc::new(node2);
    let dest_peer = node1.peer_id();

    // Pre-build all payloads.
    let payloads: Vec<Vec<u8>> = (0..N)
        .map(|i| make_signed_envelope(i as u64 * 7 + 1))
        .collect();

    // Spawn N concurrent senders.
    let mut send_handles = Vec::new();
    for (i, payload) in payloads.iter().enumerate() {
        let node2 = Arc::clone(&node2);
        let bytes = Bytes::from(payload.clone());
        send_handles.push(tokio::spawn(async move {
            GossipTransport::send_to_peer(&node2, dest_peer, GossipStreamType::PubSub, bytes)
                .await
                .expect("send_to_peer");
            i
        }));
    }

    // Receive N messages and collect them.
    let mut received: Vec<(usize, Vec<u8>)> = Vec::with_capacity(N);
    for _ in 0..N {
        let recv = timeout(RECV_TIMEOUT, GossipTransport::receive_message(&node1))
            .await
            .expect("recv timed out")
            .expect("recv failed");
        // The first byte after the stream_type framing is the v2 version byte (0x02).
        // We identify by the agent_id (bytes [1..33] of the payload), which encodes the seed.
        let (_sender, _st, data) = recv;
        let seed_byte = data.get(1).copied().unwrap_or(0xFF);
        received.push((seed_byte as usize, data.to_vec()));
    }

    // Wait for all senders.
    for h in send_handles {
        h.await.expect("send task");
    }

    // Verify every received payload against its expected seed.
    let mut failures = 0;
    for (seed_byte, data) in &received {
        // Recover the seed: seed_byte is (seed & 0xFF), so seed = seed_byte * 7 + 1
        // when seed_byte < 256 and seed = i*7+1. For i in 0..50, seed ranges 1..344.
        // seed_byte = (i*7+1) & 0xFF. Since i<50, max seed = 344, seed_byte = 344&0xFF = 88.
        // i*7+1 for i=0..50 → seeds 1,8,15,...,344. seed_byte = seed & 0xFF.
        // We can recover: seed = seed_byte if seed_byte % 7 == 1 ... but simpler: try all.
        let mut found = false;
        for i in 0..N {
            let seed = i as u64 * 7 + 1;
            if (seed as u8) == *seed_byte as u8 {
                if let Err(e) = verify_envelope(data, seed) {
                    failures += 1;
                    eprintln!("CORRUPTION DETECTED: seed={seed}: {e}");
                }
                found = true;
                break;
            }
        }
        if !found {
            failures += 1;
            eprintln!(
                "UNMATCHED PAYLOAD: seed_byte={seed_byte}, len={}",
                data.len()
            );
        }
    }

    assert_eq!(
        failures, 0,
        "{failures}/{N} payloads corrupted in concurrent stress"
    );

    eprintln!("concurrent_signed_envelope_stress: {N} payloads × ~5.5 KB all verified OK");

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}

/// Burst of mixed sizes and stream types to surface size/type-dependent issues.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn mixed_size_burst() {
    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1 = Arc::new(node1);
    let node2 = Arc::new(node2);
    let dest_peer = node1.peer_id();

    // Mix of sizes spanning 1-packet (≤1448 B) to multi-packet.
    let sizes: &[usize] = &[512, 1448, 1449, 2896, 4344, 5550, 8192, 16384];
    let stream_types = [GossipStreamType::PubSub, GossipStreamType::Bulk];

    let mut send_handles = Vec::new();
    for &size in sizes {
        for &st in &stream_types {
            let payload = make_positional_bytes(size, size as u64 * 1000 + st.to_byte() as u64);

            let node2 = Arc::clone(&node2);
            let bytes = Bytes::from(payload.clone());
            send_handles.push(tokio::spawn(async move {
                GossipTransport::send_to_peer(&node2, dest_peer, st, bytes)
                    .await
                    .expect("send_to_peer");
            }));
        }
    }

    let total = sizes.len() * stream_types.len();
    let mut failures = 0;
    for _ in 0..total {
        let recv = timeout(RECV_TIMEOUT, GossipTransport::receive_message(&node1))
            .await
            .expect("recv timed out")
            .expect("recv failed");
        let (_sender, st, data) = recv;

        // Reconstruct expected payload for this tag.
        let size = data.len();
        let seed = size as u64 * 1000 + st.to_byte() as u64;
        let expected = make_positional_bytes(size, seed);

        if data.as_ref() != expected.as_slice() {
            failures += 1;
            let first_diff = data.iter().zip(expected.iter()).position(|(a, b)| a != b);
            eprintln!("CORRUPTION: size={size} stream={st:?} first_diff_at={first_diff:?}");
        }
    }

    for h in send_handles {
        h.await.expect("send task");
    }

    assert_eq!(
        failures, 0,
        "{failures}/{total} payloads corrupted in mixed burst"
    );
    eprintln!("mixed_size_burst: {total} payloads (512B–16KB, PubSub+Bulk) all verified OK");

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}

/// Continuous reassembly soak: sends signed-shaped payloads back-to-back for a
/// fixed duration, verifying every byte. On loopback this establishes the
/// in-order baseline. A failure here would mean the corruption reproduces even
/// without packet reordering — pointing at a systematic bug, not a race.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "long-running soak; run with --run-ignored only"]
async fn continuous_reassembly_soak() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    let (node1, _addr1, node2, _addr2) = common::connected_pair().await;
    let node1 = Arc::new(node1);
    let node2 = Arc::new(node2);
    let dest_peer = node1.peer_id();

    const DURATION_SECS: u64 = 60;
    const BATCH_SIZE: usize = 20;

    let seq = Arc::new(AtomicU64::new(0));
    let total_sent = Arc::new(AtomicU64::new(0));
    let total_recv = Arc::new(AtomicU64::new(0));
    let total_corrupt = Arc::new(AtomicU64::new(0));

    // Receiver: continuously receive and verify.
    let recv_node = Arc::clone(&node1);
    let _recv_seq = Arc::clone(&seq);
    let recv_total = Arc::clone(&total_recv);
    let recv_corrupt = Arc::clone(&total_corrupt);
    let recv_handle = tokio::spawn(async move {
        loop {
            let recv = match timeout(
                Duration::from_secs(10),
                GossipTransport::receive_message(&recv_node),
            )
            .await
            {
                Ok(Ok((_s, _st, data))) => data,
                Ok(Err(_)) => break,
                Err(_) => continue, // timeout — sender may have stopped
            };
            recv_total.fetch_add(1, Ordering::Relaxed);

            // Each payload starts with the v2 version byte (0x02), then a
            // 32-byte agent_id that encodes the sequence number (repeated).
            if recv.len() < 33 {
                recv_corrupt.fetch_add(1, Ordering::Relaxed);
                eprintln!("SOAK: TRUNCATED payload len={}", recv.len());
                continue;
            }
            let seed_bytes = &recv[1..9]; // first 8 bytes of agent_id
            let seed = u64::from_le_bytes(seed_bytes.try_into().unwrap_or([0u8; 8]));
            let expected = make_signed_envelope(seed);
            if recv.as_ref() != expected.as_slice() {
                recv_corrupt.fetch_add(1, Ordering::Relaxed);
                let first_diff = recv.iter().zip(expected.iter()).position(|(a, b)| a != b);
                eprintln!(
                    "SOAK CORRUPTION: seed={seed} first_diff={first_diff:?} recv_len={} expected_len={}",
                    recv.len(), expected.len()
                );
            }
        }
    });

    // Sender: send batches for DURATION_SECS.
    let deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
    while Instant::now() < deadline {
        let mut handles = Vec::new();
        for _ in 0..BATCH_SIZE {
            let s = seq.fetch_add(1, Ordering::Relaxed);
            let payload = make_signed_envelope(s);
            let node2 = Arc::clone(&node2);
            let bytes = Bytes::from(payload);
            total_sent.fetch_add(1, Ordering::Relaxed);
            handles.push(tokio::spawn(async move {
                let _ = GossipTransport::send_to_peer(
                    &node2,
                    dest_peer,
                    GossipStreamType::PubSub,
                    bytes,
                )
                .await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    }

    // Give receiver a moment to drain.
    tokio::time::sleep(Duration::from_secs(2)).await;
    recv_handle.abort();

    let sent = total_sent.load(Ordering::Relaxed);
    let recvd = total_recv.load(Ordering::Relaxed);
    let corrupt = total_corrupt.load(Ordering::Relaxed);

    eprintln!(
        "continuous_reassembly_soak: sent={sent} recv={recvd} corrupt={corrupt} (duration={DURATION_SECS}s)"
    );

    assert_eq!(
        corrupt, 0,
        "{corrupt} payloads corrupted in {DURATION_SECS}s soak (sent={sent} recv={recvd})"
    );

    GossipTransport::close(&node1).await.expect("close node1");
    GossipTransport::close(&node2).await.expect("close node2");
}
