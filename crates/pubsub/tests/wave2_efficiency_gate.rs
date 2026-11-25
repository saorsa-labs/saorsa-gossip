//! Wave 2 (#137) before/after efficiency gate & mixed-fleet interop tests.
//!
//! Compares Wave 1 `0.5.97`-format wire mode (`set_wave2_capabilities_enabled(false)`,
//! verified frame-by-frame with a strict `0.5.97` wire decoder) against Wave 2
//! (`set_wave2_capabilities_enabled(true)` + `PublishV3`) on an identical 6-node
//! multi-topic mesh (1 Critical + 2 Normal + 1 Bulk, 2 KiB payloads), asserting:
//! - Exact per-node delivery set equality (`HashSet<(TopicId, u64)>`, 100%)
//! - `eager_send_causes` (`publish_fanout`, `relay_forward`, `iwant_reply`, `anti_entropy_push`)
//! - `> 10x` reduction in `IHAVE` and total 1-hop control wire bytes
//! - `0` ML-DSA-65 control-frame verifications (`verify_by_kind`)
//! - Non-regressing p50 / p95 / Critical p95 delivery latency
//!
//! Also verifies mixed-version interop (Wave 2 + legacy `0.5.97` peers in the
//! same mesh, plus a `PublishV3` speaker) by intercepting EVERY raw wire frame
//! addressed to every legacy node and decoding it with a verbatim `0.5.97`
//! wire decoder (`MessageKind 0..=8`, `MessageHeader v1/v2` only, no `0xffSGCB\x01`,
//! at most 1 one-shot `Control::Capabilities` probe per Wave-2 peer session).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
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
use serde::Deserialize;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Verbatim `0.5.97` wire decoder (matching tag `4e6d852`) used to audit EVERY
// raw wire frame addressed to legacy peers — detects any Wave 2 leakage
// (unsigned batches, MessageKind >= 9, MessageHeader v3) before delivery.
// ---------------------------------------------------------------------------

const LEGACY_0597_KEY_CACHE_MARKER: &[u8; 6] = b"\xffSGKC\x03";
const WAVE2_CONTROL_BATCH_MARKER: &[u8; 6] = b"\xffSGCB\x01";
const KEY_CACHE_CONTROL_MAGIC: [u8; 8] = *b"SGKEYC01";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum Legacy0597MessageKind {
    Eager = 0,
    IHave = 1,
    IWant = 2,
    Ping = 3,
    Pong = 4,
    AntiEntropy = 5,
    Join = 6,
    Leave = 7,
    Shuffle = 8,
}

#[derive(Debug, Clone)]
struct Legacy0597MessageHeader {
    version: u8,
    topic: TopicId,
    _msg_id: [u8; 32],
    kind: Legacy0597MessageKind,
    _hop: u8,
    _ttl: u8,
    _payload_hash: Option<[u8; 32]>,
}

impl<'de> Deserialize<'de> for Legacy0597MessageHeader {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Legacy0597MessageHeader;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("0.5.97 MessageHeader (v1 or v2 only)")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                macro_rules! field {
                    ($ty:ty) => {
                        seq.next_element::<$ty>()?.ok_or_else(|| {
                            <A::Error as serde::de::Error>::custom("truncated 0.5.97 header")
                        })?
                    };
                }
                let version = field!(u8);
                if version != 1 && version != 2 {
                    return Err(<A::Error as serde::de::Error>::custom(format!(
                        "unsupported 0.5.97 header version {version}"
                    )));
                }
                let topic = field!(TopicId);
                let _msg_id = field!([u8; 32]);
                let kind = field!(Legacy0597MessageKind);
                let _hop = field!(u8);
                let _ttl = field!(u8);
                let _payload_hash = if version == 2 {
                    field!(Option<[u8; 32]>)
                } else {
                    None
                };
                Ok(Legacy0597MessageHeader {
                    version,
                    topic,
                    _msg_id,
                    kind,
                    _hop,
                    _ttl,
                    _payload_hash,
                })
            }
        }
        deserializer.deserialize_tuple(7, Visitor)
    }
}

#[derive(Debug, Deserialize)]
enum Legacy0597KeyMaterial {
    Full {
        _key_id: PeerId,
        _public_key: Vec<u8>,
    },
    Ref {
        _key_id: PeerId,
    },
}

