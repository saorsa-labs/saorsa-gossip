//! `GossipRuntime` end-to-end smoke test.
//!
//! Builds two real `GossipRuntime`s on top of a loopback transport pair,
//! wires the transport→pubsub message pump, then publishes on one and
//! verifies delivery via the other's subscriber. This is the "does the
//! whole thing actually work" assertion that exercises:
//!
//! - `GossipRuntimeBuilder::with_transport(...)` injection path
//! - Full runtime composition (transport + membership + pubsub + …)
//! - The `Arc<RwLock<Box<dyn PubSub>>>` runtime wrapper
//! - Real wire round-trip via the existing `connected_pair` fixture
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_runtime::GossipRuntime;
use saorsa_gossip_runtime::GossipRuntimeBuilder;
use saorsa_gossip_transport::testing::{loopback_from, wait_until_connected};
use saorsa_gossip_transport::{
    GossipStreamType, GossipTransport, UdpTransportAdapter, UdpTransportAdapterConfig,
};
use saorsa_gossip_types::TopicId;
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn a transport→pubsub message pump for the given runtime. Reads
/// every incoming transport message and dispatches `PubSub` stream-type
/// payloads through the runtime's `PubSub::handle_message`. The runtime
/// itself does not start this loop — applications normally do.
fn spawn_pubsub_pump(
    transport: Arc<UdpTransportAdapter>,
    runtime: Arc<GossipRuntime>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match GossipTransport::receive_message(&transport).await {
                Ok((sender, GossipStreamType::PubSub, data)) => {
                    let pubsub = runtime.pubsub.read().await;
                    if let Err(err) = pubsub.handle_message(sender, data).await {
                        tracing::warn!(target: "runtime_test::pump", "handle_message: {err}");
                    }
                }
                Ok((_, _, _)) => {
                    // Other stream types are out of scope for this test.
                }
                Err(err) => {
                    tracing::debug!(target: "runtime_test::pump", "pump exit: {err}");
                    break;
                }
            }
        }
    })
}

/// Build a transport whose internal peer id matches the supplied keypair.
/// We pass the same keypair to the runtime via `.identity(...)` so the
/// runtime's application-level peer id matches the transport's wire-level
/// peer id. Without this, pubsub addresses peers using
/// `runtime.peer_id()` but the transport routing table is keyed on
/// `transport.peer_id()` and sends fail with `Peer not connected`.
async fn transport_with_identity(keypair: &MlDsaKeyPair) -> Arc<UdpTransportAdapter> {
    let any: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let cfg = UdpTransportAdapterConfig::new(any, vec![])
        .with_keypair(keypair.public_key().to_vec(), keypair.secret_key().to_vec());
    Arc::new(
        UdpTransportAdapter::with_config(cfg, None)
            .await
            .expect("transport startup"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_runtimes_publish_subscribe_round_trip() {
    let kp1 = MlDsaKeyPair::generate().expect("kp1");
    let kp2 = MlDsaKeyPair::generate().expect("kp2");

    // Bring up node1 first so we know its bound address; node2 bootstraps
    // by adding node1's loopback addr to its config's known_peers.
    let t1 = transport_with_identity(&kp1).await;
    let addr1 = loopback_from(t1.node().local_addr().expect("node1 local_addr"));

    let any: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let cfg2 = UdpTransportAdapterConfig::new(any, vec![addr1])
        .with_keypair(kp2.public_key().to_vec(), kp2.secret_key().to_vec());
    let t2 = Arc::new(
        UdpTransportAdapter::with_config(cfg2, None)
            .await
            .expect("transport2 startup"),
    );

    wait_until_connected(&t1, t2.peer_id(), Duration::from_secs(5)).await;
    wait_until_connected(&t2, t1.peer_id(), Duration::from_secs(5)).await;

    let rt1 = Arc::new(
        GossipRuntimeBuilder::new()
            .identity(kp1.clone())
            .with_transport(Arc::clone(&t1))
            .build()
            .await
            .expect("build runtime 1"),
    );
    let rt2 = Arc::new(
        GossipRuntimeBuilder::new()
            .identity(kp2.clone())
            .with_transport(Arc::clone(&t2))
            .build()
            .await
            .expect("build runtime 2"),
    );

    let peer1 = rt1.peer_id();
    let peer2 = rt2.peer_id();
    assert_eq!(peer1, t1.peer_id(), "rt1 identity must match transport");
    assert_eq!(peer2, t2.peer_id(), "rt2 identity must match transport");
    assert_ne!(peer1, peer2, "two runtimes must have distinct peer ids");

    let _pump1 = spawn_pubsub_pump(Arc::clone(&t1), Arc::clone(&rt1));
    let _pump2 = spawn_pubsub_pump(Arc::clone(&t2), Arc::clone(&rt2));

    let topic = TopicId::new([0xCD; 32]);

    // Subscribe on rt1 BEFORE seeding eager peers on rt2. Use the runtime
    // readiness barrier so the first publish cannot race ahead of local
    // subscriber registration.
    let mut rx = rt1.subscribe_ready(topic).await;
    {
        let pubsub = rt1.pubsub.read().await;
        pubsub.initialize_topic_peers(topic, vec![peer2]).await;
    }
    {
        let pubsub = rt2.pubsub.read().await;
        pubsub.initialize_topic_peers(topic, vec![peer1]).await;
    }

    let payload = Bytes::from_static(b"runtime-end-to-end-roundtrip");
    {
        let pubsub = rt2.pubsub.read().await;
        pubsub
            .publish(topic, payload.clone())
            .await
            .expect("publish on rt2");
    }

    let received = timeout(RECV_TIMEOUT, rx.recv())
        .await
        .expect("subscriber recv timed out")
        .expect("subscriber channel closed");

    let (sender, body) = received;
    assert_eq!(sender, peer2, "sender peer id mismatch");
    assert_eq!(body, payload, "payload bytes mismatch");
}
