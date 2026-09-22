//! Multi-node pubsub publish/subscribe integration test.
//!
//! Stands up two nodes with real UDP/QUIC transport and a `PlumtreePubSub`
//! per node, runs a transport→pubsub message pump per node, subscribes on
//! the receiver, publishes on the sender, and verifies delivery.
//!
//! This is the first true end-to-end test of the gossip pubsub layer over
//! the real transport (existing inline tests use a mock transport).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_pubsub::{
    BytePolicy, GossipMessage, LeafEgressConfig, LeafEgressSnapshot, PlumtreePubSub, PubSub,
};
use saorsa_gossip_transport::testing::{connected_pair, loopback_star};
use saorsa_gossip_transport::{
    AuthenticatedSession, GossipStreamType, GossipTransport, SessionAdmission, UdpTransportAdapter,
};
use saorsa_gossip_types::{MessageHeader, MessageKind, PeerId, TopicId};
use tokio::sync::Notify;
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn a transport→pubsub message pump. Reads all incoming messages on
/// the transport and dispatches `PubSub` stream-type payloads to
/// `pubsub.handle_message`. Other stream types are ignored (membership and
/// bulk are out of scope for this test).
fn spawn_pubsub_pump<T>(
    transport: Arc<T>,
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

#[derive(Clone, Copy)]
struct HandlerRecord {
    local: PeerId,
    from: PeerId,
    kind: MessageKind,
    msg_id: [u8; 32],
}

#[derive(Default)]
struct HandlerLedger {
    completed: Mutex<Vec<HandlerRecord>>,
    changed: Notify,
}

impl HandlerLedger {
    fn record(&self, record: HandlerRecord) {
        self.completed
            .lock()
            .expect("handler ledger lock")
            .push(record);
        self.changed.notify_waiters();
    }

    async fn wait_for(&self, predicate: impl Fn(&HandlerRecord) -> bool) {
        loop {
            let changed = self.changed.notified();
            if self
                .completed
                .lock()
                .expect("handler ledger lock")
                .iter()
                .any(&predicate)
            {
                return;
            }
            changed.await;
        }
    }
}

fn spawn_traced_pubsub_pump(
    transport: Arc<TracingTransport>,
    pubsub: Arc<PlumtreePubSub<TracingTransport>>,
    handlers: Arc<HandlerLedger>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match GossipTransport::receive_message(&transport).await {
                Ok((sender, GossipStreamType::PubSub, data)) => {
                    let header = postcard::take_from_bytes::<GossipMessage>(&data)
                        .ok()
                        .map(|(message, _)| message.header);
                    if let Err(err) = pubsub.handle_message(sender, data).await {
                        tracing::warn!(
                            target: "pubsub_test::pump",
                            "handle_message returned error: {err}"
                        );
                    } else if let Some(header) = header {
                        handlers.record(HandlerRecord {
                            local: GossipTransport::local_peer_id(&transport),
                            from: sender,
                            kind: header.kind,
                            msg_id: header.msg_id,
                        });
                    }
                }
                Ok((_, _, _)) => {}
                Err(err) => {
                    tracing::debug!(target: "pubsub_test::pump", "transport recv ended: {err}");
                    break;
                }
            }
        }
    })
}

#[derive(Clone, Debug)]
struct FrameRecord {
    sequence: u64,
    from: PeerId,
    to: PeerId,
    kind: MessageKind,
    msg_id: [u8; 32],
    wire_len: usize,
    payload_hash: Option<[u8; 32]>,
    control_ids: Vec<[u8; 32]>,
}

#[derive(Default)]
struct FrameLedger {
    next_sequence: AtomicU64,
    frames: Mutex<Vec<FrameRecord>>,
    changed: Notify,
}

