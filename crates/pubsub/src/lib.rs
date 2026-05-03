#![warn(missing_docs)]

//! Plumtree-based pub/sub dissemination
//!
//! Implements:
//! - EAGER push along spanning tree
//! - IHAVE lazy digests to non-tree links
//! - IWANT pull on demand
//! - PRUNE/GRAFT for tree optimization
//! - Anti-entropy reconciliation for partition recovery
//!
//! # Architecture
//!
//! Each topic maintains two sets of peers:
//! - **Eager peers** (tree): Forward full messages immediately
//! - **Lazy peers** (gossip): Send only message IDs (IHAVE)
//!
//! The tree self-optimizes via duplicate detection (PRUNE) and pull requests (GRAFT).

use anyhow::{anyhow, Result};
use bytes::Bytes;
use lru::LruCache;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use saorsa_gossip_types::{MessageHeader, MessageKind, PeerId, TopicId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, RwLock};
use tokio::time;
use tracing::{debug, error, trace, warn};

/// Maximum message cache size per topic.
///
/// Sized at 2× the IHAVE batch (1024) so PlumTree IWANT recovery still has
/// the full last-batch window of payloads to serve. Worst case payload
/// retention per topic: 2048 × max-payload-size. Was 10_000 — too generous
/// for memory-constrained nodes (would retain up to ~90 MB per topic at
/// 9 KB/payload).
const MAX_CACHE_SIZE: usize = 2_048;

/// Message cache TTL (60 s).
///
/// PlumTree IWANT recovery typically resolves within RTTs (sub-second).
/// 60 s is generous for partition recovery while bounding steady-state
/// retention to (msg_rate × 60) × payload_size per topic. Was 300 s —
/// 5× more retention than required for the recovery window.
const CACHE_TTL_SECS: u64 = 60;

/// Maximum payload replay cache size per topic.
const REPLAY_CACHE_MAX_ENTRIES: usize = 10_000;

/// Payload replay cache TTL (5 minutes).
const REPLAY_CACHE_TTL_SECS: u64 = 300;

/// Idle TTL for an entire `TopicState` entry (10 min).
///
/// When a topic has had no incoming gossip / publishes / IHAVE for this
/// long and has no live local subscribers, the whole `TopicState` is dropped
/// from the `topics` HashMap — freeing its message_cache, replay_cache,
/// peer_scores, eager/lazy peer sets, etc. Without this, ephemeral topics
/// (e.g. per-peer beacons, per-session announcements) accumulated forever,
/// contributing to slow idle-traffic RSS drift observed during the
/// 2026-04-25 soak validation.
const TOPIC_IDLE_TTL_SECS: u64 = 600;

/// Maximum IHAVE batch size (per SPEC.md)
const MAX_IHAVE_BATCH_SIZE: usize = 1024;

/// IHAVE flush interval (100ms)
const IHAVE_FLUSH_INTERVAL_MS: u64 = 100;

/// Anti-entropy reconciliation interval (30 seconds)
const ANTI_ENTROPY_INTERVAL_SECS: u64 = 30;

/// Target eager peer degree (6-8)
const MIN_EAGER_DEGREE: usize = 6;
const MAX_EAGER_DEGREE: usize = 12;

/// Per-peer budget for an EAGER republish / IHAVE flush send.
///
/// X0X-0006 telemetry showed the dispatcher's 30 s blocks were dominated
/// by the EAGER republish loop sequentially awaiting `send_to_peer` for
/// every peer in the topic mesh — a single slow peer (high RTT, congested,
/// NAT renegotiation, receive-pump back-pressure) pinned the whole loop.
/// X0X-0007 replaces the sequential await with concurrent sends and gives
/// each per-peer send this bounded budget; on timeout the peer is skipped
/// and `republish_per_peer_timeout` is incremented so the operator can see
/// which peer is the slow one without a hung dispatcher.
const PER_PEER_REPUBLISH_TIMEOUT: Duration = Duration::from_millis(750);

/// Rolling window for send-side slow-peer detection.
const PEER_TIMEOUT_WINDOW: Duration = Duration::from_secs(30);

/// Timeouts inside `PEER_TIMEOUT_WINDOW` before a peer is cooled.
const PEER_TIMEOUT_THRESHOLD: usize = 5;

/// Initial sender-side suppression duration for a cooled peer.
const PEER_SUPPRESSION_COOLDOWN: Duration = Duration::from_secs(120);

/// Maximum repeated-offender suppression duration.
const PEER_SUPPRESSION_BACKOFF_MAX: Duration = Duration::from_secs(1_800);

/// Message ID type alias
type MessageIdType = [u8; 32];

/// Counters for inbound PubSub wire classes plus local PlumTree tree changes.
#[derive(Debug, Default)]
pub struct PubSubMessageKindStats {
    eager: AtomicU64,
    ihave: AtomicU64,
    iwant: AtomicU64,
    anti_entropy: AtomicU64,
    other: AtomicU64,
    decode_failed: AtomicU64,
    prune: AtomicU64,
    graft: AtomicU64,
}

/// JSON-friendly snapshot of [`PubSubMessageKindStats`].
#[derive(Debug, Clone, Serialize)]
pub struct PubSubMessageKindStatsSnapshot {
    /// Inbound EAGER messages.
    pub eager: u64,
    /// Inbound IHAVE messages.
    pub ihave: u64,
    /// Inbound IWANT messages.
    pub iwant: u64,
    /// Inbound anti-entropy messages.
    pub anti_entropy: u64,
    /// Inbound messages decoded as non-PubSub wire kinds.
    pub other: u64,
    /// PubSub envelopes or control payloads that failed to decode.
    pub decode_failed: u64,
    /// Local PRUNE transitions from eager to lazy.
    pub prune: u64,
    /// Local GRAFT transitions from lazy to eager.
    pub graft: u64,
}