#[derive(Debug, Deserialize)]
struct Legacy0597V3WireMessage {
    header: Legacy0597MessageHeader,
    payload: Option<Bytes>,
    _signature: Vec<u8>,
    _key: Legacy0597KeyMaterial,
}

#[derive(Debug, Deserialize)]
struct Legacy0597GossipMessage {
    header: Legacy0597MessageHeader,
    payload: Option<Bytes>,
    _signature: Vec<u8>,
    _public_key: Vec<u8>,
}

#[derive(Debug, Deserialize)]
enum Legacy0597Control {
    Hello { _key_ref_version: u8 },
    Ack { _key_ids: Vec<PeerId> },
    Request { _key_ids: Vec<PeerId> },
    Response { _entries: Vec<(PeerId, Vec<u8>)> },
}

#[derive(Debug, Deserialize)]
struct Legacy0597ControlPayload {
    magic: [u8; 8],
    version: u8,
    _body: Legacy0597Control,
}

#[derive(Debug, PartialEq, Eq)]
enum Legacy0597FrameAudit {
    /// Fully valid `0.5.97` frame (v1/v2 header, kind 0..=8, exact body).
    Valid0597,
    /// Expected one-shot `Control::Capabilities` probe on the signed key-cache
    /// control topic (ticks `malformed_controls` once on `<= 0.5.97`, no penalty).
    OneShotCapabilitiesProbe,
    /// Protocol leak! Frame would break or be unexpected on a `0.5.97` node.
    Leak(String),
}

fn audit_wire_frame_as_0597(bytes: &[u8]) -> Legacy0597FrameAudit {
    if bytes.starts_with(WAVE2_CONTROL_BATCH_MARKER) {
        return Legacy0597FrameAudit::Leak("unsigned Wave 2 control batch leaked".to_string());
    }
    let (header, payload) = if let Some(body) = bytes.strip_prefix(LEGACY_0597_KEY_CACHE_MARKER) {
        match postcard::take_from_bytes::<Legacy0597V3WireMessage>(body) {
            Ok((msg, [])) => (msg.header, msg.payload),
            Ok((_, trailing)) => {
                return Legacy0597FrameAudit::Leak(format!(
                    "trailing {} bytes in SG76 v3 frame",
                    trailing.len()
                ));
            }
            Err(e) => {
                return Legacy0597FrameAudit::Leak(format!("0.5.97 SG76 v3 decode failed: {e}"));
            }
        }
    } else {
        match postcard::take_from_bytes::<Legacy0597GossipMessage>(bytes) {
            Ok((msg, [])) => (msg.header, msg.payload),
            Ok((_, trailing)) => {
                return Legacy0597FrameAudit::Leak(format!(
                    "trailing {} bytes in legacy frame",
                    trailing.len()
                ));
            }
            Err(e) => {
                return Legacy0597FrameAudit::Leak(format!("0.5.97 frame decode failed: {e}"));
            }
        }
    };

    if header.version != 1 && header.version != 2 {
        return Legacy0597FrameAudit::Leak(format!(
            "unexpected header version {} for 0.5.97 peer",
            header.version
        ));
    }

    let control_topic = TopicId::from_entity("saorsa-gossip/key-cache-control/v1");
    if header.topic == control_topic {
        if header.kind != Legacy0597MessageKind::Ping {
            return Legacy0597FrameAudit::Leak(format!(
                "non-Ping control frame kind {:?}",
                header.kind
            ));
        }
        let Some(payload) = payload else {
            return Legacy0597FrameAudit::Leak("missing control payload".to_string());
        };
        match postcard::take_from_bytes::<Legacy0597ControlPayload>(&payload) {
            Ok((ctrl, trailing))
                if trailing.is_empty()
                    && ctrl.magic == KEY_CACHE_CONTROL_MAGIC
                    && ctrl.version == 1 =>
            {
                return Legacy0597FrameAudit::Valid0597;
            }
            _ => {
                // Check if this is specifically variant 4 (`Control::Capabilities`)
                // immediately after `[magic(8) || version(1)]`.
                if payload.len() >= 10
                    && payload[..8] == KEY_CACHE_CONTROL_MAGIC
                    && payload[8] == 1
                    && payload[9] == 4
                {
                    return Legacy0597FrameAudit::OneShotCapabilitiesProbe;
                }
                return Legacy0597FrameAudit::Leak(
                    "unrecognized key-cache control payload for 0.5.97 peer".to_string(),
                );
            }
        }
    }

    Legacy0597FrameAudit::Valid0597
}

