//! Wave 2 (#137) before/after efficiency gate & mixed-fleet interop tests.
//!
//! Compares the `0.5.97` Wave 1 baseline (`set_wave2_capabilities_enabled(false)`)
//! against Wave 2 (`set_wave2_capabilities_enabled(true)` + `PublishV3`) on an
//! identical 8-node multi-topic mesh (1 Critical + 3 Normal + 1 Bulk, >= 1 KiB
//! payloads), measuring:
//! - `eager_send_causes` (`publish_fanout`, `relay_forward`, `iwant_reply`, `anti_entropy_push`)
//! - `IHAVE` & total 1-hop control wire bytes
//! - ML-DSA-65 control-frame verifications (`verify_by_kind`)
//! - Delivery rate and p50 / p95 delivery latency (overall + Critical)
//!
//! Also verifies mixed-version interop (Wave 2 + legacy `0.5.97` peers in the
//! same mesh) with 100% delivery and zero Wave 2 wire leakage to `0.5.97` peers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_pubsub::{ContentBoundMsgIdPolicy, PlumtreePubSub, PubSub};
use saorsa_gossip_transport::{
    AuthenticatedSession, GossipStreamType, GossipTransport, SessionAdmission,
};
use saorsa_gossip_types::{MessageKind, PeerId, TopicId, TopicPriority};
use tokio::sync::mpsc;

type InboundFrame = (PeerId, GossipStreamType, Bytes, AuthenticatedSession);

struct MeshBus {
    inboxes: Mutex<HashMap<PeerId, mpsc::UnboundedSender<InboundFrame>>>,
}

impl MeshBus {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inboxes: Mutex::new(HashMap::new()),
        })
    }

    fn register(&self, peer: PeerId) -> mpsc::UnboundedReceiver<InboundFrame> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inboxes.lock().unwrap().insert(peer, tx);
        rx
    }

    fn peers(&self, exclude: PeerId) -> Vec<PeerId> {
        self.inboxes
            .lock()
            .unwrap()
            .keys()
            .copied()
            .filter(|&p| p != exclude)
            .collect()
    }

    fn deliver(
        &self,
        from: PeerId,
        to: PeerId,
        stream: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        let tx = self
            .inboxes
            .lock()
            .unwrap()
            .get(&to)
            .cloned()
            .ok_or_else(|| anyhow!("peer {to} not registered"))?;
        let session = AuthenticatedSession {
            peer: from,
            generation: 1,
        };
        tx.send((from, stream, data, session))
            .map_err(|_| anyhow!("peer {to} inbox closed"))
    }
}

struct BusTransport {
    local: PeerId,
    bus: Arc<MeshBus>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<InboundFrame>>,
}

impl BusTransport {
    fn new(local: PeerId, bus: Arc<MeshBus>) -> Arc<Self> {
        let rx = bus.register(local);
        Arc::new(Self {
            local,
            bus,
            rx: tokio::sync::Mutex::new(rx),
        })
    }
}

#[async_trait::async_trait]
impl GossipTransport for BusTransport {
    async fn dial(&self, _peer: PeerId, _addr: SocketAddr) -> Result<()> {
        Ok(())
    }

    async fn dial_bootstrap(&self, _addr: SocketAddr) -> Result<PeerId> {
        Err(anyhow!("unused in in-memory mesh"))
    }

    async fn listen(&self, _addr: SocketAddr) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        self.bus.deliver(self.local, peer, stream_type, data)
    }

    async fn send_to_peer_guarded(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        admit: SessionAdmission,
    ) -> Result<()> {
        let session = self
            .authenticated_session(peer)
            .ok_or_else(|| anyhow!("peer {peer} not connected"))?;
        let data = admit(session)?;
        self.bus.deliver(self.local, peer, stream_type, data)
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        let (from, stream, data, _) = self.receive_message_with_session().await?;
        Ok((from, stream, data))
    }

    async fn receive_message_with_session(
        &self,
    ) -> Result<(
        PeerId,
        GossipStreamType,
        Bytes,
        Option<AuthenticatedSession>,
    )> {
        let mut rx = self.rx.lock().await;
        let (from, stream, data, session) = rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("bus channel closed"))?;
        Ok((from, stream, data, Some(session)))
    }

    async fn connected_peer_ids(&self) -> Vec<PeerId> {
        self.bus.peers(self.local)
    }

    fn local_peer_id(&self) -> PeerId {
        self.local
    }

    fn authenticated_session(&self, peer: PeerId) -> Option<AuthenticatedSession> {
        self.bus
            .inboxes
            .lock()
            .unwrap()
            .contains_key(&peer)
            .then_some(AuthenticatedSession {
                peer,
                generation: 1,
            })
    }
}