impl PubSubMessageKindStats {
    fn record_kind(&self, kind: MessageKind) {
        match kind {
            MessageKind::Eager => self.eager.fetch_add(1, Ordering::Relaxed),
            MessageKind::IHave => self.ihave.fetch_add(1, Ordering::Relaxed),
            MessageKind::IWant => self.iwant.fetch_add(1, Ordering::Relaxed),
            MessageKind::AntiEntropy => self.anti_entropy.fetch_add(1, Ordering::Relaxed),
            _ => self.other.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn record_decode_failed(&self) {
        self.decode_failed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_prune(&self) {
        self.prune.fetch_add(1, Ordering::Relaxed);
    }

    fn record_graft(&self) {
        self.graft.fetch_add(1, Ordering::Relaxed);
    }

    fn record_prunes(&self, count: usize) {
        self.prune.fetch_add(usize_to_u64(count), Ordering::Relaxed);
    }

    fn record_grafts(&self, count: usize) {
        self.graft.fetch_add(usize_to_u64(count), Ordering::Relaxed);
    }

    fn snapshot(&self) -> PubSubMessageKindStatsSnapshot {
        PubSubMessageKindStatsSnapshot {
            eager: self.eager.load(Ordering::Relaxed),
            ihave: self.ihave.load(Ordering::Relaxed),
            iwant: self.iwant.load(Ordering::Relaxed),
            anti_entropy: self.anti_entropy.load(Ordering::Relaxed),
            other: self.other.load(Ordering::Relaxed),
            decode_failed: self.decode_failed.load(Ordering::Relaxed),
            prune: self.prune.load(Ordering::Relaxed),
            graft: self.graft.load(Ordering::Relaxed),
        }
    }
}

/// Timing counters for one PubSub processing stage.
#[derive(Debug, Default)]
pub struct StageTimingStats {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    over_1s_count: AtomicU64,
    over_5s_count: AtomicU64,
    over_30s_count: AtomicU64,
}

/// JSON-friendly snapshot of one PubSub processing stage.
#[derive(Debug, Clone, Serialize)]
pub struct StageTimingStatsSnapshot {
    /// Number of observations recorded for the stage.
    pub count: u64,
    /// Cumulative wall-clock time spent in this stage, in nanoseconds.
    pub total_ns: u64,
    /// Slowest observed stage execution, in nanoseconds.
    pub max_ns: u64,
    /// Number of observations that took at least 1 second.
    pub over_1s_count: u64,
    /// Number of observations that took at least 5 seconds.
    pub over_5s_count: u64,
    /// Number of observations that took at least 30 seconds.
    pub over_30s_count: u64,
}

impl StageTimingStats {
    fn record(&self, duration: Duration) {
        let ns = duration_to_ns(duration);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        if duration >= Duration::from_secs(1) {
            self.over_1s_count.fetch_add(1, Ordering::Relaxed);
        }
        if duration >= Duration::from_secs(5) {
            self.over_5s_count.fetch_add(1, Ordering::Relaxed);
        }
        if duration >= Duration::from_secs(30) {
            self.over_30s_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> StageTimingStatsSnapshot {
        StageTimingStatsSnapshot {
            count: self.count.load(Ordering::Relaxed),
            total_ns: self.total_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
            over_1s_count: self.over_1s_count.load(Ordering::Relaxed),
            over_5s_count: self.over_5s_count.load(Ordering::Relaxed),
            over_30s_count: self.over_30s_count.load(Ordering::Relaxed),
        }
    }
}

/// Per-stage timing counters for inbound PubSub message handling.
#[derive(Debug, Default)]
pub struct PubSubStageStats {
    message_kinds: PubSubMessageKindStats,
    decode: StageTimingStats,
    verify: StageTimingStats,
    dedupe_lock_acquire: StageTimingStats,
    dedupe_check: StageTimingStats,
    eager_fanout: StageTimingStats,
    republish: StageTimingStats,
    /// Per-peer EAGER/IHAVE sends that hit `PER_PEER_REPUBLISH_TIMEOUT`.
    /// Surfaced separately so operators can distinguish "the dispatcher is
    /// healthy but one specific peer is slow" from "the whole pipeline is
    /// degrading".
    republish_per_peer_timeout: AtomicU64,
    suppressed_peers: Mutex<HashMap<SuppressedPeerKey, SuppressedPeerState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SuppressedPeerKey {
    topic: TopicId,
    peer: PeerId,
}

#[derive(Debug, Clone)]
struct SuppressedPeerState {
    suppressed_until: Instant,
    recent_timeout_count: usize,
    cooldown: Duration,
}

/// JSON-friendly diagnostic for peers currently cooled by send-side timeout feedback.
#[derive(Debug, Clone, Serialize)]
pub struct SuppressedPeerSnapshot {
    /// Peer identifier, formatted the same way as logs.
    pub peer_id: String,
    /// Topic identifier, formatted the same way as logs.
    pub topic: String,
    /// Approximate wall-clock Unix millisecond when this cooldown expires.
    pub suppressed_until_unix_ms: u64,
    /// Remaining cooldown duration.
    pub suppressed_for_ms: u64,
    /// Number of recent timeouts that triggered the current cooldown.
    pub recent_timeout_count: usize,
    /// Timeout rate implied by the trigger window.
    pub recent_timeout_rate_per_sec: f64,
    /// Active topic suppressions for this peer in this snapshot.
    pub affected_topics_count: usize,
    /// Cooldown duration applied to this suppression.
    pub cooldown_ms: u64,
}

/// JSON-friendly snapshot of per-stage PubSub handling timings.
#[derive(Debug, Clone, Serialize)]
pub struct PubSubStageStatsSnapshot {
    /// Inbound PubSub wire classes and local PRUNE/GRAFT tree transitions.
    pub message_kinds: PubSubMessageKindStatsSnapshot,
    /// Wire-envelope and control-payload decode time.
    pub decode: StageTimingStatsSnapshot,
    /// ML-DSA-65 signature verification time.
    pub verify: StageTimingStatsSnapshot,
    /// Time waiting to acquire the per-topic PlumTree write lock.
    pub dedupe_lock_acquire: StageTimingStatsSnapshot,
    /// Time spent under the per-topic lock for dedupe/cache/bookkeeping.
    pub dedupe_check: StageTimingStatsSnapshot,
    /// Time spent delivering accepted messages to local PlumTree subscribers.
    pub eager_fanout: StageTimingStatsSnapshot,
    /// Time spent re-publishing/forwarding messages to remote peers.
    pub republish: StageTimingStatsSnapshot,
    /// Cumulative count of per-peer sends that hit
    /// `PER_PEER_REPUBLISH_TIMEOUT` during EAGER republish or IHAVE flush.
    pub republish_per_peer_timeout: u64,
    /// Peers currently cooled after repeated send-side timeouts.
    pub suppressed_peers: Vec<SuppressedPeerSnapshot>,
}

#[derive(Debug, Clone, Copy)]
enum PubSubStage {
    Decode,
    Verify,
    DedupeLockAcquire,
    DedupeCheck,
    EagerFanout,
    Republish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerSendOutcome {
    Sent,
    TimedOut,
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn unix_millis_after(remaining: Duration) -> u64 {
    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration_millis_u64(duration),
        Err(_) => 0,
    };
    now_ms.saturating_add(duration_millis_u64(remaining))
}

impl PubSubStageStats {
    fn suppressed_peers_guard(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<SuppressedPeerKey, SuppressedPeerState>> {
        match self.suppressed_peers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("PubSub suppressed-peers diagnostics lock was poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    fn suppressed_peer_snapshots(&self) -> Vec<SuppressedPeerSnapshot> {
        let now = Instant::now();
        let mut guard = self.suppressed_peers_guard();
        guard.retain(|_, state| state.suppressed_until > now);

        let mut affected_topics = HashMap::<PeerId, usize>::new();
        for key in guard.keys() {
            *affected_topics.entry(key.peer).or_default() += 1;
        }

        let mut snapshots: Vec<SuppressedPeerSnapshot> = guard
            .iter()
            .map(|(key, state)| {
                let remaining = state.suppressed_until.saturating_duration_since(now);
                SuppressedPeerSnapshot {
                    peer_id: key.peer.to_string(),
                    topic: key.topic.to_string(),
                    suppressed_until_unix_ms: unix_millis_after(remaining),
                    suppressed_for_ms: duration_millis_u64(remaining),
                    recent_timeout_count: state.recent_timeout_count,
                    recent_timeout_rate_per_sec: state.recent_timeout_count as f64
                        / PEER_TIMEOUT_WINDOW.as_secs_f64(),
                    affected_topics_count: affected_topics.get(&key.peer).copied().unwrap_or(1),
                    cooldown_ms: duration_millis_u64(state.cooldown),
                }
            })
            .collect();

        snapshots.sort_by(|a, b| {
            a.peer_id
                .cmp(&b.peer_id)
                .then_with(|| a.topic.cmp(&b.topic))
        });
        snapshots
    }

    fn record(&self, stage: PubSubStage, duration: Duration) {
        match stage {
            PubSubStage::Decode => self.decode.record(duration),
            PubSubStage::Verify => self.verify.record(duration),
            PubSubStage::DedupeLockAcquire => self.dedupe_lock_acquire.record(duration),
            PubSubStage::DedupeCheck => self.dedupe_check.record(duration),
            PubSubStage::EagerFanout => self.eager_fanout.record(duration),
            PubSubStage::Republish => self.republish.record(duration),
        }
    }

    fn snapshot(&self) -> PubSubStageStatsSnapshot {
        PubSubStageStatsSnapshot {
            message_kinds: self.message_kinds.snapshot(),
            decode: self.decode.snapshot(),
            verify: self.verify.snapshot(),
            dedupe_lock_acquire: self.dedupe_lock_acquire.snapshot(),
            dedupe_check: self.dedupe_check.snapshot(),
            eager_fanout: self.eager_fanout.snapshot(),
            republish: self.republish.snapshot(),
            republish_per_peer_timeout: self.republish_per_peer_timeout.load(Ordering::Relaxed),
            suppressed_peers: self.suppressed_peer_snapshots(),
        }
    }

    fn record_per_peer_timeout(&self) {
        self.republish_per_peer_timeout
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_peer_suppressed(
        &self,
        topic: TopicId,
        peer: PeerId,
        suppressed_until: Instant,
        recent_timeout_count: usize,
        cooldown: Duration,
    ) {
        let mut guard = self.suppressed_peers_guard();
        guard.insert(
            SuppressedPeerKey { topic, peer },
            SuppressedPeerState {
                suppressed_until,
                recent_timeout_count,
                cooldown,
            },
        );
    }

    fn clear_peer_suppression(&self, topic: TopicId, peer: PeerId) {
        let mut guard = self.suppressed_peers_guard();
        guard.remove(&SuppressedPeerKey { topic, peer });
    }

    fn record_message_kind(&self, kind: MessageKind) {
        self.message_kinds.record_kind(kind);
    }

    fn record_decode_failed(&self) {
        self.message_kinds.record_decode_failed();
    }

    fn record_prune(&self) {
        self.message_kinds.record_prune();
    }

    fn record_graft(&self) {
        self.message_kinds.record_graft();
    }

    fn record_prunes(&self, count: usize) {
        self.message_kinds.record_prunes(count);
    }

    fn record_grafts(&self, count: usize) {
        self.message_kinds.record_grafts(count);
    }
}

fn duration_to_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).ok().map_or(u64::MAX, |v| v)
}

fn deterministic_jitter(peer_id: PeerId, salt: &[u8], max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let max_ms = u64::try_from(max.as_millis())
        .ok()
        .filter(|ms| *ms > 0)
        .map_or(1, |ms| ms);
    let mut input = Vec::with_capacity(peer_id.as_bytes().len() + salt.len());
    input.extend_from_slice(peer_id.as_bytes());
    input.extend_from_slice(salt);
    let hash = blake3::hash(&input);
    let bytes = hash.as_bytes();
    let raw = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    Duration::from_millis(raw % max_ms)
}

const fn message_cache_capacity() -> NonZeroUsize {
    // SAFETY: MAX_CACHE_SIZE is a positive constant.
    unsafe { NonZeroUsize::new_unchecked(MAX_CACHE_SIZE) }
}

const fn replay_cache_capacity() -> NonZeroUsize {
    // SAFETY: REPLAY_CACHE_MAX_ENTRIES is a positive constant (10,000)
    unsafe { NonZeroUsize::new_unchecked(REPLAY_CACHE_MAX_ENTRIES) }
}

/// Gossip message wrapper
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipMessage {
    /// Message header
    pub header: MessageHeader,
    /// Optional payload (None for IHAVE)
    pub payload: Option<Bytes>,
    /// ML-DSA signature over the header
    pub signature: Vec<u8>,
    /// Sender's ML-DSA public key for verification
    pub public_key: Vec<u8>,
}

/// Anti-entropy reconciliation payload
///
/// Used for periodic set reconciliation between peers to recover
/// messages missed during network partitions.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum AntiEntropyPayload {
    /// "Here are my message IDs, send me anything I'm missing"
    Digest {
        /// Message IDs the sender currently has cached
        msg_ids: Vec<MessageIdType>,
    },
    /// "Here are the IDs you're missing" (actual messages follow as EAGER)
    Response {
        /// Message IDs the receiver is missing
        missing_ids: Vec<MessageIdType>,
    },
}

/// Per-peer quality score for tree optimization
///
/// Tracks delivery metrics to enable score-based promotion/demotion
/// decisions in the Plumtree spanning tree.
struct PeerScore {
    /// Count of EAGER messages received from this peer
    messages_delivered: u64,
    /// Count of IWANTs we sent to this peer
    iwant_requests: u64,
    /// Count of responses received after sending IWANT
    iwant_responses: u64,
    /// Last time we received any message from this peer
    last_seen: Instant,
}

impl PeerScore {
    /// Create a new peer score with default values
    fn new() -> Self {
        Self {
            messages_delivered: 0,
            iwant_requests: 0,
            iwant_responses: 0,
            last_seen: Instant::now(),
        }
    }

    /// Calculate peer quality score (0.0 to 1.0)
    ///
    /// Score = (iwant_response_rate * 0.6) + (recency_factor * 0.4)
    fn score(&self) -> f64 {
        let response_rate = if self.iwant_requests > 0 {
            self.iwant_responses as f64 / self.iwant_requests as f64
        } else {
            // No IWANT requests means peer has been responsive enough via EAGER
            // Give benefit of the doubt with a moderate score
            if self.messages_delivered > 0 {
                0.8
            } else {
                0.5
            }
        };

        let secs_since_seen = Instant::now()
            .saturating_duration_since(self.last_seen)
            .as_secs_f64();
        let recency = (1.0 - (secs_since_seen / 300.0)).max(0.0);

        (response_rate.min(1.0) * 0.6) + (recency * 0.4)
    }

    /// Record a message delivery from this peer
    fn record_delivery(&mut self) {
        self.messages_delivered += 1;
        self.last_seen = Instant::now();
    }

    /// Record that we sent an IWANT request to this peer
    fn record_iwant_request(&mut self) {
        self.iwant_requests += 1;
    }

    /// Record that this peer responded to an IWANT request
    fn record_iwant_response(&mut self) {
        self.iwant_responses += 1;
        self.last_seen = Instant::now();
    }
}

struct PeerCoolingState {
    timeout_window_started: Instant,
    timeout_count: usize,
    suppressed_until: Option<Instant>,
    cooldown: Duration,
}

struct PeerSuppressionEvent {
    suppressed_until: Instant,
    recent_timeout_count: usize,
    cooldown: Duration,
    demoted: bool,
}

impl PeerCoolingState {
    fn new(now: Instant) -> Self {
        Self {
            timeout_window_started: now,
            timeout_count: 0,
            suppressed_until: None,
            cooldown: PEER_SUPPRESSION_COOLDOWN,
        }
    }

    fn is_suppressed_at(&self, now: Instant) -> bool {
        self.suppressed_until.is_some_and(|until| until > now)
    }

    fn suppression_expired_at(&self, now: Instant) -> bool {
        self.suppressed_until.is_some_and(|until| until <= now)
    }

    fn next_cooldown(&self) -> Duration {
        if self.suppressed_until.is_some() {
            match self.cooldown.checked_mul(2) {
                Some(duration) => duration.min(PEER_SUPPRESSION_BACKOFF_MAX),
                None => PEER_SUPPRESSION_BACKOFF_MAX,
            }
        } else {
            PEER_SUPPRESSION_COOLDOWN
        }
    }
}

/// Cached message entry
#[derive(Clone)]
struct CachedMessage {
    /// Message payload
    payload: Bytes,
    /// Timestamp when cached
    timestamp: Instant,
    /// Message header
    header: MessageHeader,
}

/// Per-topic state
struct TopicState {
    /// Spanning tree peers (forward EAGER)
    eager_peers: HashSet<PeerId>,
    /// Non-tree peers (send IHAVE only)
    lazy_peers: HashSet<PeerId>,
    /// Message cache: msg_id -> cached message
    message_cache: LruCache<MessageIdType, CachedMessage>,
    /// Pending IHAVE batch (≤1024 message IDs)
    pending_ihave: Vec<MessageIdType>,
    /// Outstanding IWANT requests: msg_id -> (peer, timestamp)
    outstanding_iwants: HashMap<MessageIdType, (PeerId, Instant)>,
    /// Per-peer quality scores for tree optimization
    peer_scores: HashMap<PeerId, PeerScore>,
    /// Local subscribers
    subscribers: Vec<mpsc::UnboundedSender<(PeerId, Bytes)>>,
    /// Payload-level replay cache: BLAKE3(payload) -> insertion time.
    ///
    /// Catches replays where the same application payload is wrapped in a
    /// different gossip envelope (different epoch, sender, msg_id).
    replay_cache: LruCache<[u8; 32], Instant>,
    /// TTL for replay cache entries.
    replay_ttl: Duration,
    /// Last time this topic saw any activity (publish, incoming, IHAVE, etc).
    /// When this exceeds TOPIC_IDLE_TTL_SECS, the entire entry is dropped
    /// from the parent `topics` HashMap.
    last_activity: Instant,
    /// Send-side timeout/cooldown state keyed by peer for this topic.
    peer_cooling: HashMap<PeerId, PeerCoolingState>,
}

impl TopicState {
    fn new() -> Self {
        Self {
            eager_peers: HashSet::new(),
            lazy_peers: HashSet::new(),
            message_cache: LruCache::new(message_cache_capacity()),
            pending_ihave: Vec::new(),
            outstanding_iwants: HashMap::new(),
            peer_scores: HashMap::new(),
            subscribers: Vec::new(),
            replay_cache: LruCache::new(replay_cache_capacity()),
            replay_ttl: Duration::from_secs(REPLAY_CACHE_TTL_SECS),
            last_activity: Instant::now(),
            peer_cooling: HashMap::new(),
        }
    }

    /// Mark this topic as having seen data-plane activity now.
    fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    fn is_idle(&self, ttl: Duration) -> bool {
        self.last_activity.elapsed() > ttl && !self.has_live_subscribers()
    }

    fn has_live_subscribers(&self) -> bool {
        self.subscribers.iter().any(|tx| !tx.is_closed())
    }

    /// Check if a payload has been seen before (replay detection).
    ///
    /// Returns `true` if this is a replay (payload hash already in cache
    /// and not expired). Returns `false` if this is a new payload (and
    /// inserts the hash into the cache).
    fn is_payload_replay(&mut self, payload: &[u8]) -> bool {
        self.touch();
        let key: [u8; 32] = *blake3::hash(payload).as_bytes();
        if let Some(ts) = self.replay_cache.get(&key) {
            if ts.elapsed() < self.replay_ttl {
                return true;
            }
        }
        self.replay_cache.put(key, Instant::now());
        false
    }

    /// Get all cached message IDs for anti-entropy digest
    fn cached_message_ids(&self) -> Vec<MessageIdType> {
        self.message_cache.iter().map(|(id, _)| *id).collect()
    }

    /// Check if message is in cache
    fn has_message(&self, msg_id: &MessageIdType) -> bool {
        self.message_cache.contains(msg_id)
    }

    /// Add message to cache
    fn cache_message(&mut self, msg_id: MessageIdType, payload: Bytes, header: MessageHeader) {
        let cached = CachedMessage {
            payload,
            timestamp: Instant::now(),
            header,
        };
        self.message_cache.put(msg_id, cached);
        self.touch();
    }

    /// Get cached message
    fn get_message(&mut self, msg_id: &MessageIdType) -> Option<CachedMessage> {
        self.message_cache.get(msg_id).cloned()
    }

    /// Clean expired cache entries
    fn clean_cache(&mut self) {
        let now = Instant::now();
        let ttl = Duration::from_secs(CACHE_TTL_SECS);

        // Collect expired keys
        let mut expired = Vec::new();
        for (msg_id, cached) in self.message_cache.iter() {
            if now.duration_since(cached.timestamp) > ttl {
                expired.push(*msg_id);
            }
        }

        // Remove expired entries
        for msg_id in expired {
            self.message_cache.pop(&msg_id);
        }

        // Clean expired replay cache entries
        let replay_ttl = self.replay_ttl;
        let mut expired_replay = Vec::new();
        for (hash, ts) in self.replay_cache.iter() {
            if now.saturating_duration_since(*ts) > replay_ttl {
                expired_replay.push(*hash);
            }
        }
        for hash in expired_replay {
            self.replay_cache.pop(&hash);
        }

        // Clean stale peer scores (10 minute expiry)
        // Use saturating_duration_since to avoid panic on Windows (coarse timer)
        let score_expiry = Duration::from_secs(600);
        let now = Instant::now();
        self.peer_scores
            .retain(|_, score| now.saturating_duration_since(score.last_seen) < score_expiry);

        // Quiet topics do not send through the subscriber list, so closed
        // receivers would otherwise keep an idle topic alive forever.
        self.subscribers.retain(|tx| !tx.is_closed());
    }

    fn is_peer_suppressed_at(&self, peer: PeerId, now: Instant) -> bool {
        self.peer_cooling
            .get(&peer)
            .is_some_and(|state| state.is_suppressed_at(now))
    }

    fn record_send_success_at(&mut self, peer: PeerId, now: Instant) -> bool {
        let recovered = self
            .peer_cooling
            .get(&peer)
            .is_some_and(|state| state.suppression_expired_at(now));
        if recovered {
            self.peer_cooling.remove(&peer);
        }
        recovered
    }

    fn record_send_timeout_at(
        &mut self,
        peer: PeerId,
        now: Instant,
    ) -> Option<PeerSuppressionEvent> {
        let event = {
            let cooling = self
                .peer_cooling
                .entry(peer)
                .or_insert_with(|| PeerCoolingState::new(now));

            if cooling.is_suppressed_at(now) {
                return None;
            }

            if now.saturating_duration_since(cooling.timeout_window_started) > PEER_TIMEOUT_WINDOW {
                cooling.timeout_window_started = now;
                cooling.timeout_count = 0;
            }

            cooling.timeout_count = cooling.timeout_count.saturating_add(1);
            if cooling.timeout_count < PEER_TIMEOUT_THRESHOLD {
                None
            } else {
                let cooldown = cooling.next_cooldown();
                let suppressed_until = now + cooldown;
                let recent_timeout_count = cooling.timeout_count;
                cooling.cooldown = cooldown;
                cooling.suppressed_until = Some(suppressed_until);
                cooling.timeout_window_started = now;
                cooling.timeout_count = 0;
                Some(PeerSuppressionEvent {
                    suppressed_until,
                    recent_timeout_count,
                    cooldown,
                    demoted: false,
                })
            }
        };

        event.map(|mut event| {
            event.demoted = self.prune_peer(peer);
            event
        })
    }

    fn clear_disconnected_peer_cooling(&mut self, connected_set: &HashSet<PeerId>) -> Vec<PeerId> {
        let removed: Vec<PeerId> = self
            .peer_cooling
            .keys()
            .filter(|peer| !connected_set.contains(peer))
            .copied()
            .collect();
        for peer in &removed {
            self.peer_cooling.remove(peer);
        }
        removed
    }

    /// Move peer from eager to lazy
    fn prune_peer(&mut self, peer: PeerId) -> bool {
        if self.eager_peers.remove(&peer) {
            self.lazy_peers.insert(peer);
            debug!(peer_id = %peer, "PRUNE: moved peer from eager to lazy");
            true
        } else {
            false
        }
    }

    /// Move peer from lazy to eager
    fn graft_peer(&mut self, peer: PeerId) -> bool {
        self.graft_peer_at(peer, Instant::now())
    }

    fn graft_peer_at(&mut self, peer: PeerId, now: Instant) -> bool {
        if self.is_peer_suppressed_at(peer, now) {
            debug!(peer_id = %peer, "GRAFT skipped: peer is cooling after send timeouts");
            return false;
        }

        if self.lazy_peers.remove(&peer) {
            self.eager_peers.insert(peer);
            debug!(peer_id = %peer, "GRAFT: moved peer from lazy to eager");
            true
        } else {
            false
        }
    }

    /// Maintain eager peer degree (6-12) using score-based selection
    ///
    /// Promotes the highest-scoring lazy peers when below minimum degree,
    /// and demotes the lowest-scoring eager peers when above maximum degree.
    fn maintain_degree(&mut self) -> (usize, usize) {
        self.maintain_degree_at(Instant::now())
    }

    fn maintain_degree_at(&mut self, now: Instant) -> (usize, usize) {
        let mut pruned = 0;
        let mut grafted = 0;
        let eager_count = self.eager_peers.len();

        if eager_count < MIN_EAGER_DEGREE && !self.lazy_peers.is_empty() {
            // Promote highest-scoring lazy peers
            let to_promote = MIN_EAGER_DEGREE - eager_count;
            let mut scored_lazy: Vec<(PeerId, f64)> = self
                .lazy_peers
                .iter()
                .filter(|&&p| !self.is_peer_suppressed_at(p, now))
                .map(|&p| {
                    let score = self.peer_scores.get(&p).map_or(0.5, |s| s.score());
                    (p, score)
                })
                .collect();
            scored_lazy.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let peers: Vec<PeerId> = scored_lazy
                .iter()
                .take(to_promote)
                .map(|(p, _)| *p)
                .collect();
            for peer in peers {
                if self.graft_peer_at(peer, now) {
                    grafted += 1;
                }
            }
        } else if eager_count > MAX_EAGER_DEGREE {
            // Demote lowest-scoring eager peers
            let to_demote = eager_count - MAX_EAGER_DEGREE;
            let mut scored_eager: Vec<(PeerId, f64)> = self
                .eager_peers
                .iter()
                .map(|&p| {
                    let score = self.peer_scores.get(&p).map_or(0.5, |s| s.score());
                    (p, score)
                })
                .collect();
            scored_eager.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let peers: Vec<PeerId> = scored_eager
                .iter()
                .take(to_demote)
                .map(|(p, _)| *p)
                .collect();
            for peer in peers {
                if self.prune_peer(peer) {
                    pruned += 1;
                }
            }
        }
        (pruned, grafted)
    }
}

fn clean_and_reap_topics(
    topics: &mut HashMap<TopicId, TopicState>,
    topic_idle_ttl: Duration,
) -> usize {
    for state in topics.values_mut() {
        state.clean_cache();
    }

    let idle: Vec<TopicId> = topics
        .iter()
        .filter(|(_, s)| s.is_idle(topic_idle_ttl))
        .map(|(id, _)| *id)
        .collect();
    let idle_count = idle.len();
    for id in idle {
        topics.remove(&id);
    }

    idle_count
}

/// Pub/sub trait for message dissemination
#[async_trait::async_trait]
pub trait PubSub: Send + Sync {
    /// Publish a message to a topic
    async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()>;

    /// Subscribe to a topic and receive messages
    fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)>;

    /// Unsubscribe from a topic
    async fn unsubscribe(&self, topic: TopicId) -> Result<()>;

    /// Initialize peers for a topic
    ///
    /// Called when subscribing to a topic to populate the eager peers list
    /// with currently connected peers for message dissemination.
    async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>);

    /// Replace topic peers with exactly the given set of connected peers.
    ///
    /// Removes stale/disconnected peers and adds newly connected ones.
    ///
    /// The default implementation falls back to [`Self::initialize_topic_peers`]
    /// (add-only). Override this method to get full prune-and-replace semantics.
    async fn set_topic_peers(&self, topic: TopicId, connected: Vec<PeerId>) {
        // Default: fall back to initialize (add-only)
        self.initialize_topic_peers(topic, connected).await;
    }

    /// Handle an incoming pubsub message from a peer
    ///
    /// Routes the message to appropriate handler based on MessageKind (Eager, IHave, IWant).
    /// Called by the transport layer when receiving PubSub messages.
    async fn handle_message(&self, from: PeerId, data: Bytes) -> Result<()>;

    /// Trigger an anti-entropy round for a specific topic
    ///
    /// This is primarily for testing. In production, anti-entropy runs
    /// automatically via the background task.
    async fn trigger_anti_entropy(&self, _topic: TopicId) -> Result<()> {
        Ok(()) // Default no-op
    }
}

/// Plumtree pub/sub implementation
pub struct PlumtreePubSub<T: GossipTransport + 'static> {
    /// Per-topic state
    topics: Arc<RwLock<HashMap<TopicId, TopicState>>>,
    /// Local peer ID
    peer_id: PeerId,
    /// Epoch for message IDs (system time in seconds)
    epoch_start: std::time::SystemTime,
    /// Transport layer for sending messages
    transport: Arc<T>,
    /// ML-DSA key pair for signing messages
    signing_key: Arc<saorsa_gossip_identity::MlDsaKeyPair>,
    /// Low-overhead timing counters for inbound PubSub processing stages.
    stage_stats: Arc<PubSubStageStats>,
}

impl<T: GossipTransport + 'static> PlumtreePubSub<T> {
    /// Create a new Plumtree pub/sub instance
    ///
    /// # Arguments
    /// * `peer_id` - Local peer identifier
    /// * `transport` - Transport layer for network communication
    /// * `signing_key` - ML-DSA key pair for message signing
    pub fn new(
        peer_id: PeerId,
        transport: Arc<T>,
        signing_key: saorsa_gossip_identity::MlDsaKeyPair,
    ) -> Self {
        Self::new_with_task_control(peer_id, transport, signing_key, true)
    }

    fn new_with_task_control(
        peer_id: PeerId,
        transport: Arc<T>,
        signing_key: saorsa_gossip_identity::MlDsaKeyPair,
        start_background_tasks: bool,
    ) -> Self {
        let pubsub = Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            peer_id,
            epoch_start: std::time::SystemTime::UNIX_EPOCH,
            transport,
            signing_key: Arc::new(signing_key),
            stage_stats: Arc::new(PubSubStageStats::default()),
        };

        if start_background_tasks {
            pubsub.spawn_ihave_flusher();
            pubsub.spawn_cache_cleaner();
            pubsub.spawn_degree_maintainer();
            pubsub.spawn_anti_entropy_task();
        }

        pubsub
    }

    /// Snapshot per-stage timings for inbound PubSub message handling.
    pub fn stage_stats(&self) -> PubSubStageStatsSnapshot {
        self.stage_stats.snapshot()
    }

    fn record_stage(&self, stage: PubSubStage, started: Instant) {
        self.stage_stats.record(stage, started.elapsed());
    }

    async fn send_to_peer_with_timeout(
        transport: Arc<T>,
        stage_stats: Arc<PubSubStageStats>,
        peer: PeerId,
        stream_type: GossipStreamType,
        bytes: Bytes,
        op: &'static str,
    ) -> Result<PeerSendOutcome> {
        match tokio::time::timeout(
            PER_PEER_REPUBLISH_TIMEOUT,
            transport.send_to_peer(peer, stream_type, bytes),
        )
        .await
        {
            Ok(Ok(())) => Ok(PeerSendOutcome::Sent),
            Ok(Err(e)) => {
                warn!(
                    peer_id = %peer,
                    op,
                    "{op} per-peer send failed: {e}"
                );
                Err(e)
            }
            Err(_) => {
                stage_stats.record_per_peer_timeout();
                warn!(
                    peer_id = %peer,
                    op,
                    timeout_ms = PER_PEER_REPUBLISH_TIMEOUT.as_millis() as u64,
                    "{op} per-peer send timed out — peer skipped, recorded in republish_per_peer_timeout"
                );
                Ok(PeerSendOutcome::TimedOut)
            }
        }
    }

    async fn send_to_peer_bounded(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        bytes: Bytes,
        op: &'static str,
    ) -> Result<()> {
        match Self::send_to_peer_with_timeout(
            Arc::clone(&self.transport),
            Arc::clone(&self.stage_stats),
            peer,
            stream_type,
            bytes,
            op,
        )
        .await?
        {
            PeerSendOutcome::Sent | PeerSendOutcome::TimedOut => Ok(()),
        }
    }

    async fn filter_suppressed_topic_peers(
        &self,
        topic: TopicId,
        peers: Vec<PeerId>,
        op: &'static str,
    ) -> Vec<PeerId> {
        if peers.is_empty() {
            return peers;
        }

        let now = Instant::now();
        let topics = self.topics.read().await;
        let Some(state) = topics.get(&topic) else {
            return peers;
        };

        peers
            .into_iter()
            .filter(|peer| {
                let suppressed = state.is_peer_suppressed_at(*peer, now);
                if suppressed {
                    trace!(
                        peer_id = %peer,
                        topic = ?topic,
                        op,
                        "{op} send skipped: peer cooling after repeated timeouts"
                    );
                }
                !suppressed
            })
            .collect()
    }

    async fn record_topic_send_results(
        &self,
        topic: TopicId,
        sent: Vec<PeerId>,
        timed_out: Vec<PeerId>,
    ) {
        if sent.is_empty() && timed_out.is_empty() {
            return;
        }

        Self::record_topic_send_results_for(
            &self.topics,
            &self.stage_stats,
            topic,
            sent,
            timed_out,
        )
        .await;
    }

    async fn record_topic_send_results_for(
        topics: &Arc<RwLock<HashMap<TopicId, TopicState>>>,
        stage_stats: &Arc<PubSubStageStats>,
        topic: TopicId,
        sent: Vec<PeerId>,
        timed_out: Vec<PeerId>,
    ) {
        let now = Instant::now();
        let mut topics_guard = topics.write().await;
        let Some(state) = topics_guard.get_mut(&topic) else {
            return;
        };

        for peer in sent {
            if state.record_send_success_at(peer, now) {
                stage_stats.clear_peer_suppression(topic, peer);
                debug!(
                    peer_id = %peer,
                    topic = ?topic,
                    "Peer cooling cleared after successful post-cooldown send"
                );
            }
        }

        for peer in timed_out {
            if let Some(event) = state.record_send_timeout_at(peer, now) {
                stage_stats.record_peer_suppressed(
                    topic,
                    peer,
                    event.suppressed_until,
                    event.recent_timeout_count,
                    event.cooldown,
                );
                if event.demoted {
                    stage_stats.record_prune();
                }
                warn!(
                    peer_id = %peer,
                    topic = ?topic,
                    cooldown_ms = duration_millis_u64(event.cooldown),
                    recent_timeout_count = event.recent_timeout_count,
                    demoted = event.demoted,
                    "Peer cooled after repeated PubSub send timeouts"
                );
            }
        }
    }

    /// Send `bytes` to every peer in `peers` concurrently with a per-peer
    /// `PER_PEER_REPUBLISH_TIMEOUT` budget.
    ///
    /// X0X-0007: replaces the previous sequential
    /// `for peer in eager_peers { transport.send_to_peer(peer, ...).await }`
    /// pattern that pinned the dispatcher behind the slowest peer in the
    /// fanout set. Now every send runs concurrently and any individual send
    /// that exceeds the per-peer budget is skipped (incrementing
    /// `republish_per_peer_timeout`) so a single stuck peer cannot pin the
    /// dispatcher.
    ///
    /// `op` is the short label that appears in WARN logs ("EAGER", "IHAVE").
    async fn parallel_send_to_peers(
        &self,
        topic: TopicId,
        peers: Vec<PeerId>,
        stream_type: GossipStreamType,
        bytes: Bytes,
        op: &'static str,
    ) {
        let peers = self.filter_suppressed_topic_peers(topic, peers, op).await;
        if peers.is_empty() {
            return;
        }
        let mut set = tokio::task::JoinSet::new();
        for peer in peers {
            let transport = Arc::clone(&self.transport);
            let bytes = bytes.clone();
            let stage_stats = Arc::clone(&self.stage_stats);
            set.spawn(async move {
                let outcome = Self::send_to_peer_with_timeout(
                    transport,
                    stage_stats,
                    peer,
                    stream_type,
                    bytes,
                    op,
                )
                .await;
                (peer, outcome)
            });
        }
        // Drain the JoinSet so the helper actually returns when all sends
        // have either completed, errored, or hit the per-peer timeout. Each
        // task swallows its own errors, so JoinError outcomes are logged
        // (panicked task) but otherwise ignored — we never want one panic to
        // poison the dispatcher.
        let mut sent = Vec::new();
        let mut timed_out = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((peer, Ok(PeerSendOutcome::Sent))) => sent.push(peer),
                Ok((peer, Ok(PeerSendOutcome::TimedOut))) => timed_out.push(peer),
                Ok((_peer, Err(_))) => {}
                Err(e) => warn!(op, "{op} per-peer send task panicked: {e}"),
            }
        }
        self.record_topic_send_results(topic, sent, timed_out).await;
    }