impl FrameLedger {
    fn record(&self, from: PeerId, to: PeerId, stream: GossipStreamType, data: &Bytes) {
        if stream != GossipStreamType::PubSub {
            return;
        }
        let Ok(message) = postcard::from_bytes::<GossipMessage>(data) else {
            return;
        };
        let payload_hash = message
            .payload
            .as_deref()
            .map(|payload| *blake3::hash(payload).as_bytes());
        let control_ids = match message.header.kind {
            MessageKind::IHave | MessageKind::IWant => message
                .payload
                .as_deref()
                .and_then(|payload| postcard::from_bytes(payload).ok())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        self.frames
            .lock()
            .expect("frame ledger lock")
            .push(FrameRecord {
                sequence,
                from,
                to,
                kind: message.header.kind,
                msg_id: message.header.msg_id,
                wire_len: data.len(),
                payload_hash,
                control_ids,
            });
        self.changed.notify_waiters();
    }

    fn find(&self, predicate: impl Fn(&FrameRecord) -> bool) -> Option<FrameRecord> {
        self.frames
            .lock()
            .expect("frame ledger lock")
            .iter()
            .find(|frame| predicate(frame))
            .cloned()
    }

    async fn wait_for(&self, predicate: impl Fn(&FrameRecord) -> bool) -> FrameRecord {
        loop {
            let changed = self.changed.notified();
            if let Some(frame) = self.find(&predicate) {
                return frame;
            }
            changed.await;
        }
    }
}

struct TracingTransport {
    inner: UdpTransportAdapter,
    ledger: Arc<FrameLedger>,
}

impl TracingTransport {
    fn new(inner: UdpTransportAdapter, ledger: Arc<FrameLedger>) -> Arc<Self> {
        Arc::new(Self { inner, ledger })
    }
}

#[async_trait::async_trait]
impl GossipTransport for TracingTransport {
    async fn dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()> {
        GossipTransport::dial(&self.inner, peer, addr).await
    }

    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId> {
        GossipTransport::dial_bootstrap(&self.inner, addr).await
    }

    async fn listen(&self, bind: SocketAddr) -> Result<()> {
        GossipTransport::listen(&self.inner, bind).await
    }

    async fn close(&self) -> Result<()> {
        GossipTransport::close(&self.inner).await
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        GossipTransport::send_to_peer(&self.inner, peer, stream, data.clone()).await?;
        self.ledger.record(
            GossipTransport::local_peer_id(&self.inner),
            peer,
            stream,
            &data,
        );
        Ok(())
    }

    fn authenticated_session(&self, peer: PeerId) -> Option<AuthenticatedSession> {
        GossipTransport::authenticated_session(&self.inner, peer)
    }

    async fn send_to_peer_guarded(
        &self,
        peer: PeerId,
        stream: GossipStreamType,
        admit: SessionAdmission,
    ) -> Result<()> {
        let ledger = Arc::clone(&self.ledger);
        let from = GossipTransport::local_peer_id(&self.inner);
        let traced_admit: SessionAdmission = Arc::new(move |session| {
            let data = admit(session)?;
            ledger.record(from, peer, stream, &data);
            Ok(data)
        });
        GossipTransport::send_to_peer_guarded(&self.inner, peer, stream, traced_admit).await
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        GossipTransport::receive_message(&self.inner).await
    }

    async fn connected_peer_ids(&self) -> Vec<PeerId> {
        GossipTransport::connected_peer_ids(&self.inner).await
    }

