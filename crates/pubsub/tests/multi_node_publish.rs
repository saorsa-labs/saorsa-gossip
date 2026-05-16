//! Multi-node pubsub publish/subscribe integration test.
//!
//! Stands up two nodes with real UDP/QUIC transport and a `PlumtreePubSub`
//! per node, runs a transport→pubsub message pump per node, subscribes on
//! the receiver, publishes on the sender, and verifies delivery.
//!
//! This is the first true end-to-end test of the gossip pubsub layer over
//! the real transport (existing inline tests use a mock transport).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_pubsub::{PlumtreePubSub, PubSub};
use saorsa_gossip_transport::testing::connected_pair;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport, UdpTransportAdapter};
use saorsa_gossip_types::{PeerId, TopicId};
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn a transport→pubsub message pump. Reads all incoming messages on
/// the transport and dispatches `PubSub` stream-type payloads to
/// `pubsub.handle_message`. Other stream types are ignored (membership and
/// bulk are out of scope for this test).
fn spawn_pubsub_pump<T>(
    transport: Arc<UdpTransportAdapter>,
    pubsub: Arc<PlumtreePubSub<T>>,
) -> tokio::task::JoinHandle<()>
where
    T: GossipTransport + 'static,
{
    tokio::spawn(async move {
        loop {
            match GossipTransport::receive_message(&transport).await {
                Ok((sender, GossipStreamType::PubSub, data)) => {
                    if let Err(err) = pubsub.handle_message(sender, data).await {
                        tracing::warn!(
                            target: "pubsub_test::pump",
                            "handle_message returned error: {err}"
                        );
                    }
                }
                Ok((_, _, _)) => {
                    // Non-pubsub stream types are ignored in this test.
                }
                Err(err) => {
                    tracing::debug!(target: "pubsub_test::pump", "transport recv ended: {err}");
                    break;
                }
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_publish_subscribe_round_trip() {
    let (node1_t, _addr1, node2_t, _addr2) = connected_pair().await;
    let node1_t = Arc::new(node1_t);
    let node2_t = Arc::new(node2_t);

    let node1_peer: PeerId = node1_t.peer_id();
    let node2_peer: PeerId = node2_t.peer_id();

    let pubsub1 = Arc::new(PlumtreePubSub::new(
        node1_peer,
        Arc::clone(&node1_t),
        MlDsaKeyPair::generate().expect("keypair node1"),
    ));
    let pubsub2 = Arc::new(PlumtreePubSub::new(
        node2_peer,
        Arc::clone(&node2_t),
        MlDsaKeyPair::generate().expect("keypair node2"),
    ));

    let _pump1 = spawn_pubsub_pump(Arc::clone(&node1_t), Arc::clone(&pubsub1));
    let _pump2 = spawn_pubsub_pump(Arc::clone(&node2_t), Arc::clone(&pubsub2));

    let topic = TopicId::new([0xAB; 32]);

    // Receiver subscribes BEFORE publish; eager-fanout on the sender needs
    // node1 in its eager-peer set, so we seed both sides with the other's
    // peer id (publish_local fans out to known eager peers).
    let mut rx = pubsub1.subscribe(topic);
    pubsub1
        .initialize_topic_peers(topic, vec![node2_peer])
        .await;
    pubsub2
        .initialize_topic_peers(topic, vec![node1_peer])
        .await;

    // Give the eager-peer initialisation a moment to propagate state.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = Bytes::from_static(b"hello-multi-node");
    pubsub2
        .publish(topic, payload.clone())
        .await
        .expect("publish on node2");

    let received = timeout(RECV_TIMEOUT, rx.recv())
        .await
        .expect("subscriber recv timed out")
        .expect("subscriber channel closed");

    let (sender, body) = received;
    assert_eq!(sender, node2_peer, "sender peer id mismatch");
    assert_eq!(body, payload, "payload bytes mismatch");
}