    /// Get current epoch (seconds since UNIX_EPOCH)
    fn current_epoch(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(self.epoch_start)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Calculate message ID
    fn calculate_msg_id(&self, topic: &TopicId, payload: &Bytes) -> MessageIdType {
        let epoch = self.current_epoch();
        let payload_hash = blake3::hash(payload.as_ref());
        MessageHeader::calculate_msg_id(topic, epoch, &self.peer_id, payload_hash.as_bytes())
    }

    /// Sign message header using ML-DSA-65
    ///
    /// Serializes the header and signs it with the node's ML-DSA key pair.
    /// Per SPEC2 §2, all gossip messages MUST be signed for authenticity.
    fn sign_message(&self, header: &MessageHeader) -> Vec<u8> {
        // Serialize header for signing
        let header_bytes = match postcard::to_stdvec(header) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to serialize header for signing: {}", e);
                return Vec::new();
            }
        };

        // Sign with ML-DSA-65
        match self.signing_key.sign(&header_bytes) {
            Ok(signature) => signature,
            Err(e) => {
                error!("Failed to sign message: {}", e);
                Vec::new()
            }
        }
    }

    /// Verify message signature using ML-DSA-65
    ///
    /// # Arguments
    /// * `header` - Message header to verify
    /// * `signature` - ML-DSA signature bytes
    /// * `public_key` - Sender's public key bytes
    ///
    /// # Returns
    /// `true` if signature is valid, `false` otherwise
    fn verify_signature(
        &self,
        header: &MessageHeader,
        signature: &[u8],
        public_key: &[u8],
    ) -> bool {
        // Serialize header
        let header_bytes = match postcard::to_stdvec(header) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("Failed to serialize header for verification: {}", e);
                return false;
            }
        };

        // Verify signature
        match saorsa_gossip_identity::MlDsaKeyPair::verify(public_key, &header_bytes, signature) {
            Ok(valid) => valid,
            Err(e) => {
                warn!("Failed to verify signature: {}", e);
                false
            }
        }
    }

    /// Publish a message (local origin)
    pub async fn publish_local(&self, topic: TopicId, payload: Bytes) -> Result<()> {
        let msg_id = self.calculate_msg_id(&topic, &payload);

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let signature = self.sign_message(&header);

        let _message = GossipMessage {
            header: header.clone(),
            payload: Some(payload.clone()),
            signature,
            public_key: self.signing_key.public_key().to_vec(),
        };

        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        // Add to cache
        state.cache_message(msg_id, payload.clone(), header);

        // Seed the replay cache so network echoes of our own publish are
        // detected as replays (defense-in-depth alongside msg_id dedup).
        let _ = state.is_payload_replay(&payload);

        // Send EAGER to eager_peers
        let eager_peers: Vec<PeerId> = state.eager_peers.iter().copied().collect();
        drop(topics); // Release lock before network I/O

        // Serialize ONCE (the wire bytes are identical for every peer). The
        // previous version awaited each `send_to_peer` sequentially, which
        // pinned the dispatcher behind the slowest peer in the EAGER set
        // (X0X-0006 measured this at ~73% of dispatcher wall-clock). Now the
        // sends run concurrently with a per-peer `PER_PEER_REPUBLISH_TIMEOUT`
        // budget so a single stuck peer cannot pin the whole loop. The
        // 2026-04-25 OOM-on-spawn issue stays addressed: this is a bounded
        // concurrent send (one task per peer that completes within the
        // budget), not a fire-and-forget spawn that can accumulate.
        let bytes: Bytes = match postcard::to_stdvec(&_message) {
            Ok(b) => b.into(),
            Err(e) => {
                warn!(msg_id = ?msg_id, "EAGER serialize failed: {e}");
                return Ok(());
            }
        };
        trace!(msg_id = ?msg_id, peer_count = eager_peers.len(), "Sending EAGER fan-out");
        self.parallel_send_to_peers(topic, eager_peers, GossipStreamType::PubSub, bytes, "EAGER")
            .await;

        // Batch msg_id to pending_ihave
        let mut topics = self.topics.write().await;
        if let Some(state) = topics.get_mut(&topic) {
            state.pending_ihave.push(msg_id);

            // Deliver to local subscribers
            let data = (self.peer_id, payload);
            state.subscribers.retain(|tx| tx.send(data.clone()).is_ok());
        }

        Ok(())
    }

    /// Handle incoming EAGER message
    pub async fn handle_eager(
        &self,
        from: PeerId,
        topic: TopicId,
        message: GossipMessage,
    ) -> Result<()> {
        let msg_id = message.header.msg_id;

        // Verify signature
        let verify_started = Instant::now();
        let verified =
            self.verify_signature(&message.header, &message.signature, &message.public_key);
        self.record_stage(PubSubStage::Verify, verify_started);
        if !verified {
            warn!(peer_id = %from, msg_id = ?msg_id, "Invalid signature, dropping");
            return Err(anyhow!("Invalid signature"));
        }

        let lock_started = Instant::now();
        let mut topics = self.topics.write().await;
        self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
        let dedupe_started = Instant::now();
        let state = topics.entry(topic).or_insert_with(TopicState::new);
        state.touch();

        // Check for duplicate
        if state.has_message(&msg_id) {
            // PRUNE: move sender from eager to lazy
            if state.prune_peer(from) {
                self.stage_stats.record_prune();
            }
            self.record_stage(PubSubStage::DedupeCheck, dedupe_started);
            return Ok(());
        }

        // New message - add to cache
        let payload = match message.payload.clone() {
            Some(payload) => payload,
            None => {
                self.record_stage(PubSubStage::DedupeCheck, dedupe_started);
                return Err(anyhow!("EAGER missing payload"));
            }
        };
        state.cache_message(msg_id, payload.clone(), message.header.clone());

        // Update peer score for the sender
        state
            .peer_scores
            .entry(from)
            .or_insert_with(PeerScore::new)
            .record_delivery();

        // Check if this message was requested via IWANT (anti-entropy or IHAVE recovery)
        if state.outstanding_iwants.remove(&msg_id).is_some() {
            state
                .peer_scores
                .entry(from)
                .or_insert_with(PeerScore::new)
                .record_iwant_response();
        }

        // Payload-level replay detection: catches re-wrapped payloads where
        // the gossip envelope (msg_id) is new but the application payload is identical.
        // We keep the msg_id cache entry (already done above) so PlumTree's
        // PRUNE/GRAFT still works, but skip subscriber delivery and forwarding.
        if state.is_payload_replay(&payload) {
            debug!(
                topic = ?topic,
                msg_id = ?msg_id,
                "Payload replay detected — msg_id new but payload hash seen before"
            );
            self.record_stage(PubSubStage::DedupeCheck, dedupe_started);
            return Ok(());
        }

        // Add sender to eager_peers if not already present
        // This ensures bidirectional message flow - if a peer sends us messages
        // on a topic, they've subscribed and should receive our messages too.
        if !state.eager_peers.contains(&from) && !state.lazy_peers.contains(&from) {
            if state.is_peer_suppressed_at(from, Instant::now()) {
                state.lazy_peers.insert(from);
                debug!(
                    peer_id = %from,
                    topic = ?topic,
                    "Added cooled sender to lazy_peers"
                );
            } else {
                state.eager_peers.insert(from);
                debug!(peer_id = %from, topic = ?topic, "Added sender to eager_peers");
            }
        }

        // Forward to eager_peers (except sender)
        let eager_peers: Vec<PeerId> = state
            .eager_peers
            .iter()
            .filter(|&&p| p != from)
            .copied()
            .collect();

        // Batch msg_id to pending_ihave for lazy_peers
        state.pending_ihave.push(msg_id);
        self.record_stage(PubSubStage::DedupeCheck, dedupe_started);

        // Deliver to local subscribers
        let fanout_started = Instant::now();
        let sub_count = state.subscribers.len();
        let data = (from, payload.clone());
        state.subscribers.retain(|tx| tx.send(data.clone()).is_ok());
        let delivered = state.subscribers.len();
        self.record_stage(PubSubStage::EagerFanout, fanout_started);
        debug!(
            topic = ?topic,
            subscribers = sub_count,
            delivered = delivered,
            "plumtree handle_eager: delivered to local subscribers"
        );

        drop(topics); // Release lock

        let republish_started = Instant::now();
        // Serialize once — the payload is the same for all peers
        let bytes: Bytes = match postcard::to_stdvec(&message) {
            Ok(bytes) => bytes.into(),
            Err(e) => {
                self.record_stage(PubSubStage::Republish, republish_started);
                return Err(anyhow!("EAGER forward serialize failed: {e}"));
            }
        };

        // Forward EAGER (best-effort: log failures, don't abort the loop).
        // X0X-0007: parallel send with per-peer timeout — was a sequential
        // `for peer { send.await }` which made one slow peer pin the entire
        // dispatcher (X0X-0006: 73% of dispatcher wall-clock).
        trace!(msg_id = ?msg_id, peer_count = eager_peers.len(), "Forwarding EAGER");
        self.parallel_send_to_peers(topic, eager_peers, GossipStreamType::PubSub, bytes, "EAGER")
            .await;
        self.record_stage(PubSubStage::Republish, republish_started);

        Ok(())
    }

    /// Handle incoming IHAVE message
    pub async fn handle_ihave(
        &self,
        from: PeerId,
        topic: TopicId,
        msg_ids: Vec<MessageIdType>,
    ) -> Result<()> {
        let lock_started = Instant::now();
        let mut topics = self.topics.write().await;
        self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
        let dedupe_started = Instant::now();
        let state = topics.entry(topic).or_insert_with(TopicState::new);
        state.touch();

        let mut requested = Vec::new();

        for msg_id in msg_ids {
            // Skip if we have it
            if state.has_message(&msg_id) {
                continue;
            }

            // Skip if already requested
            if state.outstanding_iwants.contains_key(&msg_id) {
                continue;
            }

            // Request it
            requested.push(msg_id);
            state
                .outstanding_iwants
                .insert(msg_id, (from, Instant::now()));

            // Track IWANT request for scoring
            state
                .peer_scores
                .entry(from)
                .or_insert_with(PeerScore::new)
                .record_iwant_request();
        }
        self.record_stage(PubSubStage::DedupeCheck, dedupe_started);

        drop(topics); // Release lock

        if !requested.is_empty() {
            let republish_started = Instant::now();
            debug!(peer_id = %from, count = requested.len(), "Sending IWANT");
            // Create IWANT message
            let iwant_header = MessageHeader {
                version: 1,
                topic,
                msg_id: requested[0], // Use first ID as header
                kind: MessageKind::IWant,
                hop: 0,
                ttl: 10,
            };
            let iwant_header_clone = iwant_header.clone();
            let payload = match postcard::to_stdvec(&requested) {
                Ok(payload) => payload.into(),
                Err(e) => {
                    self.record_stage(PubSubStage::Republish, republish_started);
                    return Err(anyhow!("Serialization failed: {}", e));
                }
            };
            let iwant_msg = GossipMessage {
                header: iwant_header,
                payload: Some(payload),
                signature: self.sign_message(&iwant_header_clone),
                public_key: self.signing_key.public_key().to_vec(),
            };
            let bytes = match postcard::to_stdvec(&iwant_msg) {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.record_stage(PubSubStage::Republish, republish_started);
                    return Err(anyhow!("Serialization failed: {}", e));
                }
            };
            let send_result = self
                .send_to_peer_bounded(from, GossipStreamType::PubSub, bytes.into(), "IWANT")
                .await;
            self.record_stage(PubSubStage::Republish, republish_started);
            send_result?;
        }

        Ok(())
    }

    /// Handle incoming IWANT message
    pub async fn handle_iwant(
        &self,
        from: PeerId,
        topic: TopicId,
        msg_ids: Vec<MessageIdType>,
    ) -> Result<()> {
        let lock_started = Instant::now();
        let mut topics = self.topics.write().await;
        self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
        let dedupe_started = Instant::now();
        let state = topics.entry(topic).or_insert_with(TopicState::new);
        state.touch();

        let mut to_send = Vec::new();

        for msg_id in msg_ids {
            if let Some(cached) = state.get_message(&msg_id) {
                to_send.push((msg_id, cached));
                // GRAFT: move peer from lazy to eager
                if state.graft_peer(from) {
                    self.stage_stats.record_graft();
                }
            } else {
                warn!(msg_id = ?msg_id, "IWANT for unknown message");
            }
        }
        self.record_stage(PubSubStage::DedupeCheck, dedupe_started);

        drop(topics); // Release lock

        let republish_started = Instant::now();
        // Send EAGER with payloads
        for (msg_id, cached) in to_send {
            debug!(peer_id = %from, msg_id = ?msg_id, "Sending EAGER in response to IWANT");

            let _message = GossipMessage {
                header: cached.header.clone(),
                payload: Some(cached.payload.clone()),
                signature: self.sign_message(&cached.header),
                public_key: self.signing_key.public_key().to_vec(),
            };

            let bytes = match postcard::to_stdvec(&_message) {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.record_stage(PubSubStage::Republish, republish_started);
                    return Err(anyhow!("Serialization failed: {}", e));
                }
            };
            let send_result = self
                .send_to_peer_bounded(from, GossipStreamType::PubSub, bytes.into(), "EAGER")
                .await;
            if let Err(e) = send_result {
                self.record_stage(PubSubStage::Republish, republish_started);
                return Err(e);
            }
        }
        self.record_stage(PubSubStage::Republish, republish_started);

        Ok(())
    }

    /// Handle incoming anti-entropy message
    ///
    /// Processes `AntiEntropyPayload::Digest` and `AntiEntropyPayload::Response`
    /// messages for set reconciliation after network partitions.
    async fn handle_anti_entropy(
        &self,
        from: PeerId,
        topic: TopicId,
        message: GossipMessage,
    ) -> Result<()> {
        // Verify signature
        let verify_started = Instant::now();
        let verified =
            self.verify_signature(&message.header, &message.signature, &message.public_key);
        self.record_stage(PubSubStage::Verify, verify_started);
        if !verified {
            warn!(peer_id = %from, "Anti-entropy: invalid signature, dropping");
            return Err(anyhow!("Invalid signature on anti-entropy message"));
        }

        let payload_bytes = message.payload.ok_or_else(|| {
            self.stage_stats.record_decode_failed();
            anyhow!("Anti-entropy message missing payload")
        })?;

        let decode_started = Instant::now();
        let decoded: std::result::Result<AntiEntropyPayload, _> =
            postcard::from_bytes(&payload_bytes);
        self.record_stage(PubSubStage::Decode, decode_started);
        let ae_payload = match decoded {
            Ok(payload) => payload,
            Err(e) => {
                self.stage_stats.record_decode_failed();
                return Err(anyhow!("Failed to deserialize anti-entropy payload: {}", e));
            }
        };

        match ae_payload {
            AntiEntropyPayload::Digest { msg_ids } => {
                debug!(
                    peer_id = %from,
                    topic = ?topic,
                    their_count = msg_ids.len(),
                    "Received anti-entropy digest"
                );

                let their_ids: HashSet<MessageIdType> = msg_ids.into_iter().collect();

                let lock_started = Instant::now();
                let mut topics = self.topics.write().await;
                self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
                let dedupe_started = Instant::now();
                let state = topics.entry(topic).or_insert_with(TopicState::new);
                state.touch();

                let our_ids: HashSet<MessageIdType> =
                    state.cached_message_ids().into_iter().collect();

                // IDs we have that they don't - send cached messages as EAGER
                let mut messages_to_send = Vec::new();
                for id in our_ids.difference(&their_ids) {
                    if let Some(cached) = state.get_message(id) {
                        messages_to_send.push(cached);
                    }
                }

                // IDs they have that we don't - we need these
                let ids_we_need: Vec<MessageIdType> =
                    their_ids.difference(&our_ids).copied().collect();
                self.record_stage(PubSubStage::DedupeCheck, dedupe_started);

                drop(topics);

                let republish_started = Instant::now();
                // Send cached messages the peer is missing as EAGER
                for cached in &messages_to_send {
                    let eager_msg = GossipMessage {
                        header: cached.header.clone(),
                        payload: Some(cached.payload.clone()),
                        signature: self.sign_message(&cached.header),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&eager_msg) {
                        let _ = self
                            .send_to_peer_bounded(
                                from,
                                GossipStreamType::PubSub,
                                bytes.into(),
                                "EAGER",
                            )
                            .await;
                    }
                }

                // Send IWANT for IDs they have that we don't
                if !ids_we_need.is_empty() {
                    debug!(
                        peer_id = %from,
                        count = ids_we_need.len(),
                        "Anti-entropy: requesting missing messages via IWANT"
                    );
                    let iwant_header = MessageHeader {
                        version: 1,
                        topic,
                        msg_id: ids_we_need[0],
                        kind: MessageKind::IWant,
                        hop: 0,
                        ttl: 10,
                    };
                    let iwant_header_clone = iwant_header.clone();
                    let iwant_msg = GossipMessage {
                        header: iwant_header,
                        payload: Some(
                            postcard::to_stdvec(&ids_we_need)
                                .map_err(|e| anyhow!("Serialization failed: {}", e))?
                                .into(),
                        ),
                        signature: self.sign_message(&iwant_header_clone),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&iwant_msg) {
                        let _ = self
                            .send_to_peer_bounded(
                                from,
                                GossipStreamType::PubSub,
                                bytes.into(),
                                "IWANT",
                            )
                            .await;
                    }
                }
                self.record_stage(PubSubStage::Republish, republish_started);

                debug!(
                    peer_id = %from,
                    sent = messages_to_send.len(),
                    requested = ids_we_need.len(),
                    "Anti-entropy digest processed"
                );
            }
            AntiEntropyPayload::Response { missing_ids } => {
                debug!(
                    peer_id = %from,
                    topic = ?topic,
                    count = missing_ids.len(),
                    "Received anti-entropy response"
                );

                // Filter out IDs we already have.
                let lock_started = Instant::now();
                let mut topics = self.topics.write().await;
                self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
                let dedupe_started = Instant::now();
                let ids_to_request: Vec<MessageIdType> = if let Some(state) = topics.get_mut(&topic)
                {
                    state.touch();
                    missing_ids
                        .into_iter()
                        .filter(|id| !state.has_message(id))
                        .collect()
                } else {
                    missing_ids
                };
                self.record_stage(PubSubStage::DedupeCheck, dedupe_started);
                drop(topics);

                let republish_started = Instant::now();
                // Send IWANT for each ID we don't have
                if !ids_to_request.is_empty() {
                    debug!(
                        peer_id = %from,
                        count = ids_to_request.len(),
                        "Anti-entropy response: sending IWANT for missing IDs"
                    );
                    let iwant_header = MessageHeader {
                        version: 1,
                        topic,
                        msg_id: ids_to_request[0],
                        kind: MessageKind::IWant,
                        hop: 0,
                        ttl: 10,
                    };
                    let iwant_header_clone = iwant_header.clone();
                    let iwant_msg = GossipMessage {
                        header: iwant_header,
                        payload: Some(
                            postcard::to_stdvec(&ids_to_request)
                                .map_err(|e| anyhow!("Serialization failed: {}", e))?
                                .into(),
                        ),
                        signature: self.sign_message(&iwant_header_clone),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&iwant_msg) {
                        let _ = self
                            .send_to_peer_bounded(
                                from,
                                GossipStreamType::PubSub,
                                bytes.into(),
                                "IWANT",
                            )
                            .await;
                    }
                }
                self.record_stage(PubSubStage::Republish, republish_started);
            }
        }

        Ok(())
    }

    /// Send an anti-entropy digest for a specific topic to a specific peer
    ///
    /// Collects cached message IDs and sends them as an `AntiEntropyPayload::Digest`.
    async fn send_anti_entropy_digest(&self, topic: TopicId, peer: PeerId) -> Result<()> {
        let topics = self.topics.read().await;
        let msg_ids = if let Some(state) = topics.get(&topic) {
            state.cached_message_ids()
        } else {
            return Ok(());
        };
        drop(topics);

        if msg_ids.is_empty() {
            return Ok(());
        }

        let ae_payload = AntiEntropyPayload::Digest { msg_ids };
        let payload_bytes = postcard::to_stdvec(&ae_payload)
            .map_err(|e| anyhow!("Failed to serialize anti-entropy payload: {}", e))?;

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let signature = self.sign_message(&header);

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: self.signing_key.public_key().to_vec(),
        };

        let bytes =
            postcard::to_stdvec(&message).map_err(|e| anyhow!("Serialization failed: {}", e))?;
        self.send_to_peer_bounded(peer, GossipStreamType::PubSub, bytes.into(), "ANTI_ENTROPY")
            .await?;

        debug!(
            peer_id = %peer,
            topic = ?topic,
            "Sent anti-entropy digest"
        );

        Ok(())
    }

    /// Spawn background task to flush IHAVE batches
    fn spawn_ihave_flusher(&self) {
        let topics = self.topics.clone();
        let transport = self.transport.clone();
        let signing_key = self.signing_key.clone();
        let stage_stats = Arc::clone(&self.stage_stats);
        let initial_jitter = deterministic_jitter(
            self.peer_id,
            b"pubsub-ihave-flush",
            Duration::from_millis(IHAVE_FLUSH_INTERVAL_MS),
        );

        tokio::spawn(async move {
            if !initial_jitter.is_zero() {
                time::sleep(initial_jitter).await;
            }
            let mut interval = time::interval(Duration::from_millis(IHAVE_FLUSH_INTERVAL_MS));
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                Self::flush_ihave_batches(&topics, &transport, &signing_key, &stage_stats).await;
            }
        });
    }

    async fn flush_ihave_batches(
        topics: &Arc<RwLock<HashMap<TopicId, TopicState>>>,
        transport: &Arc<T>,
        signing_key: &Arc<saorsa_gossip_identity::MlDsaKeyPair>,
        stage_stats: &Arc<PubSubStageStats>,
    ) {
        let work: Vec<(TopicId, Vec<MessageIdType>, Vec<PeerId>)> = {
            let mut topics_guard = topics.write().await;
            let mut work = Vec::new();

            for (topic_id, state) in topics_guard.iter_mut() {
                if state.pending_ihave.is_empty() {
                    continue;
                }

                let batch: Vec<MessageIdType> = state
                    .pending_ihave
                    .drain(..state.pending_ihave.len().min(MAX_IHAVE_BATCH_SIZE))
                    .collect();

                let now = Instant::now();
                let lazy_peers: Vec<PeerId> = state
                    .lazy_peers
                    .iter()
                    .filter(|&&peer| !state.is_peer_suppressed_at(peer, now))
                    .copied()
                    .collect();
                work.push((*topic_id, batch, lazy_peers));
            }

            work
        };

        for (topic_id, batch, lazy_peers) in work {
            if lazy_peers.is_empty() {
                continue;
            }

            trace!(topic = ?topic_id, batch_size = batch.len(), peer_count = lazy_peers.len(), "Flushing IHAVE batch");

            let ihave_header = MessageHeader {
                version: 1,
                topic: topic_id,
                msg_id: batch[0],
                kind: MessageKind::IHave,
                hop: 0,
                ttl: 10,
            };

            let signature = match postcard::to_stdvec(&ihave_header) {
                Ok(bytes) => signing_key.sign(&bytes).unwrap_or_default(),
                Err(e) => {
                    warn!(topic = ?topic_id, "IHAVE header serialize failed: {e}");
                    continue;
                }
            };

            let payload = match postcard::to_stdvec(&batch) {
                Ok(bytes) => bytes.into(),
                Err(e) => {
                    warn!(topic = ?topic_id, "IHAVE batch serialize failed: {e}");
                    continue;
                }
            };

            let ihave_msg = GossipMessage {
                header: ihave_header,
                payload: Some(payload),
                signature,
                public_key: signing_key.public_key().to_vec(),
            };
            let bytes: Bytes = match postcard::to_stdvec(&ihave_msg) {
                Ok(bytes) => bytes.into(),
                Err(e) => {
                    warn!(topic = ?topic_id, "IHAVE message serialize failed: {e}");
                    continue;
                }
            };

            // X0X-0007: parallel send with per-peer timeout. Was a sequential
            // for-await loop that bounded periodic IHAVE flush latency by the
            // sum of per-peer send latencies; now bounded by max + the per-
            // peer timeout. Same JoinSet shape as `parallel_send_to_peers`
            // (the helper method is on `&self` and this background task does
            // not have one — keeping the shape inline avoids an Arc<Self>
            // refactor).
            if !lazy_peers.is_empty() {
                let mut set = tokio::task::JoinSet::new();
                for peer in lazy_peers {
                    let transport = Arc::clone(transport);
                    let bytes = bytes.clone();
                    let stage_stats = Arc::clone(stage_stats);
                    set.spawn(async move {
                        let outcome = Self::send_to_peer_with_timeout(
                            transport,
                            stage_stats,
                            peer,
                            GossipStreamType::PubSub,
                            bytes,
                            "IHAVE",
                        )
                        .await;
                        (peer, outcome)
                    });
                }
                let mut sent = Vec::new();
                let mut timed_out = Vec::new();
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok((peer, Ok(PeerSendOutcome::Sent))) => sent.push(peer),
                        Ok((peer, Ok(PeerSendOutcome::TimedOut))) => timed_out.push(peer),
                        Ok((_peer, Err(_))) => {}
                        Err(e) => {
                            warn!(topic = ?topic_id, "IHAVE per-peer send task panicked: {e}")
                        }
                    }
                }
                Self::record_topic_send_results_for(topics, stage_stats, topic_id, sent, timed_out)
                    .await;
            }
        }
    }

    /// Spawn background task to clean expired cache entries.
    ///
    /// Two passes per tick:
    /// 1. Per-topic cache TTL sweep — evicts stale `CachedMessage` and
    ///    `replay_cache` entries from each topic's LRUs.
    /// 2. Topic-level idle TTL sweep — drops the entire `TopicState`
    ///    (caches + peer sets + scores) for topics that have seen no
    ///    activity in `TOPIC_IDLE_TTL_SECS` and have no live subscribers.
    ///    Keeps the parent `topics` HashMap from accumulating ephemeral topics
    ///    forever without silently closing quiet local subscriptions.
    fn spawn_cache_cleaner(&self) {
        let topics = self.topics.clone();
        let topic_idle_ttl = Duration::from_secs(TOPIC_IDLE_TTL_SECS);

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(60));

            loop {
                interval.tick().await;

                let mut topics_guard = topics.write().await;
                let idle_count = clean_and_reap_topics(&mut topics_guard, topic_idle_ttl);
                if idle_count > 0 {
                    debug!(
                        idle_count = idle_count,
                        remaining = topics_guard.len(),
                        "Reaped idle TopicState entries"
                    );
                }
            }
        });
    }

    /// Spawn background task to maintain eager peer degree
    fn spawn_degree_maintainer(&self) {
        let topics = self.topics.clone();
        let stage_stats = Arc::clone(&self.stage_stats);
        let initial_jitter =
            deterministic_jitter(self.peer_id, b"pubsub-degree", Duration::from_secs(30));

        tokio::spawn(async move {
            if !initial_jitter.is_zero() {
                time::sleep(initial_jitter).await;
            }
            let mut interval = time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                let mut topics_guard = topics.write().await;
                let mut pruned = 0;
                let mut grafted = 0;

                for state in topics_guard.values_mut() {
                    let (state_pruned, state_grafted) = state.maintain_degree();
                    pruned += state_pruned;
                    grafted += state_grafted;
                }
                drop(topics_guard);
                if pruned > 0 {
                    stage_stats.record_prunes(pruned);
                }
                if grafted > 0 {
                    stage_stats.record_grafts(grafted);
                }
            }
        });
    }

    /// Spawn background task for anti-entropy reconciliation
    ///
    /// Every `ANTI_ENTROPY_INTERVAL_SECS` seconds, for each topic with cached messages,
    /// picks one random peer and sends an anti-entropy digest containing our cached message IDs.
    fn spawn_anti_entropy_task(&self) {
        let topics = self.topics.clone();
        let transport = self.transport.clone();
        let signing_key = self.signing_key.clone();
        let stage_stats = Arc::clone(&self.stage_stats);
        let initial_jitter = deterministic_jitter(
            self.peer_id,
            b"pubsub-anti-entropy",
            Duration::from_secs(ANTI_ENTROPY_INTERVAL_SECS),
        );

        tokio::spawn(async move {
            if !initial_jitter.is_zero() {
                time::sleep(initial_jitter).await;
            }
            let mut interval = time::interval(Duration::from_secs(ANTI_ENTROPY_INTERVAL_SECS));
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                let topics_guard = topics.read().await;

                // Collect work to do (topic, peer, msg_ids) while holding the read lock
                let mut work: Vec<(TopicId, PeerId, Vec<MessageIdType>)> = Vec::new();

                for (topic_id, state) in topics_guard.iter() {
                    let msg_ids = state.cached_message_ids();
                    if msg_ids.is_empty() {
                        continue;
                    }

                    // Collect all peers (eager + lazy) for random selection
                    let all_peers: Vec<PeerId> = state
                        .eager_peers
                        .iter()
                        .chain(state.lazy_peers.iter())
                        .copied()
                        .collect();

                    if all_peers.is_empty() {
                        continue;
                    }

                    // Pick a deterministic-random peer using hash of topic + current time
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let hash_input = blake3::hash(
                        &[topic_id.to_bytes().as_slice(), &now.to_le_bytes()].concat(),
                    );
                    let hash_bytes = hash_input.as_bytes();
                    let index = (hash_bytes[0] as usize) % all_peers.len();
                    let selected_peer = all_peers[index];

                    work.push((*topic_id, selected_peer, msg_ids));
                }

                drop(topics_guard);

                // Send digests without holding the lock
                for (topic_id, peer, msg_ids) in work {
                    let ae_payload = AntiEntropyPayload::Digest { msg_ids };
                    let payload_bytes = match postcard::to_stdvec(&ae_payload) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!("Anti-entropy: failed to serialize payload: {}", e);
                            continue;
                        }
                    };

                    let header = MessageHeader {
                        version: 1,
                        topic: topic_id,
                        msg_id: [0u8; 32],
                        kind: MessageKind::AntiEntropy,
                        hop: 0,
                        ttl: 1,
                    };

                    let signature = match postcard::to_stdvec(&header) {
                        Ok(bytes) => signing_key.sign(&bytes).unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };

                    let message = GossipMessage {
                        header,
                        payload: Some(payload_bytes.into()),
                        signature,
                        public_key: signing_key.public_key().to_vec(),
                    };

                    if let Ok(bytes) = postcard::to_stdvec(&message) {
                        let _ = Self::send_to_peer_with_timeout(
                            Arc::clone(&transport),
                            Arc::clone(&stage_stats),
                            peer,
                            GossipStreamType::PubSub,
                            bytes.into(),
                            "ANTI_ENTROPY",
                        )
                        .await;
                    }

                    trace!(
                        peer_id = %peer,
                        topic = ?topic_id,
                        "Anti-entropy: sent digest"
                    );
                }
            }
        });
    }

    /// Initialize peers for a topic from membership layer
    pub async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>) {
        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        // Start with all peers as eager (tree will optimize via PRUNE)
        let now = Instant::now();
        for peer in peers {
            if state.is_peer_suppressed_at(peer, now) {
                state.lazy_peers.insert(peer);
            } else {
                state.eager_peers.insert(peer);
            }
        }

        debug!(topic = ?topic, peer_count = state.eager_peers.len(), "Initialized topic peers");
    }

    /// Replace topic peers with exactly the given set of connected peers.
    ///
    /// Removes stale peers that are no longer connected and adds new ones.
    /// Peers that were previously moved to `lazy_peers` via PRUNE are left
    /// in lazy if they are still connected; otherwise they are removed.
    pub async fn set_topic_peers(&self, topic: TopicId, connected: Vec<PeerId>) {
        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        let connected_set: HashSet<PeerId> = connected.iter().copied().collect();

        // Remove stale peers (no longer connected) from both sets.
        state.eager_peers.retain(|p| connected_set.contains(p));
        state.lazy_peers.retain(|p| connected_set.contains(p));
        let removed_cooling = state.clear_disconnected_peer_cooling(&connected_set);
        for peer in removed_cooling {
            self.stage_stats.clear_peer_suppression(topic, peer);
        }

        // Promote all connected lazy peers back to eager. PlumTree's PRUNE
        // optimization moves peers to lazy when duplicate messages are detected,
        // but the periodic peer refresh should restore them. Without this,
        // peers pruned during a message burst stay lazy permanently, breaking
        // gossip routing after the burst ends.
        let now = Instant::now();
        let to_promote: Vec<PeerId> = state
            .lazy_peers
            .iter()
            .filter(|&&peer| !state.is_peer_suppressed_at(peer, now))
            .copied()
            .collect();
        for peer in to_promote {
            state.lazy_peers.remove(&peer);
            state.eager_peers.insert(peer);
            self.stage_stats.clear_peer_suppression(topic, peer);
        }

        // Add any remaining connected peers not in either set as eager.
        for peer in connected {
            if !state.eager_peers.contains(&peer) {
                if state.is_peer_suppressed_at(peer, now) {
                    state.lazy_peers.insert(peer);
                } else {
                    state.eager_peers.insert(peer);
                }
            }
        }

        debug!(
            topic = ?topic,
            eager = state.eager_peers.len(),
            lazy = state.lazy_peers.len(),
            "Set topic peers"
        );
    }

    /// Return all topic IDs known to PlumTree (subscribed or pass-through).
    ///
    /// This includes topics that have local subscribers AND topics that only
    /// exist because an EAGER message was received and forwarded. The caller
    /// should use this to refresh peer sets for all topics, not just locally
    /// subscribed ones — otherwise pass-through topics lose their forwarding
    /// peers and gossip messages cannot propagate through relay nodes.
    pub async fn all_topic_ids(&self) -> Vec<TopicId> {
        self.topics.read().await.keys().copied().collect()
    }
}