    fn local_peer_id(&self) -> PeerId {
        GossipTransport::local_peer_id(&self.inner)
    }
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

/// #504: exercise the real loopback transport while proving that a Leaf relay
/// which defers a Normal EAGER forward retains custody through the exact
/// IHAVE -> IWANT -> cached EAGER recovery chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real loopback QUIC; run only through isolated Linux CI"]
async fn leaf_shed_normal_recovers_exact_message_via_ihave_iwant() {
    timeout(Duration::from_secs(30), async {
        // loopback_star makes the first transport the hub. Assign that hub to
        // B so A and C have no direct connection and recovery must cross B.
        let mut transports = loopback_star(3).await.into_iter();
        let (b_inner, _) = transports.next().expect("relay transport");
        let (a_inner, _) = transports.next().expect("origin transport");
        let (c_inner, _) = transports.next().expect("receiver transport");
        let (a_peer, b_peer, c_peer) = (a_inner.peer_id(), b_inner.peer_id(), c_inner.peer_id());

        let ledger = Arc::new(FrameLedger::default());
        let a_transport = TracingTransport::new(a_inner, Arc::clone(&ledger));
        let b_transport = TracingTransport::new(b_inner, Arc::clone(&ledger));
        let c_transport = TracingTransport::new(c_inner, Arc::clone(&ledger));

        let node_a = Arc::new(PlumtreePubSub::new(
            a_peer,
            Arc::clone(&a_transport),
            MlDsaKeyPair::generate().expect("origin signing key"),
        ));
        let node_b = Arc::new(PlumtreePubSub::new(
            b_peer,
            Arc::clone(&b_transport),
            MlDsaKeyPair::generate().expect("relay signing key"),
        ));
        let node_c = Arc::new(PlumtreePubSub::new(
            c_peer,
            Arc::clone(&c_transport),
            MlDsaKeyPair::generate().expect("receiver signing key"),
        ));

        let topic = TopicId::new([0x50; 32]);
        let mut relay_observer = node_b.subscribe_ready(topic).await;
        let mut receiver = node_c.subscribe_ready(topic).await;
        node_a.initialize_topic_peers(topic, vec![b_peer]).await;
        node_b
            .initialize_topic_peers(topic, vec![a_peer, c_peer])
            .await;
        node_c.initialize_topic_peers(topic, vec![b_peer]).await;

        // Measure one real same-sized signed EAGER frame without entering any
        // pubsub cache. A cached calibration could later be replayed by
        // anti-entropy after the limiter is enabled and become a second Normal
        // B→C data candidate. This direct production-wire frame has no such
        // background custody.
        let calibration = Bytes::from(vec![0x43; 8 * 1024]);
        let calibration_hash = *blake3::hash(&calibration).as_bytes();
        let calibration_key = MlDsaKeyPair::generate().expect("calibration signing key");
        let mut calibration_header = MessageHeader {
            version: 1,
            topic,
            msg_id: MessageHeader::calculate_msg_id(
                &topic,
                0,
                &calibration_key.peer_id(),
                &calibration_hash,
            ),
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
            payload_hash: None,
        };
        calibration_header.seal_payload_hash(Some(&calibration));
        let calibration_header_bytes =
            postcard::to_stdvec(&calibration_header).expect("serialize calibration header");
        let calibration_message = GossipMessage {
            header: calibration_header,
            payload: Some(calibration.clone()),
            signature: calibration_key
                .sign(&calibration_header_bytes)
                .expect("sign calibration header"),
            public_key: calibration_key.public_key().to_vec(),
        };
        let calibration_wire: Bytes = postcard::to_stdvec(&calibration_message)
            .expect("serialize calibration frame")
            .into();
        GossipTransport::send_to_peer(
            a_transport.as_ref(),
            b_peer,
            GossipStreamType::PubSub,
            calibration_wire.clone(),
        )
            .await
            .expect("send calibration frame");
        let (calibration_from, calibration_stream, received_calibration_wire) =
            timeout(RECV_TIMEOUT, GossipTransport::receive_message(&b_transport))
                .await
                .expect("calibration transport receive timed out")
                .expect("calibration transport receive");
        assert_eq!(calibration_from, a_peer);
        assert_eq!(calibration_stream, GossipStreamType::PubSub);
        assert_eq!(received_calibration_wire, calibration_wire);
        let received_calibration: GossipMessage = postcard::from_bytes(&calibration_wire)
            .expect("calibration is a signed pubsub frame");
        assert_eq!(received_calibration.header.version, 2);
        assert_eq!(received_calibration.header.topic, topic);
        assert_eq!(received_calibration.header.kind, MessageKind::Eager);
        assert_eq!(received_calibration.header.hop, 0);
        assert_eq!(received_calibration.header.ttl, 10);
        assert_eq!(received_calibration.payload.as_ref(), Some(&calibration));
        let received_header_bytes =
            postcard::to_stdvec(&received_calibration.header).expect("serialize received header");
        assert!(
            MlDsaKeyPair::verify(
                &received_calibration.public_key,
                &received_header_bytes,
                &received_calibration.signature,
            )
            .expect("verify calibration signature"),
            "calibration must carry a valid production signature"
        );
        let calibration_frame = ledger
            .wait_for(|frame| {
                frame.from == a_peer
                    && frame.to == b_peer
                    && frame.kind == MessageKind::Eager
                    && frame.payload_hash == Some(calibration_hash)
            })
            .await;
        assert_eq!(calibration_frame.wire_len, calibration_wire.len());
        let fixed_burst = u64::try_from(calibration_frame.wire_len).expect("wire length fits u64");
        // Normal data must leave the limiter's recovery reserve intact. A
        // frame equal to the full burst is therefore denied even after a full
        // refill; target deferral does not depend on scheduler timing. The
        // separate recovery path can still send it using natural refill.
        assert!(node_b.configure_leaf_egress(Some(LeafEgressConfig {
            soft_bytes_per_second: 0,
            hard_bytes_per_second: 2 * 1024,
            burst_bytes: fixed_burst,
            max_serialized_frame_bytes: calibration_frame.wire_len,
            policy: BytePolicy::ShedNormal,
        })));
        let eager_accounting = |snapshot: &LeafEgressSnapshot| {
            snapshot
                .by_topic_and_purpose
                .iter()
                .find(|row| row.topic == topic.to_bytes() && row.purpose == "EAGER")
                .map(|row| (row.charged_bytes, row.sent_bytes, row.deferred))
                .unwrap_or_default()
        };
        let before_filler = node_b.leaf_egress_snapshot();
        let eager_before_filler = eager_accounting(&before_filler);
        let handlers = Arc::new(HandlerLedger::default());
        let pumps = [
            spawn_traced_pubsub_pump(
                Arc::clone(&a_transport),
                Arc::clone(&node_a),
                Arc::clone(&handlers),
            ),
            spawn_traced_pubsub_pump(
                Arc::clone(&b_transport),
                Arc::clone(&node_b),
                Arc::clone(&handlers),
            ),
            spawn_traced_pubsub_pump(
                Arc::clone(&c_transport),
                Arc::clone(&node_c),
                Arc::clone(&handlers),
            ),
        ];

        // Establish a completed full-frame relay and recovery baseline.
        // C proves transport delivery; the exact accounting barrier below
        // separately proves B finished its detached send bookkeeping.
        let filler = Bytes::from(vec![0x46; 8 * 1024]);
        let filler_hash = *blake3::hash(&filler).as_bytes();
        node_a
            .publish(topic, filler.clone())
            .await
            .expect("publish filler");
        let (_, relay_filler) = relay_observer
            .recv()
            .await
            .expect("relay observer remains live");
        assert_eq!(relay_filler, filler);
        let (_, received_filler) = receiver.recv().await.expect("receiver remains live");
        assert_eq!(received_filler, filler);
        let filler_forward = ledger
            .wait_for(|frame| {
                frame.from == b_peer
                    && frame.to == c_peer
                    && frame.kind == MessageKind::Eager
                    && frame.payload_hash == Some(filler_hash)
            })
            .await;
        let filler_origin = ledger
            .wait_for(|frame| {
                frame.from == a_peer
                    && frame.to == b_peer
                    && frame.kind == MessageKind::Eager
                    && frame.payload_hash == Some(filler_hash)
            })
            .await;
        handlers
            .wait_for(|handled| {
                handled.local == b_peer
                    && handled.from == a_peer
                    && handled.kind == MessageKind::Eager
                    && handled.msg_id == filler_origin.msg_id
            })
            .await;
        // Purpose-level `deferred` counts budget attempts. A filler that is
        // initially shed can accumulate retries before this successful send;
        // charged/sent bytes are the exact one-frame completion barrier.
        timeout(Duration::from_secs(5), async {
            loop {
                let accounting = eager_accounting(&node_b.leaf_egress_snapshot());
                let expected_charged = eager_before_filler.0 + fixed_burst;
                let expected_sent = eager_before_filler.1 + fixed_burst;
                assert!(
                    accounting.0 <= expected_charged && accounting.1 <= expected_sent,
                    "filler byte accounting exceeded its exact causal target: actual={accounting:?}, expected_charged={expected_charged}, expected_sent={expected_sent}"
                );
                if accounting.0 == expected_charged && accounting.1 == expected_sent {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("filler send bookkeeping did not complete");
        let after_filler = node_b.leaf_egress_snapshot();
        assert_eq!(
            filler_forward.wire_len, calibration_frame.wire_len,
            "the completed filler has one full calibrated EAGER frame"
        );
        let eager_after_filler = eager_accounting(&after_filler);
        assert_eq!(
            eager_after_filler.0,
            eager_before_filler.0 + fixed_burst,
            "exactly the filler EAGER is charged in this topic/purpose lane"
        );
        assert_eq!(
            eager_after_filler.1,
            eager_before_filler.1 + fixed_burst,
            "the charged filler EAGER completed its transport send"
        );
        assert!(
            eager_after_filler.2 >= eager_before_filler.2,
            "EAGER recovery-attempt accounting must be monotonic"
        );
        assert!(
            after_filler.data_deferred >= before_filler.data_deferred
                && after_filler.data_deferred <= before_filler.data_deferred + 1,
            "filler preconditioning may shed at most its one unique Normal data frame: before={}, after={}",
            before_filler.data_deferred,
            after_filler.data_deferred,
        );
        assert!(
            ledger
                .find(|frame| {
                    frame.from == b_peer
                        && frame.to == c_peer
                        && frame.kind == MessageKind::Eager
                        && frame.payload_hash == Some(calibration_hash)
                })
                .is_none(),
            "direct calibration must never become a B→C pubsub forward"
        );

        let target = Bytes::from(vec![0x54; 8 * 1024]);
        let target_hash = *blake3::hash(&target).as_bytes();
        node_a
            .publish(topic, target.clone())
            .await
            .expect("publish target");

        let origin_eager = ledger
            .wait_for(|frame| {
                frame.from == a_peer
                    && frame.to == b_peer
                    && frame.kind == MessageKind::Eager
                    && frame.payload_hash == Some(target_hash)
            })
            .await;
        let target_id = origin_eager.msg_id;

        // B's subscriber proves local admission, but delivery occurs before
        // the forward limiter. The exact B→C IHAVE below is the post-decision
        // barrier for reading the deferral counter.
        let (origin_peer, observed_target) = relay_observer
            .recv()
            .await
            .expect("relay observer remains live");
        assert_eq!(origin_peer, a_peer);
        assert_eq!(observed_target, target);

        let ihave = ledger
            .wait_for(|frame| {
                frame.from == b_peer
                    && frame.to == c_peer
                    && frame.kind == MessageKind::IHave
                    && frame.control_ids.contains(&target_id)
            })
            .await;
        handlers
            .wait_for(|handled| {
                handled.local == b_peer
                    && handled.from == a_peer
                    && handled.kind == MessageKind::Eager
                    && handled.msg_id == target_id
            })
            .await;
        let after_target = node_b.leaf_egress_snapshot();
        assert_eq!(
            after_target.data_deferred,
            after_filler.data_deferred + 1,
            "target EAGER must be shed to lazy recovery"
        );
        // This counter is retry demand, not a unique-frame count: require
        // positive monotonic evidence without constraining scheduler ticks.
        let target_eager_deferred = eager_accounting(&after_target).2;
        assert!(
            target_eager_deferred > eager_after_filler.2,
            "the target must add EAGER budget-deferral attempt evidence"
        );
        let iwant = ledger
            .wait_for(|frame| {
                frame.from == c_peer
                    && frame.to == b_peer
                    && frame.kind == MessageKind::IWant
                    && frame.control_ids == [target_id]
            })
            .await;
        handlers
            .wait_for(|handled| {
                handled.local == c_peer
                    && handled.from == b_peer
                    && handled.kind == MessageKind::IHave
                    && handled.msg_id == ihave.msg_id
            })
            .await;
        let recovered = ledger
            .wait_for(|frame| {
                frame.from == b_peer
                    && frame.to == c_peer
                    && frame.kind == MessageKind::Eager
                    && frame.msg_id == target_id
                    && frame.payload_hash == Some(target_hash)
            })
            .await;
        assert_eq!(
            recovered.wire_len, calibration_frame.wire_len,
            "the recovered EAGER consumes one complete calibrated burst"
        );
        handlers
            .wait_for(|handled| {
                handled.local == b_peer
                    && handled.from == c_peer
                    && handled.kind == MessageKind::IWant
                    && handled.msg_id == iwant.msg_id
            })
            .await;

        let (delivering_peer, delivered) = receiver.recv().await.expect("receiver remains live");
        assert_eq!(delivering_peer, b_peer);
        assert_eq!(delivered, target);
        assert!(
            filler_forward.sequence < origin_eager.sequence
                && origin_eager.sequence < ihave.sequence
                && ihave.sequence < iwant.sequence
                && iwant.sequence < recovered.sequence,
            "ledger must retain the exact causal recovery order"
        );

        // An IWANT handler may enqueue recovery and return before its
        // background send. Delivery also precedes send-outcome bookkeeping.
        // Exact charged/sent bytes prove the second real EAGER completed;
        // deferred attempts can continue increasing until that reservation.
        timeout(Duration::from_secs(5), async {
            loop {
                let accounting = eager_accounting(&node_b.leaf_egress_snapshot());
                let expected_charged = eager_after_filler.0 + fixed_burst;
                let expected_sent = eager_after_filler.1 + fixed_burst;
                assert!(
                    accounting.0 <= expected_charged && accounting.1 <= expected_sent,
                    "recovered EAGER byte accounting exceeded its exact causal target: actual={accounting:?}, expected_charged={expected_charged}, expected_sent={expected_sent}"
                );
                if accounting.0 == expected_charged && accounting.1 == expected_sent {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovered EAGER send bookkeeping did not complete");
        let leaf = node_b.leaf_egress_snapshot();
        assert!(
            eager_accounting(&leaf).2 >= target_eager_deferred,
            "EAGER recovery-attempt accounting must remain monotonic"
        );
        assert_eq!(leaf.data_deferred, after_filler.data_deferred + 1);
        assert_eq!(leaf.invariant_violations, 0);
        assert_eq!(leaf.send_failures, 0);

        for pump in pumps {
            pump.abort();
        }
        let _ = node_a.shutdown().await;
        let _ = node_b.shutdown().await;
        let _ = node_c.shutdown().await;
        a_transport.close().await.expect("close origin transport");
        b_transport.close().await.expect("close relay transport");
        c_transport.close().await.expect("close receiver transport");
    })
    .await
    .expect("Leaf recovery proof exceeded its outer deadline");
}