type InboundFrame = (PeerId, GossipStreamType, Bytes, AuthenticatedSession);

#[derive(Default)]
struct WireAuditRecord {
    total_frames: usize,
    valid_0597_frames: usize,
    probes_by_sender: HashMap<PeerId, usize>,
    leaks: Vec<(PeerId, String)>,
}

struct MeshBus {
    inboxes: Mutex<HashMap<PeerId, mpsc::UnboundedSender<InboundFrame>>>,
    audited_targets: Mutex<HashMap<PeerId, WireAuditRecord>>,
}

impl MeshBus {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inboxes: Mutex::new(HashMap::new()),
            audited_targets: Mutex::new(HashMap::new()),
        })
    }

    fn enable_0597_wire_audit(&self, target: PeerId) {
        self.audited_targets
            .lock()
            .unwrap()
            .insert(target, WireAuditRecord::default());
    }

    fn audit_snapshot(
        &self,
        target: PeerId,
    ) -> (usize, usize, HashMap<PeerId, usize>, Vec<String>) {
        let guard = self.audited_targets.lock().unwrap();
        let rec = guard.get(&target).expect("audited target");
        (
            rec.total_frames,
            rec.valid_0597_frames,
            rec.probes_by_sender.clone(),
            rec.leaks
                .iter()
                .map(|(from, why)| format!("from {from}: {why}"))
                .collect(),
        )
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
        if let Some(record) = self.audited_targets.lock().unwrap().get_mut(&to) {
            record.total_frames += 1;
            match audit_wire_frame_as_0597(&data) {
                Legacy0597FrameAudit::Valid0597 => record.valid_0597_frames += 1,
                Legacy0597FrameAudit::OneShotCapabilitiesProbe => {
                    *record.probes_by_sender.entry(from).or_default() += 1;
                }
                Legacy0597FrameAudit::Leak(reason) => {
                    record.leaks.push((from, reason));
                }
            }
        }
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
    per_node_delivered: Vec<HashSet<(TopicId, u64)>>,
    expected_set: HashSet<(TopicId, u64)>,
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
        if !wave2_enabled {
            // Audit every single baseline wire frame with the strict 0.5.97
            // decoder so the "before" run is proven 100% 0.5.97 wire-faithful.
            bus.enable_0597_wire_audit(peer_id);
        }
        let transport = BusTransport::new(peer_id, Arc::clone(&bus));
        let pubsub = Arc::new(PlumtreePubSub::new(peer_id, Arc::clone(&transport), kp));
        pubsub.set_wave2_capabilities_enabled(wave2_enabled);
        // Start in default AcceptOnly while sessions complete Hello + 2-phase
        // Capabilities negotiation during warmup; Wave 2 flips to PublishV3
        // once capability negotiation completes (matching A15 rollout).
        pubsub.set_content_bound_msg_id_policy(ContentBoundMsgIdPolicy::AcceptOnly);
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

    type DeliveryEvent = (usize, TopicId, u64, Instant);
    let peer_ids: Vec<PeerId> = nodes.iter().map(|(_, t)| t.local).collect();
    let delivery_log: Arc<Mutex<Vec<DeliveryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let mut sub_tasks = Vec::new();
    for (idx, (node, _)) in nodes.iter().enumerate() {
        for &(topic, _) in &all_topics {
            let mut rx = node.subscribe(topic);
            let log = Arc::clone(&delivery_log);
            sub_tasks.push(tokio::spawn(async move {
                while let Some((_, payload)) = rx.recv().await {
                    if payload.len() >= 8 {
                        let msg_seq = u64::from_le_bytes(payload[..8].try_into().unwrap());
                        log.lock()
                            .unwrap()
                            .push((idx, topic, msg_seq, Instant::now()));
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

    // Warm up session Hello + 2-phase Capabilities confirmation across ALL pairs.
    // Poll deterministically on `confirmed_wave2_session_count()` when Wave 2 is
    // enabled so slow 2-vCPU CI runners NEVER race the measurement window.
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
    if wave2_enabled {
        let expected_sessions = node_count - 1;
        let warmup_deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < warmup_deadline {
            if nodes
                .iter()
                .all(|(n, _)| n.confirmed_wave2_session_count() == expected_sessions)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            nodes
                .iter()
                .all(|(n, _)| n.confirmed_wave2_session_count() == expected_sessions),
            "all {} nodes must reach {} confirmed Wave 2 sessions before measurement (got {:?})",
            node_count,
            expected_sessions,
            nodes
                .iter()
                .map(|(n, _)| n.confirmed_wave2_session_count())
                .collect::<Vec<_>>()
        );
        // Let any in-flight warmup frames drain before snapshotting start_totals.
        tokio::time::sleep(Duration::from_millis(250)).await;
        for (node, _) in &nodes {
            node.set_content_bound_msg_id_policy(ContentBoundMsgIdPolicy::PublishV3);
        }
    } else {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

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

    let mut published_times: HashMap<(TopicId, u64), Instant> = HashMap::new();
    let mut expected_set: HashSet<(TopicId, u64)> = HashSet::new();
    let mut seq: u64 = 0;
    for round in 0..ROUNDS {
        for &(topic, _) in &all_topics {
            seq += 1;
            let publisher_idx = (round + seq as usize) % node_count;
            let mut body = vec![0x42u8; 2048];
            body[..8].copy_from_slice(&seq.to_le_bytes());
            published_times.insert((topic, seq), Instant::now());
            expected_set.insert((topic, seq));
            nodes[publisher_idx]
                .0
                .publish_local(topic, Bytes::from(body))
                .await
                .expect("publish");
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    // Wait up to 3 s for 100% delivery + one coalesced IHAVE flush window.
    let expected_total = node_count * expected_set.len();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let count = delivery_log.lock().unwrap().len();
        if count >= expected_total {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut all_latencies_ms = Vec::new();
    let mut crit_latencies_ms = Vec::new();
    let mut per_node_delivered: Vec<HashSet<(TopicId, u64)>> = vec![HashSet::new(); node_count];

    for (node_idx, topic, msg_seq, recv_at) in delivery_log.lock().unwrap().iter().copied() {
        if let Some(&sent_at) = published_times.get(&(topic, msg_seq)) {
            per_node_delivered[node_idx].insert((topic, msg_seq));
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

    if !wave2_enabled {
        // Verify that the Wave 1 baseline emitted 100% verbatim 0.5.97 wire frames!
        for &pid in &peer_ids {
            let (total, valid, probes, leaks) = bus.audit_snapshot(pid);
            assert!(total > 0, "baseline peer {pid} must have received frames");
            assert!(
                probes.is_empty(),
                "Wave 1 baseline must emit ZERO Capabilities probes, got {probes:?}"
            );
            assert!(
                leaks.is_empty(),
                "Wave 1 baseline emitted non-0.5.97 wire frames: {leaks:?}"
            );
            assert_eq!(total, valid);
        }
    }

    for (node, _) in &nodes {
        let _ = node.shutdown().await;
    }
    for pump in pumps {
        pump.abort();
    }

    GateMetrics {
        per_node_delivered,
        expected_set,
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
    let keypairs: Vec<MlDsaKeyPair> = (0..6)
        .map(|_| MlDsaKeyPair::generate().expect("keypair"))
        .collect();
    let before = run_mesh_scenario(&keypairs, false).await;
    let after = run_mesh_scenario(&keypairs, true).await;

    let before_delivered: usize = before.per_node_delivered.iter().map(HashSet::len).sum();
    let after_delivered: usize = after.per_node_delivered.iter().map(HashSet::len).sum();
    let expected_total = keypairs.len() * before.expected_set.len();

    eprintln!(
        "\n=== Wave 2 (#137) Before (0.5.97-wire Wave 1 mode) vs After (Wave 2) ===\n\
         Delivery:       {}/{} -> {}/{}\n\
         EAGER causes:   publish_fanout {} -> {}, relay_forward {} -> {}, iwant_reply {} -> {}, ae_push {} -> {}\n\
         IHAVE bytes:    {} B ({} ops) -> {} B ({} ops)  [{:.1}x reduction]\n\
         IWANT bytes:    {} B -> {} B\n\
         Control bytes:  {} B -> {} B  [{:.1}x reduction]\n\
         Ctrl ML-DSA v:  {} -> {}\n\
         Wave 2 signals: wire_prune={} idontwant_sent={} idontwant_suppressed={} unsigned_batches={} v3_accepted={}\n\
         Latency (ms):   p50={}->{} p95={}->{} crit_p95={}->{}\n",
        before_delivered,
        expected_total,
        after_delivered,
        expected_total,
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

    // Exact per-node set equality (not just total count).
    for (idx, set) in before.per_node_delivered.iter().enumerate() {
        assert_eq!(
            set, &before.expected_set,
            "Wave 1 baseline node {idx} missing or unexpected deliveries"
        );
    }
    for (idx, set) in after.per_node_delivered.iter().enumerate() {
        assert_eq!(
            set, &after.expected_set,
            "Wave 2 node {idx} missing or unexpected deliveries"
        );
    }
    assert!(
        before.ihave_bytes > 0 && after.ihave_bytes > 0,
        "both runs must exercise IHAVE control traffic"
    );
    assert!(
        after.ihave_bytes * 10 < before.ihave_bytes,
        "Wave 2 IHAVE wire bytes ({}) must be >10x smaller than Wave 1 baseline ({})",
        after.ihave_bytes,
        before.ihave_bytes
    );
    assert!(
        after.control_bytes_total * 10 < before.control_bytes_total,
        "Wave 2 total control wire bytes ({}) must be >10x smaller than Wave 1 baseline ({})",
        after.control_bytes_total,
        before.control_bytes_total
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
    // Assert latency bounds: p50 and Critical p95 must not regress; overall p95
    // intentionally reflects the 300 ms Normal/Bulk coalesce + 80 ms GRAFT window
    // (+280 ms over 100 ms legacy flush) and must stay tightly bounded.
    assert!(
        after.p50_ms <= before.p50_ms + 75,
        "Wave 2 p50 latency ({} ms) regressed vs baseline ({} ms)",
        after.p50_ms,
        before.p50_ms
    );
    assert!(
        after.p95_ms <= before.p95_ms + 350,
        "Wave 2 p95 latency ({} ms) exceeded coalesce+GRAFT bound vs baseline ({} ms)",
        after.p95_ms,
        before.p95_ms
    );
    assert!(
        after.crit_p95_ms <= before.crit_p95_ms + 100,
        "Wave 2 Critical p95 latency ({} ms) regressed vs baseline ({} ms)",
        after.crit_p95_ms,
        before.crit_p95_ms
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
        if !is_wave2 {
            // Audit EVERY raw wire frame addressed to every legacy 0.5.97 node
            // with our verbatim 0.5.97 wire decoder!
            bus.enable_0597_wire_audit(peer_id);
        }
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
    let wave2_peer_ids: HashSet<PeerId> = nodes
        .iter()
        .filter(|(_, _, is_w2)| *is_w2)
        .map(|(_, t, _)| t.local)
        .collect();
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

    // Warm up Hello + 2-phase Capabilities across mixed mesh (publish from all
    // 3 Wave-2 nodes so every Wave-2 pair completes confirmation deterministically).
    for (i, (node, _, is_w2)) in nodes.iter().enumerate() {
        if *is_w2 {
            let _ = node
                .publish_local(topic, Bytes::from(format!("mixed-warmup-{i}")))
                .await;
        }
    }
    let warmup_deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < warmup_deadline {
        if nodes
            .iter()
            .filter(|(_, _, is_w2)| *is_w2)
            .all(|(n, _, _)| n.confirmed_wave2_session_count() == 2)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        nodes
            .iter()
            .filter(|(_, _, is_w2)| *is_w2)
            .all(|(n, _, _)| n.confirmed_wave2_session_count() == 2),
        "all 3 Wave-2 nodes must confirm capabilities with their 2 Wave-2 peers"
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
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
    let expected_payloads: HashSet<Vec<u8>> = payloads.iter().map(|p| p.to_vec()).collect();
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

    // Bounded poll up to 5 s (25 ms step) so slow 2-vCPU CI runners never flake.
    let mut received_sets: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); NODE_COUNT];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        for (idx, rx) in rxs.iter_mut().enumerate() {
            while let Ok((_, data)) = rx.try_recv() {
                if expected_payloads.contains(data.as_ref()) {
                    received_sets[idx].insert(data.to_vec());
                }
            }
        }
        if received_sets.iter().all(|s| *s == expected_payloads) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    for (idx, set) in received_sets.iter().enumerate() {
        assert_eq!(
            set,
            &expected_payloads,
            "mixed-fleet node {idx} must receive exact expected set of {} messages",
            expected_payloads.len()
        );
    }

    // Also test defense-in-depth per-peer v3 egress gating (Opus Major #1):
    // even if a Wave-2 node switches to `PublishV3` in a mixed fleet, all 3
    // Wave-2 nodes receive the v3 payload while ZERO v3 headers AND ZERO v3
    // IHAVEs leak on the wire to any legacy 0.5.97 peer (neither direct nor
    // via Wave-2 relay forwarding, stranded retry, or deferred replies!).
    let legacy_iwants_before: u64 = nodes
        .iter()
        .filter(|(_, _, is_w2)| !*is_w2)
        .map(|(n, _, _)| {
            n.stage_stats()
                .outbound_by_kind
                .get("iwant")
                .map_or(0, |m| m.msgs)
        })
        .sum();
    let v3_payload = Bytes::from(vec![0xee; 1500]);
    nodes[0]
        .0
        .set_content_bound_msg_id_policy(ContentBoundMsgIdPolicy::PublishV3);
    nodes[0]
        .0
        .publish_local(topic, v3_payload.clone())
        .await
        .expect("v3 pub in mixed fleet");

    let mut v3_received = [false; NODE_COUNT];
    let v3_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < v3_deadline {
        for (idx, rx) in rxs.iter_mut().enumerate() {
            while let Ok((_, data)) = rx.try_recv() {
                if data == v3_payload {
                    v3_received[idx] = true;
                }
            }
        }
        if v3_received[..3].iter().all(|&got| got) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Wait past >=2 legacy 100 ms flush cycles to catch any late v3 IHAVE/retry leak.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (idx, rx) in rxs.iter_mut().enumerate() {
        while let Ok((_, data)) = rx.try_recv() {
            if data == v3_payload {
                v3_received[idx] = true;
            }
        }
    }
    for (idx, &got) in v3_received.iter().enumerate() {
        if idx < 3 {
            assert!(
                got,
                "Wave-2 node {idx} must receive the PublishV3 message in a mixed fleet"
            );
        } else {
            assert!(
                !got,
                "legacy 0.5.97 node {idx} must NOT receive a PublishV3 message"
            );
        }
    }
    let legacy_iwants_after: u64 = nodes
        .iter()
        .filter(|(_, _, is_w2)| !*is_w2)
        .map(|(n, _, _)| {
            n.stage_stats()
                .outbound_by_kind
                .get("iwant")
                .map_or(0, |m| m.msgs)
        })
        .sum();
    assert_eq!(
        legacy_iwants_after, legacy_iwants_before,
        "legacy 0.5.97 nodes must not emit wasted IWANT pulls for filtered v3 IDs"
    );

    // Raw wire-level 0.5.97 decoder audit on EVERY legacy peer!
    for (idx, (node, tr, is_wave2)) in nodes.iter().enumerate() {
        if !is_wave2 {
            let (total, valid, probes, leaks) = bus.audit_snapshot(tr.local);
            assert!(
                total > 0 && valid > 0,
                "legacy node {idx} must have received real 0.5.97 wire frames"
            );
            assert!(
                leaks.is_empty(),
                "legacy node {idx} received Wave 2 wire leaks! {leaks:?}"
            );
            for (sender, count) in &probes {
                assert!(
                    wave2_peer_ids.contains(sender),
                    "only Wave 2 peers may emit Capabilities probe; unexpected sender {sender}"
                );
                assert!(
                    *count <= 1,
                    "legacy node {idx} received {count} Capabilities probes from {sender} (must be <= 1 per session)"
                );
            }
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