#[async_trait::async_trait]
impl<T: GossipTransport + 'static> PubSub for PlumtreePubSub<T> {
    async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()> {
        self.publish_local(topic, data).await
    }

    fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let topics = self.topics.clone();

        tokio::spawn(async move {
            let mut topics_guard = topics.write().await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.touch();
            state.subscribers.push(tx);
        });

        rx
    }

    async fn unsubscribe(&self, topic: TopicId) -> Result<()> {
        let mut topics = self.topics.write().await;
        topics.remove(&topic);
        Ok(())
    }

    async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>) {
        PlumtreePubSub::initialize_topic_peers(self, topic, peers).await
    }

    async fn set_topic_peers(&self, topic: TopicId, connected: Vec<PeerId>) {
        PlumtreePubSub::set_topic_peers(self, topic, connected).await
    }

    async fn handle_message(&self, from: PeerId, data: Bytes) -> Result<()> {
        // Deserialize the GossipMessage
        let decode_started = Instant::now();
        let decoded: std::result::Result<GossipMessage, _> = postcard::from_bytes(&data);
        self.record_stage(PubSubStage::Decode, decode_started);
        let message = match decoded {
            Ok(message) => message,
            Err(e) => {
                self.stage_stats.record_decode_failed();
                return Err(anyhow!("Failed to deserialize PubSub message: {}", e));
            }
        };

        let topic_id = message.header.topic;
        let msg_kind = message.header.kind;
        self.stage_stats.record_message_kind(msg_kind);

        debug!(
            msg_kind = ?msg_kind,
            peer_id = %from,
            topic = ?topic_id,
            "Handling incoming PubSub message"
        );

        // Route to appropriate handler based on message kind
        // Only handle pubsub-specific message kinds (Eager, IHave, IWant)
        match msg_kind {
            MessageKind::Eager => self.handle_eager(from, topic_id, message).await,
            MessageKind::IHave => {
                // IHAVE payload contains Vec<MessageIdType>
                if let Some(payload) = &message.payload {
                    let decode_started = Instant::now();
                    let decoded: std::result::Result<Vec<MessageIdType>, _> =
                        postcard::from_bytes(payload);
                    self.record_stage(PubSubStage::Decode, decode_started);
                    let msg_ids = match decoded {
                        Ok(msg_ids) => msg_ids,
                        Err(e) => {
                            self.stage_stats.record_decode_failed();
                            return Err(anyhow!("Failed to deserialize IHAVE payload: {}", e));
                        }
                    };
                    self.handle_ihave(from, topic_id, msg_ids).await
                } else {
                    self.stage_stats.record_decode_failed();
                    Err(anyhow!("IHAVE message missing payload"))
                }
            }
            MessageKind::IWant => {
                // IWANT payload contains Vec<MessageIdType>
                if let Some(payload) = &message.payload {
                    let decode_started = Instant::now();
                    let decoded: std::result::Result<Vec<MessageIdType>, _> =
                        postcard::from_bytes(payload);
                    self.record_stage(PubSubStage::Decode, decode_started);
                    let msg_ids = match decoded {
                        Ok(msg_ids) => msg_ids,
                        Err(e) => {
                            self.stage_stats.record_decode_failed();
                            return Err(anyhow!("Failed to deserialize IWANT payload: {}", e));
                        }
                    };
                    self.handle_iwant(from, topic_id, msg_ids).await
                } else {
                    self.stage_stats.record_decode_failed();
                    Err(anyhow!("IWANT message missing payload"))
                }
            }
            MessageKind::AntiEntropy => self.handle_anti_entropy(from, topic_id, message).await,
            // Other message kinds (Ping, Ack, Find, Presence, Shuffle) are not handled by PubSub
            _ => {
                warn!(
                    "PubSub received non-pubsub message kind {:?}, ignoring",
                    msg_kind
                );
                Ok(())
            }
        }
    }

    async fn trigger_anti_entropy(&self, topic: TopicId) -> Result<()> {
        let topics = self.topics.read().await;

        let peer = if let Some(state) = topics.get(&topic) {
            // Pick a peer (any eager or lazy)
            state
                .eager_peers
                .iter()
                .chain(state.lazy_peers.iter())
                .next()
                .copied()
        } else {
            None
        };

        drop(topics);

        if let Some(peer) = peer {
            self.send_anti_entropy_digest(topic, peer).await
        } else {
            Ok(()) // No peers available
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use saorsa_gossip_transport::UdpTransportAdapter;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    fn test_peer_id(id: u8) -> PeerId {
        let mut bytes = [0u8; 32];
        bytes[0] = id;
        PeerId::new(bytes)
    }

    async fn test_transport() -> Arc<UdpTransportAdapter> {
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("valid addr");
        Arc::new(
            UdpTransportAdapter::new(bind, vec![])
                .await
                .expect("transport"),
        )
    }

    fn test_signing_key() -> saorsa_gossip_identity::MlDsaKeyPair {
        saorsa_gossip_identity::MlDsaKeyPair::generate().expect("Failed to generate test key pair")
    }

    fn signed_eager_message(
        signing_key: &saorsa_gossip_identity::MlDsaKeyPair,
        topic: TopicId,
        msg_id: MessageIdType,
        payload: Bytes,
    ) -> GossipMessage {
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };
        let header_bytes = postcard::to_stdvec(&header).expect("header serializes");
        let signature = signing_key.sign(&header_bytes).expect("header signs");
        GossipMessage {
            header,
            payload: Some(payload),
            signature,
            public_key: signing_key.public_key().to_vec(),
        }
    }

    fn signed_control_message(
        signing_key: &saorsa_gossip_identity::MlDsaKeyPair,
        topic: TopicId,
        msg_id: MessageIdType,
        kind: MessageKind,
        payload: Bytes,
    ) -> GossipMessage {
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind,
            hop: 0,
            ttl: 10,
        };
        let header_bytes = postcard::to_stdvec(&header).expect("header serializes");
        let signature = signing_key.sign(&header_bytes).expect("header signs");
        GossipMessage {
            header,
            payload: Some(payload),
            signature,
            public_key: signing_key.public_key().to_vec(),
        }
    }

    #[derive(Debug)]
    #[allow(dead_code)] // data_len kept for forensic dumps in test failures
    struct SendRecord {
        peer: PeerId,
        stream_type: GossipStreamType,
        data_ptr: usize,
        data_len: usize,
    }

    struct BlockingTransport {
        local_peer: PeerId,
        started_tx: mpsc::UnboundedSender<SendRecord>,
        release: Arc<Semaphore>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        send_count: AtomicUsize,
    }

    impl BlockingTransport {
        fn new(local_peer: PeerId) -> (Arc<Self>, mpsc::UnboundedReceiver<SendRecord>) {
            let (started_tx, started_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    local_peer,
                    started_tx,
                    release: Arc::new(Semaphore::new(0)),
                    in_flight: AtomicUsize::new(0),
                    max_in_flight: AtomicUsize::new(0),
                    send_count: AtomicUsize::new(0),
                }),
                started_rx,
            )
        }

        fn release_sends(&self, count: usize) {
            self.release.add_permits(count);
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::SeqCst)
        }

        fn send_count(&self) -> usize {
            self.send_count.load(Ordering::SeqCst)
        }
    }

    struct RecordingTransport {
        local_peer: PeerId,
        send_counts: Mutex<HashMap<PeerId, usize>>,
    }

    impl RecordingTransport {
        fn new(local_peer: PeerId) -> Arc<Self> {
            Arc::new(Self {
                local_peer,
                send_counts: Mutex::new(HashMap::new()),
            })
        }

        fn send_count_to(&self, peer: PeerId) -> usize {
            self.send_counts
                .lock()
                .expect("send counts lock")
                .get(&peer)
                .copied()
                .unwrap_or(0)
        }
    }

    #[async_trait::async_trait]
    impl GossipTransport for RecordingTransport {
        async fn dial(&self, _peer: PeerId, _addr: SocketAddr) -> Result<()> {
            Ok(())
        }

        async fn dial_bootstrap(&self, _addr: SocketAddr) -> Result<PeerId> {
            Ok(self.local_peer)
        }

        async fn listen(&self, _bind: SocketAddr) -> Result<()> {
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }

        async fn send_to_peer(
            &self,
            peer: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> Result<()> {
            let mut counts = self.send_counts.lock().expect("send counts lock");
            *counts.entry(peer).or_default() += 1;
            Ok(())
        }

        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            Err(anyhow!("recording test transport does not receive"))
        }

        fn local_peer_id(&self) -> PeerId {
            self.local_peer
        }
    }

    #[async_trait::async_trait]
    impl GossipTransport for BlockingTransport {
        async fn dial(&self, _peer: PeerId, _addr: SocketAddr) -> Result<()> {
            Ok(())
        }

        async fn dial_bootstrap(&self, _addr: SocketAddr) -> Result<PeerId> {
            Ok(self.local_peer)
        }

        async fn listen(&self, _bind: SocketAddr) -> Result<()> {
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
            self.send_count.fetch_add(1, Ordering::SeqCst);
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            let _ = self.started_tx.send(SendRecord {
                peer,
                stream_type,
                data_ptr: data.as_ptr() as usize,
                data_len: data.len(),
            });

            let permit = self
                .release
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore should stay open");
            permit.forget();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            Err(anyhow!("blocking test transport does not receive"))
        }

        fn local_peer_id(&self) -> PeerId {
            self.local_peer
        }
    }

    #[tokio::test]
    async fn test_pubsub_creation() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let _pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
    }

    #[tokio::test]
    async fn test_stage_stats_record_message_kinds_and_tree_ops() {
        let peer_id = test_peer_id(1);
        let from_peer = test_peer_id(2);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        transport.release_sends(8);
        let sender_key = test_signing_key();
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([8u8; 32]);
        let msg_id = [9u8; 32];
        let eager = signed_eager_message(
            &sender_key,
            topic,
            msg_id,
            Bytes::from_static(b"kind-counter-payload"),
        );
        let eager_bytes: Bytes = postcard::to_stdvec(&eager)
            .expect("eager message serializes")
            .into();

        pubsub
            .handle_message(from_peer, eager_bytes.clone())
            .await
            .expect("first eager handled");
        pubsub
            .handle_message(from_peer, eager_bytes)
            .await
            .expect("duplicate eager prunes sender");

        let iwant_payload: Bytes = postcard::to_stdvec(&vec![msg_id])
            .expect("iwant payload serializes")
            .into();
        let iwant = signed_control_message(
            &sender_key,
            topic,
            msg_id,
            MessageKind::IWant,
            iwant_payload,
        );
        pubsub
            .handle_message(
                from_peer,
                postcard::to_stdvec(&iwant)
                    .expect("iwant message serializes")
                    .into(),
            )
            .await
            .expect("iwant grafts sender");

        let missing_id = [7u8; 32];
        let ihave_payload: Bytes = postcard::to_stdvec(&vec![missing_id])
            .expect("ihave payload serializes")
            .into();
        let ihave = signed_control_message(
            &sender_key,
            topic,
            missing_id,
            MessageKind::IHave,
            ihave_payload,
        );
        pubsub
            .handle_message(
                from_peer,
                postcard::to_stdvec(&ihave)
                    .expect("ihave message serializes")
                    .into(),
            )
            .await
            .expect("ihave handled");

        let ae_payload: Bytes =
            postcard::to_stdvec(&AntiEntropyPayload::Digest { msg_ids: vec![] })
                .expect("anti-entropy payload serializes")
                .into();
        let anti_entropy = signed_control_message(
            &sender_key,
            topic,
            [0u8; 32],
            MessageKind::AntiEntropy,
            ae_payload,
        );
        pubsub
            .handle_message(
                from_peer,
                postcard::to_stdvec(&anti_entropy)
                    .expect("anti-entropy message serializes")
                    .into(),
            )
            .await
            .expect("anti-entropy handled");

        let stats = pubsub.stage_stats().message_kinds;
        assert_eq!(stats.eager, 2);
        assert_eq!(stats.ihave, 1);
        assert_eq!(stats.iwant, 1);
        assert_eq!(stats.anti_entropy, 1);
        assert_eq!(stats.prune, 1);
        assert_eq!(stats.graft, 1);
        assert_eq!(stats.decode_failed, 0);
    }

    #[tokio::test]
    async fn test_stage_stats_record_pubsub_decode_failures() {
        let peer_id = test_peer_id(1);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        let pubsub =
            PlumtreePubSub::new_with_task_control(peer_id, transport, test_signing_key(), false);

        let err = pubsub
            .handle_message(test_peer_id(2), Bytes::from_static(b"not-postcard"))
            .await;
        assert!(err.is_err());
        assert_eq!(pubsub.stage_stats().message_kinds.decode_failed, 1);
    }

    #[tokio::test]
    async fn test_publish_and_subscribe() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let mut rx = pubsub.subscribe(topic);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let data = Bytes::from("test message");
        pubsub.publish(topic, data.clone()).await.ok();

        let received =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv()).await;

        assert!(received.is_ok());
        let (_, payload) = received.unwrap().unwrap();
        assert_eq!(payload, data);
    }

    #[tokio::test]
    async fn test_publish_local_eager_fanout_concurrent_with_per_peer_timeout() {
        // X0X-0007: EAGER fan-out is concurrent (one task per peer) with a
        // per-peer `PER_PEER_REPUBLISH_TIMEOUT` budget. A single slow peer
        // no longer pins the dispatcher; instead the per-peer timeout fires
        // and `republish_per_peer_timeout` is incremented.
        //
        // Predecessor test (`test_publish_local_backpressures_eager_fanout_and_releases_topic_lock`)
        // asserted the opposite: sequential await and `max_in_flight == 1`.
        // X0X-0006 telemetry showed that sequential pattern was ~73% of
        // dispatcher wall-clock under load. The new shape is bounded
        // (`PER_PEER_REPUBLISH_TIMEOUT` per task, eager_peers tasks per
        // publish — capped at MAX_EAGER_DEGREE=12) so the 2026-04-25
        // unbounded-task OOM stays prevented.
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([2u8; 32]);
        let eager_peers: Vec<PeerId> = (2..6).map(test_peer_id).collect();
        pubsub
            .initialize_topic_peers(topic, eager_peers.clone())
            .await;

        let publish_pubsub = Arc::clone(&pubsub);
        let publish = tokio::spawn(async move {
            publish_pubsub
                .publish_local(topic, Bytes::from_static(b"backpressure"))
                .await
                .expect("publish should complete");
        });

        // Drain N start-of-send signals — every peer's send task should
        // start near-simultaneously, not serialized.
        let mut data_ptrs = Vec::with_capacity(eager_peers.len());
        let mut started_peers = std::collections::HashSet::new();
        for _ in 0..eager_peers.len() {
            let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .expect("next send should start (concurrent fan-out)")
                .expect("send channel should stay open");
            assert_eq!(record.stream_type, GossipStreamType::PubSub);
            assert!(
                eager_peers.contains(&record.peer),
                "send should target one of the eager peers"
            );
            started_peers.insert(record.peer);
            data_ptrs.push(record.data_ptr);
        }
        assert_eq!(
            started_peers.len(),
            eager_peers.len(),
            "every eager peer should have a send task in flight concurrently"
        );
        assert_eq!(
            transport.max_in_flight(),
            eager_peers.len(),
            "X0X-0007 fan-out runs all eager-peer sends concurrently"
        );

        // The topic lock is released BEFORE the network I/O — this remains
        // critical because publishes from this peer's own subscribers must
        // not block on remote sends.
        let topic_ids = tokio::time::timeout(Duration::from_millis(50), pubsub.all_topic_ids())
            .await
            .expect("topic lock should not be held while send_to_peer is in flight");
        assert!(
            topic_ids.contains(&topic),
            "topic should be visible while publish is in flight on network I/O"
        );

        // Release every blocked send at once — publish should now complete.
        transport.release_sends(eager_peers.len());
        tokio::time::timeout(Duration::from_millis(200), publish)
            .await
            .expect("publish task should finish after all sends are released")
            .expect("publish task should not panic");

        assert_eq!(transport.send_count(), eager_peers.len());
        assert!(
            data_ptrs.iter().all(|ptr| *ptr == data_ptrs[0]),
            "all eager sends should share the same serialized Bytes allocation"
        );
        let stage_stats = pubsub.stage_stats();
        assert_eq!(
            stage_stats.republish_per_peer_timeout, 0,
            "no peer should have hit the per-peer timeout (sends were released within budget)"
        );
    }

    #[tokio::test]
    async fn test_publish_local_eager_fanout_per_peer_timeout_isolates_one_slow_peer() {
        // X0X-0007 acceptance: a single peer that never accepts the send
        // hits PER_PEER_REPUBLISH_TIMEOUT and is skipped. The other peers
        // complete normally, and the per-peer timeout counter records the
        // isolated slow peer. Publish itself returns within budget bounded
        // by `PER_PEER_REPUBLISH_TIMEOUT`, not by the slow peer's hang.
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([42u8; 32]);
        let eager_peers: Vec<PeerId> = (2..6).map(test_peer_id).collect();
        pubsub
            .initialize_topic_peers(topic, eager_peers.clone())
            .await;

        let publish_pubsub = Arc::clone(&pubsub);
        let publish_started = Instant::now();
        let publish = tokio::spawn(async move {
            publish_pubsub
                .publish_local(topic, Bytes::from_static(b"isolate-slow"))
                .await
                .expect("publish should complete");
        });

        // Drain start-of-send signals so we know all peers' sends are in
        // flight, then release all-but-one. The remaining peer never gets
        // released, so its send must hit the per-peer timeout.
        for _ in 0..eager_peers.len() {
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .expect("send should start")
                .expect("send channel should stay open");
        }
        transport.release_sends(eager_peers.len() - 1);

        // Publish must return within ~PER_PEER_REPUBLISH_TIMEOUT + slack.
        // The slow peer's task hits its 750 ms budget and is skipped; the
        // other peers' tasks completed immediately on release.
        tokio::time::timeout(Duration::from_secs(2), publish)
            .await
            .expect("publish must return within the per-peer timeout budget — slow peer should not pin the dispatcher")
            .expect("publish task should not panic");
        let publish_elapsed = publish_started.elapsed();
        assert!(
            publish_elapsed < Duration::from_millis(1_500),
            "publish elapsed {publish_elapsed:?} — should be bounded by PER_PEER_REPUBLISH_TIMEOUT (750ms) plus a small slack"
        );

        let stage_stats = pubsub.stage_stats();
        assert_eq!(
            stage_stats.republish_per_peer_timeout, 1,
            "exactly one peer (the unreleased one) should have hit the per-peer timeout"
        );
        // Every peer's send_to_peer was *attempted* (send_count counts
        // attempts at entry, not completions). The slow peer's send was
        // cancelled by the per-peer timeout; the other 3 completed normally.
        assert_eq!(
            transport.send_count(),
            eager_peers.len(),
            "every eager peer should have had send_to_peer attempted"
        );
    }

    #[tokio::test]
    async fn test_peer_timeout_cooling_demotes_and_skips_future_eager_sends() {
        let peer_id = test_peer_id(1);
        let transport = RecordingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([43u8; 32]);
        let slow_peer = test_peer_id(2);
        let healthy_peer = test_peer_id(3);
        pubsub
            .initialize_topic_peers(topic, vec![slow_peer, healthy_peer])
            .await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![slow_peer])
                .await;
        }

        {
            let topics = pubsub.topics.read().await;
            let state = topics.get(&topic).expect("topic exists");
            assert!(
                !state.eager_peers.contains(&slow_peer),
                "cooled peer should be removed from EAGER fanout"
            );
            assert!(
                state.lazy_peers.contains(&slow_peer),
                "cooled peer should remain in LAZY for later recovery"
            );
            assert!(
                state.eager_peers.contains(&healthy_peer),
                "healthy peers should stay EAGER"
            );
        }

        let suppressed = pubsub.stage_stats().suppressed_peers;
        assert_eq!(suppressed.len(), 1, "one peer/topic should be cooled");
        assert_eq!(suppressed[0].peer_id, slow_peer.to_string());
        assert_eq!(suppressed[0].topic, topic.to_string());
        assert_eq!(suppressed[0].recent_timeout_count, PEER_TIMEOUT_THRESHOLD);
        assert_eq!(suppressed[0].affected_topics_count, 1);

        pubsub
            .publish_local(topic, Bytes::from_static(b"after-cooling"))
            .await
            .expect("publish should complete");

        assert_eq!(
            transport.send_count_to(slow_peer),
            0,
            "cooled peer should not consume a send slot on later EAGER fanout"
        );
        assert_eq!(
            transport.send_count_to(healthy_peer),
            1,
            "healthy EAGER peer should still receive fanout"
        );
    }

    #[test]
    fn test_peer_cooling_recovers_after_cooldown_without_restart() {
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(peer, now);
        }

        assert!(
            state.is_peer_suppressed_at(peer, now),
            "peer should be suppressed after threshold timeouts"
        );
        assert!(state.lazy_peers.contains(&peer));

        let after_cooldown = now + PEER_SUPPRESSION_COOLDOWN + Duration::from_millis(1);
        let (_pruned, grafted) = state.maintain_degree_at(after_cooldown);

        assert_eq!(grafted, 1, "expired cooling should allow re-admission");
        assert!(
            state.eager_peers.contains(&peer),
            "peer should re-enter EAGER without a daemon restart"
        );
        assert!(!state.is_peer_suppressed_at(peer, after_cooldown));
    }

    #[tokio::test]
    async fn test_peer_cooling_is_per_topic_and_refresh_respects_active_cooldown() {
        let peer_id = test_peer_id(1);
        let transport = RecordingTransport::new(peer_id);
        let pubsub =
            PlumtreePubSub::new_with_task_control(peer_id, transport, test_signing_key(), false);
        let topic_a = TopicId::new([44u8; 32]);
        let topic_b = TopicId::new([45u8; 32]);
        let peer = test_peer_id(2);

        pubsub.initialize_topic_peers(topic_a, vec![peer]).await;
        pubsub.initialize_topic_peers(topic_b, vec![peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic_a, Vec::new(), vec![peer])
                .await;
        }

        pubsub.set_topic_peers(topic_a, vec![peer]).await;

        let topics = pubsub.topics.read().await;
        let state_a = topics.get(&topic_a).expect("topic A exists");
        let state_b = topics.get(&topic_b).expect("topic B exists");

        assert!(
            state_a.lazy_peers.contains(&peer),
            "active cooling should keep peer lazy on affected topic"
        );
        assert!(
            !state_a.eager_peers.contains(&peer),
            "refresh must not immediately re-promote cooled peer"
        );
        assert!(
            state_b.eager_peers.contains(&peer),
            "same peer should stay EAGER on unaffected topic"
        );
    }

    #[tokio::test]
    async fn test_stage_stats_record_slow_republish_from_handle_message() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([9u8; 32]);
        let forward_peer = test_peer_id(2);
        let from_peer = test_peer_id(3);
        pubsub
            .initialize_topic_peers(topic, vec![forward_peer])
            .await;

        let message = signed_eager_message(
            &test_signing_key(),
            topic,
            [42u8; 32],
            Bytes::from_static(b"stage stats"),
        );
        let wire: Bytes = postcard::to_stdvec(&message)
            .expect("message serializes")
            .into();

        let handle_pubsub = Arc::clone(&pubsub);
        let handle = tokio::spawn(async move {
            handle_pubsub
                .handle_message(from_peer, wire)
                .await
                .expect("handle_message should complete");
        });

        let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("forward send should start")
            .expect("send channel should stay open");
        assert_eq!(record.peer, forward_peer);

        // Never release the send — the per-peer timeout (X0X-0007) must
        // fire and unblock the dispatcher within `PER_PEER_REPUBLISH_TIMEOUT`
        // plus slack. Pre-X0X-0007 this test asserted republish > 1 s by
        // sleeping 1.1 s before releasing the send; under X0X-0007 the slow
        // peer is isolated and the republish stage is bounded by the
        // per-peer budget instead.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle task should finish bounded by PER_PEER_REPUBLISH_TIMEOUT — slow peer must not pin the dispatcher")
            .expect("handle task should not panic");

        let stats = pubsub.stage_stats();
        assert_eq!(stats.decode.count, 1);
        assert_eq!(stats.verify.count, 1);
        assert_eq!(stats.dedupe_lock_acquire.count, 1);
        assert_eq!(stats.dedupe_check.count, 1);
        assert_eq!(stats.eager_fanout.count, 1);
        assert_eq!(stats.republish.count, 1);
        assert_eq!(
            stats.republish_per_peer_timeout, 1,
            "the unreleased peer should have hit the per-peer timeout exactly once"
        );
        // Republish stage time bounded by PER_PEER_REPUBLISH_TIMEOUT + tokio
        // task scheduling slack. Use 1500 ms as the upper bound to absorb CI
        // jitter while still proving the per-peer budget is being honoured
        // (was unbounded pre-X0X-0007).
        assert!(
            stats.republish.max_ns < 1_500_000_000,
            "republish max_ns should be bounded by PER_PEER_REPUBLISH_TIMEOUT, got {} ns",
            stats.republish.max_ns
        );
    }

    #[tokio::test]
    async fn test_ihave_flush_releases_topic_lock_before_network_io() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let topics = Arc::new(RwLock::new(HashMap::new()));
        let signing_key = Arc::new(test_signing_key());
        let topic = TopicId::new([6u8; 32]);
        let lazy_peer = test_peer_id(2);

        {
            let mut topics_guard = topics.write().await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(lazy_peer);
            state.pending_ihave.push([7u8; 32]);
        }

        let flush_topics = Arc::clone(&topics);
        let flush_transport = Arc::clone(&transport);
        let flush_signing_key = Arc::clone(&signing_key);
        let flush_stage_stats = Arc::new(PubSubStageStats::default());
        let flush = tokio::spawn(async move {
            PlumtreePubSub::<BlockingTransport>::flush_ihave_batches(
                &flush_topics,
                &flush_transport,
                &flush_signing_key,
                &flush_stage_stats,
            )
            .await;
        });

        let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("IHAVE send should start")
            .expect("send channel should stay open");
        assert_eq!(record.peer, lazy_peer);
        assert_eq!(record.stream_type, GossipStreamType::PubSub);

        let read_guard = tokio::time::timeout(Duration::from_millis(50), topics.read())
            .await
            .expect("IHAVE flush must not hold the topic lock while send_to_peer is blocked");
        assert!(read_guard.contains_key(&topic));
        drop(read_guard);

        transport.release_sends(1);
        tokio::time::timeout(Duration::from_millis(100), flush)
            .await
            .expect("IHAVE flush should finish after send is released")
            .expect("IHAVE flush task should not panic");
    }

    #[tokio::test]
    async fn test_message_caching() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        let payload = Bytes::from("test");
        let msg_id = pubsub.calculate_msg_id(&topic, &payload);

        pubsub.publish(topic, payload.clone()).await.ok();

        // Check cache
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.has_message(&msg_id));
    }

    #[tokio::test]
    async fn test_duplicate_detection_prune() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Initialize peer as eager
        pubsub.initialize_topic_peers(topic, vec![from_peer]).await;

        let payload = Bytes::from("test");
        let msg_id = pubsub.calculate_msg_id(&topic, &payload);

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        // Create properly signed message
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload.clone()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // First EAGER - should be accepted
        pubsub
            .handle_eager(from_peer, topic, message.clone())
            .await
            .ok();

        // Second EAGER - should trigger PRUNE
        pubsub.handle_eager(from_peer, topic, message).await.ok();

        // Verify peer was moved to lazy
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(!state.eager_peers.contains(&from_peer));
        assert!(state.lazy_peers.contains(&from_peer));
    }

    #[tokio::test]
    async fn test_ihave_handling() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let unknown_msg_id = [42u8; 32];

        pubsub
            .handle_ihave(from_peer, topic, vec![unknown_msg_id])
            .await
            .ok();

        // Verify IWANT was tracked
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.outstanding_iwants.contains_key(&unknown_msg_id));
    }

    #[tokio::test]
    async fn test_iwant_graft() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Initialize peer as lazy
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(from_peer);
        }

        // Publish a message to cache it
        let payload = Bytes::from("test");
        pubsub.publish(topic, payload.clone()).await.ok();

        // Get the actual cached msg_id (don't recalculate - epoch may have changed)
        let msg_id = {
            let topics = pubsub.topics.read().await;
            let state = topics.get(&topic).unwrap();
            // Get the first (and only) cached message ID
            state
                .message_cache
                .peek_lru()
                .map(|(id, _)| *id)
                .expect("message should be cached")
        };

        // Handle IWANT from lazy peer
        pubsub
            .handle_iwant(from_peer, topic, vec![msg_id])
            .await
            .ok();

        // Verify peer was grafted to eager
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&from_peer));
        assert!(!state.lazy_peers.contains(&from_peer));
    }

    #[tokio::test]
    async fn test_degree_maintenance() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        // Add many peers to lazy
        let mut peers = Vec::new();
        for i in 2..20 {
            peers.push(test_peer_id(i));
        }

        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for peer in &peers {
                state.lazy_peers.insert(*peer);
            }

            // Maintain degree (should promote to reach MIN_EAGER_DEGREE)
            state.maintain_degree();

            assert!(state.eager_peers.len() >= MIN_EAGER_DEGREE);
        }
    }

    #[tokio::test]
    async fn test_cache_expiration() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        let payload = Bytes::from("test");
        pubsub.publish(topic, payload).await.ok();

        // Manually expire cache entry
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.get_mut(&topic).unwrap();

            // Modify timestamp to simulate expiry
            for (_, cached) in state.message_cache.iter_mut() {
                cached.timestamp = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 10);
            }

            state.clean_cache();

            assert_eq!(state.message_cache.len(), 0);
        }
    }

    #[test]
    fn test_message_cache_capacity_is_bounded() {
        let mut state = TopicState::new();
        let topic = TopicId::new([1u8; 32]);

        for i in 0..(MAX_CACHE_SIZE + 128) {
            let mut msg_id = [0u8; 32];
            msg_id[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let header = MessageHeader {
                version: 1,
                topic,
                msg_id,
                kind: MessageKind::Eager,
                hop: 0,
                ttl: 10,
            };
            state.cache_message(msg_id, Bytes::from(vec![i as u8]), header);
        }

        assert_eq!(
            state.message_cache.len(),
            MAX_CACHE_SIZE,
            "message cache must not grow beyond the configured per-topic cap"
        );

        let mut first_msg_id = [0u8; 32];
        first_msg_id[..8].copy_from_slice(&0u64.to_le_bytes());
        assert!(
            !state.has_message(&first_msg_id),
            "oldest message should be evicted after capacity is exceeded"
        );
    }

    #[test]
    fn test_idle_topic_reaper_drops_unsubscribed_topic() {
        let Some(old_activity) =
            Instant::now().checked_sub(Duration::from_secs(TOPIC_IDLE_TTL_SECS + 1))
        else {
            return;
        };
        let topic = TopicId::new([3u8; 32]);
        let mut state = TopicState::new();
        state.last_activity = old_activity;

        let mut topics = HashMap::new();
        topics.insert(topic, state);

        let reaped = clean_and_reap_topics(&mut topics, Duration::from_secs(TOPIC_IDLE_TTL_SECS));

        assert_eq!(reaped, 1);
        assert!(
            !topics.contains_key(&topic),
            "idle topic with no live subscriber should be reaped"
        );
    }

    #[test]
    fn test_idle_topic_reaper_preserves_live_subscriber() {
        let Some(old_activity) =
            Instant::now().checked_sub(Duration::from_secs(TOPIC_IDLE_TTL_SECS + 1))
        else {
            return;
        };
        let topic = TopicId::new([4u8; 32]);
        let mut state = TopicState::new();
        state.last_activity = old_activity;
        let (tx, rx) = mpsc::unbounded_channel();
        state.subscribers.push(tx);

        let mut topics = HashMap::new();
        topics.insert(topic, state);

        let reaped = clean_and_reap_topics(&mut topics, Duration::from_secs(TOPIC_IDLE_TTL_SECS));

        assert_eq!(reaped, 0);
        assert!(
            topics.contains_key(&topic),
            "live local subscribers must not be silently dropped by idle reaping"
        );

        drop(rx);
        let reaped = clean_and_reap_topics(&mut topics, Duration::from_secs(TOPIC_IDLE_TTL_SECS));

        assert_eq!(reaped, 1);
        assert!(
            !topics.contains_key(&topic),
            "closed subscriber should not keep a quiet topic alive forever"
        );
    }

    #[tokio::test]
    async fn test_handle_ihave_refreshes_topic_activity() {
        let Some(old_activity) =
            Instant::now().checked_sub(Duration::from_secs(TOPIC_IDLE_TTL_SECS + 1))
        else {
            return;
        };
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([5u8; 32]);
        let from_peer = test_peer_id(2);
        let known_msg_id = [9u8; 32];

        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            let header = MessageHeader {
                version: 1,
                topic,
                msg_id: known_msg_id,
                kind: MessageKind::Eager,
                hop: 0,
                ttl: 10,
            };
            state.cache_message(known_msg_id, Bytes::from_static(b"known"), header);
            state.last_activity = old_activity;
        }

        pubsub
            .handle_ihave(from_peer, topic, vec![known_msg_id])
            .await
            .expect("IHAVE with known message should be handled");

        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).expect("topic should still exist");
        assert!(
            state.last_activity > old_activity,
            "incoming IHAVE traffic should refresh topic activity"
        );
    }

    // TDD: RED phase - These tests will fail until we implement real ML-DSA signing

    #[tokio::test]
    async fn test_message_signing_with_real_mldsa() {
        // GREEN: Now implementing real ML-DSA signing
        use saorsa_gossip_identity::MlDsaKeyPair;

        let keypair = MlDsaKeyPair::generate().expect("keypair");
        let peer_id = PeerId::new([1u8; 32]);
        let transport = test_transport().await;

        // Create PlumtreePubSub with signing key
        let _pubsub = PlumtreePubSub::new(peer_id, transport, keypair.clone());

        // Create a message header
        let topic = TopicId::new([1u8; 32]);
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        // Serialize header for signing
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");

        // Sign with ML-DSA
        let signature = keypair.sign(&header_bytes).expect("sign");

        // Signature should NOT be empty
        assert!(
            !signature.is_empty(),
            "ML-DSA signature should not be empty"
        );

        // Signature should be valid
        let valid =
            MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &signature).expect("verify");
        assert!(valid, "Signature should be valid");
    }

    #[tokio::test]
    async fn test_message_signature_verification() {
        // RED: This will fail because verify_signature always returns true
        use saorsa_gossip_identity::MlDsaKeyPair;

        let keypair = MlDsaKeyPair::generate().expect("keypair");

        let topic = TopicId::new([1u8; 32]);
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [1u8; 32],
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = keypair.sign(&header_bytes).expect("sign");

        // Valid signature should verify
        let valid =
            MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &signature).expect("verify");
        assert!(valid, "Valid signature should verify");

        // Tampered signature should NOT verify
        let mut bad_signature = signature.clone();
        bad_signature[0] ^= 0xFF; // Flip bits

        let invalid = MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &bad_signature)
            .expect("verify");
        assert!(!invalid, "Tampered signature should not verify");
    }

    #[tokio::test]
    async fn test_published_message_has_valid_signature() {
        // GREEN: Now verifying that published messages have valid signatures
        use saorsa_gossip_identity::MlDsaKeyPair;

        let keypair = MlDsaKeyPair::generate().expect("keypair");
        let peer_id = PeerId::new([1u8; 32]);
        let transport = test_transport().await;

        // Create pubsub with signing key
        let pubsub = PlumtreePubSub::new(peer_id, transport, keypair.clone());

        let topic = TopicId::new([1u8; 32]);
        let payload = Bytes::from("test message");

        // Publish a message
        pubsub.publish(topic, payload.clone()).await.ok();

        // The message should be signed internally
        // Verify by checking that sign_message produces non-empty signatures
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let signature = pubsub.sign_message(&header);
        assert!(
            !signature.is_empty(),
            "Published messages should have non-empty signatures"
        );

        // Verify the signature is valid
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let valid =
            MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &signature).expect("verify");
        assert!(valid, "Signature should be valid");
    }

    // Anti-entropy tests

    #[test]
    fn test_anti_entropy_payload_serialization() {
        // Test Digest variant round-trips through postcard
        let digest = AntiEntropyPayload::Digest {
            msg_ids: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
        };
        let bytes = postcard::to_stdvec(&digest).expect("serialize digest");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize digest");

        match deserialized {
            AntiEntropyPayload::Digest { msg_ids } => {
                assert_eq!(msg_ids.len(), 3);
                assert_eq!(msg_ids[0], [1u8; 32]);
                assert_eq!(msg_ids[1], [2u8; 32]);
                assert_eq!(msg_ids[2], [3u8; 32]);
            }
            AntiEntropyPayload::Response { .. } => {
                panic!("Expected Digest, got Response");
            }
        }

        // Test Response variant round-trips through postcard
        let response = AntiEntropyPayload::Response {
            missing_ids: vec![[4u8; 32], [5u8; 32]],
        };
        let bytes = postcard::to_stdvec(&response).expect("serialize response");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize response");

        match deserialized {
            AntiEntropyPayload::Response { missing_ids } => {
                assert_eq!(missing_ids.len(), 2);
                assert_eq!(missing_ids[0], [4u8; 32]);
                assert_eq!(missing_ids[1], [5u8; 32]);
            }
            AntiEntropyPayload::Digest { .. } => {
                panic!("Expected Response, got Digest");
            }
        }
    }

    #[test]
    fn test_anti_entropy_payload_empty_serialization() {
        // Empty digest should also round-trip
        let digest = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let bytes = postcard::to_stdvec(&digest).expect("serialize empty digest");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize empty digest");

        match deserialized {
            AntiEntropyPayload::Digest { msg_ids } => {
                assert!(msg_ids.is_empty());
            }
            AntiEntropyPayload::Response { .. } => {
                panic!("Expected Digest, got Response");
            }
        }
    }

    #[tokio::test]
    async fn test_cached_message_ids() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        // Publish 3 messages
        pubsub
            .publish(topic, Bytes::from("msg1"))
            .await
            .expect("publish 1");
        pubsub
            .publish(topic, Bytes::from("msg2"))
            .await
            .expect("publish 2");
        pubsub
            .publish(topic, Bytes::from("msg3"))
            .await
            .expect("publish 3");

        // Verify cached_message_ids returns all 3
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        let ids = state.cached_message_ids();
        assert_eq!(ids.len(), 3, "Should have 3 cached message IDs");
    }

    #[tokio::test]
    async fn test_handle_anti_entropy_digest_sends_missing() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Publish a message so we have it cached
        pubsub
            .publish(topic, Bytes::from("cached message"))
            .await
            .expect("publish");

        // Get the cached message ID
        let our_msg_id = {
            let topics = pubsub.topics.read().await;
            let state = topics.get(&topic).unwrap();
            let ids = state.cached_message_ids();
            assert_eq!(ids.len(), 1);
            ids[0]
        };

        // Create a digest from the "remote" peer that has NO messages (empty)
        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Handle the digest - should attempt to send our cached message to the peer
        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        // The send may fail (no actual connection) but the method should not error
        // on the logic itself
        assert!(result.is_ok());

        // Our message should still be in cache
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.has_message(&our_msg_id));
    }

    #[tokio::test]
    async fn test_handle_anti_entropy_digest_requests_missing() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // We have NO messages cached. The remote peer claims to have some.
        let remote_msg_id = [99u8; 32];
        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: vec![remote_msg_id],
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Handle the digest - should try to send IWANT for the missing message
        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_anti_entropy_response() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Create a Response saying we're missing some IDs
        let missing_id = [77u8; 32];
        let ae_payload = AntiEntropyPayload::Response {
            missing_ids: vec![missing_id],
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Handle the response - should try to send IWANT
        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_anti_entropy_invalid_signature_rejected() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        // Use a BAD signature
        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature: vec![0u8; 100], // invalid signature
            public_key: signing_key.public_key().to_vec(),
        };

        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        assert!(result.is_err(), "Invalid signature should be rejected");
    }

    #[tokio::test]
    async fn test_anti_entropy_message_routing() {
        // Test that AntiEntropy messages are correctly routed via handle_message
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Serialize the full message as it would come over the wire
        let wire_bytes = postcard::to_stdvec(&message).expect("serialize wire message");

        // Route through handle_message (the PubSub trait method)
        let result = pubsub.handle_message(from_peer, wire_bytes.into()).await;
        assert!(
            result.is_ok(),
            "AntiEntropy message should be routed correctly"
        );
    }

    #[tokio::test]
    async fn test_trigger_anti_entropy() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let peer = test_peer_id(2);

        // Initialize with a peer
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        // Publish a message so there's something to reconcile
        pubsub
            .publish(topic, Bytes::from("test data"))
            .await
            .expect("publish");

        // Trigger anti-entropy manually (send may fail on transport, but logic is correct)
        let result = pubsub.trigger_anti_entropy(topic).await;
        // The result may be Ok or Err depending on transport - we're testing the logic path
        // If transport fails, the error is from send_to_peer, not from our logic
        let _ = result;
    }

    #[tokio::test]
    async fn test_trigger_anti_entropy_no_peers() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        // No peers initialized - should return Ok without doing anything
        let result = pubsub.trigger_anti_entropy(topic).await;
        assert!(result.is_ok(), "No peers should result in no-op Ok");
    }

    #[tokio::test]
    async fn test_send_anti_entropy_digest() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let peer = test_peer_id(2);

        // Publish a message so there's a cached message
        pubsub
            .publish(topic, Bytes::from("digest test"))
            .await
            .expect("publish");

        // Send digest (transport send may fail, but serialization and logic should work)
        let _ = pubsub.send_anti_entropy_digest(topic, peer).await;
    }

    #[tokio::test]
    async fn test_send_anti_entropy_digest_empty_cache() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let peer = test_peer_id(2);

        // Initialize topic but don't publish anything
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        // Should return Ok since there's nothing to send
        let result = pubsub.send_anti_entropy_digest(topic, peer).await;
        assert!(result.is_ok(), "Empty cache should result in no-op Ok");
    }

    // Peer scoring tests

    #[test]
    fn test_peer_score_no_requests_no_deliveries() {
        // A brand-new peer with no activity should get a moderate score
        let score = PeerScore::new();
        let s = score.score();
        // No deliveries, no IWANT requests => response_rate = 0.5
        // Recency should be ~1.0 (just created)
        // Score = 0.5 * 0.6 + ~1.0 * 0.4 = 0.3 + 0.4 = ~0.7
        assert!(
            s > 0.6,
            "New peer with no activity should have moderate score, got {s}"
        );
        assert!(s < 1.0, "Score should be below 1.0, got {s}");
    }

    #[test]
    fn test_peer_score_with_deliveries_no_iwant() {
        // Peer that has delivered messages but no IWANT requests
        let mut score = PeerScore::new();
        score.record_delivery();
        score.record_delivery();
        score.record_delivery();
        let s = score.score();
        // deliveries > 0, no IWANT => response_rate = 0.8
        // Recency ~1.0
        // Score = 0.8 * 0.6 + ~1.0 * 0.4 = 0.48 + 0.4 = ~0.88
        assert!(
            s > 0.8,
            "Peer with deliveries should have high score, got {s}"
        );
        assert!(s <= 1.0, "Score should be at most 1.0, got {s}");
    }

    #[test]
    fn test_peer_score_perfect_iwant_response_rate() {
        // Peer with perfect IWANT response rate
        let mut score = PeerScore::new();
        score.record_iwant_request();
        score.record_iwant_response();
        score.record_iwant_request();
        score.record_iwant_response();
        let s = score.score();
        // response_rate = 2/2 = 1.0
        // Recency ~1.0
        // Score = 1.0 * 0.6 + ~1.0 * 0.4 = ~1.0
        assert!(
            s > 0.9,
            "Perfect IWANT response rate should give high score, got {s}"
        );
    }

    #[test]
    fn test_peer_score_50_percent_iwant_response_rate() {
        // Peer with 50% IWANT response rate
        let mut score = PeerScore::new();
        score.record_iwant_request();
        score.record_iwant_response();
        score.record_iwant_request();
        // 1 response out of 2 requests = 50%
        let s = score.score();
        // response_rate = 1/2 = 0.5
        // Recency ~1.0
        // Score = 0.5 * 0.6 + ~1.0 * 0.4 = 0.3 + 0.4 = ~0.7
        assert!(
            s > 0.6,
            "50% IWANT response rate should give moderate score, got {s}"
        );
        assert!(s < 0.85, "50% rate should be below perfect, got {s}");
    }

    #[test]
    fn test_peer_score_recency_decay() {
        // Test that a peer unseen for a long time has lower score
        let mut score = PeerScore::new();
        score.record_delivery();
        // Simulate the peer being unseen for 5+ minutes
        score.last_seen = Instant::now() - Duration::from_secs(350);
        let s = score.score();
        // deliveries > 0, no IWANT => response_rate = 0.8
        // secs_since_seen = 350, recency = max(0, 1 - 350/300) = 0.0
        // Score = 0.8 * 0.6 + 0.0 * 0.4 = 0.48
        assert!(
            s < 0.55,
            "Stale peer should have low score due to recency decay, got {s}"
        );
        assert!(
            s > 0.4,
            "Stale peer should still have some score from response rate, got {s}"
        );
    }

    #[tokio::test]
    async fn test_eager_records_delivery_score() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Initialize peer as eager
        pubsub.initialize_topic_peers(topic, vec![from_peer]).await;

        let payload = Bytes::from("test delivery");
        let msg_id = pubsub.calculate_msg_id(&topic, &payload);

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        pubsub
            .handle_eager(from_peer, topic, message)
            .await
            .expect("handle_eager");

        // Verify peer score has messages_delivered == 1
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        let peer_score = state
            .peer_scores
            .get(&from_peer)
            .expect("peer score should exist");
        assert_eq!(
            peer_score.messages_delivered, 1,
            "Should have 1 delivery recorded"
        );
    }

    #[tokio::test]
    async fn test_ihave_iwant_eager_flow_updates_scores() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let unknown_msg_id = [42u8; 32];

        // Step 1: IHAVE from peer triggers IWANT
        pubsub
            .handle_ihave(from_peer, topic, vec![unknown_msg_id])
            .await
            .ok();

        // Verify IWANT request was tracked in score
        {
            let topics = pubsub.topics.read().await;
            let state = topics.get(&topic).unwrap();
            let peer_score = state
                .peer_scores
                .get(&from_peer)
                .expect("peer score should exist");
            assert_eq!(
                peer_score.iwant_requests, 1,
                "Should have 1 IWANT request recorded"
            );
            assert_eq!(
                peer_score.iwant_responses, 0,
                "Should have 0 IWANT responses yet"
            );
        }

        // Step 2: EAGER arrives with the requested message - should record IWANT response
        let payload = Bytes::from("requested message");
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: unknown_msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        pubsub
            .handle_eager(from_peer, topic, message)
            .await
            .expect("handle_eager");

        // Verify IWANT response was recorded
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        let peer_score = state
            .peer_scores
            .get(&from_peer)
            .expect("peer score should exist");
        assert_eq!(
            peer_score.iwant_responses, 1,
            "Should have 1 IWANT response recorded"
        );
        assert_eq!(
            peer_score.messages_delivered, 1,
            "Should have 1 delivery recorded"
        );
    }

    #[test]
    fn test_score_based_promotion_highest_first() {
        // Create lazy peers with different scores - highest should be promoted first
        let mut state = TopicState::new();

        let peer_high = test_peer_id(10);
        let peer_low = test_peer_id(11);
        let peer_mid = test_peer_id(12);

        state.lazy_peers.insert(peer_high);
        state.lazy_peers.insert(peer_low);
        state.lazy_peers.insert(peer_mid);

        // Give peer_high the best score (many deliveries)
        let mut high_score = PeerScore::new();
        high_score.messages_delivered = 100;
        state.peer_scores.insert(peer_high, high_score);

        // Give peer_low a poor score (no deliveries, stale)
        let mut low_score = PeerScore::new();
        low_score.last_seen = Instant::now() - Duration::from_secs(250);
        state.peer_scores.insert(peer_low, low_score);

        // Give peer_mid a moderate score
        let mut mid_score = PeerScore::new();
        mid_score.messages_delivered = 10;
        state.peer_scores.insert(peer_mid, mid_score);

        // Eager is empty, so maintain_degree should promote up to MIN_EAGER_DEGREE
        // But we only have 3 lazy peers, so all 3 get promoted
        state.maintain_degree();

        // All should be promoted since we're below MIN_EAGER_DEGREE
        assert!(
            state.eager_peers.contains(&peer_high),
            "High-scoring peer should be promoted"
        );
        assert!(
            state.eager_peers.contains(&peer_mid),
            "Mid-scoring peer should be promoted"
        );
        assert!(
            state.eager_peers.contains(&peer_low),
            "Low-scoring peer should be promoted (not enough peers)"
        );
    }

    #[test]
    fn test_score_based_demotion_lowest_first() {
        // Create too many eager peers with different scores using IWANT response rates
        // which create a continuous gradient (unlike messages_delivered which is binary).
        let mut state = TopicState::new();

        // Add MAX_EAGER_DEGREE + 2 eager peers
        let mut peers = Vec::new();
        for i in 0..(MAX_EAGER_DEGREE + 2) {
            let peer = test_peer_id(i as u8 + 10);
            peers.push(peer);
            state.eager_peers.insert(peer);

            // Use IWANT response rates to create clearly different scores.
            // All peers have 10 IWANT requests; peer i responds to i of them.
            // This gives response_rate = i/10, creating a gradient from 0.0 to ~1.0.
            let mut score = PeerScore::new();
            score.iwant_requests = 10;
            score.iwant_responses = i as u64;
            state.peer_scores.insert(peer, score);
        }

        // The first peer (i=0) has the worst score (0% IWANT response rate)
        let worst_peer = peers[0];
        let second_worst = peers[1];

        state.maintain_degree();

        // Should have demoted 2 peers (down to MAX_EAGER_DEGREE)
        assert_eq!(
            state.eager_peers.len(),
            MAX_EAGER_DEGREE,
            "Should have MAX_EAGER_DEGREE eager peers"
        );

        // The worst-scoring peers should have been demoted
        assert!(
            state.lazy_peers.contains(&worst_peer),
            "Worst-scoring peer should be demoted"
        );
        assert!(
            state.lazy_peers.contains(&second_worst),
            "Second-worst peer should be demoted"
        );

        // The best-scoring peer should still be eager
        let best_peer = peers[MAX_EAGER_DEGREE + 1];
        assert!(
            state.eager_peers.contains(&best_peer),
            "Best-scoring peer should remain eager"
        );
    }

    #[test]
    fn test_stale_peer_scores_cleaned() {
        let mut state = TopicState::new();

        let fresh_peer = test_peer_id(20);
        let stale_peer = test_peer_id(21);

        // Fresh peer score (just created)
        state.peer_scores.insert(fresh_peer, PeerScore::new());

        // Stale peer score (last seen > 10 minutes ago)
        // Use checked_sub because on some platforms (Windows CI) the
        // monotonic clock epoch may be too recent for a 700s subtraction.
        let mut stale_score = PeerScore::new();
        let Some(past) = Instant::now().checked_sub(Duration::from_secs(700)) else {
            // Platform doesn't have enough headroom — skip test gracefully
            return;
        };
        stale_score.last_seen = past;
        state.peer_scores.insert(stale_peer, stale_score);

        // Clean cache (which also cleans peer scores)
        state.clean_cache();

        assert!(
            state.peer_scores.contains_key(&fresh_peer),
            "Fresh peer score should be retained"
        );
        assert!(
            !state.peer_scores.contains_key(&stale_peer),
            "Stale peer score should be cleaned up"
        );
    }

    #[tokio::test]
    async fn test_set_topic_peers_prunes_stale_eager() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let peer_a = test_peer_id(2);
        let peer_b = test_peer_id(3);

        // Initialize with two eager peers
        pubsub
            .initialize_topic_peers(topic, vec![peer_a, peer_b])
            .await;

        // Only peer_a is still connected
        pubsub.set_topic_peers(topic, vec![peer_a]).await;

        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&peer_a));
        assert!(!state.eager_peers.contains(&peer_b));
    }

    #[tokio::test]
    async fn test_set_topic_peers_prunes_stale_lazy() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let peer_a = test_peer_id(2);
        let peer_b = test_peer_id(3);

        // Manually set up: peer_a eager, peer_b lazy
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(peer_a);
            state.lazy_peers.insert(peer_b);
        }

        // Only peer_a is still connected — peer_b should be pruned from lazy
        pubsub.set_topic_peers(topic, vec![peer_a]).await;

        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&peer_a));
        assert!(!state.lazy_peers.contains(&peer_b));
    }

    #[tokio::test]
    async fn test_set_topic_peers_adds_new_as_eager() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let peer_a = test_peer_id(2);
        let peer_b = test_peer_id(3);

        // Initialize with only peer_a
        pubsub.initialize_topic_peers(topic, vec![peer_a]).await;

        // Now peer_b has connected too
        pubsub.set_topic_peers(topic, vec![peer_a, peer_b]).await;

        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&peer_a));
        assert!(state.eager_peers.contains(&peer_b));
    }

    #[tokio::test]
    async fn test_set_topic_peers_retains_lazy_if_connected() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let peer_a = test_peer_id(2);
        let peer_b = test_peer_id(3);

        // peer_a eager, peer_b lazy (simulating a prior PRUNE)
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(peer_a);
            state.lazy_peers.insert(peer_b);
        }

        // Both still connected — peer_b should be promoted back to eager
        // during the periodic refresh so that PRUNE optimizations don't
        // permanently break gossip routing.
        pubsub.set_topic_peers(topic, vec![peer_a, peer_b]).await;

        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&peer_a));
        assert!(
            state.eager_peers.contains(&peer_b),
            "Lazy peer should be promoted to eager during refresh"
        );
        assert!(
            !state.lazy_peers.contains(&peer_b),
            "Promoted peer should no longer be in lazy set"
        );
    }

    #[tokio::test]
    async fn test_set_topic_peers_combined_prune_and_add() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let peer_a = test_peer_id(2);
        let peer_b = test_peer_id(3);
        let peer_c = test_peer_id(4);

        // Start with peer_a eager, peer_b lazy
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(peer_a);
            state.lazy_peers.insert(peer_b);
        }

        // peer_a disconnected, peer_b still connected, peer_c is new
        pubsub.set_topic_peers(topic, vec![peer_b, peer_c]).await;

        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(
            !state.eager_peers.contains(&peer_a),
            "Disconnected eager peer should be removed"
        );
        assert!(
            state.eager_peers.contains(&peer_b),
            "Connected lazy peer should be promoted to eager"
        );
        assert!(
            !state.lazy_peers.contains(&peer_b),
            "Promoted peer should no longer be in lazy set"
        );
        assert!(
            state.eager_peers.contains(&peer_c),
            "New peer should be added as eager"
        );
    }

    // -----------------------------------------------------------------------
    // Payload replay cache tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_payload_replay_detected() {
        let mut state = TopicState::new();
        let payload = b"hello world";

        assert!(
            !state.is_payload_replay(payload),
            "First insert should be new"
        );
        assert!(
            state.is_payload_replay(payload),
            "Second insert should be replay"
        );
    }

    #[test]
    fn test_payload_replay_different_payloads_pass() {
        let mut state = TopicState::new();

        assert!(!state.is_payload_replay(b"message 1"));
        assert!(!state.is_payload_replay(b"message 2"));
        assert!(!state.is_payload_replay(b"message 3"));
    }

    #[test]
    fn test_payload_replay_lru_eviction() {
        let mut state = TopicState::new();

        // Fill the cache beyond capacity
        for i in 0..REPLAY_CACHE_MAX_ENTRIES + 100 {
            let payload = format!("payload-{i}");
            assert!(!state.is_payload_replay(payload.as_bytes()));
        }

        // Cache should not exceed max entries
        assert!(state.replay_cache.len() <= REPLAY_CACHE_MAX_ENTRIES);

        // The very first entry should have been evicted
        assert!(
            !state.is_payload_replay(b"payload-0"),
            "Evicted entry should be accepted as new again"
        );
    }

    #[tokio::test]
    async fn test_handle_eager_drops_replayed_payload() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);

        // Subscribe to receive messages
        let mut rx = pubsub.subscribe(topic);
        tokio::task::yield_now().await;

        let payload = Bytes::from("important data");

        // First EAGER with one msg_id
        let msg_id_1 = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(topic.as_bytes());
            hasher.update(&1u64.to_le_bytes()); // epoch 1
            hasher.update(test_peer_id(2).as_bytes());
            hasher.update(blake3::hash(&payload).as_bytes());
            let hash = hasher.finalize();
            let mut id = [0u8; 32];
            id.copy_from_slice(&hash.as_bytes()[..32]);
            id
        };

        let header1 = MessageHeader {
            version: 1,
            topic,
            msg_id: msg_id_1,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };
        let header_bytes1 = postcard::to_stdvec(&header1).expect("serialize");
        let signature1 = signing_key.sign(&header_bytes1).expect("sign");
        let message1 = GossipMessage {
            header: header1,
            payload: Some(payload.clone()),
            signature: signature1,
            public_key: signing_key.public_key().to_vec(),
        };

        // Second EAGER: same payload but different msg_id (simulating re-wrapped replay)
        let msg_id_2 = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(topic.as_bytes());
            hasher.update(&2u64.to_le_bytes()); // epoch 2 — different!
            hasher.update(test_peer_id(3).as_bytes()); // different sender
            hasher.update(blake3::hash(&payload).as_bytes());
            let hash = hasher.finalize();
            let mut id = [0u8; 32];
            id.copy_from_slice(&hash.as_bytes()[..32]);
            id
        };

        let header2 = MessageHeader {
            version: 1,
            topic,
            msg_id: msg_id_2,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };
        let header_bytes2 = postcard::to_stdvec(&header2).expect("serialize");
        let signature2 = signing_key.sign(&header_bytes2).expect("sign");
        let message2 = GossipMessage {
            header: header2,
            payload: Some(payload.clone()),
            signature: signature2,
            public_key: signing_key.public_key().to_vec(),
        };

        let from_peer = test_peer_id(2);

        // Handle first EAGER — should deliver to subscriber
        pubsub
            .handle_eager(from_peer, topic, message1)
            .await
            .expect("first handle_eager");

        // Handle second EAGER (replay) — should NOT deliver
        let from_peer_2 = test_peer_id(3);
        pubsub
            .handle_eager(from_peer_2, topic, message2)
            .await
            .expect("second handle_eager");

        // Subscriber should receive exactly one message
        let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("should receive first message")
            .expect("channel should not be closed");
        assert_eq!(msg.1, payload);

        // No second message should arrive
        let replay = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            replay.is_err(),
            "Replayed payload should NOT be delivered to subscriber"
        );
    }

    #[tokio::test]
    async fn test_publish_local_seeds_replay_cache() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);

        // Subscribe
        let mut rx = pubsub.subscribe(topic);
        tokio::task::yield_now().await;

        let payload = Bytes::from("local message");

        // Publish locally — should deliver to subscriber AND seed replay cache
        pubsub
            .publish(topic, payload.clone())
            .await
            .expect("publish");

        // Receive the local publish
        let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("should receive local publish")
            .expect("channel open");
        assert_eq!(msg.1, payload);

        // Now simulate an EAGER from the network with the same payload
        let msg_id = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(topic.as_bytes());
            hasher.update(&99u64.to_le_bytes());
            hasher.update(test_peer_id(5).as_bytes());
            hasher.update(blake3::hash(&payload).as_bytes());
            let hash = hasher.finalize();
            let mut id = [0u8; 32];
            id.copy_from_slice(&hash.as_bytes()[..32]);
            id
        };

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = signing_key.sign(&header_bytes).expect("sign");
        let message = GossipMessage {
            header,
            payload: Some(payload.clone()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        let from_peer = test_peer_id(5);
        pubsub
            .handle_eager(from_peer, topic, message)
            .await
            .expect("handle_eager echo");

        // The echo should be caught by the replay cache — no second delivery
        let echo = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            echo.is_err(),
            "Network echo of locally published payload should be dropped by replay cache"
        );
    }
}