struct GateMetrics {
    delivered: usize,
    expected: usize,
    publish_fanout: u64,
    relay_forward: u64,
    iwant_reply: u64,
    anti_entropy_push: u64,
    ihave_bytes: u64,
    ihave_msgs: u64,
    iwant_bytes: u64,
    control_bytes_total: u64,
    control_verifies: u64,
    wire_prune_sent: u64,
    idontwant_sent: u64,
    idontwant_suppressed: u64,
    unsigned_batches_sent: u64,
    v3_accepted: u64,
    p50_ms: u64,
    p95_ms: u64,
    crit_p95_ms: u64,
}

fn percentile_ms(samples: &mut [u64], pct: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() - 1) * pct) / 100;
    samples[idx]
}

async fn run_mesh_scenario(keypairs: &[MlDsaKeyPair], wave2_enabled: bool) -> GateMetrics {
    let node_count = keypairs.len();
    const ROUNDS: usize = 8;
    let bus = MeshBus::new();

    let crit_topic = TopicId::new([0x10; 32]);
    let norm_topics = [TopicId::new([0x21; 32]), TopicId::new([0x22; 32])];
    let bulk_topic = TopicId::new([0x30; 32]);
    let all_topics = [
        (crit_topic, TopicPriority::Critical),
        (norm_topics[0], TopicPriority::Normal),
        (norm_topics[1], TopicPriority::Normal),
        (bulk_topic, TopicPriority::Bulk),
    ];

    let mut nodes: Vec<(Arc<PlumtreePubSub<BusTransport>>, Arc<BusTransport>)> = Vec::new();
    let mut pumps = Vec::new();
    for kp in keypairs.iter().cloned() {
        let peer_id = kp.peer_id();
        let transport = BusTransport::new(peer_id, Arc::clone(&bus));
        let pubsub = Arc::new(PlumtreePubSub::new(peer_id, Arc::clone(&transport), kp));
        pubsub.set_wave2_capabilities_enabled(wave2_enabled);
        if wave2_enabled {
            pubsub.set_content_bound_msg_id_policy(ContentBoundMsgIdPolicy::PublishV3);
        } else {
            pubsub.set_content_bound_msg_id_policy(ContentBoundMsgIdPolicy::AcceptOnly);
        }
        for &(t, prio) in &all_topics {
            pubsub.admission().registry().register(t, prio);
        }
        let pump_t = Arc::clone(&transport);
        let pump_p = Arc::clone(&pubsub);
        pumps.push(tokio::spawn(async move {
            loop {
                match pump_t.receive_message_with_session().await {
                    Ok((from, GossipStreamType::PubSub, data, Some(session)))
                        if session.peer == from =>
                    {
                        let _ = pump_p.handle_authenticated_message(session, data).await;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }));
        nodes.push((pubsub, transport));
    }

    // Cap eager degree to 3 per topic so every node has both eager peers
    // (for EAGER/PRUNE/IDONTWANT) AND lazy peers (for IHAVE/GRAFT/coalescing).
    for (node, _) in &nodes {
        node.set_eager_degree_ceiling(3).await;
    }

    // Wire mesh: 3 eager peers + 2 lazy peers per node per topic.
    let peer_ids: Vec<PeerId> = nodes.iter().map(|(_, t)| t.local).collect();
    let delivery_log: Arc<Mutex<Vec<(TopicId, u64, Instant)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut sub_tasks = Vec::new();
    for (idx, (node, _)) in nodes.iter().enumerate() {
        for &(topic, _) in &all_topics {
            let mut rx = node.subscribe(topic);
            let log = Arc::clone(&delivery_log);
            sub_tasks.push(tokio::spawn(async move {
                while let Some((_, payload)) = rx.recv().await {
                    if payload.len() >= 8 {
                        let msg_seq = u64::from_le_bytes(payload[..8].try_into().unwrap());
                        log.lock().unwrap().push((topic, msg_seq, Instant::now()));
                    }
                }
            }));
            let others: Vec<PeerId> = peer_ids
                .iter()
                .enumerate()
                .filter_map(|(j, &p)| (j != idx).then_some(p))
                .collect();
            node.initialize_topic_peers(topic, others).await;
        }
    }

    // Warm up session Hello + Wave 2 Capabilities exchange across ALL pairs
    // (~900 ms covers two 250 ms key-cache flusher ticks cleanly).
    let warmup_topic = TopicId::new([0x99; 32]);
    for (idx, (node, _)) in nodes.iter().enumerate() {
        let others: Vec<PeerId> = peer_ids
            .iter()
            .enumerate()
            .filter_map(|(j, &p)| (j != idx).then_some(p))
            .collect();
        node.initialize_topic_peers(warmup_topic, others).await;
    }
    for (i, (node, _)) in nodes.iter().enumerate() {
        let _ = node
            .publish_local(warmup_topic, Bytes::from(format!("warmup-{i}")))
            .await;
    }
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    // Snapshot steady-state counters AFTER Hello + Capabilities negotiation so
    // the before/after comparison measures the real steady-state window.
    let snapshot_totals = |nodes: &[(Arc<PlumtreePubSub<BusTransport>>, Arc<BusTransport>)]| {
        let mut publish_fanout = 0u64;
        let mut relay_forward = 0u64;
        let mut iwant_reply = 0u64;
        let mut anti_entropy_push = 0u64;
        let mut ihave_bytes = 0u64;
        let mut ihave_msgs = 0u64;
        let mut iwant_bytes = 0u64;
        let mut control_bytes_total = 0u64;
        let mut control_verifies = 0u64;
        let mut wire_prune_sent = 0u64;
        let mut idontwant_sent = 0u64;
        let mut idontwant_suppressed = 0u64;
        let mut unsigned_batches_sent = 0u64;
        let mut v3_accepted = 0u64;
        for (node, _) in nodes {
            let stats = node.stage_stats();
            publish_fanout += stats.eager_send_causes.publish_fanout;
            relay_forward += stats.eager_send_causes.relay_forward;
            iwant_reply += stats.eager_send_causes.iwant_reply;
            anti_entropy_push += stats.eager_send_causes.anti_entropy_push;
            if let Some(m) = stats.outbound_by_kind.get("ihave") {
                ihave_bytes += m.bytes;
                ihave_msgs += m.msgs;
                control_bytes_total += m.bytes;
            }
            for k in ["iwant", "anti_entropy", "prune", "graft", "idontwant"] {
                if let Some(m) = stats.outbound_by_kind.get(k) {
                    if k == "iwant" {
                        iwant_bytes += m.bytes;
                    }
                    control_bytes_total += m.bytes;
                }
            }
            let v = node.verify_by_kind_stats();
            for kind in [
                MessageKind::IHave,
                MessageKind::IWant,
                MessageKind::AntiEntropy,
                MessageKind::Prune,
                MessageKind::Graft,
                MessageKind::IDontWant,
            ] {
                control_verifies += v.for_kind(kind).attempts;
            }
            let w2 = node.wave2_stats();
            wire_prune_sent += w2.wire_prune_sent;
            idontwant_sent += w2.idontwant_sent;
            idontwant_suppressed += w2.idontwant_eager_suppressed;
            unsigned_batches_sent += w2.unsigned_control_batches_sent;
            v3_accepted += w2.v3_content_bound_accepted;
        }
        (
            publish_fanout,
            relay_forward,
            iwant_reply,
            anti_entropy_push,
            ihave_bytes,
            ihave_msgs,
            iwant_bytes,
            control_bytes_total,
            control_verifies,
            wire_prune_sent,
            idontwant_sent,
            idontwant_suppressed,
            unsigned_batches_sent,
            v3_accepted,
        )
    };
    let start_totals = snapshot_totals(&nodes);

    // Publish realistic >= 1 KiB payloads across all 4 topics and measure delivery latencies.
    let mut published_times: HashMap<(TopicId, u64), Instant> = HashMap::new();
    let mut seq: u64 = 0;
    for round in 0..ROUNDS {
        for &(topic, _) in &all_topics {
            seq += 1;
            let publisher_idx = (round + seq as usize) % node_count;
            let mut body = vec![0x42u8; 2048];
            body[..8].copy_from_slice(&seq.to_le_bytes());
            published_times.insert((topic, seq), Instant::now());
            nodes[publisher_idx]
                .0
                .publish_local(topic, Bytes::from(body))
                .await
                .expect("publish");
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    // Allow coalesced IHAVE / GRAFT repair to finish cleanly.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let mut all_latencies_ms = Vec::new();
    let mut crit_latencies_ms = Vec::new();
    let mut delivered = 0usize;
    let expected = node_count * ROUNDS * all_topics.len();

    for (topic, msg_seq, recv_at) in delivery_log.lock().unwrap().iter().copied() {
        if let Some(&sent_at) = published_times.get(&(topic, msg_seq)) {
            delivered += 1;
            let lat = recv_at.saturating_duration_since(sent_at).as_millis() as u64;
            all_latencies_ms.push(lat);
            if topic == crit_topic {
                crit_latencies_ms.push(lat);
            }
        }
    }
    for task in sub_tasks {
        task.abort();
    }

    let end_totals = snapshot_totals(&nodes);
    let publish_fanout = end_totals.0.saturating_sub(start_totals.0);
    let relay_forward = end_totals.1.saturating_sub(start_totals.1);
    let iwant_reply = end_totals.2.saturating_sub(start_totals.2);
    let anti_entropy_push = end_totals.3.saturating_sub(start_totals.3);
    let ihave_bytes = end_totals.4.saturating_sub(start_totals.4);
    let ihave_msgs = end_totals.5.saturating_sub(start_totals.5);
    let iwant_bytes = end_totals.6.saturating_sub(start_totals.6);
    let control_bytes_total = end_totals.7.saturating_sub(start_totals.7);
    let control_verifies = end_totals.8.saturating_sub(start_totals.8);
    let wire_prune_sent = end_totals.9.saturating_sub(start_totals.9);
    let idontwant_sent = end_totals.10.saturating_sub(start_totals.10);
    let idontwant_suppressed = end_totals.11.saturating_sub(start_totals.11);
    let unsigned_batches_sent = end_totals.12.saturating_sub(start_totals.12);
    let v3_accepted = end_totals.13.saturating_sub(start_totals.13);

    for (node, _) in &nodes {
        let _ = node.shutdown().await;
    }
    for pump in pumps {
        pump.abort();
    }

    GateMetrics {
        delivered,
        expected,
        publish_fanout,
        relay_forward,
        iwant_reply,
        anti_entropy_push,
        ihave_bytes,
        ihave_msgs,
        iwant_bytes,
        control_bytes_total,
        control_verifies,
        wire_prune_sent,
        idontwant_sent,
        idontwant_suppressed,
        unsigned_batches_sent,
        v3_accepted,
        p50_ms: percentile_ms(&mut all_latencies_ms, 50),
        p95_ms: percentile_ms(&mut all_latencies_ms, 95),
        crit_p95_ms: percentile_ms(&mut crit_latencies_ms, 95),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wave2_before_after_efficiency_gate_against_wave1_0597_baseline() {
    // Use identical peer identities & initial mesh topology for both runs so
    // the before/after comparison has zero random-graph variance.
    let keypairs: Vec<MlDsaKeyPair> = (0..6)
        .map(|_| MlDsaKeyPair::generate().expect("keypair"))
        .collect();
    let before = run_mesh_scenario(&keypairs, false).await;
    let after = run_mesh_scenario(&keypairs, true).await;

    eprintln!(
        "\n=== Wave 2 (#137) Before (0.5.97 Wave 1) vs After (Wave 2) ===\n\
         Delivery:       {}/{} -> {}/{}\n\
         EAGER causes:   publish_fanout {} -> {}, relay_forward {} -> {}, iwant_reply {} -> {}, ae_push {} -> {}\n\
         IHAVE bytes:    {} B ({} ops) -> {} B ({} ops)  [{:.1}x reduction]\n\
         IWANT bytes:    {} B -> {} B\n\
         Control bytes:  {} B -> {} B  [{:.1}x reduction]\n\
         Ctrl ML-DSA v:  {} -> {}\n\
         Wave 2 signals: wire_prune={} idontwant_sent={} idontwant_suppressed={} unsigned_batches={} v3_accepted={}\n\
         Latency (ms):   p50={}->{} p95={}->{} crit_p95={}->{}\n",
        before.delivered,
        before.expected,
        after.delivered,
        after.expected,
        before.publish_fanout,
        after.publish_fanout,
        before.relay_forward,
        after.relay_forward,
        before.iwant_reply,
        after.iwant_reply,
        before.anti_entropy_push,
        after.anti_entropy_push,
        before.ihave_bytes,
        before.ihave_msgs,
        after.ihave_bytes,
        after.ihave_msgs,
        before.ihave_bytes as f64 / after.ihave_bytes.max(1) as f64,
        before.iwant_bytes,
        after.iwant_bytes,
        before.control_bytes_total,
        after.control_bytes_total,
        before.control_bytes_total as f64 / after.control_bytes_total.max(1) as f64,
        before.control_verifies,
        after.control_verifies,
        after.wire_prune_sent,
        after.idontwant_sent,
        after.idontwant_suppressed,
        after.unsigned_batches_sent,
        after.v3_accepted,
        before.p50_ms,
        after.p50_ms,
        before.p95_ms,
        after.p95_ms,
        before.crit_p95_ms,
        after.crit_p95_ms,
    );

    assert_eq!(
        before.delivered, before.expected,
        "0.5.97 Wave 1 baseline must achieve 100% delivery"
    );
    assert_eq!(
        after.delivered, after.expected,
        "Wave 2 must maintain 100% delivery"
    );
    assert!(
        before.ihave_bytes > 0 && after.ihave_bytes > 0,
        "both runs must exercise IHAVE control traffic"
    );
    assert!(
        after.ihave_bytes * 10 < before.ihave_bytes,
        "Wave 2 IHAVE wire bytes ({}) must be >10x smaller than 0.5.97 baseline ({})",
        after.ihave_bytes,
        before.ihave_bytes
    );
    assert_eq!(
        after.control_verifies, 0,
        "Wave 2 must perform ZERO per-frame ML-DSA-65 verifications on 1-hop control frames (baseline did {})",
        before.control_verifies
    );
    assert!(
        after.relay_forward + after.iwant_reply < before.relay_forward + before.iwant_reply,
        "Wave 2 PRUNE + GRAFT + IDONTWANT must reduce non-origin EAGER sends ({} + {} vs {} + {})",
        after.relay_forward,
        after.iwant_reply,
        before.relay_forward,
        before.iwant_reply
    );
    assert!(
        after.unsigned_batches_sent > 0,
        "Wave 2 must emit unsigned coalesced control batches"
    );
    assert!(
        after.v3_accepted > 0,
        "Wave 2 must accept v3 content-bound messages"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wave2_mixed_fleet_interop_with_legacy_0597_peers_delivers_100_percent() {
    const NODE_COUNT: usize = 6; // 3 Wave 2 + 3 legacy 0.5.97
    let bus = MeshBus::new();
    let topic = TopicId::new([0x88; 32]);

    let mut nodes: Vec<(Arc<PlumtreePubSub<BusTransport>>, Arc<BusTransport>, bool)> = Vec::new();
    let mut pumps = Vec::new();
    for idx in 0..NODE_COUNT {
        let is_wave2 = idx < 3;
        let kp = MlDsaKeyPair::generate().expect("keypair");
        let peer_id = kp.peer_id();
        let transport = BusTransport::new(peer_id, Arc::clone(&bus));
        let pubsub = Arc::new(PlumtreePubSub::new(peer_id, Arc::clone(&transport), kp));
        pubsub.set_wave2_capabilities_enabled(is_wave2);
        // Mixed fleet keeps default AcceptOnly for multi-hop v3 safety.
        assert_eq!(
            pubsub.content_bound_msg_id_policy(),
            ContentBoundMsgIdPolicy::AcceptOnly
        );
        let pump_t = Arc::clone(&transport);
        let pump_p = Arc::clone(&pubsub);
        pumps.push(tokio::spawn(async move {
            loop {
                match pump_t.receive_message_with_session().await {
                    Ok((from, GossipStreamType::PubSub, data, Some(session)))
                        if session.peer == from =>
                    {
                        let _ = pump_p.handle_authenticated_message(session, data).await;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }));
        nodes.push((pubsub, transport, is_wave2));
    }

    let peer_ids: Vec<PeerId> = nodes.iter().map(|(_, t, _)| t.local).collect();
    let mut rxs = Vec::new();
    for (idx, (node, _, _)) in nodes.iter().enumerate() {
        rxs.push(node.subscribe(topic));
        let others: Vec<PeerId> = peer_ids
            .iter()
            .enumerate()
            .filter_map(|(j, &p)| (j != idx).then_some(p))
            .collect();
        node.initialize_topic_peers(topic, others).await;
    }

    // Warm up Hello + Capabilities across mixed mesh.
    let _ = nodes[0]
        .0
        .publish_local(topic, Bytes::from_static(b"mixed-warmup"))
        .await;
    tokio::time::sleep(Duration::from_millis(650)).await;
    for rx in &mut rxs {
        while rx.try_recv().is_ok() {}
    }

    // Publish from both Wave 2 and legacy 0.5.97 nodes.
    let payloads = [
        Bytes::from(vec![0xaa; 1500]),
        Bytes::from(vec![0xbb; 1500]),
        Bytes::from(vec![0xcc; 1500]),
        Bytes::from(vec![0xdd; 1500]),
    ];
    nodes[0]
        .0
        .publish_local(topic, payloads[0].clone())
        .await
        .expect("w2 pub 1");
    nodes[3]
        .0
        .publish_local(topic, payloads[1].clone())
        .await
        .expect("legacy pub 1");
    nodes[1]
        .0
        .publish_local(topic, payloads[2].clone())
        .await
        .expect("w2 pub 2");
    nodes[5]
        .0
        .publish_local(topic, payloads[3].clone())
        .await
        .expect("legacy pub 2");

    tokio::time::sleep(Duration::from_millis(600)).await;

    for (idx, rx) in rxs.iter_mut().enumerate() {
        let mut received = Vec::new();
        while let Ok((_, data)) = rx.try_recv() {
            received.push(data);
        }
        assert_eq!(
            received.len(),
            payloads.len(),
            "mixed-fleet node {idx} must receive all {} messages",
            payloads.len()
        );
    }

    // Legacy 0.5.97 peers must have zero unsigned control batches received.
    for (idx, (node, _, is_wave2)) in nodes.iter().enumerate() {
        if !is_wave2 {
            assert_eq!(
                node.wave2_stats().unsigned_control_batches_received,
                0,
                "legacy node {idx} must never receive unsigned control batches"
            );
            assert_eq!(
                node.wave2_stats().wire_prune_received,
                0,
                "legacy node {idx} must never receive wire PRUNE"
            );
        }
        let _ = node.shutdown().await;
    }
    for pump in pumps {
        pump.abort();
    }
}
