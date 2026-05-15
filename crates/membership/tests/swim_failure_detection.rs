//! SWIM ping/ack roundtrip + failure-detection integration test.
//!
//! Stands up two `HyParViewMembership` instances on top of real
//! UDP/QUIC transports and verifies:
//!
//! 1. Wire roundtrip: a `send_ping` from node1 to node2 results in an
//!    Ack on node1's swim detector, which clears the pending probe and
//!    marks node2 alive.
//! 2. Failure detection: when node2's transport is closed and node1
//!    issues a probe to node2 with no Ack, the SWIM background tasks
//!    promote node2 from `Alive` → `Suspect` → `Dead` within budget.
//!
//! Existing inline tests cover the SWIM state machine in isolation
//! (mock transport). This is the first end-to-end exercise of SWIM
//! over the real wire.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use saorsa_gossip_membership::{HyParViewMembership, MembershipConfig, PeerState};
use saorsa_gossip_transport::testing::connected_pair;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport, UdpTransportAdapter};
use tokio::time::sleep;

/// Spawn a transport→membership message pump that dispatches Membership
/// stream payloads via `HyParViewMembership::dispatch_message`.
fn spawn_membership_pump(
    transport: Arc<UdpTransportAdapter>,
    membership: Arc<HyParViewMembership<UdpTransportAdapter>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match GossipTransport::receive_message(&transport).await {
                Ok((sender, GossipStreamType::Membership, data)) => {
                    if let Err(err) = membership.dispatch_message(sender, &data).await {
                        tracing::warn!(
                            target: "membership_test::pump",
                            "dispatch_message error: {err}"
                        );
                    }
                }
                Ok((_, _, _)) => {
                    // Ignore non-membership stream types in this test.
                }
                Err(err) => {
                    tracing::debug!(target: "membership_test::pump", "transport recv ended: {err}");
                    break;
                }
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swim_ping_ack_roundtrip_over_real_transport() {
    let (node1_t, _addr1, node2_t, _addr2) = connected_pair().await;
    let node1_t = Arc::new(node1_t);
    let node2_t = Arc::new(node2_t);

    let node1_peer = node1_t.peer_id();
    let node2_peer = node2_t.peer_id();

    let m1 = Arc::new(HyParViewMembership::new(
        node1_peer,
        MembershipConfig::default(),
        Arc::clone(&node1_t),
    ));
    let m2 = Arc::new(HyParViewMembership::new(
        node2_peer,
        MembershipConfig::default(),
        Arc::clone(&node2_t),
    ));

    let _pump1 = spawn_membership_pump(Arc::clone(&node1_t), Arc::clone(&m1));
    let _pump2 = spawn_membership_pump(Arc::clone(&node2_t), Arc::clone(&m2));

    // Record a pending probe for node2 on node1's swim detector. After the
    // ping/ack roundtrip, this probe should be cleared.
    m1.swim().record_probe(node2_peer).await;
    assert_eq!(
        m1.swim().get_state(&node2_peer).await,
        None,
        "node2 should be unknown to node1 prior to first ack"
    );

    m1.send_ping(node2_peer).await.expect("send_ping");

    // Wait for node2 to receive the ping, send Ack, and node1 to process it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(
            m1.swim().get_state(&node2_peer).await,
            Some(PeerState::Alive)
        ) {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        m1.swim().get_state(&node2_peer).await,
        Some(PeerState::Alive),
        "node1 should mark node2 as Alive after Ack roundtrip"
    );
    assert!(
        !m1.swim().clear_probe(&node2_peer).await,
        "ack handler should have already cleared the pending probe"
    );

    // Symmetric path: node2's swim detector should also have learned node1
    // is Alive (handle_ping calls mark_alive(sender)).
    assert_eq!(
        m2.swim().get_state(&node1_peer).await,
        Some(PeerState::Alive),
        "node2 should mark node1 as Alive after handling the Ping"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swim_promotes_unresponsive_peer_to_suspect_then_dead() {
    let (node1_t, _addr1, node2_t, _addr2) = connected_pair().await;
    let node1_t = Arc::new(node1_t);
    let node2_t = Arc::new(node2_t);

    let node1_peer = node1_t.peer_id();
    let node2_peer = node2_t.peer_id();

    let m1 = Arc::new(HyParViewMembership::new(
        node1_peer,
        MembershipConfig::default(),
        Arc::clone(&node1_t),
    ));
    let m2 = Arc::new(HyParViewMembership::new(
        node2_peer,
        MembershipConfig::default(),
        Arc::clone(&node2_t),
    ));

    let _pump1 = spawn_membership_pump(Arc::clone(&node1_t), Arc::clone(&m1));
    let pump2 = spawn_membership_pump(Arc::clone(&node2_t), Arc::clone(&m2));

    // Establish node2 as Alive on node1 via a handshake roundtrip.
    m1.swim().mark_alive(node2_peer).await;
    m2.swim().mark_alive(node1_peer).await;
    m1.send_ping(node2_peer).await.expect("initial ping");
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        m1.swim().get_state(&node2_peer).await,
        Some(PeerState::Alive)
    );

    // Simulate node2 failure: abort the pump so node1's pings no longer
    // get dispatched to `handle_ping`. The QUIC connection is still up
    // (saorsa-gossip's `close()` only clears tracking, not the underlying
    // ant-quic Node), but with no dispatcher node2 is functionally dead
    // from SWIM's point of view.
    pump2.abort();

    // Record a probe on node1 — the SWIM background task uses pending
    // probe age to decide Suspect promotion (SWIM_ACK_TIMEOUT_MS=500ms;
    // default suspect_timeout is 3s).
    m1.swim().record_probe(node2_peer).await;
    let _ = m1.send_ping(node2_peer).await;

    // Wait up to 8s (3s suspect + 3s dead promotion + slack) for Dead.
    let mut saw_suspect = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match m1.swim().get_state(&node2_peer).await {
            Some(PeerState::Suspect) => saw_suspect = true,
            Some(PeerState::Dead) => break,
            _ => {}
        }
        sleep(Duration::from_millis(100)).await;
    }

    let final_state = m1.swim().get_state(&node2_peer).await;
    assert!(
        saw_suspect || final_state == Some(PeerState::Dead),
        "expected node1 to observe Suspect at some point or end at Dead, got {:?}",
        final_state
    );
    assert_eq!(
        final_state,
        Some(PeerState::Dead),
        "node1 should promote node2 to Dead within budget"
    );
}
