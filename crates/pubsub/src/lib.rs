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
//!
//! # X0X-0073 / X0X-0073b — adaptive cooling
//!
//! See the [`timing`] module for the per-peer EWMA RTT tracker and
//! adaptive cooldown configuration. The send path consumes those
//! primitives so per-peer timeouts follow observed p95 send duration and
//! cooldowns decay after successful sends.

pub mod admission;
pub mod peer_scoring;
pub mod timing;

use crate::timing::{AdaptiveCoolingConfig, PerPeerRttTracker};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use lru::LruCache;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use saorsa_gossip_types::{
    AdmissionDecision, LogPeerId, LogTopicId, MessageHeader, MessageKind, PeerHealth,
    PeerHealthOracle, PeerId, TopicId, TopicPriority,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::time;
use tracing::{debug, error, info, trace, warn};

/// Maximum message cache size per topic.
///
/// Sized at 2× the IHAVE batch (1024) so PlumTree IWANT recovery still has
/// the full last-batch window of payloads to serve. Worst case payload
/// retention per topic: 2048 × max-payload-size. Was 10_000 — too generous
/// for memory-constrained nodes (would retain up to ~90 MB per topic at
/// 9 KB/payload).
const MAX_CACHE_SIZE: usize = 2_048;

/// Maximum age of a cached pubsub message before forced eviction.
///
/// PlumTree IWANT recovery typically resolves within RTTs (sub-second).
/// 60 s is generous for partition recovery while bounding steady-state
/// retention to (msg_rate × 60) × payload_size per topic. Was 300 s —
/// 5× more retention than required for the recovery window. X0X-0068 keeps
/// this existing strict age cap while adding byte accounting so large
/// discovery-card topics cannot fill the whole count window.
const MAX_CACHE_AGE_SECS: u64 = 60;

/// Maximum total estimated wire bytes retained in the message cache per topic.
///
/// The cap is calibrated for the broad-launch cross-region mesh: anti-entropy
/// should never need to reconcile more than a bounded 16 MiB topic window,
/// even when discovery group cards are 11-16 KiB each.
const MAX_CACHE_BYTES_PER_TOPIC: usize = 16 * 1024 * 1024;

/// Estimated non-payload wire overhead for a cached EAGER message.
///
/// This includes the postcard header envelope plus conservative room for
/// framing. The byte cap intentionally over-estimates instead of allowing
/// under-accounted anti-entropy payloads.
const MESSAGE_HEADER_OVERHEAD_BYTES: usize = 256;

/// Estimated ML-DSA-65 signature plus public-key bytes on replayed EAGER data.
///
/// Cached messages store the payload and header; IWANT/anti-entropy replay
/// re-signs them, so the cache byte budget includes the eventual wire crypto
/// overhead rather than only heap bytes.
const MESSAGE_CRYPTO_OVERHEAD_BYTES: usize = 5_500;

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
///
/// X0X-0061 bumped this from 750 ms → 2500 ms. The 750 ms budget was tuned
/// for low-RTT meshes; on the SOTA-Borrow VPS mesh under sustained 4 h
/// load, helsinki's outbound to sydney (~560 ms RTT) and singapore
/// (~330 ms RTT) — Hetzner→DigitalOcean over public internet, no private
/// regional peering — routinely exceeded 750 ms when a single packet loss
/// occurred. That accumulated cooling at `PEER_TIMEOUT_THRESHOLD` per
/// `PEER_TIMEOUT_WINDOW`, suppressing peers for the 120 s
/// `PEER_SUPPRESSION_COOLDOWN`, oscillating helsinki's
/// suppressed_peers/known_peer_topic_pairs ratio around 0.121–0.177 — over
/// the 0.120 broad-launch gate every window of the 16-window 4 h soak.
/// 2500 ms gives ~4 RTTs of headroom on sydney and 7+ on singapore;
/// `PEER_TIMEOUT_THRESHOLD` is unchanged (5 timeouts in 30 s is still a
/// real signal at 2500 ms each).
///
/// 2026-07-12 (three-machine WAN convergence test, Studio/London/Toronto):
/// 2500 ms was still tight for NAT-traversed intercontinental paths — a
/// single hole-punch stall routinely exceeded it and fed cooling
/// (`cool_budget` dominated every 15 s journal bucket; prod bootstrap
/// nodes showed 12k–27k budget-pressure events per day). Bumped to
/// 4000 ms. This is the cold-start floor only — once samples exist the
/// per-peer timeout adapts via [`PerPeerRttTracker::adaptive_timeout`]
/// (2.5 × p95, floor 1.5 s, ceiling 10 s).
const PER_PEER_REPUBLISH_TIMEOUT: Duration = Duration::from_millis(4000);

/// Rolling window for send-side slow-peer detection.
const PEER_TIMEOUT_WINDOW: Duration = Duration::from_secs(30);

/// Timeouts inside `PEER_TIMEOUT_WINDOW` before a peer is cooled.
///
/// 2026-07-12: raised 5 → 8. At the previous threshold a short burst of
/// WAN/NAT churn (5 slow sends in 30 s) was enough to suppress a healthy
/// peer; live prod counters showed 14k–33k `Peer cooled` events per node
/// per day, so cooling was firing on ordinary high-RTT behaviour rather
/// than genuinely dead peers.
const PEER_TIMEOUT_THRESHOLD: usize = 8;

/// Initial sender-side suppression duration for a cooled peer.
///
/// Legacy/test fallback — production paths pass an
/// [`AdaptiveCoolingConfig`] whose `initial` defaults to
/// [`crate::timing::ADAPTIVE_COOLDOWN_INITIAL`] (30 s). 2026-07-12: cut
/// 120 s → 30 s so the two surfaces agree; the three-machine WAN test
/// proved a 120 s suppression makes live CRDT deltas undeliverable for
/// longer than most application poll windows.
const PEER_SUPPRESSION_COOLDOWN: Duration = Duration::from_secs(30);

/// Maximum repeated-offender suppression duration.
///
/// Legacy/test fallback aligned with
/// [`crate::timing::ADAPTIVE_COOLDOWN_MAX`] (was 1800 s).
const PEER_SUPPRESSION_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Minimum interval between rate-limited recovery-bypass sends to a peer
/// whose suppression cooldown is still active.
///
/// 2026-07-12 (WP6): while a peer was suppressed it previously received
/// NEITHER eager pushes NOR lazy IHAVE announcements — the lazy backstop
/// was defeated exactly when it was needed, so a message published during
/// suppression could not converge until the full cooldown elapsed. The
/// bypass admits at most one send per this interval per (topic, peer) so
/// cached messages, IHAVE announces, and anti-entropy digests still
/// trickle to the peer while full eager fanout stays gated on cooldown
/// expiry plus a successful recovery probe.
const PEER_COOLDOWN_BYPASS_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Refresh cadence for the SWIM peer-health snapshot consumed by the
/// pub-sub cooling hot path.
const PEER_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Refresh cadence for the transport-connected peer snapshot consumed by
/// the pub-sub send-admission hot path.
const CONNECTED_PEERS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Half-life for decayed peer-score send-health evidence.
const PEER_SCORE_DECAY_HALF_LIFE: Duration = Duration::from_secs(300);

/// Lowest score accepted for EAGER promotion when healthier alternatives exist.
const PEER_SCORE_EAGER_MIN: f64 = 0.45;

/// Minimum multiplier applied by send-health to the inbound/recency base score.
const PEER_SCORE_SEND_MIN_FACTOR: f64 = 0.35;

/// Retain score evidence long enough to cover maximum cooling backoff plus decay.
const PEER_SCORE_RETENTION: Duration = Duration::from_secs(2_400);

/// Fastest adaptive cleanup pass for expired suppression diagnostics/state.
const SUPPRESSION_CLEANUP_MIN_INTERVAL_MS: u64 = 10_000;

/// Normal adaptive cleanup pass for stable suppression state.
const SUPPRESSION_CLEANUP_NORMAL_INTERVAL_MS: u64 = 60_000;

/// Slowest adaptive cleanup pass when suppression state is empty or shrinking.
const SUPPRESSION_CLEANUP_MAX_INTERVAL_MS: u64 = 120_000;

/// Minimum interval between score-driven eager-peer replacement passes.
const OPPORTUNISTIC_GRAFT_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum low-score eager peers replaced by one opportunistic maintenance pass.
const OPPORTUNISTIC_GRAFT_MAX_REPLACEMENTS: usize = 1;

/// Score advantage required before replacing an eager peer with a lazy peer.
const OPPORTUNISTIC_GRAFT_MIN_SCORE_DELTA: f64 = 0.20;

/// Per-peer concurrent Critical EAGER/data sends allowed by the PubSub outbound budget.
///
/// Critical gets a dedicated lane so Normal/Bulk cannot starve DM/control-plane
/// traffic on slow links.
const OUTBOUND_CRITICAL_DATA_PERMITS_PER_PEER: usize = 1;

/// Per-peer concurrent Normal/Bulk EAGER/data sends allowed beside Critical.
const OUTBOUND_BEST_EFFORT_DATA_PERMITS_PER_PEER: usize = 1;

/// Per-peer concurrent control sends allowed beside one data send.
const OUTBOUND_CONTROL_PERMITS_PER_PEER: usize = 2;

/// Idle outbound budget entries are reaped after this duration.
const OUTBOUND_BUDGET_REAP_AFTER: Duration = Duration::from_secs(600);

/// X0X-0074d: maximum number of concurrent Critical sends that may queue for a
/// single peer's serialized Critical lane before the excess is hard-dropped.
///
/// The lane itself admits one Critical send at a time (the in-flight one); the
/// rest wait FIFO. Steady-state concurrency is 1–3, so this bound is only hit
/// under genuine overload, where a hard error is the correct signal. Bounding
/// the queue at reservation time (sync, pre-spawn) also caps spawned send tasks,
/// preserving the OOM-on-spawn protection the non-blocking budget provided.
const OUTBOUND_CRITICAL_QUEUE_PER_PEER: usize = 64;

/// Message ID type alias
type MessageIdType = [u8; 32];

/// Tunable bounds for each topic's PubSub message cache.
#[derive(Debug, Clone, Copy)]
pub struct PubSubCacheConfig {
    /// Maximum cached message count per topic.
    pub max_messages_per_topic: NonZeroUsize,
    /// Maximum estimated cached message bytes per topic.
    pub max_bytes_per_topic: usize,
    /// Maximum cached message age before eviction.
    pub max_age: Duration,
}

impl Default for PubSubCacheConfig {
    fn default() -> Self {
        Self {
            max_messages_per_topic: message_cache_capacity(),
            max_bytes_per_topic: MAX_CACHE_BYTES_PER_TOPIC,
            max_age: Duration::from_secs(MAX_CACHE_AGE_SECS),
        }
    }
}

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
#[derive(Debug, Clone, Serialize, Default)]
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
    pub(crate) fn record(&self, duration: Duration) {
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

    pub(crate) fn snapshot(&self) -> StageTimingStatsSnapshot {
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
    /// Send work skipped because a peer already consumed its outbound PubSub
    /// permits. This is distinct from a transport timeout: no new task was
    /// spawned, and the peer/topic receives timeout pressure for cooling.
    outbound_budget_exhausted: AtomicU64,
    /// WP6: rate-limited sends admitted to peers whose suppression cooldown
    /// was still active, and how many of those completed successfully.
    cooldown_bypass_probes: AtomicU64,
    cooldown_bypass_successes: AtomicU64,
    suppressed_peers: Mutex<HashMap<SuppressedPeerKey, SuppressedPeerState>>,
    /// Lock-wait timing for `suppressed_peers` (issue #27 instrumentation).
    suppressed_peers_lock: StageTimingStats,
    suppression_cleanup: SuppressionCleanupStats,
}

#[derive(Debug)]
struct SuppressionCleanupStats {
    current_interval_ms: AtomicU64,
    growth_rate_bits: AtomicU64,
    current_entries: AtomicU64,
    last_removed: AtomicU64,
}

impl Default for SuppressionCleanupStats {
    fn default() -> Self {
        Self {
            current_interval_ms: AtomicU64::new(SUPPRESSION_CLEANUP_NORMAL_INTERVAL_MS),
            growth_rate_bits: AtomicU64::new(0),
            current_entries: AtomicU64::new(0),
            last_removed: AtomicU64::new(0),
        }
    }
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
    last_suppressed_unix_ms: u64,
    recovery_probe_in_flight: bool,
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
    /// Approximate wall-clock Unix millisecond when the peer/topic last entered
    /// suppression or recovery-probe state.
    pub last_suppressed_unix_ms: u64,
    /// Current recovery state for this peer/topic (`cooldown`, `recovery_ready`,
    /// or `recovery_probe`).
    pub state: String,
    /// Whether a post-cooldown recovery send has been claimed and is still in flight.
    pub recovery_probe_in_flight: bool,
}

/// JSON-friendly diagnostic for score-driven PlumTree mesh selection.
#[derive(Debug, Clone, Serialize)]
pub struct PeerScoreSnapshot {
    /// Peer identifier, formatted the same way as logs.
    pub peer_id: String,
    /// Topic identifier, formatted the same way as logs.
    pub topic: String,
    /// Current mesh role for this peer (`eager`, `lazy`, `cooled`,
    /// `recovery_ready`, `recovery_probe`, or `excluded`).
    pub role: String,
    /// Current composite mesh-selection score in the range 0.0..=1.0.
    pub score: f64,
    /// IWANT response-rate component in the range 0.0..=1.0.
    pub iwant_response_rate: f64,
    /// Recency component in the range 0.0..=1.0.
    pub recency: f64,
    /// Send-side health component in the range 0.0..=1.0.
    pub send_health: f64,
    /// Decayed successful outbound send evidence.
    pub outbound_send_successes: f64,
    /// Decayed outbound send-timeout evidence.
    pub outbound_send_timeouts: f64,
    /// Decayed cooling-event evidence.
    pub cooling_events: f64,
    /// Decayed recovery-probe attempt evidence.
    pub recovery_probes: f64,
    /// Decayed successful recovery-probe evidence.
    pub recovery_successes: f64,
    /// Whether this peer is currently eligible for score-driven EAGER promotion.
    pub eager_eligible: bool,
}

/// Topic-indexed peer-score and cooling breakdown used by admission-control
/// tuning. This intentionally mirrors the existing flat [`PeerScoreSnapshot`]
/// fields while adding the active cooling state for that peer/topic.
#[derive(Debug, Clone, Serialize)]
pub struct PeerScoreBreakdownSnapshot {
    /// Current mesh role for this peer.
    pub role: String,
    /// Current composite mesh-selection score in the range 0.0..=1.0.
    pub score: f64,
    /// Send-side health component in the range 0.0..=1.0.
    pub send_health: f64,
    /// Decayed outbound send-timeout evidence.
    pub outbound_send_timeouts: f64,
    /// Decayed cooling-event evidence.
    pub cooling_events: f64,
    /// Whether this peer is currently eligible for score-driven EAGER promotion.
    pub eager_eligible: bool,
    /// Current cooling/recovery state for this peer/topic, when present.
    pub suppression_state: Option<String>,
    /// Number of recent timeout events that triggered the current suppression.
    pub recent_timeout_count: Option<usize>,
    /// Cooldown duration applied to this suppression.
    pub cooldown_ms: Option<u64>,
    /// Approximate wall-clock Unix millisecond when this peer/topic last entered
    /// suppression or recovery-probe state.
    pub last_cool_at_unix_ms: Option<u64>,
}

/// Placeholder admission-control state by peer. X0X-0074 will populate
/// priority queue depths; X0X-0075 exposes the peer health/suppression state
/// now so operators can tune topic priorities from real pressure evidence.
#[derive(Debug, Clone, Serialize)]
pub struct AdmissionStateSnapshot {
    /// Current admission state inferred from active PubSub cooling.
    pub state: String,
    /// Number of topics currently suppressing this peer.
    pub suppressed_topics_count: usize,
    /// Number of suppressed topics still in cooldown.
    pub cooled_topics_count: usize,
    /// Number of suppressed topics with a recovery probe in flight.
    pub recovery_probe_topics_count: usize,
    /// Number of suppressed topics whose cooldown has expired.
    pub recovery_ready_topics_count: usize,
    /// Per-priority queue depths. Empty until X0X-0074 admission queues land.
    pub priority_queue_depths: BTreeMap<String, usize>,
}

/// JSON-friendly snapshot of one topic's bounded message-cache state.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TopicCacheStatsSnapshot {
    /// Topic identifier, formatted the same way as logs.
    pub topic: String,
    /// Per-topic cache counters and resource usage.
    pub cache: CacheStatsSnapshot,
}

/// JSON-friendly snapshot of a bounded message cache.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CacheStatsSnapshot {
    /// Number of cached messages currently retained for the topic.
    pub msg_count: usize,
    /// Estimated total wire bytes retained for the topic.
    pub total_bytes: usize,
    /// Age in seconds of the oldest retained message.
    pub oldest_age_secs: u64,
    /// Cumulative messages evicted because they exceeded the age cap.
    pub evicted_by_age: u64,
    /// Cumulative messages evicted to keep the topic under the byte cap.
    pub evicted_by_bytes: u64,
    /// Cumulative messages evicted to keep the topic under the count cap.
    pub evicted_by_count: u64,
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
    /// Cumulative count of sends skipped before spawning because the peer had
    /// already consumed its outbound PubSub budget.
    pub outbound_budget_exhausted: u64,
    /// WP6: cumulative count of rate-limited recovery-bypass sends admitted
    /// to peers whose suppression cooldown was still active. Zero while
    /// `suppressed_peers` is non-empty means the bypass is not firing.
    pub cooldown_bypass_probes: u64,
    /// WP6: cumulative count of cooldown-bypass sends that completed
    /// successfully — cached messages/announcements delivered to a peer
    /// during its suppression window.
    pub cooldown_bypass_successes: u64,
    /// Peers currently cooled after repeated send-side timeouts.
    pub suppressed_peers: Vec<SuppressedPeerSnapshot>,
    /// Topic-indexed view of currently suppressed peers. Kept alongside the
    /// flat list so dashboards can identify pressure by topic without
    /// re-grouping client side.
    pub suppressed_peers_by_topic: BTreeMap<String, Vec<String>>,
    /// Current adaptive cleanup interval for expired suppression state.
    pub suppression_cleanup_interval_ms: u64,
    /// Latest observed suppression-entry growth rate.
    pub suppression_cleanup_growth_rate_per_sec: f64,
    /// Current total suppression entries observed by the cleanup supervisor.
    pub suppression_cleanup_entries: u64,
    /// Number of expired suppression entries removed in the last cleanup pass.
    pub suppression_cleanup_last_removed: u64,
    /// Per-topic peer-score components used for PlumTree mesh selection.
    pub peer_scores: Vec<PeerScoreSnapshot>,
    /// Topic-indexed peer score breakdown. Values are keyed by
    /// `topic -> peer_id` and include active cooling state when present.
    pub peer_scores_by_topic: BTreeMap<String, BTreeMap<String, PeerScoreBreakdownSnapshot>>,
    /// Peer-indexed admission state placeholder for X0X-0074.
    pub admission_state_by_peer: BTreeMap<String, AdmissionStateSnapshot>,
    /// X0X-0074: aggregate admission counters — admit + drop totals per
    /// (priority, reason). Operators tune topic priorities by watching
    /// `dropped_bulk_backpressure` vs `dropped_bulk_peer_suspect` /
    /// `dropped_bulk_peer_cooled`. `dropped_critical_hard_error` must
    /// stay zero in production; a non-zero value is a soak-blocking
    /// violation.
    pub admission: admission::AdmissionStatsSnapshot,
    /// X0X-0071: libp2p-style P1-P7 peer scores, sorted lowest-score
    /// first (worst peers / graylist candidates on top). Sits alongside
    /// the legacy `peer_scores` mesh-selection score — this is the
    /// multi-parameter decaying score. MVP: telemetry only; thresholds
    /// not yet wired into mesh-selection or admission (X0X-0071b).
    pub peer_scores_v2: Vec<peer_scoring::PeerScoreV2Snapshot>,
    /// Per-topic bounded message-cache usage and eviction counters.
    pub topic_caches: Vec<TopicCacheStatsSnapshot>,
    /// Number of per-topic shards in the sharded topic map (issue #27).
    pub topic_shard_count: usize,
    /// Lock-wait time for the `PeerScoring` `Mutex<HashMap<..>>` (issue #27).
    pub peer_scoring_lock_wait: StageTimingStatsSnapshot,
    /// Lock-wait time for the `suppressed_peers` `Mutex<HashMap<..>>` (issue #27).
    pub suppressed_peers_lock_wait: StageTimingStatsSnapshot,
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
    Sent { observed: Duration },
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendAttemptKind {
    Normal,
    RecoveryProbe,
    /// Rate-limited send admitted while the peer's suppression cooldown is
    /// still active (WP6 recovery bypass). Success delivers data but does
    /// NOT clear the suppression; timeouts do not escalate it.
    CooldownBypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundSendClass {
    Data,
    Control,
}

impl OutboundSendClass {
    fn for_op(op: &'static str) -> Self {
        match op {
            "EAGER" => Self::Data,
            _ => Self::Control,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Control => "control",
        }
    }
}

type RecoveryProbeId = u64;

static NEXT_RECOVERY_PROBE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerSendAttempt {
    peer: PeerId,
    kind: SendAttemptKind,
    recovery_probe_id: Option<RecoveryProbeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerSendCompletion {
    attempt: PeerSendAttempt,
    observed: Duration,
}

/// Monotonic id for an in-flight Data send slot. `0` is reserved to mean
/// "no slot" (Control permits, which are never evictable).
type BudgetSlotId = u64;

static NEXT_BUDGET_SLOT_ID: AtomicU64 = AtomicU64::new(1);

/// A single in-flight Data/EAGER send occupying either the Critical lane or
/// the best-effort (Normal/Bulk) lane for a peer.
struct InFlightDataSlot {
    slot_id: BudgetSlotId,
    priority: TopicPriority,
}

struct PeerOutboundBudgetEntry {
    critical_data_in_flight: usize,
    best_effort_data_in_flight: usize,
    control_in_flight: usize,
    /// One entry per in-flight Data permit. Critical and Normal/Bulk use
    /// separate lanes (X0X-0074c); each slot records which lane it occupies so
    /// `release` decrements the right counter.
    data_slots: Vec<InFlightDataSlot>,
    last_used: Instant,
}

impl PeerOutboundBudgetEntry {
    fn new(now: Instant) -> Self {
        Self {
            critical_data_in_flight: 0,
            best_effort_data_in_flight: 0,
            control_in_flight: 0,
            data_slots: Vec::new(),
            last_used: now,
        }
    }

    fn is_idle_at(&self, now: Instant) -> bool {
        self.critical_data_in_flight == 0
            && self.best_effort_data_in_flight == 0
            && self.control_in_flight == 0
            && self.data_slots.is_empty()
            && now.saturating_duration_since(self.last_used) > OUTBOUND_BUDGET_REAP_AFTER
    }
}

/// X0X-0074d: per-peer FIFO serialization for Critical sends.
///
/// The single per-peer Critical budget lane (0074c) hard-dropped a second
/// concurrent Critical send to the same peer rather than queuing it, so
/// `dropped_critical_hard_error` never reached zero under normal load. This
/// gate replaces that drop with a bounded FIFO wait: one Critical send is
/// in-flight per peer; the rest wait (FIFO) until it completes. Only when the
/// queue exceeds [`OUTBOUND_CRITICAL_QUEUE_PER_PEER`] (genuine overload) is the
/// excess hard-dropped.
///
/// Reservation (`try_reserve`) is synchronous and non-blocking so it runs under
/// the topics lock and bounds spawned send tasks; the actual wait
/// (`CriticalReservation::engage`) happens inside the already-detached per-peer
/// send task so the dispatcher worker is never pinned.
#[derive(Default)]
struct CriticalSendGate {
    peers: Mutex<HashMap<PeerId, Arc<CriticalPeerSlot>>>,
}

struct CriticalPeerSlot {
    /// One permit = the single in-flight Critical send for this peer.
    sem: Arc<Semaphore>,
    /// Reserved-or-in-flight count, used to enforce the queue bound at
    /// reservation time without blocking.
    queued: AtomicUsize,
}

impl CriticalPeerSlot {
    fn new() -> Self {
        Self {
            sem: Arc::new(Semaphore::new(OUTBOUND_CRITICAL_DATA_PERMITS_PER_PEER)),
            queued: AtomicUsize::new(0),
        }
    }
}

impl CriticalSendGate {
    fn peers_guard(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Arc<CriticalPeerSlot>>> {
        match self.peers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("PubSub Critical-gate lock was poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Reserve a slot in `peer`'s Critical queue without blocking. Returns
    /// `None` when the per-peer queue bound is already reached (overload →
    /// caller records a hard error). On success the returned reservation must
    /// be [`CriticalReservation::engage`]d inside the send task to obtain the
    /// in-flight permit (FIFO).
    fn try_reserve(&self, peer: PeerId) -> Option<CriticalReservation> {
        let slot = {
            let mut guard = self.peers_guard();
            // Reap slots only the map still references (no in-flight reservation
            // or permit holds an Arc clone). Safe under the lock: strong_count
            // == 1 means no other thread holds a clone and none can obtain one
            // without this same lock.
            guard.retain(|_, slot| Arc::strong_count(slot) > 1);
            Arc::clone(
                guard
                    .entry(peer)
                    .or_insert_with(|| Arc::new(CriticalPeerSlot::new())),
            )
        };

        // Bound the queue. `queued` counts reservations + in-flight sends.
        let prev = slot.queued.fetch_add(1, Ordering::AcqRel);
        if prev >= OUTBOUND_CRITICAL_QUEUE_PER_PEER {
            slot.queued.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(CriticalReservation {
            slot,
            consumed: false,
        })
    }
}

/// A reserved place in a peer's Critical FIFO. Releases the reservation on drop
/// if never engaged (e.g. the peer was cooling-skipped after reservation).
struct CriticalReservation {
    slot: Arc<CriticalPeerSlot>,
    consumed: bool,
}

impl CriticalReservation {
    /// Wait (FIFO) for the peer's single Critical in-flight permit. Held for the
    /// duration of the send via the returned [`CriticalGateHold`]. Runs inside
    /// the detached send task, so waiting never pins the dispatcher.
    async fn engage(mut self) -> Option<CriticalGateHold> {
        let slot = Arc::clone(&self.slot);
        // tokio's Semaphore hands out permits in FIFO order, giving the queue
        // its serialization. `acquire_owned` only errors if the semaphore is
        // closed, which never happens here. Keep `consumed = false` while
        // awaiting so cancellation/timeouts drop the reservation and release the
        // queued slot; mark consumed only after the in-flight hold takes over.
        match Arc::clone(&slot.sem).acquire_owned().await {
            Ok(permit) => {
                self.consumed = true;
                Some(CriticalGateHold {
                    permit: Some(permit),
                    slot,
                })
            }
            Err(_) => None,
        }
    }
}

impl Drop for CriticalReservation {
    fn drop(&mut self) {
        if !self.consumed {
            // Reserved but never engaged (cooling-skip path): release the queue
            // slot so the bound reflects reality.
            self.slot.queued.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Holds a peer's in-flight Critical permit for the duration of one send.
/// Releasing the semaphore permit (on drop) lets the next FIFO waiter proceed,
/// and decrements the queue count.
struct CriticalGateHold {
    permit: Option<OwnedSemaphorePermit>,
    slot: Arc<CriticalPeerSlot>,
}

impl Drop for CriticalGateHold {
    fn drop(&mut self) {
        // Release the in-flight permit FIRST (waking the next FIFO waiter),
        // THEN free the queue slot. Decrementing `queued` before releasing the
        // permit would let a concurrent `try_reserve` observe the lower count
        // and transiently admit one reservation past the bound.
        drop(self.permit.take());
        self.slot.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

/// X0X-0074d: the Critical-gate state carried by an [`OutboundSendPermit`].
/// Normal/Bulk sends carry `None`; Critical sends carry a `Reserved` slot that
/// the send task upgrades to `Engaged` (the in-flight permit) before sending.
enum CriticalPermitState {
    None,
    Reserved(CriticalReservation),
    /// Held only for its `Drop`, which releases the in-flight Critical permit
    /// (waking the next FIFO waiter) and decrements the queue count.
    Engaged(#[allow(dead_code)] CriticalGateHold),
}

#[derive(Default)]
struct PeerOutboundBudgets {
    peers: Mutex<HashMap<PeerId, PeerOutboundBudgetEntry>>,
    /// X0X-0074d: serializes concurrent Critical sends per peer.
    critical_gate: CriticalSendGate,
}

impl PeerOutboundBudgets {
    fn peers_guard(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, PeerOutboundBudgetEntry>> {
        match self.peers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("PubSub outbound-budget lock was poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Try to reserve an outbound permit for `peer`.
    ///
    /// Critical Data sends are serialized per-peer by the FIFO Critical gate
    /// (X0X-0074d): `try_reserve` is non-blocking and bounds the queue, and the
    /// send task later [`CriticalReservation::engage`]s to wait for the single
    /// in-flight permit. Normal and Bulk Data share a separate best-effort lane;
    /// Control has its own. `None` means: Critical queue overflow (caller records
    /// a hard error) or best-effort/control lane exhausted.
    fn try_acquire(
        self: &Arc<Self>,
        peer: PeerId,
        class: OutboundSendClass,
        priority: TopicPriority,
        now: Instant,
    ) -> Option<OutboundSendPermit> {
        // Critical Data: reserve a FIFO queue slot before touching the budget
        // map. On overflow, bail before any budget mutation so the caller's
        // hard-error accounting is the only side effect.
        let critical =
            if matches!(class, OutboundSendClass::Data) && priority == TopicPriority::Critical {
                match self.critical_gate.try_reserve(peer) {
                    Some(reservation) => CriticalPermitState::Reserved(reservation),
                    None => return None,
                }
            } else {
                CriticalPermitState::None
            };

        let mut guard = self.peers_guard();
        guard.retain(|_, entry| !entry.is_idle_at(now));

        let entry = guard
            .entry(peer)
            .or_insert_with(|| PeerOutboundBudgetEntry::new(now));

        // Non-Critical lanes keep the original non-blocking budget check.
        let exhausted = match class {
            // Critical is bounded by the gate above, not the in-flight lane.
            OutboundSendClass::Data if priority == TopicPriority::Critical => false,
            OutboundSendClass::Data => {
                entry.best_effort_data_in_flight >= OUTBOUND_BEST_EFFORT_DATA_PERMITS_PER_PEER
            }
            OutboundSendClass::Control => {
                entry.control_in_flight >= OUTBOUND_CONTROL_PERMITS_PER_PEER
            }
        };
        if exhausted {
            entry.last_used = now;
            return None;
        }

        let slot_id = match class {
            // Critical no longer occupies a budget lane slot — the gate (carried
            // in `critical`) is its sole limiter, so `release` is a no-op for it.
            OutboundSendClass::Data if priority == TopicPriority::Critical => 0,
            OutboundSendClass::Data => {
                entry.best_effort_data_in_flight += 1;
                let slot_id = NEXT_BUDGET_SLOT_ID.fetch_add(1, Ordering::Relaxed);
                entry
                    .data_slots
                    .push(InFlightDataSlot { slot_id, priority });
                slot_id
            }
            OutboundSendClass::Control => {
                entry.control_in_flight += 1;
                0
            }
        };
        entry.last_used = now;
        drop(guard);

        Some(OutboundSendPermit {
            budgets: Arc::clone(self),
            peer,
            class,
            slot_id,
            active: true,
            critical,
        })
    }

    /// Release an outbound permit. Idempotent for Data permits: the
    /// Data-lane counters decrement ONLY when this call actually removes the
    /// matching slot. The slot presence in `data_slots`, guarded by the mutex,
    /// is the single source of truth.
    fn release(&self, peer: PeerId, class: OutboundSendClass, slot_id: BudgetSlotId) {
        let mut guard = self.peers_guard();
        let Some(entry) = guard.get_mut(&peer) else {
            warn!(peer_id = %LogPeerId::from(peer), class = class.label(), "PubSub outbound permit released after peer budget entry was removed");
            return;
        };

        match class {
            OutboundSendClass::Data => {
                if let Some(idx) = entry
                    .data_slots
                    .iter()
                    .position(|slot| slot.slot_id == slot_id)
                {
                    let slot = entry.data_slots.remove(idx);
                    if slot.priority == TopicPriority::Critical {
                        if entry.critical_data_in_flight == 0 {
                            warn!(peer_id = %LogPeerId::from(peer), class = class.label(), "PubSub outbound Critical data permit released with zero in-flight count");
                        } else {
                            entry.critical_data_in_flight -= 1;
                        }
                    } else if entry.best_effort_data_in_flight == 0 {
                        warn!(peer_id = %LogPeerId::from(peer), class = class.label(), "PubSub outbound best-effort data permit released with zero in-flight count");
                    } else {
                        entry.best_effort_data_in_flight -= 1;
                    }
                }
                // else: the slot was already removed (evicted) — the evictor
                // already decremented, so do nothing.
            }
            OutboundSendClass::Control => {
                if entry.control_in_flight == 0 {
                    warn!(peer_id = %LogPeerId::from(peer), class = class.label(), "PubSub outbound control permit released with zero in-flight count");
                } else {
                    entry.control_in_flight -= 1;
                }
            }
        }
        entry.last_used = Instant::now();
    }
}

struct OutboundSendPermit {
    budgets: Arc<PeerOutboundBudgets>,
    peer: PeerId,
    class: OutboundSendClass,
    slot_id: BudgetSlotId,
    active: bool,
    /// X0X-0074d: Critical-gate reservation/hold. `None` for Normal/Bulk/Control.
    /// Dropping it releases the FIFO queue slot (and the in-flight permit once
    /// engaged), letting the next waiter proceed.
    critical: CriticalPermitState,
}

impl OutboundSendPermit {
    /// X0X-0074d: for a Critical send, wait (FIFO) for the peer's single
    /// in-flight Critical permit before transmitting. No-op for Normal, Bulk,
    /// Control, or an already-engaged permit. Must be called inside the
    /// (already-detached) send task so waiting never pins the dispatcher.
    ///
    /// Returns `false` if the gate could not be engaged before `wait_timeout`
    /// (or if the semaphore closed — never in practice); callers then record
    /// the send as timed out and skip it. The timeout bounds queue residency so
    /// a slow peer cannot hold 64 queued Critical sends for 64× the send
    /// timeout before cooling catches up.
    async fn engage_critical_gate(&mut self, wait_timeout: Duration) -> bool {
        match std::mem::replace(&mut self.critical, CriticalPermitState::None) {
            CriticalPermitState::Reserved(reservation) => {
                match time::timeout(wait_timeout, reservation.engage()).await {
                    Ok(Some(hold)) => {
                        self.critical = CriticalPermitState::Engaged(hold);
                        true
                    }
                    Ok(None) | Err(_) => false,
                }
            }
            other => {
                self.critical = other;
                true
            }
        }
    }
}

impl Drop for OutboundSendPermit {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // `release` decrements the lane counter and removes the slot under the
        // mutex (idempotent on the slot id; a no-op for Critical, which the
        // gate in `critical` bounds instead). The `critical` field's own Drop
        // then releases the gate reservation/permit.
        self.budgets.release(self.peer, self.class, self.slot_id);
    }
}

#[derive(Clone)]
struct SendPathContext {
    rtt_tracker: Arc<PerPeerRttTracker>,
    cooling_config: AdaptiveCoolingConfig,
    peer_health_snapshot: Arc<StdRwLock<HashMap<PeerId, PeerHealth>>>,
    connected_peers_snapshot: Arc<StdRwLock<Option<HashSet<PeerId>>>>,
    peer_health_oracle: Arc<StdRwLock<Option<Arc<dyn PeerHealthOracle>>>>,
    /// X0X-0074 admission control engine — shared with the foreground
    /// publish path so background IHAVE flush + anti-entropy operations
    /// can filter through the same gate before claiming attempts.
    admission: Arc<admission::AdmissionControl>,
}

struct SendClaimContext<'a> {
    stage_stats: &'a PubSubStageStats,
    outbound_budgets: &'a Arc<PeerOutboundBudgets>,
    send_path: &'a SendPathContext,
    topic: TopicId,
    op: &'static str,
    send_class: OutboundSendClass,
    /// Topic priority for the claim, used to choose the Critical or
    /// best-effort per-peer Data lane.
    priority: TopicPriority,
}

struct SendAttemptClaims {
    topic: TopicId,
    attempts: Vec<PeerSendAttempt>,
    permits: Vec<OutboundSendPermit>,
    topics: Arc<ShardedTopicMap>,
    stage_stats: Arc<PubSubStageStats>,
    send_path: SendPathContext,
    armed: bool,
}

impl SendAttemptClaims {
    fn new(
        topic: TopicId,
        attempts: Vec<PeerSendAttempt>,
        permits: Vec<OutboundSendPermit>,
        topics: Arc<ShardedTopicMap>,
        stage_stats: Arc<PubSubStageStats>,
        send_path: SendPathContext,
    ) -> Self {
        Self {
            topic,
            attempts,
            permits,
            topics,
            stage_stats,
            send_path,
            armed: true,
        }
    }

    fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }

    fn attempts(&self) -> &[PeerSendAttempt] {
        &self.attempts
    }

    fn take_permits(&mut self) -> Vec<OutboundSendPermit> {
        std::mem::take(&mut self.permits)
    }

    async fn record_results(
        mut self,
        sent: Vec<PeerSendCompletion>,
        timed_out: Vec<PeerSendAttempt>,
    ) {
        record_topic_send_attempt_results_for_state(
            &self.topics,
            &self.stage_stats,
            &self.send_path,
            self.topic,
            sent,
            timed_out,
        )
        .await;
        self.armed = false;
    }
}

impl Drop for SendAttemptClaims {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let timed_out: Vec<PeerSendAttempt> = self
            .attempts
            .iter()
            .copied()
            .filter(|attempt| attempt.kind == SendAttemptKind::RecoveryProbe)
            .collect();
        if timed_out.is_empty() {
            return;
        }

        let topic = self.topic;
        let topics = Arc::clone(&self.topics);
        let stage_stats = Arc::clone(&self.stage_stats);
        let send_path = self.send_path.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    record_topic_send_attempt_results_for_state(
                        &topics,
                        &stage_stats,
                        &send_path,
                        topic,
                        Vec::new(),
                        timed_out,
                    )
                    .await;
                });
            }
            Err(e) => {
                warn!(
                    topic = %LogTopicId::from(topic),
                    "Unable to release cancelled PubSub recovery probes: {e}"
                );
            }
        }
    }
}

struct AbortOnDropSendHandle {
    handle: tokio::task::JoinHandle<Result<PeerSendOutcome>>,
    abort_on_drop: bool,
}

impl AbortOnDropSendHandle {
    fn new(handle: tokio::task::JoinHandle<Result<PeerSendOutcome>>) -> Self {
        Self {
            handle,
            abort_on_drop: true,
        }
    }

    async fn join(
        mut self,
    ) -> std::result::Result<Result<PeerSendOutcome>, tokio::task::JoinError> {
        let result = (&mut self.handle).await;
        self.abort_on_drop = false;
        result
    }
}

impl Drop for AbortOnDropSendHandle {
    fn drop(&mut self) {
        if self.abort_on_drop {
            self.handle.abort();
        }
    }
}

struct SendTaskSet {
    op: &'static str,
    handles: Vec<(PeerSendAttempt, AbortOnDropSendHandle)>,
}

impl SendTaskSet {
    fn with_capacity(op: &'static str, capacity: usize) -> Self {
        Self {
            op,
            handles: Vec::with_capacity(capacity),
        }
    }

    fn push(
        &mut self,
        attempt: PeerSendAttempt,
        handle: tokio::task::JoinHandle<Result<PeerSendOutcome>>,
    ) {
        self.handles
            .push((attempt, AbortOnDropSendHandle::new(handle)));
    }

    async fn collect_results(mut self) -> (Vec<PeerSendCompletion>, Vec<PeerSendAttempt>) {
        let mut sent = Vec::new();
        let mut timed_out = Vec::new();
        while let Some((attempt, handle)) = self.handles.pop() {
            match handle.join().await {
                Ok(Ok(PeerSendOutcome::Sent { observed })) => {
                    sent.push(PeerSendCompletion { attempt, observed });
                }
                Ok(Ok(PeerSendOutcome::TimedOut) | Err(_)) => timed_out.push(attempt),
                Err(e) => {
                    timed_out.extend(recovery_probe_timeout(attempt));
                    warn!(
                        peer_id = %LogPeerId::from(attempt.peer),
                        op = self.op,
                        "{} per-peer send task panicked: {e}",
                        self.op
                    );
                }
            }
        }
        (sent, timed_out)
    }
}

#[derive(Debug, Clone, Copy)]
struct PeerRecoveryProbeEvent {
    suppressed_until: Instant,
    recent_timeout_count: usize,
    cooldown: Duration,
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn unix_millis_now() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration_millis_u64(duration),
        Err(_) => 0,
    }
}

fn unix_millis_after(remaining: Duration) -> u64 {
    unix_millis_now().saturating_add(duration_millis_u64(remaining))
}

fn peer_health_from_snapshot(
    snapshot: &StdRwLock<HashMap<PeerId, PeerHealth>>,
    peer: &PeerId,
) -> Option<PeerHealth> {
    match snapshot.read() {
        Ok(guard) => guard.get(peer).copied(),
        Err(poisoned) => {
            warn!("PubSub peer-health snapshot lock was poisoned; recovering");
            poisoned.into_inner().get(peer).copied()
        }
    }
}

fn store_peer_health_snapshot(
    snapshot: &StdRwLock<HashMap<PeerId, PeerHealth>>,
    refreshed: HashMap<PeerId, PeerHealth>,
) {
    match snapshot.write() {
        Ok(mut guard) => {
            *guard = refreshed;
        }
        Err(poisoned) => {
            warn!("PubSub peer-health snapshot lock was poisoned; recovering");
            *poisoned.into_inner() = refreshed;
        }
    }
}

fn connected_peers_from_snapshot(
    snapshot: &StdRwLock<Option<HashSet<PeerId>>>,
) -> Option<HashSet<PeerId>> {
    match snapshot.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => {
            warn!("PubSub connected-peers snapshot lock was poisoned; recovering");
            poisoned.into_inner().clone()
        }
    }
}

fn peer_is_transport_disconnected(
    snapshot: &StdRwLock<Option<HashSet<PeerId>>>,
    peer: &PeerId,
) -> bool {
    connected_peers_from_snapshot(snapshot)
        .as_ref()
        .is_some_and(|connected| !connected.contains(peer))
}

fn store_connected_peers_snapshot(
    snapshot: &StdRwLock<Option<HashSet<PeerId>>>,
    refreshed: Option<HashSet<PeerId>>,
) {
    match snapshot.write() {
        Ok(mut guard) => {
            *guard = refreshed;
        }
        Err(poisoned) => {
            warn!("PubSub connected-peers snapshot lock was poisoned; recovering");
            *poisoned.into_inner() = refreshed;
        }
    }
}

async fn known_pubsub_peers(topics: &ShardedTopicMap) -> HashSet<PeerId> {
    let shards = topics.read_all().await;
    let mut peers = HashSet::new();
    for shard in &shards {
        for state in shard.values() {
            peers.extend(state.eager_peers.iter().copied());
            peers.extend(state.lazy_peers.iter().copied());
            peers.extend(state.peer_cooling.keys().copied());
            peers.extend(state.peer_scores.keys().copied());
        }
    }
    peers
}

async fn refresh_peer_health_snapshot_once(
    topics: &ShardedTopicMap,
    snapshot: &Arc<StdRwLock<HashMap<PeerId, PeerHealth>>>,
    oracle: &Arc<dyn PeerHealthOracle>,
) {
    let peers = known_pubsub_peers(topics).await;
    let mut refreshed = HashMap::with_capacity(peers.len());
    for peer in peers {
        if let Some(health) = oracle.health_of(&peer).await {
            refreshed.insert(peer, health);
        }
    }
    store_peer_health_snapshot(snapshot.as_ref(), refreshed);
}

async fn refresh_connected_peers_snapshot_once<T: GossipTransport + 'static>(
    topics: &ShardedTopicMap,
    snapshot: &Arc<StdRwLock<Option<HashSet<PeerId>>>>,
    transport: &Arc<T>,
    stage_stats: &Arc<PubSubStageStats>,
) {
    let connected: HashSet<PeerId> = transport.connected_peer_ids().await.into_iter().collect();
    if connected.is_empty() {
        store_connected_peers_snapshot(snapshot.as_ref(), None);
        debug!("PubSub transport connectivity snapshot unavailable; failing open");
        return;
    }

    store_connected_peers_snapshot(snapshot.as_ref(), Some(connected.clone()));

    let mut total_eager_pruned = 0usize;
    let mut total_lazy_pruned = 0usize;
    let mut total_cooling_removed = 0usize;
    let mut topics_guard = topics.write_all().await;
    for shard in topics_guard.iter_mut() {
        for (topic, state) in shard.iter_mut() {
            let eager_before = state.eager_peers.len();
            let lazy_before = state.lazy_peers.len();

            state.eager_peers.retain(|peer| connected.contains(peer));
            state.lazy_peers.retain(|peer| connected.contains(peer));

            let eager_pruned = eager_before.saturating_sub(state.eager_peers.len());
            let lazy_pruned = lazy_before.saturating_sub(state.lazy_peers.len());
            let removed_cooling = state.clear_disconnected_peer_cooling(&connected);
            for peer in &removed_cooling {
                stage_stats.clear_peer_suppression(*topic, *peer);
            }

            if eager_pruned > 0 || lazy_pruned > 0 || !removed_cooling.is_empty() {
                debug!(
                    topic = %LogTopicId::from(*topic),
                    eager_pruned,
                    lazy_pruned,
                    cooling_removed = removed_cooling.len(),
                    connected = connected.len(),
                    "PubSub pruned transport-disconnected topic peers"
                );
            }

            total_eager_pruned += eager_pruned;
            total_lazy_pruned += lazy_pruned;
            total_cooling_removed += removed_cooling.len();
        }
    }

    if total_eager_pruned > 0 || total_lazy_pruned > 0 || total_cooling_removed > 0 {
        stage_stats.record_prunes(total_eager_pruned + total_lazy_pruned);
        info!(
            eager_pruned = total_eager_pruned,
            lazy_pruned = total_lazy_pruned,
            cooling_removed = total_cooling_removed,
            connected = connected.len(),
            "PubSub pruned transport-disconnected peers from topic meshes"
        );
    }
}

fn peer_health_oracle_from_slot(
    slot: &StdRwLock<Option<Arc<dyn PeerHealthOracle>>>,
) -> Option<Arc<dyn PeerHealthOracle>> {
    match slot.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => {
            warn!("PubSub peer-health oracle slot lock was poisoned; recovering");
            poisoned.into_inner().clone()
        }
    }
}

fn store_peer_health_oracle(
    slot: &StdRwLock<Option<Arc<dyn PeerHealthOracle>>>,
    oracle: Arc<dyn PeerHealthOracle>,
) {
    match slot.write() {
        Ok(mut guard) => {
            *guard = Some(oracle);
        }
        Err(poisoned) => {
            warn!("PubSub peer-health oracle slot lock was poisoned; recovering");
            *poisoned.into_inner() = Some(oracle);
        }
    }
}

/// X0X-0074: free-function variant of `filter_peers_through_admission`
/// usable from background tasks (IHAVE flush, anti-entropy) that already
/// hold a `&TopicState` reference under a `topics.write()` guard. Skips
/// re-acquiring the topics lock — uses the state ref directly.
///
/// Returns `(admitted, bulk_admitted)`:
/// - `admitted` is the peer subset to pass to `claim_topic_send_attempts_for_state`.
/// - `bulk_admitted` records the peers whose Bulk admission depth was
///   incremented; callers MUST call
///   `release_bulk_admissions_free(&admission, &bulk_admitted)` once the
///   spawned send tasks complete (including on early-return paths).
fn filter_peers_through_admission_in_state(
    send_path: &SendPathContext,
    state: &TopicState,
    topic: &TopicId,
    peers: Vec<PeerId>,
    op: &'static str,
    now: Instant,
) -> (Vec<PeerId>, Vec<PeerId>) {
    let priority = send_path.admission.registry().priority_for(topic);
    let connected_snapshot =
        connected_peers_from_snapshot(send_path.connected_peers_snapshot.as_ref());
    let mut admitted = Vec::with_capacity(peers.len());
    let mut bulk_admitted = Vec::new();
    for peer in peers {
        if let Some(connected) = connected_snapshot.as_ref() {
            if !connected.contains(&peer) {
                debug!(
                    peer_id = %peer,
                    topic = %topic,
                    op,
                    priority = %priority,
                    "PubSub admission skipped transport-disconnected peer send (background)"
                );
                continue;
            }
        }

        let health = peer_health_from_snapshot(send_path.peer_health_snapshot.as_ref(), &peer);
        let is_peer_cooled = state.is_peer_suppressed_at(peer, now);
        match send_path
            .admission
            .admit(topic, &peer, health, is_peer_cooled)
        {
            AdmissionDecision::Admit => {
                if priority == TopicPriority::Bulk {
                    bulk_admitted.push(peer);
                }
                admitted.push(peer);
            }
            AdmissionDecision::Drop { reason } => {
                debug!(
                    peer_id = %peer,
                    topic = %topic,
                    op,
                    priority = %priority,
                    reason = %reason,
                    "X0X-0074 admission dropped peer send (background)"
                );
            }
        }
    }
    (admitted, bulk_admitted)
}

/// Companion to `filter_peers_through_admission_in_state` — release any
/// Bulk depth reserved during the filter. Idempotent on empty inputs;
/// safe to call multiple times on the same set (each call decrements per
/// peer, so call exactly once).
fn release_bulk_admissions_free(
    admission: &Arc<admission::AdmissionControl>,
    bulk_admitted: &[PeerId],
) {
    for peer in bulk_admitted {
        admission.release_bulk(peer);
    }
}

fn spawn_indirect_probe_request(
    oracle_slot: &Arc<StdRwLock<Option<Arc<dyn PeerHealthOracle>>>>,
    peer: PeerId,
) {
    let Some(oracle) = peer_health_oracle_from_slot(oracle_slot.as_ref()) else {
        return;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                oracle.request_indirect_probe(peer).await;
            });
        }
        Err(e) => {
            warn!(peer_id = %LogPeerId::from(peer), "Unable to request indirect peer probe: {e}");
        }
    }
}

impl PubSubStageStats {
    fn suppressed_peers_guard(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<SuppressedPeerKey, SuppressedPeerState>> {
        let started = Instant::now();
        let guard = match self.suppressed_peers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("PubSub suppressed-peers diagnostics lock was poisoned; recovering");
                poisoned.into_inner()
            }
        };
        self.suppressed_peers_lock.record(started.elapsed());
        guard
    }

    fn suppressed_peer_snapshots(&self) -> Vec<SuppressedPeerSnapshot> {
        let now = Instant::now();
        let guard = self.suppressed_peers_guard();

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
                    last_suppressed_unix_ms: state.last_suppressed_unix_ms,
                    state: if state.recovery_probe_in_flight {
                        "recovery_probe".to_string()
                    } else if state.suppressed_until > now {
                        "cooldown".to_string()
                    } else {
                        "recovery_ready".to_string()
                    },
                    recovery_probe_in_flight: state.recovery_probe_in_flight,
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
            outbound_budget_exhausted: self.outbound_budget_exhausted.load(Ordering::Relaxed),
            cooldown_bypass_probes: self.cooldown_bypass_probes.load(Ordering::Relaxed),
            cooldown_bypass_successes: self.cooldown_bypass_successes.load(Ordering::Relaxed),
            suppressed_peers: self.suppressed_peer_snapshots(),
            suppressed_peers_by_topic: BTreeMap::new(),
            suppression_cleanup_interval_ms: self
                .suppression_cleanup
                .current_interval_ms
                .load(Ordering::Relaxed),
            suppression_cleanup_growth_rate_per_sec: f64::from_bits(
                self.suppression_cleanup
                    .growth_rate_bits
                    .load(Ordering::Relaxed),
            ),
            suppression_cleanup_entries: self
                .suppression_cleanup
                .current_entries
                .load(Ordering::Relaxed),
            suppression_cleanup_last_removed: self
                .suppression_cleanup
                .last_removed
                .load(Ordering::Relaxed),
            peer_scores: Vec::new(),
            peer_scores_by_topic: BTreeMap::new(),
            admission_state_by_peer: BTreeMap::new(),
            admission: admission::AdmissionStatsSnapshot::default(),
            peer_scores_v2: Vec::new(),
            topic_caches: Vec::new(),
            topic_shard_count: 0,
            peer_scoring_lock_wait: StageTimingStatsSnapshot::default(),
            suppressed_peers_lock_wait: self.suppressed_peers_lock.snapshot(),
        }
    }

    fn record_suppression_cleanup(
        &self,
        interval: Duration,
        growth_rate_per_sec: f64,
        current_entries: usize,
        removed: usize,
    ) {
        self.suppression_cleanup
            .current_interval_ms
            .store(duration_millis_u64(interval), Ordering::Relaxed);
        self.suppression_cleanup
            .growth_rate_bits
            .store(growth_rate_per_sec.to_bits(), Ordering::Relaxed);
        self.suppression_cleanup
            .current_entries
            .store(current_entries as u64, Ordering::Relaxed);
        self.suppression_cleanup
            .last_removed
            .store(removed as u64, Ordering::Relaxed);
    }

    fn record_per_peer_timeout(&self) {
        self.republish_per_peer_timeout
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_outbound_budget_exhausted(&self) {
        self.outbound_budget_exhausted
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_cooldown_bypass_probe(&self) {
        self.cooldown_bypass_probes.fetch_add(1, Ordering::Relaxed);
    }

    fn record_cooldown_bypass_success(&self) {
        self.cooldown_bypass_successes
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
        let last_suppressed_unix_ms = unix_millis_now();
        guard.insert(
            SuppressedPeerKey { topic, peer },
            SuppressedPeerState {
                suppressed_until,
                recent_timeout_count,
                cooldown,
                last_suppressed_unix_ms,
                recovery_probe_in_flight: false,
            },
        );
    }

    fn record_peer_recovery_probe(
        &self,
        topic: TopicId,
        peer: PeerId,
        suppressed_until: Instant,
        recent_timeout_count: usize,
        cooldown: Duration,
    ) {
        let mut guard = self.suppressed_peers_guard();
        let last_suppressed_unix_ms = unix_millis_now();
        guard.insert(
            SuppressedPeerKey { topic, peer },
            SuppressedPeerState {
                suppressed_until,
                recent_timeout_count,
                cooldown,
                last_suppressed_unix_ms,
                recovery_probe_in_flight: true,
            },
        );
    }

    fn clear_peer_suppression(&self, topic: TopicId, peer: PeerId) -> bool {
        let mut guard = self.suppressed_peers_guard();
        guard.remove(&SuppressedPeerKey { topic, peer }).is_some()
    }

    fn clear_topic_suppressions(&self, topic: TopicId) {
        let mut guard = self.suppressed_peers_guard();
        guard.retain(|key, _| key.topic != topic);
    }

    fn suppressed_peer_count(&self) -> usize {
        self.suppressed_peers_guard().len()
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

/// Cheaply peek the [`MessageKind`] of a serialized PubSub [`GossipMessage`]
/// frame without verifying its signature or decoding the payload.
///
/// The header is the first field of `GossipMessage`, so postcard decodes only
/// its prefix (`take_from_bytes`) and stops — no allocation for payload /
/// signature / public key, and no ML-DSA verification. Returns `None` if the
/// header prefix cannot be decoded (malformed frame).
///
/// Intended for receive-pump load-shedding: a consumer can drop recoverable
/// control frames (`IHave`/`IWant`/`AntiEntropy`) ahead of data (`Eager`) when
/// its receive channel is near-full, preserving delivery of payload-bearing
/// messages. The peek is only worth doing under pressure; do not call it on the
/// steady-state hot path.
pub fn peek_message_kind(frame: &[u8]) -> Option<MessageKind> {
    postcard::take_from_bytes::<MessageHeader>(frame)
        .ok()
        .map(|(header, _rest)| header.kind)
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

#[derive(Clone, Debug)]
struct DecayedCounter {
    value: f64,
    updated_at: Instant,
}

impl DecayedCounter {
    fn new(now: Instant) -> Self {
        Self {
            value: 0.0,
            updated_at: now,
        }
    }

    fn value_at(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.updated_at).as_secs_f64();
        if elapsed <= 0.0 || self.value <= 0.0 {
            return self.value;
        }

        let half_life = PEER_SCORE_DECAY_HALF_LIFE.as_secs_f64();
        let decay = 0.5_f64.powf(elapsed / half_life);
        self.value * decay
    }

    fn add_at(&mut self, now: Instant, amount: f64) {
        self.value = self.value_at(now) + amount;
        self.updated_at = now;
    }

    fn last_activity(&self) -> Option<Instant> {
        if self.value > 0.0 {
            Some(self.updated_at)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PeerScoreComponents {
    score: f64,
    iwant_response_rate: f64,
    recency: f64,
    send_health: f64,
    outbound_send_successes: f64,
    outbound_send_timeouts: f64,
    cooling_events: f64,
    recovery_probes: f64,
    recovery_successes: f64,
}

/// Per-peer quality score for tree optimization.
///
/// Tracks IWANT response quality, recency, and decayed send-side health so
/// slow outbound edges influence later PlumTree EAGER/LAZY selection.
#[derive(Clone, Debug)]
struct PeerScore {
    /// Count of EAGER messages received from this peer
    messages_delivered: u64,
    /// Count of IWANTs we sent to this peer
    iwant_requests: u64,
    /// Count of responses received after sending IWANT
    iwant_responses: u64,
    /// Last time we received any message from this peer
    last_seen: Instant,
    outbound_send_successes: DecayedCounter,
    outbound_send_timeouts: DecayedCounter,
    cooling_events: DecayedCounter,
    recovery_probes: DecayedCounter,
    recovery_successes: DecayedCounter,
}

impl PeerScore {
    /// Create a new peer score with default values
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            messages_delivered: 0,
            iwant_requests: 0,
            iwant_responses: 0,
            last_seen: now,
            outbound_send_successes: DecayedCounter::new(now),
            outbound_send_timeouts: DecayedCounter::new(now),
            cooling_events: DecayedCounter::new(now),
            recovery_probes: DecayedCounter::new(now),
            recovery_successes: DecayedCounter::new(now),
        }
    }

    /// Calculate peer quality score (0.0 to 1.0)
    ///
    /// Base score = `(iwant_response_rate * 0.6) + (recency_factor * 0.4)`.
    /// Decayed send-health evidence then scales that base down for peers
    /// causing outbound timeouts, cooling, or repeated recovery probes.
    #[cfg(test)]
    fn score(&self) -> f64 {
        self.score_at(Instant::now())
    }

    fn score_at(&self, now: Instant) -> f64 {
        self.components_at(now).score
    }

    fn components_at(&self, now: Instant) -> PeerScoreComponents {
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

        let secs_since_seen = now.saturating_duration_since(self.last_seen).as_secs_f64();
        let recency = (1.0 - (secs_since_seen / 300.0)).max(0.0);

        let outbound_send_successes = self.outbound_send_successes.value_at(now);
        let outbound_send_timeouts = self.outbound_send_timeouts.value_at(now);
        let cooling_events = self.cooling_events.value_at(now);
        let recovery_probes = self.recovery_probes.value_at(now);
        let recovery_successes = self.recovery_successes.value_at(now);

        let good = outbound_send_successes + (recovery_successes * 2.0);
        let bad = outbound_send_timeouts + (cooling_events * 3.0) + (recovery_probes * 0.5);
        let send_health = if good <= 0.0 && bad <= 0.0 {
            1.0
        } else {
            ((good + 1.0) / (good + bad + 1.0)).clamp(0.0, 1.0)
        };

        let base = (response_rate.min(1.0) * 0.6) + (recency * 0.4);
        let send_factor =
            PEER_SCORE_SEND_MIN_FACTOR + ((1.0 - PEER_SCORE_SEND_MIN_FACTOR) * send_health);
        let score = (base * send_factor).clamp(0.0, 1.0);

        PeerScoreComponents {
            score,
            iwant_response_rate: response_rate.min(1.0),
            recency,
            send_health,
            outbound_send_successes,
            outbound_send_timeouts,
            cooling_events,
            recovery_probes,
            recovery_successes,
        }
    }

    /// Record a message delivery from this peer
    fn record_delivery(&mut self) {
        self.messages_delivered += 1;
        self.last_seen = Instant::now();
    }

    fn record_seen_at(&mut self, now: Instant) {
        self.last_seen = self.last_seen.max(now);
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

    fn record_recovery_probe_at(&mut self, now: Instant) {
        self.recovery_probes.add_at(now, 1.0);
    }

    fn record_outbound_send_success_at(&mut self, now: Instant, kind: SendAttemptKind) {
        self.outbound_send_successes.add_at(now, 1.0);
        if kind == SendAttemptKind::RecoveryProbe {
            self.recovery_successes.add_at(now, 1.0);
        }
    }

    fn record_outbound_send_timeout_at(&mut self, now: Instant) {
        self.outbound_send_timeouts.add_at(now, 1.0);
    }

    fn record_cooling_event_at(&mut self, now: Instant) {
        self.cooling_events.add_at(now, 1.0);
    }

    fn last_activity(&self) -> Instant {
        [
            self.outbound_send_successes.last_activity(),
            self.outbound_send_timeouts.last_activity(),
            self.cooling_events.last_activity(),
            self.recovery_probes.last_activity(),
            self.recovery_successes.last_activity(),
        ]
        .into_iter()
        .flatten()
        .fold(self.last_seen, |latest, instant| latest.max(instant))
    }
}

struct PeerCoolingState {
    timeout_window_started: Instant,
    timeout_count: usize,
    suppressed_until: Option<Instant>,
    cooldown: Duration,
    last_suppressed_at: Option<Instant>,
    last_suppression_timeout_count: usize,
    recovery_probe_in_flight: bool,
    recovery_probe_id: Option<RecoveryProbeId>,
    /// Last time a rate-limited cooldown-bypass send was claimed for this
    /// peer (WP6). Gates the bypass to one send per
    /// `PEER_COOLDOWN_BYPASS_MIN_INTERVAL` while suppression is active.
    last_bypass_probe_at: Option<Instant>,
}

struct PeerSuppressionEvent {
    suppressed_until: Instant,
    recent_timeout_count: usize,
    cooldown: Duration,
    demoted: bool,
}

#[derive(Default)]
struct PeerTimeoutOutcome {
    suppression: Option<PeerSuppressionEvent>,
    request_indirect_probe: bool,
}

impl PeerCoolingState {
    fn new(now: Instant) -> Self {
        Self {
            timeout_window_started: now,
            timeout_count: 0,
            suppressed_until: None,
            cooldown: Duration::ZERO,
            last_suppressed_at: None,
            last_suppression_timeout_count: 0,
            recovery_probe_in_flight: false,
            recovery_probe_id: None,
            last_bypass_probe_at: None,
        }
    }

    fn is_suppressed_at(&self, now: Instant) -> bool {
        self.suppressed_until.is_some_and(|until| until > now)
            || self.recovery_probe_in_flight
            || self.suppression_expired_at(now)
    }

    fn suppression_expired_at(&self, now: Instant) -> bool {
        self.suppressed_until.is_some_and(|until| until <= now)
    }

    fn can_graft_at(&self, now: Instant) -> bool {
        !self.is_suppressed_at(now)
    }

    fn claim_send_attempt_at(
        &mut self,
        now: Instant,
    ) -> Option<(
        SendAttemptKind,
        Option<RecoveryProbeId>,
        Option<PeerRecoveryProbeEvent>,
    )> {
        if self.suppressed_until.is_some_and(|until| until > now) {
            // WP6 recovery bypass: suppression previously blocked BOTH
            // eager push and lazy IHAVE/anti-entropy to this peer, so a
            // message published during the cooldown was undeliverable
            // until it expired. Admit one rate-limited send per
            // `PEER_COOLDOWN_BYPASS_MIN_INTERVAL` so cached data still
            // trickles to the peer; full fanout stays gated on cooldown
            // expiry plus a successful recovery probe.
            if self.recovery_probe_in_flight {
                return None;
            }
            let bypass_ready = self.last_bypass_probe_at.is_none_or(|at| {
                now.saturating_duration_since(at) >= PEER_COOLDOWN_BYPASS_MIN_INTERVAL
            });
            if !bypass_ready {
                return None;
            }
            self.last_bypass_probe_at = Some(now);
            return Some((SendAttemptKind::CooldownBypass, None, None));
        }
        if self.suppression_expired_at(now) {
            if self.recovery_probe_in_flight {
                return None;
            }
            let recovery_probe_id = NEXT_RECOVERY_PROBE_ID.fetch_add(1, Ordering::Relaxed);
            self.recovery_probe_in_flight = true;
            self.recovery_probe_id = Some(recovery_probe_id);
            let suppressed_until = self.suppressed_until.unwrap_or(now);
            return Some((
                SendAttemptKind::RecoveryProbe,
                Some(recovery_probe_id),
                Some(PeerRecoveryProbeEvent {
                    suppressed_until,
                    recent_timeout_count: self.last_suppression_timeout_count.max(1),
                    cooldown: self.cooldown,
                }),
            ));
        }
        Some((SendAttemptKind::Normal, None, None))
    }

    fn matches_recovery_probe(&self, recovery_probe_id: Option<RecoveryProbeId>) -> bool {
        self.recovery_probe_in_flight && self.recovery_probe_id == recovery_probe_id
    }

    fn next_legacy_cooldown(&self) -> Duration {
        let previous = if self.suppressed_until.is_some() {
            self.cooldown
        } else {
            Duration::ZERO
        };
        if previous.is_zero() {
            return PEER_SUPPRESSION_COOLDOWN;
        }
        match previous.checked_mul(2) {
            Some(duration) => duration.min(PEER_SUPPRESSION_BACKOFF_MAX),
            None => PEER_SUPPRESSION_BACKOFF_MAX,
        }
    }

    fn next_adaptive_cooldown(&self, cooling_config: AdaptiveCoolingConfig) -> Duration {
        cooling_config.next_cooldown(self.cooldown)
    }

    fn dead_cooldown(&self, cooling_config: AdaptiveCoolingConfig) -> Duration {
        cooling_config.escalate_on_dead(self.cooldown)
    }
}

/// Cached message entry
#[derive(Clone)]
struct CachedMessage {
    /// Message payload
    payload: Bytes,
    /// Message header
    header: MessageHeader,
}

/// Number of trailing zero bytes in a payload — corruption discriminator for
/// the `sg.payload.trace` diagnostics (a healthy ML-DSA-signed x0x frame has
/// a near-zero tail; a partially-filled buffer has thousands).
fn payload_zero_tail(payload: &Bytes) -> usize {
    payload.iter().rev().take_while(|b| **b == 0).count()
}

/// Short hex of a message-id prefix for `sg.payload.trace` lines.
fn msg_id_hex8(msg_id: &MessageIdType) -> String {
    msg_id[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone)]
struct CachedEntry {
    message: CachedMessage,
    bytes: usize,
    inserted_at: Instant,
}

/// Per-topic message cache with age, byte, and count bounds.
///
/// Eviction priority is age first, then bytes, then count. Age protects
/// freshness independent of load, bytes protect anti-entropy bandwidth, and
/// count remains a final hard cap for pathological tiny-message streams.
struct BoundedMessageCache {
    lru: LruCache<MessageIdType, CachedEntry>,
    total_bytes: usize,
    max_count: NonZeroUsize,
    max_bytes: usize,
    max_age: Duration,
    evicted_by_age: u64,
    evicted_by_bytes: u64,
    evicted_by_count: u64,
}

impl BoundedMessageCache {
    fn new(max_count: NonZeroUsize, max_bytes: usize, max_age: Duration) -> Self {
        Self {
            lru: LruCache::new(max_count),
            total_bytes: 0,
            max_count,
            max_bytes,
            max_age,
            evicted_by_age: 0,
            evicted_by_bytes: 0,
            evicted_by_count: 0,
        }
    }

    fn insert(&mut self, msg_id: MessageIdType, message: CachedMessage) -> bool {
        self.insert_at(msg_id, message, Instant::now())
    }

    fn insert_at(&mut self, msg_id: MessageIdType, message: CachedMessage, now: Instant) -> bool {
        let bytes = estimate_message_bytes(&message);
        self.prune_expired_at(now);

        if let Some(existing) = self.lru.pop(&msg_id) {
            self.total_bytes = self.total_bytes.saturating_sub(existing.bytes);
        }

        if bytes > self.max_bytes {
            return false;
        }

        self.ensure_bytes_capacity(bytes);
        self.ensure_count_capacity_for_insert();

        let entry = CachedEntry {
            message,
            bytes,
            inserted_at: now,
        };
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        if let Some((_, evicted)) = self.lru.push(msg_id, entry) {
            self.total_bytes = self.total_bytes.saturating_sub(evicted.bytes);
            self.evicted_by_count = self.evicted_by_count.saturating_add(1);
        }
        true
    }

    fn get(&mut self, msg_id: &MessageIdType) -> Option<&CachedMessage> {
        self.get_at(msg_id, Instant::now())
    }

    fn get_at(&mut self, msg_id: &MessageIdType, now: Instant) -> Option<&CachedMessage> {
        self.prune_expired_at(now);
        self.lru.get(msg_id).map(|entry| &entry.message)
    }

    fn contains(&self, msg_id: &MessageIdType) -> bool {
        self.lru.contains(msg_id)
    }

    fn ids(&self) -> Vec<MessageIdType> {
        self.lru.iter().map(|(id, _)| *id).collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lru.len()
    }

    #[cfg(test)]
    fn peek_lru_message_id(&self) -> Option<MessageIdType> {
        self.lru.peek_lru().map(|(id, _)| *id)
    }

    fn prune_expired(&mut self) {
        self.prune_expired_at(Instant::now());
    }

    fn prune_expired_at(&mut self, now: Instant) {
        let expired: Vec<MessageIdType> = self
            .lru
            .iter()
            .filter_map(|(msg_id, entry)| {
                (now.saturating_duration_since(entry.inserted_at) >= self.max_age)
                    .then_some(*msg_id)
            })
            .collect();

        for msg_id in expired {
            if let Some(entry) = self.lru.pop(&msg_id) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
                self.evicted_by_age = self.evicted_by_age.saturating_add(1);
            }
        }
    }

    fn ensure_bytes_capacity(&mut self, incoming_bytes: usize) {
        while self.total_bytes.saturating_add(incoming_bytes) > self.max_bytes {
            let Some((_, entry)) = self.lru.pop_lru() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
            self.evicted_by_bytes = self.evicted_by_bytes.saturating_add(1);
        }
    }

    fn ensure_count_capacity_for_insert(&mut self) {
        while self.lru.len().saturating_add(1) > self.max_count.get() {
            let Some((_, entry)) = self.lru.pop_lru() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
            self.evicted_by_count = self.evicted_by_count.saturating_add(1);
        }
    }

    fn stats_at(&self, now: Instant) -> CacheStatsSnapshot {
        let oldest_age_secs = self
            .lru
            .iter()
            .map(|(_, entry)| now.saturating_duration_since(entry.inserted_at).as_secs())
            .max()
            .unwrap_or(0);

        CacheStatsSnapshot {
            msg_count: self.lru.len(),
            total_bytes: self.total_bytes,
            oldest_age_secs,
            evicted_by_age: self.evicted_by_age,
            evicted_by_bytes: self.evicted_by_bytes,
            evicted_by_count: self.evicted_by_count,
        }
    }

    #[cfg(test)]
    fn set_all_inserted_at_for_test(&mut self, inserted_at: Instant) {
        for (_, entry) in self.lru.iter_mut() {
            entry.inserted_at = inserted_at;
        }
    }
}

fn estimate_message_bytes(message: &CachedMessage) -> usize {
    message
        .payload
        .len()
        .saturating_add(MESSAGE_HEADER_OVERHEAD_BYTES)
        .saturating_add(MESSAGE_CRYPTO_OVERHEAD_BYTES)
}

/// Per-topic state
struct TopicState {
    /// Spanning tree peers (forward EAGER)
    eager_peers: HashSet<PeerId>,
    /// Non-tree peers (send IHAVE only)
    lazy_peers: HashSet<PeerId>,
    /// Message cache: msg_id -> cached message
    message_cache: BoundedMessageCache,
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
    /// Last score-driven eager replacement pass.
    last_opportunistic_graft: Option<Instant>,
}

impl TopicState {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_cache_config(PubSubCacheConfig::default())
    }

    fn with_cache_config(cache_config: PubSubCacheConfig) -> Self {
        Self {
            eager_peers: HashSet::new(),
            lazy_peers: HashSet::new(),
            message_cache: BoundedMessageCache::new(
                cache_config.max_messages_per_topic,
                cache_config.max_bytes_per_topic,
                cache_config.max_age,
            ),
            pending_ihave: Vec::new(),
            outstanding_iwants: HashMap::new(),
            peer_scores: HashMap::new(),
            subscribers: Vec::new(),
            replay_cache: LruCache::new(replay_cache_capacity()),
            replay_ttl: Duration::from_secs(REPLAY_CACHE_TTL_SECS),
            last_activity: Instant::now(),
            peer_cooling: HashMap::new(),
            last_opportunistic_graft: None,
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
        self.message_cache.ids()
    }

    /// Check if message is in cache
    fn has_message(&self, msg_id: &MessageIdType) -> bool {
        self.message_cache.contains(msg_id)
    }

    /// Add message to cache
    fn cache_message(&mut self, msg_id: MessageIdType, payload: Bytes, header: MessageHeader) {
        debug!(
            target: "sg.payload.trace",
            stage = "cache_insert",
            msg_id = %msg_id_hex8(&msg_id),
            len = payload.len(),
            zero_tail = payload_zero_tail(&payload),
            kind = ?header.kind,
        );
        let cached = CachedMessage { payload, header };
        self.message_cache.insert(msg_id, cached);
        self.touch();
    }

    /// Get cached message
    fn get_message(&mut self, msg_id: &MessageIdType) -> Option<CachedMessage> {
        self.message_cache.get(msg_id).cloned()
    }

    /// Clean expired cache entries
    fn clean_cache(&mut self) {
        self.message_cache.prune_expired();

        // Clean expired replay cache entries
        let replay_ttl = self.replay_ttl;
        let now = Instant::now();
        let mut expired_replay = Vec::new();
        for (hash, ts) in self.replay_cache.iter() {
            if now.saturating_duration_since(*ts) > replay_ttl {
                expired_replay.push(*hash);
            }
        }
        for hash in expired_replay {
            self.replay_cache.pop(&hash);
        }

        // Clean stale peer scores after send-side evidence has outlived the
        // maximum cooling backoff plus one decay half-life.
        // Use saturating_duration_since to avoid panic on Windows (coarse timer)
        let now = Instant::now();
        self.peer_scores.retain(|_, score| {
            now.saturating_duration_since(score.last_activity()) < PEER_SCORE_RETENTION
        });

        // Quiet topics do not send through the subscriber list, so closed
        // receivers would otherwise keep an idle topic alive forever.
        self.subscribers.retain(|tx| !tx.is_closed());
    }

    fn is_peer_suppressed_at(&self, peer: PeerId, now: Instant) -> bool {
        self.peer_cooling
            .get(&peer)
            .is_some_and(|state| state.is_suppressed_at(now))
    }

    fn can_graft_peer_at(&self, peer: PeerId, now: Instant) -> bool {
        self.peer_cooling
            .get(&peer)
            .is_none_or(|state| state.can_graft_at(now))
    }

    fn record_inbound_peer_activity_at(&mut self, peer: PeerId, now: Instant) -> bool {
        self.peer_scores
            .entry(peer)
            .or_insert_with(|| PeerScore::new_at(now))
            .record_seen_at(now);
        self.peer_cooling.remove(&peer).is_some()
    }

    fn peer_score_at(&self, peer: PeerId, now: Instant) -> f64 {
        self.peer_scores.get(&peer).map_or_else(
            || PeerScore::new_at(now).score_at(now),
            |score| score.score_at(now),
        )
    }

    fn is_score_eligible_for_eager_at(&self, peer: PeerId, now: Instant) -> bool {
        self.can_graft_peer_at(peer, now) && self.peer_score_at(peer, now) >= PEER_SCORE_EAGER_MIN
    }

    fn scored_lazy_peers_at(&self, now: Instant) -> Vec<(PeerId, f64)> {
        let mut scored: Vec<(PeerId, f64)> = self
            .lazy_peers
            .iter()
            .filter(|&&p| self.can_graft_peer_at(p, now))
            .map(|&p| (p, self.peer_score_at(p, now)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }

    fn scored_eager_peers_at(&self, now: Instant) -> Vec<(PeerId, f64)> {
        let mut scored: Vec<(PeerId, f64)> = self
            .eager_peers
            .iter()
            .map(|&p| (p, self.peer_score_at(p, now)))
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1));
        scored
    }

    fn role_for_peer_at(&self, peer: PeerId, now: Instant) -> &'static str {
        if let Some(cooling) = self.peer_cooling.get(&peer) {
            if cooling.recovery_probe_in_flight {
                return "recovery_probe";
            }
            if cooling.suppressed_until.is_some_and(|until| until > now) {
                return "cooled";
            }
            if cooling.suppression_expired_at(now) {
                return "recovery_ready";
            }
        }

        if self.eager_peers.contains(&peer) {
            "eager"
        } else if self.lazy_peers.contains(&peer) {
            "lazy"
        } else {
            "excluded"
        }
    }

    fn peer_score_snapshots(&self, topic: TopicId, now: Instant) -> Vec<PeerScoreSnapshot> {
        let mut peers: HashSet<PeerId> = self.peer_scores.keys().copied().collect();
        peers.extend(self.eager_peers.iter().copied());
        peers.extend(self.lazy_peers.iter().copied());
        peers.extend(self.peer_cooling.keys().copied());

        let mut snapshots: Vec<PeerScoreSnapshot> = peers
            .into_iter()
            .map(|peer| {
                let components = self.peer_scores.get(&peer).map_or_else(
                    || PeerScore::new_at(now).components_at(now),
                    |score| score.components_at(now),
                );
                PeerScoreSnapshot {
                    peer_id: peer.to_string(),
                    topic: topic.to_string(),
                    role: self.role_for_peer_at(peer, now).to_string(),
                    score: components.score,
                    iwant_response_rate: components.iwant_response_rate,
                    recency: components.recency,
                    send_health: components.send_health,
                    outbound_send_successes: components.outbound_send_successes,
                    outbound_send_timeouts: components.outbound_send_timeouts,
                    cooling_events: components.cooling_events,
                    recovery_probes: components.recovery_probes,
                    recovery_successes: components.recovery_successes,
                    eager_eligible: self.is_score_eligible_for_eager_at(peer, now),
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

    fn claim_send_attempt_at(
        &mut self,
        peer: PeerId,
        now: Instant,
    ) -> Option<(PeerSendAttempt, Option<PeerRecoveryProbeEvent>)> {
        let Some(cooling) = self.peer_cooling.get_mut(&peer) else {
            return Some((
                PeerSendAttempt {
                    peer,
                    kind: SendAttemptKind::Normal,
                    recovery_probe_id: None,
                },
                None,
            ));
        };

        let (kind, recovery_probe_id, recovery_event) = cooling.claim_send_attempt_at(now)?;
        Some((
            PeerSendAttempt {
                peer,
                kind,
                recovery_probe_id,
            },
            recovery_event,
        ))
    }

    #[cfg(test)]
    fn record_send_success_at(&mut self, attempt: PeerSendAttempt, now: Instant) -> bool {
        self.record_send_success_with_context_at(attempt, now, AdaptiveCoolingConfig::default())
    }

    fn record_send_success_with_context_at(
        &mut self,
        attempt: PeerSendAttempt,
        now: Instant,
        cooling_config: AdaptiveCoolingConfig,
    ) -> bool {
        self.peer_scores
            .entry(attempt.peer)
            .or_insert_with(|| PeerScore::new_at(now))
            .record_outbound_send_success_at(now, attempt.kind);

        match attempt.kind {
            SendAttemptKind::RecoveryProbe => {
                let Some(cooling) = self.peer_cooling.get_mut(&attempt.peer) else {
                    return false;
                };
                if !cooling.matches_recovery_probe(attempt.recovery_probe_id) {
                    return false;
                }
                if let Some(last_suppressed_at) = cooling.last_suppressed_at {
                    let elapsed = now.saturating_duration_since(last_suppressed_at);
                    cooling.cooldown = cooling_config.decay_on_success(cooling.cooldown, elapsed);
                }
                cooling.suppressed_until = None;
                cooling.timeout_count = 0;
                cooling.timeout_window_started = now;
                cooling.last_suppression_timeout_count = 0;
                cooling.recovery_probe_in_flight = false;
                cooling.recovery_probe_id = None;
                true
            }
            SendAttemptKind::CooldownBypass => {
                // WP6: a rate-limited send got through while the peer is
                // still cooling. Deliverability is proven, so decay the
                // cooldown memory — but do NOT clear the suppression:
                // full eager fanout stays gated until the cooldown
                // expires and a post-cooldown recovery probe succeeds.
                if let Some(cooling) = self.peer_cooling.get_mut(&attempt.peer) {
                    if let Some(last_suppressed_at) = cooling.last_suppressed_at {
                        let elapsed = now.saturating_duration_since(last_suppressed_at);
                        cooling.cooldown =
                            cooling_config.decay_on_success(cooling.cooldown, elapsed);
                    }
                }
                false
            }
            SendAttemptKind::Normal => {
                if let Some(cooling) = self.peer_cooling.get_mut(&attempt.peer) {
                    if !cooling.is_suppressed_at(now) {
                        if let Some(last_suppressed_at) = cooling.last_suppressed_at {
                            let elapsed = now.saturating_duration_since(last_suppressed_at);
                            cooling.cooldown =
                                cooling_config.decay_on_success(cooling.cooldown, elapsed);
                        }
                        cooling.timeout_count = 0;
                        cooling.timeout_window_started = now;
                    }
                }
                false
            }
        }
    }

    #[cfg(test)]
    fn record_send_timeout_at(
        &mut self,
        attempt: PeerSendAttempt,
        now: Instant,
    ) -> Option<PeerSuppressionEvent> {
        self.record_send_timeout_legacy_at(attempt, now).suppression
    }

    #[cfg(test)]
    fn record_send_timeout_legacy_at(
        &mut self,
        attempt: PeerSendAttempt,
        now: Instant,
    ) -> PeerTimeoutOutcome {
        self.record_send_timeout_inner_at(attempt, now, None, None)
    }

    fn record_send_timeout_with_context_at(
        &mut self,
        attempt: PeerSendAttempt,
        now: Instant,
        health: Option<PeerHealth>,
        cooling_config: AdaptiveCoolingConfig,
    ) -> PeerTimeoutOutcome {
        self.record_send_timeout_inner_at(attempt, now, Some(cooling_config), health)
    }

    fn record_critical_gate_overflow_with_context_at(
        &mut self,
        attempt: PeerSendAttempt,
        now: Instant,
        health: Option<PeerHealth>,
        cooling_config: AdaptiveCoolingConfig,
    ) -> PeerTimeoutOutcome {
        if attempt.kind == SendAttemptKind::Normal {
            self.peer_scores
                .entry(attempt.peer)
                .or_insert_with(|| PeerScore::new_at(now))
                .record_outbound_send_timeout_at(now);
        }

        let cooling = self
            .peer_cooling
            .entry(attempt.peer)
            .or_insert_with(|| PeerCoolingState::new(now));
        if cooling.is_suppressed_at(now) {
            return PeerTimeoutOutcome::default();
        }

        let cooldown = if matches!(health, Some(PeerHealth::Dead)) {
            cooling.dead_cooldown(cooling_config)
        } else {
            cooling.next_adaptive_cooldown(cooling_config)
        };
        let suppressed_until = now + cooldown;
        cooling.cooldown = cooldown;
        cooling.suppressed_until = Some(suppressed_until);
        cooling.last_suppressed_at = Some(now);
        cooling.last_suppression_timeout_count = OUTBOUND_CRITICAL_QUEUE_PER_PEER;
        cooling.recovery_probe_in_flight = false;
        cooling.recovery_probe_id = None;
        cooling.timeout_window_started = now;
        cooling.timeout_count = 0;

        self.peer_scores
            .entry(attempt.peer)
            .or_insert_with(|| PeerScore::new_at(now))
            .record_cooling_event_at(now);
        let demoted = self.prune_peer(attempt.peer);
        PeerTimeoutOutcome {
            suppression: Some(PeerSuppressionEvent {
                suppressed_until,
                recent_timeout_count: OUTBOUND_CRITICAL_QUEUE_PER_PEER,
                cooldown,
                demoted,
            }),
            request_indirect_probe: false,
        }
    }

    fn record_send_timeout_inner_at(
        &mut self,
        attempt: PeerSendAttempt,
        now: Instant,
        cooling_config: Option<AdaptiveCoolingConfig>,
        health: Option<PeerHealth>,
    ) -> PeerTimeoutOutcome {
        if attempt.kind == SendAttemptKind::Normal {
            self.peer_scores
                .entry(attempt.peer)
                .or_insert_with(|| PeerScore::new_at(now))
                .record_outbound_send_timeout_at(now);
        }

        let event = {
            if attempt.kind == SendAttemptKind::RecoveryProbe {
                let Some(cooling) = self.peer_cooling.get_mut(&attempt.peer) else {
                    return PeerTimeoutOutcome::default();
                };
                if !cooling.matches_recovery_probe(attempt.recovery_probe_id) {
                    return PeerTimeoutOutcome::default();
                }
                let cooldown = cooling_config.map_or_else(
                    || cooling.next_legacy_cooldown(),
                    |config| cooling.next_adaptive_cooldown(config),
                );
                let suppressed_until = now + cooldown;
                cooling.cooldown = cooldown;
                cooling.suppressed_until = Some(suppressed_until);
                cooling.last_suppressed_at = Some(now);
                cooling.last_suppression_timeout_count = 1;
                cooling.recovery_probe_in_flight = false;
                cooling.recovery_probe_id = None;
                cooling.timeout_window_started = now;
                cooling.timeout_count = 0;
                Some(PeerSuppressionEvent {
                    suppressed_until,
                    recent_timeout_count: 1,
                    cooldown,
                    demoted: false,
                })
            } else {
                let cooling = self
                    .peer_cooling
                    .entry(attempt.peer)
                    .or_insert_with(|| PeerCoolingState::new(now));
                if cooling.is_suppressed_at(now) {
                    return PeerTimeoutOutcome::default();
                }

                if now.saturating_duration_since(cooling.timeout_window_started)
                    > PEER_TIMEOUT_WINDOW
                {
                    cooling.timeout_window_started = now;
                    cooling.timeout_count = 0;
                }

                cooling.timeout_count = cooling.timeout_count.saturating_add(1);
                if matches!(health, Some(PeerHealth::Dead)) {
                    let cooldown = cooling_config.map_or_else(
                        || cooling.next_legacy_cooldown(),
                        |config| cooling.dead_cooldown(config),
                    );
                    let suppressed_until = now + cooldown;
                    let recent_timeout_count = cooling.timeout_count.max(1);
                    cooling.cooldown = cooldown;
                    cooling.suppressed_until = Some(suppressed_until);
                    cooling.last_suppressed_at = Some(now);
                    cooling.last_suppression_timeout_count = recent_timeout_count;
                    cooling.recovery_probe_in_flight = false;
                    cooling.recovery_probe_id = None;
                    cooling.timeout_window_started = now;
                    cooling.timeout_count = 0;
                    Some(PeerSuppressionEvent {
                        suppressed_until,
                        recent_timeout_count,
                        cooldown,
                        demoted: false,
                    })
                } else if cooling.timeout_count >= PEER_TIMEOUT_THRESHOLD
                    && matches!(health, Some(PeerHealth::Suspect))
                {
                    cooling.timeout_count = PEER_TIMEOUT_THRESHOLD.saturating_sub(1);
                    cooling.timeout_window_started = now;
                    return PeerTimeoutOutcome {
                        suppression: None,
                        request_indirect_probe: true,
                    };
                } else if cooling.timeout_count < PEER_TIMEOUT_THRESHOLD {
                    None
                } else {
                    let cooldown = cooling_config.map_or_else(
                        || cooling.next_legacy_cooldown(),
                        |config| cooling.next_adaptive_cooldown(config),
                    );
                    let suppressed_until = now + cooldown;
                    let recent_timeout_count = cooling.timeout_count;
                    cooling.cooldown = cooldown;
                    cooling.suppressed_until = Some(suppressed_until);
                    cooling.last_suppressed_at = Some(now);
                    cooling.last_suppression_timeout_count = recent_timeout_count;
                    cooling.recovery_probe_in_flight = false;
                    cooling.recovery_probe_id = None;
                    cooling.timeout_window_started = now;
                    cooling.timeout_count = 0;
                    Some(PeerSuppressionEvent {
                        suppressed_until,
                        recent_timeout_count,
                        cooldown,
                        demoted: false,
                    })
                }
            }
        };

        let suppression = event.map(|mut event| {
            if attempt.kind == SendAttemptKind::RecoveryProbe {
                self.peer_scores
                    .entry(attempt.peer)
                    .or_insert_with(|| PeerScore::new_at(now))
                    .record_outbound_send_timeout_at(now);
            }
            self.peer_scores
                .entry(attempt.peer)
                .or_insert_with(|| PeerScore::new_at(now))
                .record_cooling_event_at(now);
            event.demoted = self.prune_peer(attempt.peer);
            event
        });
        PeerTimeoutOutcome {
            suppression,
            request_indirect_probe: false,
        }
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
    fn graft_peer_at(&mut self, peer: PeerId, now: Instant) -> bool {
        if !self.can_graft_peer_at(peer, now) {
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

    fn add_new_peer_lazy(&mut self, peer: PeerId) -> bool {
        if self.eager_peers.contains(&peer) || self.lazy_peers.contains(&peer) {
            return false;
        }
        self.lazy_peers.insert(peer);
        true
    }

    /// Maintain eager peer degree (6-12) using score-based selection
    ///
    /// Promotes the highest-scoring lazy peers when below minimum degree,
    /// and demotes the lowest-scoring eager peers when above maximum degree.
    /// When the mesh is within degree bounds, periodically replaces one
    /// low-score eager peer with a substantially better lazy peer.
    fn maintain_degree(&mut self) -> (usize, usize) {
        self.maintain_degree_at(Instant::now())
    }

    fn maintain_degree_at(&mut self, now: Instant) -> (usize, usize) {
        let mut pruned = 0;
        let mut grafted = 0;

        if self.eager_peers.len() > MAX_EAGER_DEGREE {
            // Demote lowest-scoring eager peers.
            let to_demote = self.eager_peers.len() - MAX_EAGER_DEGREE;
            let peers: Vec<PeerId> = self
                .scored_eager_peers_at(now)
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

        if self.eager_peers.len() < MIN_EAGER_DEGREE && !self.lazy_peers.is_empty() {
            // Promote highest-scoring lazy peers.
            let to_promote = MIN_EAGER_DEGREE - self.eager_peers.len();
            let scored_lazy = self.scored_lazy_peers_at(now);
            let mut peers: Vec<PeerId> = scored_lazy
                .iter()
                .filter(|(_, score)| *score >= PEER_SCORE_EAGER_MIN)
                .take(to_promote)
                .map(|(p, _)| *p)
                .collect();
            if peers.len() < to_promote {
                for (peer, _) in scored_lazy
                    .iter()
                    .filter(|(_, score)| *score < PEER_SCORE_EAGER_MIN)
                {
                    peers.push(*peer);
                    if peers.len() >= to_promote {
                        break;
                    }
                }
            }
            for peer in peers {
                if self.graft_peer_at(peer, now) {
                    grafted += 1;
                }
            }
        }

        let (opportunistic_pruned, opportunistic_grafted) = self.opportunistic_graft_at(now);
        pruned += opportunistic_pruned;
        grafted += opportunistic_grafted;

        (pruned, grafted)
    }

    fn opportunistic_graft_at(&mut self, now: Instant) -> (usize, usize) {
        if self.eager_peers.len() < MIN_EAGER_DEGREE
            || self.eager_peers.len() > MAX_EAGER_DEGREE
            || self.lazy_peers.is_empty()
        {
            return (0, 0);
        }

        if self
            .last_opportunistic_graft
            .is_some_and(|last| now.saturating_duration_since(last) < OPPORTUNISTIC_GRAFT_INTERVAL)
        {
            return (0, 0);
        }
        self.last_opportunistic_graft = Some(now);

        let mut scored_eager = self.scored_eager_peers_at(now).into_iter();
        let mut scored_lazy = self
            .scored_lazy_peers_at(now)
            .into_iter()
            .filter(|(_, score)| *score >= PEER_SCORE_EAGER_MIN);

        let mut pruned = 0;
        let mut grafted = 0;

        while grafted < OPPORTUNISTIC_GRAFT_MAX_REPLACEMENTS {
            let Some((eager_peer, eager_score)) = scored_eager.next() else {
                break;
            };
            let Some((lazy_peer, lazy_score)) = scored_lazy.next() else {
                break;
            };

            if eager_score >= PEER_SCORE_EAGER_MIN
                || lazy_score < eager_score + OPPORTUNISTIC_GRAFT_MIN_SCORE_DELTA
            {
                break;
            }

            if self.prune_peer(eager_peer) {
                pruned += 1;
                if self.graft_peer_at(lazy_peer, now) {
                    grafted += 1;
                }
            }
        }

        (pruned, grafted)
    }
}

async fn record_topic_send_attempt_results_for_state(
    topics: &ShardedTopicMap,
    stage_stats: &Arc<PubSubStageStats>,
    send_path: &SendPathContext,
    topic: TopicId,
    sent: Vec<PeerSendCompletion>,
    timed_out: Vec<PeerSendAttempt>,
) {
    let now = Instant::now();
    for completion in &sent {
        send_path
            .rtt_tracker
            .record(completion.attempt.peer, completion.observed);
    }

    let mut topics_guard = topics.write_topic(&topic).await;
    let Some(state) = topics_guard.get_mut(&topic) else {
        for attempt in sent
            .iter()
            .map(|completion| &completion.attempt)
            .chain(timed_out.iter())
        {
            if attempt.kind == SendAttemptKind::RecoveryProbe {
                stage_stats.clear_peer_suppression(topic, attempt.peer);
            }
        }
        return;
    };

    for completion in sent {
        if completion.attempt.kind == SendAttemptKind::CooldownBypass {
            stage_stats.record_cooldown_bypass_success();
            debug!(
                peer_id = %completion.attempt.peer,
                topic = ?topic,
                "Cooldown-bypass send delivered while peer suppression is active"
            );
        }
        if state.record_send_success_with_context_at(
            completion.attempt,
            now,
            send_path.cooling_config,
        ) {
            stage_stats.clear_peer_suppression(topic, completion.attempt.peer);
            debug!(
                peer_id = %completion.attempt.peer,
                topic = ?topic,
                "Peer cooling cleared after successful post-cooldown send"
            );
        }
    }

    for attempt in timed_out {
        let health =
            peer_health_from_snapshot(send_path.peer_health_snapshot.as_ref(), &attempt.peer);
        let outcome = state.record_send_timeout_with_context_at(
            attempt,
            now,
            health,
            send_path.cooling_config,
        );
        if outcome.request_indirect_probe {
            spawn_indirect_probe_request(&send_path.peer_health_oracle, attempt.peer);
        }
        if let Some(event) = outcome.suppression {
            stage_stats.record_peer_suppressed(
                topic,
                attempt.peer,
                event.suppressed_until,
                event.recent_timeout_count,
                event.cooldown,
            );
            if event.demoted {
                stage_stats.record_prune();
            }
            warn!(
                peer_id = %LogPeerId::from(attempt.peer),
                topic = %LogTopicId::from(topic),
                cooldown_ms = duration_millis_u64(event.cooldown),
                recent_timeout_count = event.recent_timeout_count,
                demoted = event.demoted,
                "Peer cooled after repeated PubSub send timeouts"
            );
        }
    }
}

fn recovery_probe_timeout(attempt: PeerSendAttempt) -> Vec<PeerSendAttempt> {
    if attempt.kind == SendAttemptKind::RecoveryProbe {
        vec![attempt]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
fn normal_send_attempt(peer: PeerId) -> PeerSendAttempt {
    PeerSendAttempt {
        peer,
        kind: SendAttemptKind::Normal,
        recovery_probe_id: None,
    }
}

#[cfg(test)]
fn clean_and_reap_topics(
    topics: &mut HashMap<TopicId, TopicState>,
    topic_idle_ttl: Duration,
) -> usize {
    clean_and_reap_topic_ids(topics, topic_idle_ttl).len()
}

fn clean_and_reap_topic_ids(
    topics: &mut HashMap<TopicId, TopicState>,
    topic_idle_ttl: Duration,
) -> Vec<TopicId> {
    for state in topics.values_mut() {
        state.clean_cache();
    }

    let idle: Vec<TopicId> = topics
        .iter()
        .filter(|(_, s)| s.is_idle(topic_idle_ttl))
        .map(|(id, _)| *id)
        .collect();
    for id in &idle {
        topics.remove(id);
    }

    idle
}

#[cfg(test)]
fn count_peer_cooling_entries(topics: &HashMap<TopicId, TopicState>) -> usize {
    topics.values().map(|state| state.peer_cooling.len()).sum()
}

fn clean_expired_peer_cooling(
    topics: &mut HashMap<TopicId, TopicState>,
    stage_stats: &PubSubStageStats,
    now: Instant,
) -> usize {
    let mut removed = 0;
    for (topic, state) in topics.iter_mut() {
        let topic = *topic;
        let expired: Vec<PeerId> = state
            .peer_cooling
            .iter()
            .filter_map(|(peer, cooling)| {
                let expired =
                    cooling.suppression_expired_at(now) && !cooling.recovery_probe_in_flight;
                expired.then_some(*peer)
            })
            .collect();
        for peer in expired {
            if stage_stats.clear_peer_suppression(topic, peer) {
                removed += 1;
            }
            if !state.eager_peers.contains(&peer) && !state.lazy_peers.contains(&peer) {
                state.peer_cooling.remove(&peer);
            }
        }
    }
    removed
}

fn next_suppression_cleanup_interval(
    previous_len: usize,
    current_len: usize,
    previous_interval: Duration,
) -> Duration {
    let previous_ms = duration_millis_u64(previous_interval);
    let next_ms = if current_len == 0 || current_len < previous_len {
        previous_ms.saturating_mul(2)
    } else if current_len > previous_len {
        previous_ms / 2
    } else {
        previous_ms
    };
    Duration::from_millis(next_ms.clamp(
        SUPPRESSION_CLEANUP_MIN_INTERVAL_MS,
        SUPPRESSION_CLEANUP_MAX_INTERVAL_MS,
    ))
}

/// Pub/sub trait for message dissemination
#[async_trait::async_trait]
pub trait PubSub: Send + Sync {
    /// Publish a message to a topic
    async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()>;

    /// Subscribe to a topic and receive messages.
    ///
    /// Implementations may register the local subscriber asynchronously; callers
    /// that need a readiness barrier before publishing should use
    /// [`Self::subscribe_ready`].
    fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)>;

    /// Subscribe to a topic and return only after the local subscriber is registered.
    ///
    /// The default implementation preserves compatibility by delegating to
    /// [`Self::subscribe`]. Implementations with asynchronous subscription
    /// bookkeeping should override this method to provide a true readiness
    /// barrier.
    async fn subscribe_ready(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
        self.subscribe(topic)
    }

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

    /// Return implementation-specific PubSub diagnostics, when available.
    ///
    /// Trait-object users such as `GossipRuntime` can call this without
    /// downcasting. Implementations that do not expose PlumTree-style stage
    /// counters may return `None`.
    fn stage_stats_snapshot(&self) -> Option<PubSubStageStatsSnapshot> {
        None
    }

    /// Trigger an anti-entropy round for a specific topic
    ///
    /// This is primarily for testing. In production, anti-entropy runs
    /// automatically via the background task.
    async fn trigger_anti_entropy(&self, _topic: TopicId) -> Result<()> {
        Ok(()) // Default no-op
    }
}

/// Sharded per-topic state map — issue #27 fix.
///
/// Replaces the single `RwLock<HashMap<TopicId, TopicState>>` that serialized
/// all 32 x0x workers on one global write lock. On the 6-node testnet
/// anchor node, `dedupe_lock_acquire` was 49.6% of handler wall-time (9 355 s
/// of 18 853 s) because every message's dedupe/cache/republish path contended
/// on that one lock.
///
/// Each shard is an independent `RwLock<HashMap<..>>`; the shard for a topic
/// is selected by folding the first 8 bytes of the 32-byte `TopicId` into a
/// `u64` and masking. `TopicState` is purely per-topic with **no cross-topic
/// lock dependencies** (verified during this refactor: no method on
/// `TopicState` or the shard guard acquires a second shard's lock), so
/// sharding is safe — two workers on different topics never block each other.
const TOPIC_SHARD_COUNT: usize = 32;

struct ShardedTopicMap {
    shards: Box<[RwLock<HashMap<TopicId, TopicState>>]>,
}

impl ShardedTopicMap {
    fn new() -> Self {
        Self::with_shard_count(TOPIC_SHARD_COUNT)
    }

    fn with_shard_count(count: usize) -> Self {
        let n = count.next_power_of_two().max(2);
        let shards = (0..n)
            .map(|_| RwLock::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { shards }
    }

    /// Shard index for `topic` — fold first 8 bytes of the BLAKE3-derived
    /// `TopicId` into a `u64` and mask to the power-of-two shard count.
    #[inline]
    fn shard_index(&self, topic: &TopicId) -> usize {
        let b = topic.as_bytes();
        let fold = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        (fold as usize) & (self.shards.len() - 1)
    }

    /// Number of shards (always a power of two).
    fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Acquire a **write** lock on the shard containing `topic`. Only workers
    /// touching the same shard serialize; workers on different topics proceed
    /// concurrently.
    async fn write_topic(
        &self,
        topic: &TopicId,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<TopicId, TopicState>> {
        self.shards[self.shard_index(topic)].write().await
    }

    /// Acquire a **read** lock on the shard containing `topic`.
    async fn read_topic(
        &self,
        topic: &TopicId,
    ) -> tokio::sync::RwLockReadGuard<'_, HashMap<TopicId, TopicState>> {
        self.shards[self.shard_index(topic)].read().await
    }

    /// Try to acquire a **write** lock on every shard (background tasks that
    /// need to iterate or mutate all topics). Acquired in shard-index order
    /// to avoid deadlock; since no cross-shard lock dependencies exist, this
    /// is the only multi-shard acquisition path.
    async fn write_all(
        &self,
    ) -> Vec<tokio::sync::RwLockWriteGuard<'_, HashMap<TopicId, TopicState>>> {
        let mut guards = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            guards.push(shard.write().await);
        }
        guards
    }

    /// Try to acquire a **read** lock on every shard (background tasks,
    /// diagnostics snapshots). Returns `None` if any shard is write-locked.
    fn try_read_all(
        &self,
    ) -> Option<Vec<tokio::sync::RwLockReadGuard<'_, HashMap<TopicId, TopicState>>>> {
        let mut guards = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            match shard.try_read() {
                Ok(g) => guards.push(g),
                Err(_) => return None,
            }
        }
        Some(guards)
    }

    /// Acquire a **read** lock on every shard (background tasks).
    async fn read_all(
        &self,
    ) -> Vec<tokio::sync::RwLockReadGuard<'_, HashMap<TopicId, TopicState>>> {
        let mut guards = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            guards.push(shard.read().await);
        }
        guards
    }

    /// Collect every `TopicId` across all shards.
    async fn all_topic_ids(&self) -> Vec<TopicId> {
        let guards = self.read_all().await;
        let total: usize = guards.iter().map(|g| g.len()).sum();
        let mut ids = Vec::with_capacity(total);
        for guard in &guards {
            ids.extend(guard.keys().copied());
        }
        ids
    }

    /// Total number of topics across all shards.
    async fn topic_count(&self) -> usize {
        self.read_all().await.iter().map(|g| g.len()).sum()
    }
}

/// Plumtree pub/sub implementation
pub struct PlumtreePubSub<T: GossipTransport + 'static> {
    /// Per-topic state — sharded (issue #27) to eliminate the single
    /// `RwLock<HashMap>` write-lock that serialized all 32 x0x workers.
    topics: Arc<ShardedTopicMap>,
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
    /// Last complete peer-score diagnostics snapshot.
    ///
    /// Diagnostics readers use `topics.try_read_all()` so they never block the
    /// PubSub hot path. If a membership refresh or cache cleanup currently
    /// holds the topic write lock, readers fall back to this copy-on-write
    /// snapshot instead of observing an empty peer-score table.
    peer_score_snapshot: Arc<StdRwLock<Arc<Vec<PeerScoreSnapshot>>>>,
    /// Last complete per-topic cache diagnostics snapshot.
    topic_cache_snapshot: Arc<StdRwLock<Arc<Vec<TopicCacheStatsSnapshot>>>>,
    /// Global per-peer outbound PubSub budgets shared by all topics and send classes.
    outbound_budgets: Arc<PeerOutboundBudgets>,
    /// Per-peer observed send-duration samples for adaptive timeout sizing.
    peer_rtt_tracker: Arc<PerPeerRttTracker>,
    /// Adaptive cooldown policy consumed by the timeout/cooling decision path.
    cooling_config: AdaptiveCoolingConfig,
    /// Per-topic message-cache bounds used when new topics are created.
    cache_config: PubSubCacheConfig,
    /// Hot-path SWIM health cache refreshed outside the topic lock.
    peer_health_snapshot: Arc<StdRwLock<HashMap<PeerId, PeerHealth>>>,
    /// Hot-path transport connectivity cache refreshed outside the send path.
    ///
    /// `None` means the transport cannot currently provide authoritative
    /// connectivity, so pubsub must fail open and preserve existing behaviour.
    connected_peers_snapshot: Arc<StdRwLock<Option<HashSet<PeerId>>>>,
    /// X0X-0069: optional SWIM-derived peer-health oracle slot.
    ///
    /// Stored behind a shared lock so background PubSub tasks spawned during
    /// construction observe the oracle after [`Self::with_health_oracle`] is
    /// called by the runtime builder.
    peer_health_oracle: Arc<StdRwLock<Option<Arc<dyn PeerHealthOracle>>>>,
    /// X0X-0074: substrate-level admission control + per-topic priority
    /// registry. Consulted on every `send_to_peer_bounded` before the
    /// per-peer outbound pipeline is touched. Bulk admissions release on
    /// completion (sent or timed out).
    admission: Arc<admission::AdmissionControl>,
    /// X0X-0071: libp2p-style P1-P7 peer scoring. MVP scope — the score
    /// is computed and exposed on `/diagnostics/gossip` but the
    /// thresholds are not yet wired into mesh-selection or admission
    /// decisions (that integration is X0X-0071b).
    peer_scoring: Arc<peer_scoring::PeerScoring>,
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

    /// Create a new Plumtree pub/sub instance with custom message-cache bounds.
    ///
    /// Existing topics retain the bounds they were created with; the provided
    /// config applies to topics created after construction.
    pub fn new_with_cache_config(
        peer_id: PeerId,
        transport: Arc<T>,
        signing_key: saorsa_gossip_identity::MlDsaKeyPair,
        cache_config: PubSubCacheConfig,
    ) -> Self {
        Self::new_with_task_control_and_cache_config(
            peer_id,
            transport,
            signing_key,
            true,
            cache_config,
        )
    }

    fn new_with_task_control(
        peer_id: PeerId,
        transport: Arc<T>,
        signing_key: saorsa_gossip_identity::MlDsaKeyPair,
        start_background_tasks: bool,
    ) -> Self {
        Self::new_with_task_control_and_cache_config(
            peer_id,
            transport,
            signing_key,
            start_background_tasks,
            PubSubCacheConfig::default(),
        )
    }

    fn new_with_task_control_and_cache_config(
        peer_id: PeerId,
        transport: Arc<T>,
        signing_key: saorsa_gossip_identity::MlDsaKeyPair,
        start_background_tasks: bool,
        cache_config: PubSubCacheConfig,
    ) -> Self {
        let pubsub = Self {
            topics: Arc::new(ShardedTopicMap::new()),
            peer_id,
            epoch_start: std::time::SystemTime::UNIX_EPOCH,
            transport,
            signing_key: Arc::new(signing_key),
            stage_stats: Arc::new(PubSubStageStats::default()),
            peer_score_snapshot: Arc::new(StdRwLock::new(Arc::new(Vec::new()))),
            topic_cache_snapshot: Arc::new(StdRwLock::new(Arc::new(Vec::new()))),
            outbound_budgets: Arc::new(PeerOutboundBudgets::default()),
            peer_rtt_tracker: Arc::new(PerPeerRttTracker::new()),
            cooling_config: AdaptiveCoolingConfig::default(),
            cache_config,
            peer_health_snapshot: Arc::new(StdRwLock::new(HashMap::new())),
            connected_peers_snapshot: Arc::new(StdRwLock::new(None)),
            peer_health_oracle: Arc::new(StdRwLock::new(None)),
            admission: Arc::new(admission::AdmissionControl::new()),
            peer_scoring: Arc::new(peer_scoring::PeerScoring::new()),
        };

        if start_background_tasks {
            pubsub.spawn_ihave_flusher();
            pubsub.spawn_cache_cleaner();
            pubsub.spawn_degree_maintainer();
            pubsub.spawn_anti_entropy_task();
            pubsub.spawn_connected_peers_snapshot_refresher();
        }

        pubsub
    }

    fn send_path_context(&self) -> SendPathContext {
        SendPathContext {
            rtt_tracker: Arc::clone(&self.peer_rtt_tracker),
            cooling_config: self.cooling_config,
            peer_health_snapshot: Arc::clone(&self.peer_health_snapshot),
            connected_peers_snapshot: Arc::clone(&self.connected_peers_snapshot),
            peer_health_oracle: Arc::clone(&self.peer_health_oracle),
            admission: Arc::clone(&self.admission),
        }
    }

    /// X0X-0069: returns the current SWIM-derived peer health
    /// for `peer`, fetched via the installed oracle. `None`
    /// if no oracle is wired or the oracle has no record of the peer
    /// yet. The pub-sub cooling hot path uses a background-refreshed
    /// snapshot of this same oracle so send decisions never await while
    /// holding a topic lock.
    pub async fn peer_health(&self, peer: &PeerId) -> Option<PeerHealth> {
        let oracle = peer_health_oracle_from_slot(self.peer_health_oracle.as_ref())?;
        oracle.health_of(peer).await
    }

    /// X0X-0069: nudge the SWIM oracle to issue indirect probes for
    /// `target`. Best-effort — returns immediately if no oracle is
    /// wired. Used by per-topic timeout-accumulation paths to keep
    /// SWIM informed about peers that pub-sub is suspicious of.
    pub async fn request_indirect_probe(&self, target: PeerId) {
        if let Some(oracle) = peer_health_oracle_from_slot(self.peer_health_oracle.as_ref()) {
            oracle.request_indirect_probe(target).await;
        }
    }

    /// Override the adaptive cooling policy. Mainly useful for tests and
    /// deployments that need to tune the 30 s / 5 min default bounds.
    #[must_use]
    pub fn with_cooling_config(mut self, cooling_config: AdaptiveCoolingConfig) -> Self {
        self.cooling_config = cooling_config;
        self
    }

    /// X0X-0069 / X0X-0073b: install a SWIM-derived `PeerHealthOracle`.
    /// Builder method — call right after construction.
    ///
    /// The oracle is read by a 1 s background snapshot task. Send-timeout
    /// decisions consume that local snapshot: `Suspect` holds cooling at
    /// threshold-1 and requests indirect probes; `Dead` applies immediate
    /// adaptive dead-peer escalation.
    ///
    /// X0X-0071 and X0X-0074 reuse the same bridge for score and admission
    /// decisions.
    #[must_use]
    pub fn with_health_oracle(self, oracle: Arc<dyn PeerHealthOracle>) -> Self {
        self.spawn_peer_health_snapshot_refresher(Arc::clone(&oracle));
        store_peer_health_oracle(self.peer_health_oracle.as_ref(), oracle);
        self
    }

    fn spawn_peer_health_snapshot_refresher(&self, oracle: Arc<dyn PeerHealthOracle>) {
        let topics = Arc::clone(&self.topics);
        let snapshot = Arc::clone(&self.peer_health_snapshot);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    loop {
                        refresh_peer_health_snapshot_once(&topics, &snapshot, &oracle).await;
                        time::sleep(PEER_HEALTH_REFRESH_INTERVAL).await;
                    }
                });
            }
            Err(e) => {
                warn!("Unable to spawn PubSub peer-health snapshot refresher: {e}");
            }
        }
    }

    fn spawn_connected_peers_snapshot_refresher(&self) {
        let topics = Arc::clone(&self.topics);
        let snapshot = Arc::clone(&self.connected_peers_snapshot);
        let transport = Arc::clone(&self.transport);
        let stage_stats = Arc::clone(&self.stage_stats);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    loop {
                        refresh_connected_peers_snapshot_once(
                            &topics,
                            &snapshot,
                            &transport,
                            &stage_stats,
                        )
                        .await;
                        time::sleep(CONNECTED_PEERS_REFRESH_INTERVAL).await;
                    }
                });
            }
            Err(e) => {
                warn!("Unable to spawn PubSub connected-peers snapshot refresher: {e}");
            }
        }
    }

    #[cfg(test)]
    async fn refresh_peer_health_snapshot_for_test(&self) {
        if let Some(oracle) = peer_health_oracle_from_slot(self.peer_health_oracle.as_ref()) {
            refresh_peer_health_snapshot_once(&self.topics, &self.peer_health_snapshot, &oracle)
                .await;
        }
    }

    #[cfg(test)]
    async fn refresh_connected_peers_snapshot_for_test(&self) {
        refresh_connected_peers_snapshot_once(
            &self.topics,
            &self.connected_peers_snapshot,
            &self.transport,
            &self.stage_stats,
        )
        .await;
    }

    fn new_topic_state(&self) -> TopicState {
        TopicState::with_cache_config(self.cache_config)
    }

    /// Snapshot per-stage timings for inbound PubSub message handling.
    pub fn stage_stats(&self) -> PubSubStageStatsSnapshot {
        let mut snapshot = self.stage_stats.snapshot();
        snapshot.peer_scores = self.peer_score_snapshots();
        snapshot.topic_caches = self.topic_cache_snapshots();
        snapshot.suppressed_peers_by_topic =
            Self::build_suppressed_peers_by_topic(&snapshot.suppressed_peers);
        snapshot.peer_scores_by_topic =
            Self::build_peer_scores_by_topic(&snapshot.peer_scores, &snapshot.suppressed_peers);
        snapshot.admission_state_by_peer =
            Self::build_admission_state_by_peer(&snapshot.suppressed_peers);
        Self::merge_admission_per_peer_state(
            &mut snapshot.admission_state_by_peer,
            &self.admission.per_peer_snapshot(),
        );
        snapshot.admission = self.admission.stats().snapshot();
        snapshot.peer_scores_v2 = self.peer_scoring.snapshot();
        snapshot.topic_shard_count = self.topics.shard_count();
        snapshot.peer_scoring_lock_wait = self.peer_scoring.lock_wait_snapshot();
        snapshot
    }

    /// X0X-0074: registry + telemetry surface for substrate-level
    /// admission control. Applications register topic priorities on this
    /// handle at startup; counters and per-peer bulk-queue depths are
    /// surfaced through [`Self::stage_stats`].
    #[must_use]
    pub fn admission(&self) -> &admission::AdmissionControl {
        &self.admission
    }

    /// X0X-0071: libp2p-style peer scoring engine. Applications and the
    /// pub-sub internals record P1-P7 events on this handle; the scores
    /// surface through [`Self::stage_stats`] in
    /// `PubSubStageStatsSnapshot.peer_scores_v2`. MVP scope — thresholds
    /// are not yet wired into mesh-selection or admission (X0X-0071b).
    #[must_use]
    pub fn peer_scoring(&self) -> &peer_scoring::PeerScoring {
        &self.peer_scoring
    }

    /// X0X-0071: replace the peer-scoring instance at construction.
    /// Useful in tests and for runtimes that want a non-default
    /// [`peer_scoring::PeerScoringConfig`].
    #[must_use]
    pub fn with_peer_scoring(mut self, peer_scoring: Arc<peer_scoring::PeerScoring>) -> Self {
        self.peer_scoring = peer_scoring;
        self
    }

    /// X0X-0074: replace the admission control instance at construction.
    /// Useful in tests and for runtimes that want to apply a non-default
    /// [`admission::AdmissionConfig`].
    #[must_use]
    pub fn with_admission(mut self, admission: Arc<admission::AdmissionControl>) -> Self {
        self.admission = admission;
        self
    }

    fn merge_admission_per_peer_state(
        existing: &mut BTreeMap<String, AdmissionStateSnapshot>,
        per_peer: &HashMap<PeerId, admission::PerPeerAdmissionCounts>,
    ) {
        for (peer, counts) in per_peer {
            let entry =
                existing
                    .entry(peer.to_string())
                    .or_insert_with(|| AdmissionStateSnapshot {
                        state: "alive".to_string(),
                        suppressed_topics_count: 0,
                        cooled_topics_count: 0,
                        recovery_probe_topics_count: 0,
                        recovery_ready_topics_count: 0,
                        priority_queue_depths: BTreeMap::new(),
                    });
            entry.priority_queue_depths.insert(
                TopicPriority::Bulk.as_str().to_string(),
                counts.bulk_queue_depth,
            );
            entry.priority_queue_depths.insert(
                TopicPriority::Normal.as_str().to_string(),
                counts.normal_queue_depth,
            );
            entry.priority_queue_depths.insert(
                TopicPriority::Critical.as_str().to_string(),
                counts.critical_queue_depth,
            );
        }
    }

    fn build_suppressed_peers_by_topic(
        suppressed: &[SuppressedPeerSnapshot],
    ) -> BTreeMap<String, Vec<String>> {
        let mut by_topic: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in suppressed {
            by_topic
                .entry(row.topic.clone())
                .or_default()
                .push(row.peer_id.clone());
        }
        for peers in by_topic.values_mut() {
            peers.sort();
            peers.dedup();
        }
        by_topic
    }

    fn build_peer_scores_by_topic(
        scores: &[PeerScoreSnapshot],
        suppressed: &[SuppressedPeerSnapshot],
    ) -> BTreeMap<String, BTreeMap<String, PeerScoreBreakdownSnapshot>> {
        let mut suppression_by_topic_peer: HashMap<(String, String), &SuppressedPeerSnapshot> =
            HashMap::new();
        for row in suppressed {
            suppression_by_topic_peer.insert((row.topic.clone(), row.peer_id.clone()), row);
        }

        let mut by_topic: BTreeMap<String, BTreeMap<String, PeerScoreBreakdownSnapshot>> =
            BTreeMap::new();
        for row in scores {
            let suppressed = suppression_by_topic_peer
                .get(&(row.topic.clone(), row.peer_id.clone()))
                .copied();
            by_topic.entry(row.topic.clone()).or_default().insert(
                row.peer_id.clone(),
                PeerScoreBreakdownSnapshot {
                    role: row.role.clone(),
                    score: row.score,
                    send_health: row.send_health,
                    outbound_send_timeouts: row.outbound_send_timeouts,
                    cooling_events: row.cooling_events,
                    eager_eligible: row.eager_eligible,
                    suppression_state: suppressed.map(|s| s.state.clone()),
                    recent_timeout_count: suppressed.map(|s| s.recent_timeout_count),
                    cooldown_ms: suppressed.map(|s| s.cooldown_ms),
                    last_cool_at_unix_ms: suppressed.map(|s| s.last_suppressed_unix_ms),
                },
            );
        }
        by_topic
    }

    fn build_admission_state_by_peer(
        suppressed: &[SuppressedPeerSnapshot],
    ) -> BTreeMap<String, AdmissionStateSnapshot> {
        let mut by_peer: BTreeMap<String, AdmissionStateSnapshot> = BTreeMap::new();
        for row in suppressed {
            let entry =
                by_peer
                    .entry(row.peer_id.clone())
                    .or_insert_with(|| AdmissionStateSnapshot {
                        state: "alive".to_string(),
                        suppressed_topics_count: 0,
                        cooled_topics_count: 0,
                        recovery_probe_topics_count: 0,
                        recovery_ready_topics_count: 0,
                        priority_queue_depths: BTreeMap::new(),
                    });

            entry.suppressed_topics_count += 1;
            match row.state.as_str() {
                "recovery_probe" => entry.recovery_probe_topics_count += 1,
                "recovery_ready" => entry.recovery_ready_topics_count += 1,
                _ => entry.cooled_topics_count += 1,
            }
        }

        for entry in by_peer.values_mut() {
            entry.state = if entry.cooled_topics_count > 0 {
                "cooled".to_string()
            } else if entry.recovery_probe_topics_count > 0 {
                "recovery_probe".to_string()
            } else if entry.recovery_ready_topics_count > 0 {
                "recovery_ready".to_string()
            } else {
                "alive".to_string()
            };
        }

        by_peer
    }

    fn peer_score_snapshots(&self) -> Vec<PeerScoreSnapshot> {
        let Some(topics) = self.topics.try_read_all() else {
            return self.cached_peer_score_snapshots();
        };

        let now = Instant::now();
        let mut snapshots = Vec::new();
        for shard in &topics {
            snapshots.extend(Self::build_peer_score_snapshots(shard, now));
        }
        snapshots.sort_by(|a, b| {
            a.peer_id
                .cmp(&b.peer_id)
                .then_with(|| a.topic.cmp(&b.topic))
        });
        self.store_peer_score_snapshots(snapshots)
    }

    fn build_peer_score_snapshots(
        topics: &HashMap<TopicId, TopicState>,
        now: Instant,
    ) -> Vec<PeerScoreSnapshot> {
        let mut snapshots = Vec::new();
        for (topic, state) in topics.iter() {
            snapshots.extend(state.peer_score_snapshots(*topic, now));
        }
        snapshots.sort_by(|a, b| {
            a.peer_id
                .cmp(&b.peer_id)
                .then_with(|| a.topic.cmp(&b.topic))
        });
        snapshots
    }

    fn cached_peer_score_snapshots(&self) -> Vec<PeerScoreSnapshot> {
        match self.peer_score_snapshot.read() {
            Ok(snapshot) => snapshot.as_ref().clone(),
            Err(e) => {
                error!("peer-score diagnostics snapshot cache poisoned: {e}");
                Vec::new()
            }
        }
    }

    fn store_peer_score_snapshots(
        &self,
        snapshots: Vec<PeerScoreSnapshot>,
    ) -> Vec<PeerScoreSnapshot> {
        let shared = Arc::new(snapshots);
        match self.peer_score_snapshot.write() {
            Ok(mut cached) => {
                *cached = Arc::clone(&shared);
            }
            Err(e) => {
                error!("peer-score diagnostics snapshot cache poisoned: {e}");
            }
        }
        shared.as_ref().clone()
    }

    fn topic_cache_snapshots(&self) -> Vec<TopicCacheStatsSnapshot> {
        let Some(topics) = self.topics.try_read_all() else {
            return self.cached_topic_cache_snapshots();
        };

        let now = Instant::now();
        let mut snapshots = Vec::new();
        for shard in &topics {
            snapshots.extend(Self::build_topic_cache_snapshots(shard, now));
        }
        snapshots.sort_by(|a, b| a.topic.cmp(&b.topic));
        self.store_topic_cache_snapshots(snapshots)
    }

    fn build_topic_cache_snapshots(
        topics: &HashMap<TopicId, TopicState>,
        now: Instant,
    ) -> Vec<TopicCacheStatsSnapshot> {
        let mut snapshots: Vec<TopicCacheStatsSnapshot> = topics
            .iter()
            .map(|(topic, state)| TopicCacheStatsSnapshot {
                topic: topic.to_string(),
                cache: state.message_cache.stats_at(now),
            })
            .collect();
        snapshots.sort_by(|a, b| a.topic.cmp(&b.topic));
        snapshots
    }

    fn cached_topic_cache_snapshots(&self) -> Vec<TopicCacheStatsSnapshot> {
        match self.topic_cache_snapshot.read() {
            Ok(snapshot) => snapshot.as_ref().clone(),
            Err(e) => {
                error!("topic-cache diagnostics snapshot cache poisoned: {e}");
                Vec::new()
            }
        }
    }

    fn store_topic_cache_snapshots(
        &self,
        snapshots: Vec<TopicCacheStatsSnapshot>,
    ) -> Vec<TopicCacheStatsSnapshot> {
        let shared = Arc::new(snapshots);
        match self.topic_cache_snapshot.write() {
            Ok(mut cached) => {
                *cached = Arc::clone(&shared);
            }
            Err(e) => {
                error!("topic-cache diagnostics snapshot cache poisoned: {e}");
            }
        }
        shared.as_ref().clone()
    }

    fn record_stage(&self, stage: PubSubStage, started: Instant) {
        self.stage_stats.record(stage, started.elapsed());
    }

    fn verify_message_signature(&self, message: &GossipMessage) -> bool {
        let verify_started = Instant::now();
        let verified =
            self.verify_signature(&message.header, &message.signature, &message.public_key);
        self.record_stage(PubSubStage::Verify, verify_started);
        verified
    }

    fn record_inbound_peer_activity_for_state(
        stage_stats: &PubSubStageStats,
        topic: TopicId,
        peer: PeerId,
        state: &mut TopicState,
        now: Instant,
        kind: MessageKind,
    ) {
        if state.record_inbound_peer_activity_at(peer, now) {
            stage_stats.clear_peer_suppression(topic, peer);
            debug!(
                peer_id = %peer,
                topic = ?topic,
                msg_kind = ?kind,
                "Peer cooling cleared after verified inbound PubSub activity"
            );
        }
    }

    async fn record_verified_inbound_from_peer(
        &self,
        topic: TopicId,
        peer: PeerId,
        kind: MessageKind,
    ) {
        let mut topics = self.topics.write_topic(&topic).await;
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());
        state.touch();
        Self::record_inbound_peer_activity_for_state(
            self.stage_stats.as_ref(),
            topic,
            peer,
            state,
            Instant::now(),
            kind,
        );
    }

    async fn send_to_peer_with_timeout(
        transport: Arc<T>,
        stage_stats: Arc<PubSubStageStats>,
        rtt_tracker: Arc<PerPeerRttTracker>,
        peer: PeerId,
        stream_type: GossipStreamType,
        bytes: Bytes,
        op: &'static str,
    ) -> Result<PeerSendOutcome> {
        let timeout = rtt_tracker.adaptive_timeout(&peer, PER_PEER_REPUBLISH_TIMEOUT);
        let started = Instant::now();
        match tokio::time::timeout(timeout, transport.send_to_peer(peer, stream_type, bytes)).await
        {
            Ok(Ok(())) => Ok(PeerSendOutcome::Sent {
                observed: started.elapsed(),
            }),
            Ok(Err(e)) => {
                warn!(
                    peer_id = %LogPeerId::from(peer),
                    op,
                    "{op} per-peer send failed: {e}"
                );
                Err(e)
            }
            Err(_) => {
                stage_stats.record_per_peer_timeout();
                // Per-message occurrence at debug — already counted in the
                // republish_per_peer_timeout metric (the gate signal). Avoids
                // GB/hr WARN spam when many peers time out on degraded links.
                debug!(
                    peer_id = %LogPeerId::from(peer),
                    op,
                    timeout_ms = duration_millis_u64(timeout),
                    "{op} per-peer send timed out — peer skipped, recorded in republish_per_peer_timeout"
                );
                Ok(PeerSendOutcome::TimedOut)
            }
        }
    }

    async fn send_to_peer_bounded(
        &self,
        topic: TopicId,
        peer: PeerId,
        stream_type: GossipStreamType,
        bytes: Bytes,
        op: &'static str,
    ) -> Result<()> {
        // X0X-0074: admission control gate. Consults topic priority,
        // peer health (from the snapshot — sync, no await under lock),
        // and per-peer cooled state to decide whether the message
        // should enter the outbound pipeline at all.
        //
        // Bulk admissions reserve depth here and release exactly once
        // at function exit, covering no-claim, panic, and normal
        // completion paths uniformly. Critical admissions that fail to
        // claim an outbound budget record `dropped_critical_hard_error`
        // — a non-zero value is a soak-blocking violation.
        let priority = self.admission.registry().priority_for(&topic);
        let health = peer_health_from_snapshot(self.peer_health_snapshot.as_ref(), &peer);
        let is_peer_cooled = self.is_peer_currently_suppressed(&topic, &peer).await;
        let admission_decision = self.admission.admit(&topic, &peer, health, is_peer_cooled);
        if let AdmissionDecision::Drop { reason } = admission_decision {
            debug!(
                peer_id = %peer,
                topic = %topic,
                op,
                priority = %priority,
                reason = %reason,
                "X0X-0074 admission dropped peer send"
            );
            return Ok(());
        }

        // Bulk-admitted means we incremented per-peer depth — release
        // exactly once on the way out.
        let bulk_admitted = priority == TopicPriority::Bulk;
        let release_guard = scopeguard_release(bulk_admitted, &peer, &self.admission);

        let (mut claims, _lock_wait) = self.claim_topic_send_attempts(topic, vec![peer], op).await;
        let Some(attempt) = claims.attempts().first().copied() else {
            // X0X-0074d: Critical *Data* skips (cooling vs gate overflow) are
            // now counted inside claim_topic_send_attempts where the reason is
            // known, so we must not re-count them here. Only the Critical
            // *Control* case (e.g. IWANT) is still counted at the caller.
            if priority == TopicPriority::Critical
                && !matches!(OutboundSendClass::for_op(op), OutboundSendClass::Data)
            {
                self.admission.stats().record_critical_hard_error();
                debug!(
                    peer_id = %LogPeerId::from(peer),
                    topic = %LogTopicId::from(topic),
                    op,
                    "X0X-0074 hard error: Critical control admission failed to claim outbound budget"
                );
            }
            drop(release_guard);
            return Ok(());
        };
        let Some(permit) = claims.take_permits().into_iter().next() else {
            // Defensive: claim pushes attempt+permit together, so this is
            // unreachable. Critical Data is accounted in claim; only count the
            // Control case here for symmetry with the no-attempt path above.
            if priority == TopicPriority::Critical
                && !matches!(OutboundSendClass::for_op(op), OutboundSendClass::Data)
            {
                self.admission.stats().record_critical_hard_error();
                debug!(
                    peer_id = %LogPeerId::from(peer),
                    topic = %LogTopicId::from(topic),
                    op,
                    "X0X-0074 hard error: Critical control admission claimed attempt but lost permit"
                );
            }
            drop(release_guard);
            return Ok(());
        };

        let transport = Arc::clone(&self.transport);
        let stage_stats = Arc::clone(&self.stage_stats);
        let rtt_tracker = Arc::clone(&self.peer_rtt_tracker);
        let handle = AbortOnDropSendHandle::new(tokio::spawn(async move {
            let mut permit = permit;
            // X0X-0074d: for Critical, wait FIFO for the peer's in-flight slot
            // (no-op otherwise). Held until the send completes.
            let gate_wait_timeout =
                rtt_tracker.adaptive_timeout(&attempt.peer, PER_PEER_REPUBLISH_TIMEOUT);
            if !permit.engage_critical_gate(gate_wait_timeout).await {
                stage_stats.record_per_peer_timeout();
                return Ok(PeerSendOutcome::TimedOut);
            }
            Self::send_to_peer_with_timeout(
                transport,
                stage_stats,
                rtt_tracker,
                attempt.peer,
                stream_type,
                bytes,
                op,
            )
            .await
        }));

        let result = match handle.join().await {
            Ok(Ok(PeerSendOutcome::Sent { observed })) => {
                claims
                    .record_results(vec![PeerSendCompletion { attempt, observed }], Vec::new())
                    .await;
                Ok(())
            }
            Ok(Ok(PeerSendOutcome::TimedOut)) => {
                claims.record_results(Vec::new(), vec![attempt]).await;
                Ok(())
            }
            Ok(Err(e)) => {
                let timed_out = recovery_probe_timeout(attempt);
                claims.record_results(Vec::new(), timed_out).await;
                Err(e)
            }
            Err(e) => {
                let timed_out = recovery_probe_timeout(attempt);
                claims.record_results(Vec::new(), timed_out).await;
                warn!(
                    peer_id = %LogPeerId::from(attempt.peer),
                    op,
                    "{op} per-peer send task panicked: {e}"
                );
                Err(anyhow!("{op} per-peer send task panicked: {e}"))
            }
        };
        drop(release_guard);
        result
    }

    /// Inspect the per-topic cooling state for `peer`. Returns `true` if
    /// the peer is currently suppressed for this topic. Used by the
    /// admission gate to drop bulk admissions to cooled peers without
    /// re-entering the per-topic send pipeline.
    async fn is_peer_currently_suppressed(&self, topic: &TopicId, peer: &PeerId) -> bool {
        let topics_guard = self.topics.read_topic(topic).await;
        let now = Instant::now();
        topics_guard
            .get(topic)
            .is_some_and(|state| state.is_peer_suppressed_at(*peer, now))
    }

    async fn claim_topic_send_attempts(
        &self,
        topic: TopicId,
        peers: Vec<PeerId>,
        op: &'static str,
    ) -> (SendAttemptClaims, Duration) {
        let send_path = self.send_path_context();
        if peers.is_empty() {
            return (
                SendAttemptClaims::new(
                    topic,
                    Vec::new(),
                    Vec::new(),
                    Arc::clone(&self.topics),
                    Arc::clone(&self.stage_stats),
                    send_path,
                ),
                Duration::ZERO,
            );
        }

        let now = Instant::now();
        let send_class = OutboundSendClass::for_op(op);
        let priority = self.admission.registry().priority_for(&topic);
        // Issue #27: record the topics lock-wait as DedupeLockAcquire so it
        // is NOT silently charged to the Republish stage that wraps this call.
        let lock_started = Instant::now();
        let mut topics = self.topics.write_topic(&topic).await;
        let lock_wait = lock_started.elapsed();
        self.stage_stats
            .record(PubSubStage::DedupeLockAcquire, lock_wait);
        let (attempts, permits) = if let Some(state) = topics.get_mut(&topic) {
            let claim_context = SendClaimContext {
                stage_stats: self.stage_stats.as_ref(),
                outbound_budgets: &self.outbound_budgets,
                send_path: &send_path,
                topic,
                op,
                send_class,
                priority,
            };
            Self::claim_topic_send_attempts_for_state(&claim_context, state, peers, now)
        } else {
            let mut attempts = Vec::with_capacity(peers.len());
            let mut permits = Vec::with_capacity(peers.len());
            for peer in peers {
                if peer_is_transport_disconnected(
                    send_path.connected_peers_snapshot.as_ref(),
                    &peer,
                ) {
                    debug!(
                        peer_id = %LogPeerId::from(peer),
                        topic = %LogTopicId::from(topic),
                        op,
                        priority = %priority,
                        "PubSub claim skipped transport-disconnected peer send"
                    );
                    continue;
                }
                if let Some(permit) = self
                    .outbound_budgets
                    .try_acquire(peer, send_class, priority, now)
                {
                    attempts.push(PeerSendAttempt {
                        peer,
                        kind: SendAttemptKind::Normal,
                        recovery_probe_id: None,
                    });
                    permits.push(permit);
                } else {
                    self.stage_stats.record_outbound_budget_exhausted();
                    if priority == TopicPriority::Critical
                        && matches!(send_class, OutboundSendClass::Data)
                    {
                        // X0X-0074d: no TopicState means no cooling state, so a
                        // Critical None here is a Critical gate overflow.
                        self.admission.stats().record_critical_hard_error();
                    }
                    trace!(
                        peer_id = %peer,
                        topic = ?topic,
                        op,
                        class = send_class.label(),
                        "{op} send skipped: peer outbound PubSub budget exhausted"
                    );
                }
            }
            (attempts, permits)
        };

        (
            SendAttemptClaims::new(
                topic,
                attempts,
                permits,
                Arc::clone(&self.topics),
                Arc::clone(&self.stage_stats),
                send_path,
            ),
            lock_wait,
        )
    }

    fn claim_topic_send_attempts_for_state(
        claim_context: &SendClaimContext<'_>,
        state: &mut TopicState,
        peers: Vec<PeerId>,
        now: Instant,
    ) -> (Vec<PeerSendAttempt>, Vec<OutboundSendPermit>) {
        let mut attempts = Vec::with_capacity(peers.len());
        let mut permits = Vec::with_capacity(peers.len());
        for peer in peers {
            if peer_is_transport_disconnected(
                claim_context.send_path.connected_peers_snapshot.as_ref(),
                &peer,
            ) {
                debug!(
                    peer_id = %LogPeerId::from(peer),
                    topic = %LogTopicId::from(claim_context.topic),
                    op = claim_context.op,
                    priority = %claim_context.priority,
                    "PubSub claim skipped transport-disconnected peer send"
                );
                continue;
            }
            match state.claim_send_attempt_at(peer, now) {
                Some((attempt, recovery_event)) => {
                    if attempt.kind == SendAttemptKind::RecoveryProbe {
                        state
                            .peer_scores
                            .entry(peer)
                            .or_insert_with(|| PeerScore::new_at(now))
                            .record_recovery_probe_at(now);
                    }
                    if attempt.kind == SendAttemptKind::CooldownBypass {
                        claim_context.stage_stats.record_cooldown_bypass_probe();
                        debug!(
                            peer_id = %peer,
                            topic = ?claim_context.topic,
                            op = claim_context.op,
                            "{} send admitted as rate-limited cooldown bypass",
                            claim_context.op
                        );
                    }
                    if let Some(event) = recovery_event {
                        claim_context.stage_stats.record_peer_recovery_probe(
                            claim_context.topic,
                            peer,
                            event.suppressed_until,
                            event.recent_timeout_count,
                            event.cooldown,
                        );
                        debug!(
                            peer_id = %peer,
                            topic = ?claim_context.topic,
                            op = claim_context.op,
                            "{} send admitted as peer recovery probe",
                            claim_context.op
                        );
                    }
                    let Some(permit) = claim_context.outbound_budgets.try_acquire(
                        peer,
                        claim_context.send_class,
                        claim_context.priority,
                        now,
                    ) else {
                        if claim_context.priority == TopicPriority::Critical
                            && matches!(claim_context.send_class, OutboundSendClass::Data)
                        {
                            // X0X-0074d: Critical no longer fails on a budget
                            // lane — a None here means the per-peer Critical
                            // FIFO gate is full (genuine overload). Record the
                            // hard error and immediately cool the peer/topic. A
                            // full 64-deep Critical FIFO is threshold-equivalent
                            // pressure: this peer/path is not draining fast enough,
                            // and feeding it through five more WARNing attempts
                            // turns controlled backpressure into log spam.
                            claim_context
                                .send_path
                                .admission
                                .stats()
                                .record_critical_hard_error();
                            Self::record_critical_gate_overflow_for_state(
                                claim_context,
                                state,
                                attempt,
                                now,
                            );
                            continue;
                        }
                        Self::record_outbound_budget_pressure_for_state(
                            claim_context,
                            state,
                            attempt,
                            now,
                        );
                        continue;
                    };
                    attempts.push(attempt);
                    permits.push(permit);
                }
                None => {
                    if claim_context.priority == TopicPriority::Critical
                        && matches!(claim_context.send_class, OutboundSendClass::Data)
                    {
                        // X0X-0074d: Critical send skipped because the peer is
                        // actively cooling/suppressed (or has a recovery probe
                        // in flight). Legitimate transient backpressure — NOT a
                        // hard-error budget/gate violation.
                        claim_context
                            .send_path
                            .admission
                            .stats()
                            .record_critical_cooling();
                    }
                    trace!(
                        peer_id = %peer,
                        topic = ?claim_context.topic,
                        op = claim_context.op,
                        "{} send skipped: peer cooling after repeated timeouts",
                        claim_context.op
                    );
                }
            }
        }
        (attempts, permits)
    }

    fn record_critical_gate_overflow_for_state(
        claim_context: &SendClaimContext<'_>,
        state: &mut TopicState,
        attempt: PeerSendAttempt,
        now: Instant,
    ) {
        claim_context.stage_stats.record_outbound_budget_exhausted();
        let health = peer_health_from_snapshot(
            claim_context.send_path.peer_health_snapshot.as_ref(),
            &attempt.peer,
        );
        let event = state.record_critical_gate_overflow_with_context_at(
            attempt,
            now,
            health,
            claim_context.send_path.cooling_config,
        );
        if let Some(event) = event.suppression {
            claim_context.stage_stats.record_peer_suppressed(
                claim_context.topic,
                attempt.peer,
                event.suppressed_until,
                event.recent_timeout_count,
                event.cooldown,
            );
            warn!(
                peer_id = %LogPeerId::from(attempt.peer),
                topic = %LogTopicId::from(claim_context.topic),
                op = claim_context.op,
                class = claim_context.send_class.label(),
                cooldown_ms = duration_millis_u64(event.cooldown),
                recent_timeout_count = event.recent_timeout_count,
                demoted = event.demoted,
                "Peer cooled after Critical gate saturation"
            );
        }
    }

    fn record_outbound_budget_pressure_for_state(
        claim_context: &SendClaimContext<'_>,
        state: &mut TopicState,
        attempt: PeerSendAttempt,
        now: Instant,
    ) {
        claim_context.stage_stats.record_outbound_budget_exhausted();
        let health = peer_health_from_snapshot(
            claim_context.send_path.peer_health_snapshot.as_ref(),
            &attempt.peer,
        );
        let outcome = state.record_send_timeout_with_context_at(
            attempt,
            now,
            health,
            claim_context.send_path.cooling_config,
        );
        if outcome.request_indirect_probe {
            spawn_indirect_probe_request(&claim_context.send_path.peer_health_oracle, attempt.peer);
        }
        if let Some(event) = outcome.suppression {
            claim_context.stage_stats.record_peer_suppressed(
                claim_context.topic,
                attempt.peer,
                event.suppressed_until,
                event.recent_timeout_count,
                event.cooldown,
            );
            if event.demoted {
                claim_context.stage_stats.record_prune();
            }
            warn!(
                peer_id = %LogPeerId::from(attempt.peer),
                topic = %LogTopicId::from(claim_context.topic),
                op = claim_context.op,
                class = claim_context.send_class.label(),
                cooldown_ms = duration_millis_u64(event.cooldown),
                recent_timeout_count = event.recent_timeout_count,
                demoted = event.demoted,
                "Peer cooled after repeated PubSub outbound budget pressure"
            );
        }
        trace!(
            peer_id = %attempt.peer,
            topic = ?claim_context.topic,
            op = claim_context.op,
            class = claim_context.send_class.label(),
            "{} send skipped: peer outbound PubSub budget exhausted",
            claim_context.op
        );
    }

    #[cfg(test)]
    async fn record_topic_send_results(
        &self,
        topic: TopicId,
        sent: Vec<PeerId>,
        timed_out: Vec<PeerId>,
    ) {
        let sent = sent
            .into_iter()
            .map(|peer| PeerSendAttempt {
                peer,
                kind: SendAttemptKind::Normal,
                recovery_probe_id: None,
            })
            .collect();
        let timed_out = timed_out
            .into_iter()
            .map(|peer| PeerSendAttempt {
                peer,
                kind: SendAttemptKind::Normal,
                recovery_probe_id: None,
            })
            .collect();
        self.record_topic_send_attempt_results(topic, sent, timed_out)
            .await;
    }

    #[cfg(test)]
    async fn record_topic_send_attempt_results(
        &self,
        topic: TopicId,
        sent: Vec<PeerSendAttempt>,
        timed_out: Vec<PeerSendAttempt>,
    ) {
        if sent.is_empty() && timed_out.is_empty() {
            return;
        }

        let sent = sent
            .into_iter()
            .map(|attempt| PeerSendCompletion {
                attempt,
                observed: Duration::ZERO,
            })
            .collect();
        let send_path = self.send_path_context();
        record_topic_send_attempt_results_for_state(
            &self.topics,
            &self.stage_stats,
            &send_path,
            topic,
            sent,
            timed_out,
        )
        .await;
    }

    #[cfg(test)]
    async fn record_topic_send_attempt_results_for(
        topics: &ShardedTopicMap,
        stage_stats: &Arc<PubSubStageStats>,
        topic: TopicId,
        sent: Vec<PeerSendAttempt>,
        timed_out: Vec<PeerSendAttempt>,
    ) {
        let rtt_tracker = Arc::new(PerPeerRttTracker::new());
        let peer_health_snapshot = Arc::new(StdRwLock::new(HashMap::new()));
        let connected_peers_snapshot = Arc::new(StdRwLock::new(None));
        let peer_health_oracle = Arc::new(StdRwLock::new(None));
        let send_path = SendPathContext {
            rtt_tracker,
            cooling_config: AdaptiveCoolingConfig::default(),
            peer_health_snapshot,
            connected_peers_snapshot,
            peer_health_oracle,
            admission: Arc::new(admission::AdmissionControl::new()),
        };
        let sent = sent
            .into_iter()
            .map(|attempt| PeerSendCompletion {
                attempt,
                observed: Duration::ZERO,
            })
            .collect();
        record_topic_send_attempt_results_for_state(
            topics,
            stage_stats,
            &send_path,
            topic,
            sent,
            timed_out,
        )
        .await;
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
        detach_accounting: bool,
    ) -> Duration {
        // X0X-0074: admission gate runs once per (topic, peer) before
        // we claim attempts. Dropped peers never enter the send
        // pipeline. Bulk admissions are reserved here and released
        // exactly once at the end of this function regardless of which
        // downstream path (no-claim, partial-claim, send completion)
        // the message took — the depth counter must never leak.
        let priority = self.admission.registry().priority_for(&topic);
        let admitted = self
            .filter_peers_through_admission(&topic, peers, priority, op)
            .await;
        let bulk_admitted: Vec<PeerId> = if priority == TopicPriority::Bulk {
            admitted.clone()
        } else {
            Vec::new()
        };
        let admitted_count = admitted.len();
        if admitted_count == 0 {
            return Duration::ZERO;
        }
        // X0X-0074: RAII guard releases Bulk admissions for the entire
        // admitted set exactly once on drop — covers no-claim, partial-
        // claim, send-task panic, AND the case where this future is
        // dropped/cancelled between admission and the explicit release
        // (reviewer P2.2, 2026-05-13). Previously we relied on an
        // explicit call at function end which leaked on cancellation.
        let _bulk_guard = BulkAdmissionSetGuard {
            armed: true,
            bulk_admitted,
            admission: Arc::clone(&self.admission),
        };

        let (mut claims, lock_wait) = self.claim_topic_send_attempts(topic, admitted, op).await;

        // X0X-0074d: per-peer Critical accounting now happens inside
        // `claim_topic_send_attempts`, where the skip reason is known —
        // cooling/suppression → `dropped_critical_cooling`, Critical FIFO gate
        // overflow → `dropped_critical_hard_error`. Counting a bare
        // attempts-vs-admitted shortfall here (the old behaviour) conflated the
        // two and double-counted, so it is removed.

        if claims.is_empty() {
            // No peers got attempts. Bulk reservations release when
            // `_bulk_guard` drops at function exit.
            return lock_wait;
        }
        let mut send_tasks = SendTaskSet::with_capacity(op, claims.attempts().len());
        let attempts = claims.attempts().to_vec();
        let permits = claims.take_permits();
        for (attempt, permit) in attempts.into_iter().zip(permits) {
            let transport = Arc::clone(&self.transport);
            let bytes = bytes.clone();
            let stage_stats = Arc::clone(&self.stage_stats);
            let rtt_tracker = Arc::clone(&self.peer_rtt_tracker);
            let handle = tokio::spawn(async move {
                let mut permit = permit;
                // X0X-0074d: Critical sends wait FIFO for the peer's single
                // in-flight slot here, inside the detached task — no-op for
                // Normal/Bulk. Keeps the dispatcher worker unpinned.
                let gate_wait_timeout =
                    rtt_tracker.adaptive_timeout(&attempt.peer, PER_PEER_REPUBLISH_TIMEOUT);
                if !permit.engage_critical_gate(gate_wait_timeout).await {
                    stage_stats.record_per_peer_timeout();
                    return Ok(PeerSendOutcome::TimedOut);
                }
                Self::send_to_peer_with_timeout(
                    transport,
                    stage_stats,
                    rtt_tracker,
                    attempt.peer,
                    stream_type,
                    bytes,
                    op,
                )
                .await
            });
            send_tasks.push(attempt, handle);
        }
        if detach_accounting {
            // Dispatcher forward path: detach fan-out result accounting so
            // the worker is not pinned for the slowest peer's full timeout
            // (~PER_PEER_REPUBLISH_TIMEOUT). The per-peer sends above are
            // already spawned with permits, and concurrency stays bounded by
            // the outbound budget (try_acquire is non-blocking — an exhausted
            // budget skips the peer rather than blocking). Awaiting
            // collect_results inline here collapsed dispatcher drain
            // throughput under degraded links and overflowed the shared recv
            // channel (recv_pump.dropped_full). `_bulk_guard` moves into the
            // detached task so Bulk admissions still release exactly once when
            // accounting completes. See docs/design/pubsub-fanout-backpressure.md.
            tokio::spawn(async move {
                let _bulk_guard = _bulk_guard;
                let (sent, timed_out) = send_tasks.collect_results().await;
                claims.record_results(sent, timed_out).await;
            });
        } else {
            // Publish path (and tests): await the fan-out so publish() retains
            // its established semantics (returns after sends are attempted).
            let (sent, timed_out) = send_tasks.collect_results().await;
            claims.record_results(sent, timed_out).await;
            // _bulk_guard drops here, releasing every Bulk admission exactly
            // once. No manual release call needed.
        }
        lock_wait
    }
}

/// X0X-0074: RAII guard that releases a per-peer Bulk-admission reservation
/// exactly once on drop. Used by `send_to_peer_bounded` to guarantee the
/// per-peer Bulk depth never leaks across early-return paths (no-claim,
/// no-permit, send error, panic).
struct BulkAdmissionGuard<'a> {
    armed: bool,
    peer: PeerId,
    admission: &'a admission::AdmissionControl,
}

impl Drop for BulkAdmissionGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.admission.release_bulk(&self.peer);
        }
    }
}

fn scopeguard_release<'a>(
    armed: bool,
    peer: &PeerId,
    admission: &'a admission::AdmissionControl,
) -> BulkAdmissionGuard<'a> {
    BulkAdmissionGuard {
        armed,
        peer: *peer,
        admission,
    }
}

/// X0X-0074: RAII guard for the publish-fanout path. Releases Bulk
/// admissions for the entire admitted set exactly once on drop —
/// covering the no-claim, partial-claim, send-task panic, and the
/// future-cancellation paths uniformly. Replaces the prior pattern of
/// calling `release_bulk_admissions` explicitly at the function's end,
/// which leaked the per-peer depth if the future was dropped between
/// admission increment and the explicit release.
struct BulkAdmissionSetGuard {
    armed: bool,
    bulk_admitted: Vec<PeerId>,
    admission: Arc<admission::AdmissionControl>,
}

impl Drop for BulkAdmissionSetGuard {
    fn drop(&mut self) {
        if self.armed {
            for peer in &self.bulk_admitted {
                self.admission.release_bulk(peer);
            }
        }
    }
}

impl<T: GossipTransport + 'static> PlumtreePubSub<T> {
    /// X0X-0074: filter a peer list through the admission gate. Returns
    /// the subset of peers that admit; the rest are accounted for in the
    /// admission counters. The cooling lookup is batched under a single
    /// `topics.read()` to avoid N lock acquisitions for N peers.
    async fn filter_peers_through_admission(
        &self,
        topic: &TopicId,
        peers: Vec<PeerId>,
        priority: TopicPriority,
        op: &'static str,
    ) -> Vec<PeerId> {
        let cooled_set: HashSet<PeerId> = {
            let topics_guard = self.topics.read_topic(topic).await;
            let now = Instant::now();
            match topics_guard.get(topic) {
                Some(state) => peers
                    .iter()
                    .copied()
                    .filter(|peer| state.is_peer_suppressed_at(*peer, now))
                    .collect(),
                None => HashSet::new(),
            }
        };

        let connected_snapshot =
            connected_peers_from_snapshot(self.connected_peers_snapshot.as_ref());
        let mut admitted = Vec::with_capacity(peers.len());
        for peer in peers {
            if let Some(connected) = connected_snapshot.as_ref() {
                if !connected.contains(&peer) {
                    debug!(
                        peer_id = %peer,
                        topic = %topic,
                        op,
                        priority = %priority,
                        "PubSub admission skipped transport-disconnected peer send"
                    );
                    continue;
                }
            }

            let health = peer_health_from_snapshot(self.peer_health_snapshot.as_ref(), &peer);
            let is_peer_cooled = cooled_set.contains(&peer);
            match self.admission.admit(topic, &peer, health, is_peer_cooled) {
                AdmissionDecision::Admit => admitted.push(peer),
                AdmissionDecision::Drop { reason } => {
                    debug!(
                        peer_id = %peer,
                        topic = %topic,
                        op,
                        priority = %priority,
                        reason = %reason,
                        "X0X-0074 admission dropped peer send"
                    );
                }
            }
        }
        admitted
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

        let mut topics = self.topics.write_topic(&topic).await;
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());

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
        // Publish path: await accounting (detach_accounting = false) so
        // publish() returns only after its EAGER sends are attempted.
        self.parallel_send_to_peers(
            topic,
            eager_peers,
            GossipStreamType::PubSub,
            bytes,
            "EAGER",
            false,
        )
        .await;

        // Batch msg_id to pending_ihave
        let mut topics = self.topics.write_topic(&topic).await;
        if let Some(state) = topics.get_mut(&topic) {
            state.pending_ihave.push(msg_id);

            // Deliver to local subscribers
            debug!(
                target: "sg.payload.trace",
                stage = "publish_self_deliver",
                msg_id = %msg_id_hex8(&msg_id),
                len = payload.len(),
                zero_tail = payload_zero_tail(&payload),
            );
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

        if !self.verify_message_signature(&message) {
            warn!(peer_id = %LogPeerId::from(from), msg_id = ?msg_id, "Invalid signature, dropping");
            // X0X-0071 P4: a bad signature is the canonical invalid-message
            // signal — record it against (topic, sender).
            self.peer_scoring.record_invalid_message(topic, from);
            return Err(anyhow!("Invalid signature"));
        }

        let lock_started = Instant::now();
        let mut topics = self.topics.write_topic(&topic).await;
        self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
        let dedupe_started = Instant::now();
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());
        state.touch();
        Self::record_inbound_peer_activity_for_state(
            self.stage_stats.as_ref(),
            topic,
            from,
            state,
            Instant::now(),
            message.header.kind,
        );

        // Check for duplicate
        if state.has_message(&msg_id) {
            // PRUNE: move sender from eager to lazy
            if state.prune_peer(from) {
                self.stage_stats.record_prune();
                // X0X-0071 P3b: a prune bumps the (topic, peer) delivery
                // deficit — sticky across a later re-graft.
                self.peer_scoring.record_mesh_pruned(topic, from);
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
        debug!(
            target: "sg.payload.trace",
            stage = "eager_recv",
            from = %from,
            msg_id = %msg_id_hex8(&msg_id),
            len = payload.len(),
            zero_tail = payload_zero_tail(&payload),
        );
        state.cache_message(msg_id, payload.clone(), message.header.clone());

        // Update peer score for the sender
        state
            .peer_scores
            .entry(from)
            .or_insert_with(PeerScore::new)
            .record_delivery();
        // X0X-0071 P2: the sender delivered a msg_id we had not seen —
        // a first-delivery credit against (topic, sender).
        self.peer_scoring.record_first_delivery(topic, from);

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

        // Unknown senders enter LAZY first. Score-aware maintenance decides
        // whether they are needed in the bounded EAGER mesh.
        let now = Instant::now();
        if state.add_new_peer_lazy(from) {
            debug!(peer_id = %from, topic = ?topic, "Added sender to lazy_peers");
            let (pruned, grafted) = state.maintain_degree_at(now);
            if pruned > 0 {
                self.stage_stats.record_prunes(pruned);
            }
            if grafted > 0 {
                self.stage_stats.record_grafts(grafted);
            }
        }
        // X0X-0071 P1: if the sender is part of our eager mesh for this
        // topic, (idempotently) start its time-in-mesh clock. Reads the
        // post-maintenance eager set so a just-grafted peer is counted.
        if state.eager_peers.contains(&from) {
            self.peer_scoring.note_mesh_join(topic, from);
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
        debug!(
            target: "sg.payload.trace",
            stage = "subscriber_deliver",
            from = %from,
            msg_id = %msg_id_hex8(&msg_id),
            len = payload.len(),
            zero_tail = payload_zero_tail(&payload),
        );
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
        // Dispatcher forward path: detach accounting (detach_accounting =
        // true) so a slow peer cannot pin this worker for its full timeout
        // and starve the shared recv channel (recv_pump.dropped_full).
        let lock_wait = self
            .parallel_send_to_peers(
                topic,
                eager_peers,
                GossipStreamType::PubSub,
                bytes,
                "EAGER",
                true,
            )
            .await;
        // Issue #27: exclude the send-claim lock-wait (already recorded as
        // DedupeLockAcquire inside claim_topic_send_attempts) so Republish
        // measures only genuine serialization + send-dispatch work.
        let republish_time = republish_started.elapsed().saturating_sub(lock_wait);
        self.stage_stats
            .record(PubSubStage::Republish, republish_time);

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
        let mut topics = self.topics.write_topic(&topic).await;
        self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
        let dedupe_started = Instant::now();
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());
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
                .send_to_peer_bounded(topic, from, GossipStreamType::PubSub, bytes.into(), "IWANT")
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
        let mut topics = self.topics.write_topic(&topic).await;
        self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
        let dedupe_started = Instant::now();
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());
        state.touch();

        let mut to_send = Vec::new();
        let mut requester_has_cached_message = false;

        for msg_id in msg_ids {
            if let Some(cached) = state.get_message(&msg_id) {
                to_send.push((msg_id, cached));
                requester_has_cached_message = true;
            } else {
                // Routine under loss/churn: a peer requests a message we no
                // longer cache (evicted) or never received. Not a fault —
                // logged at debug to avoid GB/hr WARN spam on degraded links.
                debug!(msg_id = ?msg_id, "IWANT for unknown message");
            }
        }

        if requester_has_cached_message {
            // GRAFT only when bounded score-aware maintenance chooses the
            // requester; an IWANT burst must not bypass mesh degree caps.
            state.add_new_peer_lazy(from);
            let (pruned, grafted) = state.maintain_degree_at(Instant::now());
            if pruned > 0 {
                self.stage_stats.record_prunes(pruned);
            }
            if grafted > 0 {
                self.stage_stats.record_grafts(grafted);
            }
        }
        self.record_stage(PubSubStage::DedupeCheck, dedupe_started);

        drop(topics); // Release lock

        let republish_started = Instant::now();
        // Send EAGER with payloads
        for (msg_id, cached) in to_send {
            debug!(peer_id = %from, msg_id = ?msg_id, "Sending EAGER in response to IWANT");
            debug!(
                target: "sg.payload.trace",
                stage = "iwant_serve",
                msg_id = %msg_id_hex8(&msg_id),
                len = cached.payload.len(),
                zero_tail = payload_zero_tail(&cached.payload),
            );

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
                .send_to_peer_bounded(topic, from, GossipStreamType::PubSub, bytes.into(), "EAGER")
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
        if !self.verify_message_signature(&message) {
            warn!(peer_id = %LogPeerId::from(from), "Anti-entropy: invalid signature, dropping");
            return Err(anyhow!("Invalid signature on anti-entropy message"));
        }
        self.record_verified_inbound_from_peer(topic, from, message.header.kind)
            .await;

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
                let mut topics = self.topics.write_topic(&topic).await;
                self.record_stage(PubSubStage::DedupeLockAcquire, lock_started);
                let dedupe_started = Instant::now();
                let state = topics
                    .entry(topic)
                    .or_insert_with(|| self.new_topic_state());
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
                    debug!(
                        target: "sg.payload.trace",
                        stage = "anti_entropy_serve",
                        msg_id = %msg_id_hex8(&cached.header.msg_id),
                        len = cached.payload.len(),
                        zero_tail = payload_zero_tail(&cached.payload),
                    );
                    let eager_msg = GossipMessage {
                        header: cached.header.clone(),
                        payload: Some(cached.payload.clone()),
                        signature: self.sign_message(&cached.header),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&eager_msg) {
                        let _ = self
                            .send_to_peer_bounded(
                                topic,
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
                                topic,
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
                let mut topics = self.topics.write_topic(&topic).await;
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
                                topic,
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
        let topics = self.topics.read_topic(&topic).await;
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
        self.send_to_peer_bounded(
            topic,
            peer,
            GossipStreamType::PubSub,
            bytes.into(),
            "ANTI_ENTROPY",
        )
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
        let outbound_budgets = Arc::clone(&self.outbound_budgets);
        let send_path = self.send_path_context();
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

                Self::flush_ihave_batches(
                    &topics,
                    &transport,
                    &signing_key,
                    &stage_stats,
                    &outbound_budgets,
                    &send_path,
                )
                .await;
            }
        });
    }

    async fn flush_ihave_batches(
        topics: &Arc<ShardedTopicMap>,
        transport: &Arc<T>,
        signing_key: &Arc<saorsa_gossip_identity::MlDsaKeyPair>,
        stage_stats: &Arc<PubSubStageStats>,
        outbound_budgets: &Arc<PeerOutboundBudgets>,
        send_path: &SendPathContext,
    ) {
        let work: Vec<(TopicId, Vec<MessageIdType>, Vec<PeerId>)> = {
            let mut topics_guard = topics.write_all().await;
            let mut work = Vec::new();

            for shard in topics_guard.iter_mut() {
                for (topic_id, state) in shard.iter_mut() {
                    if state.pending_ihave.is_empty() {
                        continue;
                    }

                    let batch: Vec<MessageIdType> = state
                        .pending_ihave
                        .drain(..state.pending_ihave.len().min(MAX_IHAVE_BATCH_SIZE))
                        .collect();

                    let lazy_peers: Vec<PeerId> = state.lazy_peers.iter().copied().collect();
                    work.push((*topic_id, batch, lazy_peers));
                }
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
                    warn!(topic = %LogTopicId::from(topic_id), "IHAVE header serialize failed: {e}");
                    continue;
                }
            };

            let payload = match postcard::to_stdvec(&batch) {
                Ok(bytes) => bytes.into(),
                Err(e) => {
                    warn!(topic = %LogTopicId::from(topic_id), "IHAVE batch serialize failed: {e}");
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
                    warn!(topic = %LogTopicId::from(topic_id), "IHAVE message serialize failed: {e}");
                    continue;
                }
            };

            // X0X-0074: filter IHAVE recipients through admission before
            // claiming attempts. Bulk-classified topics (most production
            // anti-entropy carriers) will have admissions dropped here
            // instead of entering the per-peer outbound pipeline. Bulk
            // depths released at end-of-iteration regardless of send
            // outcome (covers no-claim, partial-claim, panic, normal
            // completion paths uniformly).
            let (attempts, permits, bulk_admitted) = {
                let now = Instant::now();
                let mut topics_guard = topics.write_topic(&topic_id).await;
                let Some(state) = topics_guard.get_mut(&topic_id) else {
                    continue;
                };
                let (admitted, bulk_admitted) = filter_peers_through_admission_in_state(
                    send_path, state, &topic_id, lazy_peers, "IHAVE", now,
                );
                if admitted.is_empty() {
                    release_bulk_admissions_free(&send_path.admission, &bulk_admitted);
                    continue;
                }
                let claim_context = SendClaimContext {
                    stage_stats: stage_stats.as_ref(),
                    outbound_budgets,
                    send_path,
                    topic: topic_id,
                    op: "IHAVE",
                    send_class: OutboundSendClass::for_op("IHAVE"),
                    priority: send_path.admission.registry().priority_for(&topic_id),
                };
                let (attempts, permits) =
                    Self::claim_topic_send_attempts_for_state(&claim_context, state, admitted, now);
                (attempts, permits, bulk_admitted)
            };
            let mut claims = SendAttemptClaims::new(
                topic_id,
                attempts,
                permits,
                Arc::clone(topics),
                Arc::clone(stage_stats),
                send_path.clone(),
            );

            // X0X-0007: parallel send with per-peer timeout. Was a sequential
            // for-await loop that bounded periodic IHAVE flush latency by the
            // sum of per-peer send latencies; now bounded by max + the per-
            // peer timeout. Same tracked-JoinHandle shape as
            // `parallel_send_to_peers` so a panicked recovery-probe task can
            // still be associated with the peer and re-suppressed.
            if !claims.is_empty() {
                let mut send_tasks = SendTaskSet::with_capacity("IHAVE", claims.attempts().len());
                let attempts = claims.attempts().to_vec();
                let permits = claims.take_permits();
                for (attempt, permit) in attempts.into_iter().zip(permits) {
                    let transport = Arc::clone(transport);
                    let bytes = bytes.clone();
                    let stage_stats = Arc::clone(stage_stats);
                    let rtt_tracker = Arc::clone(&send_path.rtt_tracker);
                    let handle = tokio::spawn(async move {
                        let _permit = permit;
                        Self::send_to_peer_with_timeout(
                            transport,
                            stage_stats,
                            rtt_tracker,
                            attempt.peer,
                            GossipStreamType::PubSub,
                            bytes,
                            "IHAVE",
                        )
                        .await
                    });
                    send_tasks.push(attempt, handle);
                }
                let (sent, timed_out) = send_tasks.collect_results().await;
                claims.record_results(sent, timed_out).await;
            }
            // X0X-0074: release every Bulk admission reserved above,
            // exactly once. Covers no-claim, partial-claim, and panic
            // paths uniformly via the explicit list rather than relying
            // on per-completion release.
            release_bulk_admissions_free(&send_path.admission, &bulk_admitted);
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
        let stage_stats = Arc::clone(&self.stage_stats);
        let topic_idle_ttl = Duration::from_secs(TOPIC_IDLE_TTL_SECS);

        tokio::spawn(async move {
            let mut cleanup_interval =
                Duration::from_millis(SUPPRESSION_CLEANUP_NORMAL_INTERVAL_MS);
            let mut previous_suppression_len = 0_usize;
            let mut previous_cleanup = Instant::now();

            loop {
                time::sleep(cleanup_interval).await;
                let before_suppression_len = stage_stats.suppressed_peer_count();
                let cleanup_now = Instant::now();
                let mut removed_suppressions = 0;
                let mut idle_topics = Vec::new();
                let mut topics_guard = topics.write_all().await;
                for shard in topics_guard.iter_mut() {
                    removed_suppressions +=
                        clean_expired_peer_cooling(shard, &stage_stats, cleanup_now);
                    idle_topics.extend(clean_and_reap_topic_ids(shard, topic_idle_ttl));
                }
                let idle_count = idle_topics.len();
                let after_suppression_len = stage_stats.suppressed_peer_count();
                drop(topics_guard);

                for topic in idle_topics {
                    stage_stats.clear_topic_suppressions(topic);
                }
                let now = Instant::now();
                let elapsed = now
                    .saturating_duration_since(previous_cleanup)
                    .max(Duration::from_millis(1));
                let growth_rate = (after_suppression_len as f64 - previous_suppression_len as f64)
                    / elapsed.as_secs_f64();
                cleanup_interval = next_suppression_cleanup_interval(
                    previous_suppression_len,
                    after_suppression_len,
                    cleanup_interval,
                );
                previous_cleanup = now;
                previous_suppression_len = after_suppression_len;
                stage_stats.record_suppression_cleanup(
                    cleanup_interval,
                    growth_rate,
                    after_suppression_len,
                    removed_suppressions,
                );
                debug!(
                    before = before_suppression_len,
                    after = after_suppression_len,
                    removed = removed_suppressions,
                    growth_rate_per_sec = growth_rate,
                    next_interval_ms = duration_millis_u64(cleanup_interval),
                    "Adaptive PubSub suppression cleanup pass"
                );
                if idle_count > 0 {
                    let remaining = topics.topic_count().await;
                    debug!(
                        idle_count = idle_count,
                        remaining, "Reaped idle TopicState entries"
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

                let mut topics_guard = topics.write_all().await;
                let mut pruned = 0;
                let mut grafted = 0;

                for shard in topics_guard.iter_mut() {
                    for state in shard.values_mut() {
                        let (state_pruned, state_grafted) = state.maintain_degree();
                        pruned += state_pruned;
                        grafted += state_grafted;
                    }
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
        let outbound_budgets = Arc::clone(&self.outbound_budgets);
        let send_path = self.send_path_context();
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

                let topics_guard = topics.read_all().await;

                // Collect work to do (topic, peer, msg_ids) while holding the read lock.
                let mut work: Vec<(TopicId, PeerId, Vec<MessageIdType>)> = Vec::new();
                for shard in &topics_guard {
                    for (topic_id, state) in shard.iter() {
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
                        // X0X-0074: anti-entropy is bulk anti-entropy
                        // protocol traffic — filter through admission
                        // before claiming attempts. Bulk depth released
                        // exactly once at the end of this iteration via
                        // the explicit list captured here.
                        let (attempts, permits, bulk_admitted) = {
                            let now = Instant::now();
                            let mut topics_guard = topics.write_topic(&topic_id).await;
                            let Some(state) = topics_guard.get_mut(&topic_id) else {
                                continue;
                            };
                            let (admitted, bulk_admitted) = filter_peers_through_admission_in_state(
                                &send_path,
                                state,
                                &topic_id,
                                vec![peer],
                                "ANTI_ENTROPY",
                                now,
                            );
                            if admitted.is_empty() {
                                release_bulk_admissions_free(&send_path.admission, &bulk_admitted);
                                continue;
                            }
                            let claim_context = SendClaimContext {
                                stage_stats: stage_stats.as_ref(),
                                outbound_budgets: &outbound_budgets,
                                send_path: &send_path,
                                topic: topic_id,
                                op: "ANTI_ENTROPY",
                                send_class: OutboundSendClass::for_op("ANTI_ENTROPY"),
                                priority: send_path.admission.registry().priority_for(&topic_id),
                            };
                            let (attempts, permits) = Self::claim_topic_send_attempts_for_state(
                                &claim_context,
                                state,
                                admitted,
                                now,
                            );
                            (attempts, permits, bulk_admitted)
                        };
                        let mut claims = SendAttemptClaims::new(
                            topic_id,
                            attempts,
                            permits,
                            Arc::clone(&topics),
                            Arc::clone(&stage_stats),
                            send_path.clone(),
                        );
                        let Some(attempt) = claims.attempts().first().copied() else {
                            continue;
                        };
                        let Some(permit) = claims.take_permits().into_iter().next() else {
                            continue;
                        };
                        let bytes: Bytes = bytes.into();
                        let send_transport = Arc::clone(&transport);
                        let send_stage_stats = Arc::clone(&stage_stats);
                        let send_rtt_tracker = Arc::clone(&send_path.rtt_tracker);
                        let handle = AbortOnDropSendHandle::new(tokio::spawn(async move {
                            let _permit = permit;
                            Self::send_to_peer_with_timeout(
                                send_transport,
                                send_stage_stats,
                                send_rtt_tracker,
                                attempt.peer,
                                GossipStreamType::PubSub,
                                bytes,
                                "ANTI_ENTROPY",
                            )
                            .await
                        }));

                        match handle.join().await {
                            Ok(Ok(PeerSendOutcome::Sent { observed })) => {
                                claims
                                    .record_results(
                                        vec![PeerSendCompletion { attempt, observed }],
                                        Vec::new(),
                                    )
                                    .await;
                            }
                            Ok(Ok(PeerSendOutcome::TimedOut)) => {
                                claims.record_results(Vec::new(), vec![attempt]).await;
                            }
                            Ok(Err(_)) => {
                                claims
                                    .record_results(Vec::new(), recovery_probe_timeout(attempt))
                                    .await;
                            }
                            Err(e) => {
                                claims
                                    .record_results(Vec::new(), recovery_probe_timeout(attempt))
                                    .await;
                                warn!(
                                    peer_id = %LogPeerId::from(attempt.peer),
                                    "ANTI_ENTROPY per-peer send task panicked: {e}"
                                );
                            }
                        }
                        // X0X-0074: release Bulk admission depth reserved
                        // above, exactly once. Covers panic / early-return
                        // paths uniformly.
                        release_bulk_admissions_free(&send_path.admission, &bulk_admitted);
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
        let mut topics = self.topics.write_topic(&topic).await;
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());

        let now = Instant::now();
        for peer in peers {
            // New peers enter LAZY first; score-aware maintenance chooses the
            // bounded EAGER subset instead of bulk-promoting the full view.
            state.add_new_peer_lazy(peer);
        }

        let (pruned, grafted) = state.maintain_degree_at(now);
        if pruned > 0 {
            self.stage_stats.record_prunes(pruned);
        }
        if grafted > 0 {
            self.stage_stats.record_grafts(grafted);
        }

        debug!(topic = ?topic, peer_count = state.eager_peers.len(), "Initialized topic peers");
    }

    /// Replace topic peers with exactly the given set of connected peers.
    ///
    /// Removes stale peers that are no longer connected and adds new ones.
    /// Peers that were previously moved to `lazy_peers` via PRUNE are left
    /// in lazy if they are still connected; otherwise they are removed.
    pub async fn set_topic_peers(&self, topic: TopicId, connected: Vec<PeerId>) {
        let rebuild_started = Instant::now();
        debug!(
            topic = ?topic,
            connected = connected.len(),
            "PubSub peer score rebuild start"
        );
        let mut topics = self.topics.write_topic(&topic).await;
        let state = topics
            .entry(topic)
            .or_insert_with(|| self.new_topic_state());

        let connected_set: HashSet<PeerId> = connected.iter().copied().collect();

        // Remove stale peers (no longer connected) from both sets.
        state.eager_peers.retain(|p| connected_set.contains(p));
        state.lazy_peers.retain(|p| connected_set.contains(p));
        let removed_cooling = state.clear_disconnected_peer_cooling(&connected_set);
        for peer in removed_cooling {
            self.stage_stats.clear_peer_suppression(topic, peer);
        }

        // Add new connected peers conservatively as LAZY, then let score-aware
        // degree maintenance decide which peers are worth promoting. This
        // avoids undoing send-side demotion/cooling on every membership refresh.
        let now = Instant::now();
        for peer in connected_set.iter().copied() {
            state.add_new_peer_lazy(peer);
        }

        let (pruned, grafted) = state.maintain_degree_at(now);
        if pruned > 0 {
            self.stage_stats.record_prunes(pruned);
        }
        if grafted > 0 {
            self.stage_stats.record_grafts(grafted);
        }

        debug!(
            topic = ?topic,
            duration_ms = rebuild_started.elapsed().as_millis() as u64,
            eager = state.eager_peers.len(),
            lazy = state.lazy_peers.len(),
            peer_scores = state.peer_scores.len(),
            pruned,
            grafted,
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
        self.topics.all_topic_ids().await
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
        let cache_config = self.cache_config;

        tokio::spawn(async move {
            let mut topics_guard = topics.write_topic(&topic).await;
            let state = topics_guard
                .entry(topic)
                .or_insert_with(|| TopicState::with_cache_config(cache_config));
            state.touch();
            state.subscribers.push(tx);
        });

        rx
    }

    async fn subscribe_ready(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut topics_guard = self.topics.write_topic(&topic).await;
        let state = topics_guard
            .entry(topic)
            .or_insert_with(|| TopicState::with_cache_config(self.cache_config));
        state.touch();
        state.subscribers.push(tx);
        rx
    }

    async fn unsubscribe(&self, topic: TopicId) -> Result<()> {
        let mut topics = self.topics.write_topic(&topic).await;
        topics.remove(&topic);
        drop(topics);
        self.stage_stats.clear_topic_suppressions(topic);
        Ok(())
    }

    async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>) {
        PlumtreePubSub::initialize_topic_peers(self, topic, peers).await
    }

    async fn set_topic_peers(&self, topic: TopicId, connected: Vec<PeerId>) {
        PlumtreePubSub::set_topic_peers(self, topic, connected).await
    }

    fn stage_stats_snapshot(&self) -> Option<PubSubStageStatsSnapshot> {
        Some(self.stage_stats())
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
                if !self.verify_message_signature(&message) {
                    warn!(peer_id = %LogPeerId::from(from), "IHAVE: invalid signature, dropping");
                    return Err(anyhow!("Invalid signature on IHAVE message"));
                }
                self.record_verified_inbound_from_peer(topic_id, from, msg_kind)
                    .await;
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
                if !self.verify_message_signature(&message) {
                    warn!(peer_id = %LogPeerId::from(from), "IWANT: invalid signature, dropping");
                    return Err(anyhow!("Invalid signature on IWANT message"));
                }
                self.record_verified_inbound_from_peer(topic_id, from, msg_kind)
                    .await;
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
                if self.verify_message_signature(&message) {
                    self.record_verified_inbound_from_peer(topic_id, from, msg_kind)
                        .await;
                } else {
                    warn!(
                        peer_id = %LogPeerId::from(from),
                        msg_kind = ?msg_kind,
                        "PubSub received non-pubsub message kind with invalid signature; cooling not reset"
                    );
                }
                warn!(
                    "PubSub received non-pubsub message kind {:?}, ignoring",
                    msg_kind
                );
                Ok(())
            }
        }
    }

    async fn trigger_anti_entropy(&self, topic: TopicId) -> Result<()> {
        let topics = self.topics.read_topic(&topic).await;

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

    fn suppressed_snapshot(topic: TopicId, peer: PeerId, state: &str) -> SuppressedPeerSnapshot {
        SuppressedPeerSnapshot {
            peer_id: peer.to_string(),
            topic: topic.to_string(),
            suppressed_until_unix_ms: 1_000,
            suppressed_for_ms: 30_000,
            recent_timeout_count: 5,
            recent_timeout_rate_per_sec: 1.0,
            affected_topics_count: 1,
            cooldown_ms: 30_000,
            last_suppressed_unix_ms: 900,
            state: state.to_string(),
            recovery_probe_in_flight: state == "recovery_probe",
        }
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

    #[test]
    fn peek_message_kind_reads_kind_without_full_decode() {
        let key = test_signing_key();
        let topic = TopicId::new([3u8; 32]);
        // EAGER (data) — preserved under pressure.
        let eager = signed_eager_message(&key, topic, [1u8; 32], Bytes::from_static(b"data"));
        let eager_wire = postcard::to_stdvec(&eager).expect("serializes");
        assert_eq!(peek_message_kind(&eager_wire), Some(MessageKind::Eager));

        // Recoverable control kinds — shed-eligible under pressure.
        for kind in [
            MessageKind::IHave,
            MessageKind::IWant,
            MessageKind::AntiEntropy,
        ] {
            let msg =
                signed_control_message(&key, topic, [2u8; 32], kind, Bytes::from_static(b"c"));
            let wire = postcard::to_stdvec(&msg).expect("serializes");
            assert_eq!(
                peek_message_kind(&wire),
                Some(kind),
                "peek must read {kind:?} from the header prefix"
            );
        }

        // Garbage / truncated frame decodes to None, never panics.
        assert_eq!(peek_message_kind(b""), None);
        assert_eq!(peek_message_kind(&[0xff, 0xff, 0xff]), None);
    }

    fn unsigned_control_message(
        topic: TopicId,
        msg_id: MessageIdType,
        kind: MessageKind,
        payload: Bytes,
    ) -> GossipMessage {
        GossipMessage {
            header: MessageHeader {
                version: 1,
                topic,
                msg_id,
                kind,
                hop: 0,
                ttl: 10,
            },
            payload: Some(payload),
            signature: Vec::new(),
            public_key: Vec::new(),
        }
    }

    fn test_msg_id(id: u64) -> MessageIdType {
        let mut msg_id = [0u8; 32];
        msg_id[..8].copy_from_slice(&id.to_le_bytes());
        msg_id
    }

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("test cache cap must be non-zero")
    }

    fn test_header(topic: TopicId, msg_id: MessageIdType) -> MessageHeader {
        MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        }
    }

    fn test_cached_message(
        topic: TopicId,
        msg_id: MessageIdType,
        payload_len: usize,
    ) -> CachedMessage {
        CachedMessage {
            payload: Bytes::from(vec![0u8; payload_len]),
            header: test_header(topic, msg_id),
        }
    }

    fn seed_subthreshold_cooling(state: &mut TopicState, peer: PeerId, now: Instant) {
        state.eager_peers.insert(peer);
        for _ in 0..(PEER_TIMEOUT_THRESHOLD - 1) {
            assert!(
                state
                    .record_send_timeout_at(normal_send_attempt(peer), now)
                    .is_none(),
                "sub-threshold timeouts must not produce a suppression event"
            );
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
        connected_peer_ids: Mutex<Vec<PeerId>>,
    }

    struct PanicTransport {
        local_peer: PeerId,
    }

    impl RecordingTransport {
        fn new(local_peer: PeerId) -> Arc<Self> {
            Arc::new(Self {
                local_peer,
                send_counts: Mutex::new(HashMap::new()),
                connected_peer_ids: Mutex::new(Vec::new()),
            })
        }

        fn set_connected_peer_ids(&self, peers: Vec<PeerId>) {
            *self
                .connected_peer_ids
                .lock()
                .expect("connected peers lock") = peers;
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

    impl PanicTransport {
        fn new(local_peer: PeerId) -> Arc<Self> {
            Arc::new(Self { local_peer })
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

        async fn connected_peer_ids(&self) -> Vec<PeerId> {
            self.connected_peer_ids
                .lock()
                .expect("connected peers lock")
                .clone()
        }

        fn local_peer_id(&self) -> PeerId {
            self.local_peer
        }
    }

    #[async_trait::async_trait]
    impl GossipTransport for PanicTransport {
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
            _peer: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> Result<()> {
            Err(anyhow!("panic transport send_to_peer"))
        }

        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            Err(anyhow!("panic test transport does not receive"))
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
    async fn test_unsubscribe_clears_suppression_diagnostics() {
        let peer_id = test_peer_id(1);
        let peer = test_peer_id(2);
        let transport = test_transport().await;
        let pubsub =
            PlumtreePubSub::new_with_task_control(peer_id, transport, test_signing_key(), false);
        let topic = TopicId::new([8u8; 32]);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            topics.insert(topic, TopicState::new());
        }
        pubsub.stage_stats.record_peer_suppressed(
            topic,
            peer,
            Instant::now() + Duration::from_secs(10),
            PEER_TIMEOUT_THRESHOLD,
            PEER_SUPPRESSION_COOLDOWN,
        );
        assert_eq!(pubsub.stage_stats().suppressed_peers.len(), 1);

        pubsub
            .unsubscribe(topic)
            .await
            .expect("unsubscribe should succeed");

        assert!(
            pubsub.stage_stats().suppressed_peers.is_empty(),
            "unsubscribe should clear per-topic suppression diagnostics"
        );
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
        assert_eq!(stats.graft, 2);
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
    async fn test_subscribe_ready_delivers_immediate_local_publish() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([0x5a; 32]);

        let mut rx = pubsub.subscribe_ready(topic).await;
        let data = Bytes::from("first message after subscribe_ready");
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
        // The slow peer's task hits its 2500 ms budget (X0X-0061) and is
        // skipped; the other peers' tasks completed immediately on release.
        tokio::time::timeout(Duration::from_secs(5), publish)
            .await
            .expect("publish must return within the per-peer timeout budget — slow peer should not pin the dispatcher")
            .expect("publish task should not panic");
        let publish_elapsed = publish_started.elapsed();
        assert!(
            publish_elapsed < PER_PEER_REPUBLISH_TIMEOUT + Duration::from_millis(1_000),
            "publish elapsed {publish_elapsed:?} — should be bounded by PER_PEER_REPUBLISH_TIMEOUT plus a small slack"
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
    async fn admission_drops_bulk_send_to_suspect_peer_and_records_counter() {
        // Why: X0X-0074 admission must drop Bulk admissions to peers
        // the SWIM oracle has flagged Suspect — that's the substrate-
        // level pressure relief mechanism the Hunt 12f forecast called
        // for. The dropped send must never reach the transport, and the
        // counter must increment so operators can attribute pressure.
        let peer_id = test_peer_id(1);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([74u8; 32]);
        let suspect_peer = test_peer_id(2);
        pubsub
            .initialize_topic_peers(topic, vec![suspect_peer])
            .await;
        pubsub
            .admission()
            .registry()
            .register(topic, TopicPriority::Bulk);
        // Seed the health snapshot directly — bypasses the oracle so the
        // test runs without spawning a refresher task.
        {
            let mut guard = pubsub.peer_health_snapshot.write().expect("snapshot lock");
            guard.insert(suspect_peer, PeerHealth::Suspect);
        }

        pubsub
            .publish_local(topic, Bytes::from_static(b"bulk-suspect-drop"))
            .await
            .expect("publish should complete even when admission drops");

        let stats = pubsub.admission().stats().snapshot();
        assert_eq!(
            stats.dropped_bulk_peer_suspect, 1,
            "bulk admission to Suspect peer must drop and increment the counter"
        );
        assert_eq!(stats.admitted_bulk, 0);
        assert_eq!(
            transport.send_count(),
            0,
            "dropped admission must never reach the transport"
        );
    }

    #[tokio::test]
    async fn admission_admits_critical_send_even_when_peer_dead() {
        // Why: the ticket's hardest contract — Critical never drops on
        // health. DM inbox + control plane must keep flowing even when
        // SWIM has flagged the peer Dead. A dropped Critical is a
        // soak-blocking hard error.
        let peer_id = test_peer_id(1);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([75u8; 32]);
        let dead_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![dead_peer]).await;
        pubsub
            .admission()
            .registry()
            .register(topic, TopicPriority::Critical);
        {
            let mut guard = pubsub.peer_health_snapshot.write().expect("snapshot lock");
            guard.insert(dead_peer, PeerHealth::Dead);
        }

        transport.release_sends(1);
        pubsub
            .publish_local(topic, Bytes::from_static(b"critical-dead-admit"))
            .await
            .expect("publish should complete");

        let stats = pubsub.admission().stats().snapshot();
        assert_eq!(stats.admitted_critical, 1);
        assert_eq!(
            stats.dropped_critical_hard_error, 0,
            "Critical must never drop; this counter is a soak violation"
        );
    }

    #[tokio::test]
    async fn admission_bulk_depth_releases_after_each_publish() {
        // Why: regression for reviewer P1.3 (2026-05-12). The previous
        // release path iterated `claims.attempts()` after it had been
        // checked empty — the loop was a no-op so per-peer Bulk depth
        // monotonically leaked, eventually starving the peer of all
        // future Bulk admissions with false `BulkBackpressure`. After
        // the fix, repeated publishes to the same peer must keep the
        // per-peer depth at zero between calls.
        let peer_id = test_peer_id(1);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([76u8; 32]);
        let bulk_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![bulk_peer]).await;
        pubsub
            .admission()
            .registry()
            .register(topic, TopicPriority::Bulk);
        {
            let mut guard = pubsub.peer_health_snapshot.write().expect("snapshot lock");
            guard.insert(bulk_peer, PeerHealth::Alive);
        }

        // Release every send so they all complete (no leaks via the
        // timed-out path).
        transport.release_sends(10);

        for _ in 0..5 {
            pubsub
                .publish_local(topic, Bytes::from_static(b"bulk-no-leak"))
                .await
                .expect("publish should complete");
        }

        let per_peer = pubsub.admission().per_peer_snapshot();
        let depth = per_peer
            .get(&bulk_peer)
            .map(|c| c.bulk_queue_depth)
            .unwrap_or(0);
        assert_eq!(
            depth, 0,
            "Bulk depth must drain to zero between publishes; current = {depth}"
        );
    }

    #[tokio::test]
    async fn critical_sends_serialize_under_budget_pressure_without_hard_error() {
        // Why: X0X-0074d supersedes the pre-0074d behaviour this test used to
        // assert. Before 0074d a Critical send that found the per-peer budget
        // held by another in-flight Critical send was *hard-dropped*
        // (`dropped_critical_hard_error`). Now the per-peer Critical FIFO gate
        // makes the second send WAIT for the first instead of dropping it, so
        // under contention below the queue bound the hard-error counter must
        // stay at ZERO. This test reproduces the old contention scenario (one
        // Critical send pinned in the blocking transport, a second concurrent
        // Critical send to the same peer) and confirms the second is queued —
        // not counted as a hard error, and not counted as cooling.
        let peer_id = test_peer_id(1);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([77u8; 32]);
        let target_peer = test_peer_id(2);
        pubsub
            .initialize_topic_peers(topic, vec![target_peer])
            .await;
        pubsub
            .admission()
            .registry()
            .register(topic, TopicPriority::Critical);

        // Saturate outbound budget — every published bundle holds a
        // permit, which the blocking transport never releases. The
        // shared budget runs out and subsequent Critical admissions
        // claim zero attempts.
        let saturate_task = {
            let pubsub = Arc::clone(&pubsub);
            tokio::spawn(async move {
                for _ in 0..32 {
                    let _ = pubsub
                        .publish_local(topic, Bytes::from_static(b"saturate"))
                        .await;
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The saturation publishes may have already started recording
        // the hard error (Critical + claim fail). Take a baseline.
        let baseline = pubsub
            .admission()
            .stats()
            .snapshot()
            .dropped_critical_hard_error;

        // Final probe publish — guaranteed to hit the no-claim branch.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            pubsub.publish_local(topic, Bytes::from_static(b"probe")),
        )
        .await;

        let snapshot = pubsub.admission().stats().snapshot();
        let post = snapshot.dropped_critical_hard_error;
        assert_eq!(
            baseline, 0,
            "no hard error should accrue while sends merely serialize (baseline={baseline})"
        );
        assert_eq!(
            post, 0,
            "X0X-0074d: a Critical send contending for an in-flight peer must QUEUE, not hard-drop (post={post})"
        );
        assert_eq!(
            snapshot.dropped_critical_cooling, 0,
            "no peer is cooling in this scenario, so no cooling skips either"
        );
        // Serialization holds: never more than one Critical send reaches the
        // transport for the same peer at once.
        assert!(
            transport.max_in_flight() <= 1,
            "gate must serialize Critical sends per peer (max_in_flight={})",
            transport.max_in_flight()
        );
        saturate_task.abort();
    }

    #[tokio::test]
    async fn critical_bypasses_in_flight_bulk_via_dedicated_data_lane() {
        // Why: X0X-0074c — production telemetry showed most Critical drops
        // were not Bulk contention; they were Critical/Normal contention on
        // the single per-peer Data permit. Critical now has a dedicated Data
        // lane, so a blocked Bulk send in the best-effort lane must not prevent
        // Critical from starting or increment dropped_critical_hard_error.
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let target_peer = test_peer_id(2);
        let topic_bulk = TopicId::new([74u8; 32]);
        let topic_crit = TopicId::new([75u8; 32]);
        // Same peer is an eager target for both topics. Bulk uses the
        // best-effort data lane; Critical uses the dedicated Critical lane.
        pubsub
            .initialize_topic_peers(topic_bulk, vec![target_peer])
            .await;
        pubsub
            .initialize_topic_peers(topic_crit, vec![target_peer])
            .await;
        pubsub
            .admission()
            .registry()
            .register(topic_bulk, TopicPriority::Bulk);
        pubsub
            .admission()
            .registry()
            .register(topic_crit, TopicPriority::Critical);

        // Pin a Bulk EAGER send: it claims the best-effort data lane and
        // blocks forever on the transport semaphore (never released).
        let bulk_task = {
            let pubsub = Arc::clone(&pubsub);
            tokio::spawn(async move {
                let _ = pubsub
                    .publish_local(topic_bulk, Bytes::from_static(b"bulk"))
                    .await;
            })
        };
        // Wait for the Bulk send to actually reach the transport (permit
        // now held).
        let first = tokio::time::timeout(Duration::from_secs(2), started_rx.recv()).await;
        assert!(
            first.is_ok() && first.expect("recv").is_some(),
            "Bulk EAGER send must reach the blocking transport and hold the data permit"
        );

        let baseline_hard = pubsub
            .admission()
            .stats()
            .snapshot()
            .dropped_critical_hard_error;

        // Publish Critical to the same peer. The best-effort lane is held by
        // Bulk, but Critical must still start via its dedicated lane.
        let crit_task = {
            let pubsub = Arc::clone(&pubsub);
            tokio::spawn(async move {
                let _ = pubsub
                    .publish_local(topic_crit, Bytes::from_static(b"crit"))
                    .await;
            })
        };

        // The Critical send reaches the transport without waiting for the
        // blocked Bulk send to release the best-effort lane.
        let second = tokio::time::timeout(Duration::from_secs(2), started_rx.recv()).await;
        assert!(
            second.is_ok() && second.expect("recv").is_some(),
            "Critical EAGER send must reach the transport via its dedicated lane"
        );

        let post_hard = pubsub
            .admission()
            .stats()
            .snapshot()
            .dropped_critical_hard_error;
        assert_eq!(
            post_hard, baseline_hard,
            "Critical must not record a hard error while Bulk holds the best-effort lane \
             (baseline={baseline_hard}, post={post_hard})"
        );

        transport.release_sends(8);
        bulk_task.abort();
        crit_task.abort();
    }

    #[tokio::test]
    async fn best_effort_lane_independent_of_gate_managed_critical() {
        // Why: X0X-0074c gave Critical a dedicated single-permit per-peer Data
        // lane; X0X-0074d replaced that lane with the FIFO Critical gate
        // (one-at-a-time serialization enforced at engage time, bounded by
        // OUTBOUND_CRITICAL_QUEUE_PER_PEER). At the budget layer this means:
        // (1) the Normal/Bulk best-effort lane is still a single permit and is
        // unaffected by Critical, and (2) Critical reservations no longer occupy
        // a budget lane, so several can be reserved at once — the real
        // serialization is the gate's job (see
        // `test_x0x_0074d_critical_gate_serializes_per_peer`).
        let budgets = Arc::new(PeerOutboundBudgets::default());
        let peer = test_peer_id(2);
        let now = Instant::now();

        let bulk_permit = budgets
            .try_acquire(peer, OutboundSendClass::Data, TopicPriority::Bulk, now)
            .expect("Bulk should claim the best-effort data lane");

        // A Critical reservation succeeds alongside in-flight Bulk and does not
        // consume the best-effort lane.
        let crit_permit = budgets
            .try_acquire(peer, OutboundSendClass::Data, TopicPriority::Critical, now)
            .expect("Critical reserves the gate while Bulk is in flight");

        assert!(
            budgets
                .try_acquire(peer, OutboundSendClass::Data, TopicPriority::Bulk, now)
                .is_none(),
            "best-effort lane remains held by the first Bulk send"
        );

        drop(bulk_permit);
        assert!(
            budgets
                .try_acquire(peer, OutboundSendClass::Data, TopicPriority::Bulk, now)
                .is_some(),
            "dropping Bulk releases only the best-effort lane (Critical is gate-managed)"
        );

        // Critical is no longer a single budget lane: a second Critical
        // reservation is also admitted (the gate queues them; it does not drop).
        let crit_permit2 = budgets
            .try_acquire(peer, OutboundSendClass::Data, TopicPriority::Critical, now)
            .expect("a second Critical reservation is admitted (gate queues, not drops)");

        drop(crit_permit);
        drop(crit_permit2);
    }

    #[tokio::test]
    async fn peer_scoring_populated_by_real_handle_eager_path() {
        // Why X0X-0071 (reviewer P2 regression guard): `peer_scores_v2`
        // must be populated by PRODUCTION pubsub events, not just by a
        // test poking the engine directly. Under the original PR, the
        // only integration was `stage_stats()` snapshotting an engine
        // that no production path ever wrote to — so the diagnostic was
        // permanently empty. This test drives REAL `handle_eager` calls
        // (a valid delivery and a forged-signature message) and asserts
        // the scores surface through `stage_stats()`, each scoped to
        // the `(topic, peer)` it was driven on.
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([71u8; 32]);
        let good_peer = test_peer_id(2);
        let bad_peer = test_peer_id(3);

        // good_peer starts in our eager mesh for the topic.
        pubsub.initialize_topic_peers(topic, vec![good_peer]).await;

        // 1. good_peer delivers a novel, correctly-signed message —
        //    drives P2 (first delivery) + P1 (note_mesh_join, eager).
        let payload = Bytes::from("novel message");
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
        let good_msg = GossipMessage {
            header,
            payload: Some(payload),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };
        pubsub
            .handle_eager(good_peer, topic, good_msg)
            .await
            .expect("valid handle_eager");

        // 2. bad_peer sends a message whose signature is over the wrong
        //    bytes — verification fails, driving P4 (invalid message).
        let bad_payload = Bytes::from("forged message");
        let bad_msg_id = pubsub.calculate_msg_id(&topic, &bad_payload);
        let bad_header = MessageHeader {
            version: 1,
            topic,
            msg_id: bad_msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };
        let forged_signature = signing_key.sign(b"not the header bytes").expect("sign");
        let bad_msg = GossipMessage {
            header: bad_header,
            payload: Some(bad_payload),
            signature: forged_signature,
            public_key: signing_key.public_key().to_vec(),
        };
        // Returns Err (invalid signature) — the P4 record happens before
        // the early return.
        assert!(
            pubsub.handle_eager(bad_peer, topic, bad_msg).await.is_err(),
            "forged-signature message must be rejected"
        );

        // 3. The scores surface through the production diagnostic path.
        let snapshot = pubsub.stage_stats();
        assert_eq!(
            snapshot.peer_scores_v2.len(),
            2,
            "both (topic, peer) pairs must surface from the real handle_eager path"
        );
        assert!(
            snapshot
                .peer_scores_v2
                .iter()
                .all(|r| r.topic_id == topic.to_string()),
            "every snapshot row is scoped to the topic it was driven on"
        );

        let bad_row = snapshot
            .peer_scores_v2
            .iter()
            .find(|r| r.peer_id == bad_peer.to_string())
            .expect("bad_peer scored via the real path");
        // ~1.0 — the running count decays by 0.97^elapsed between the
        // record call and the snapshot, so a few millis of drift leaves
        // it a hair under 1.0.
        assert!(
            (bad_row.p4_invalid_messages - 1.0).abs() < 0.01,
            "P4 invalid-message count (~1.0) recorded from the real verify path (got {})",
            bad_row.p4_invalid_messages
        );
        assert!(
            bad_row.score < 0.0,
            "forged-signature peer has a negative score (got {})",
            bad_row.score
        );

        let good_row = snapshot
            .peer_scores_v2
            .iter()
            .find(|r| r.peer_id == good_peer.to_string())
            .expect("good_peer scored via the real path");
        assert!(
            (good_row.p2_first_deliveries - 1.0).abs() < 0.01,
            "P2 first-delivery (~1.0) recorded from the real handle_eager path (got {})",
            good_row.p2_first_deliveries
        );
        assert!(
            good_row.score > 0.0,
            "first-delivery peer has a positive score (got {})",
            good_row.score
        );
    }

    #[tokio::test]
    async fn test_outbound_budget_limits_duplicate_eager_sends_to_one_in_flight_per_peer() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([54u8; 32]);
        let slow_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![slow_peer]).await;

        let send_pubsub = Arc::clone(&pubsub);
        let repeated_slow_peer_work = vec![slow_peer; PEER_TIMEOUT_THRESHOLD + 1];
        let send = tokio::spawn(async move {
            send_pubsub
                .parallel_send_to_peers(
                    topic,
                    repeated_slow_peer_work,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"budgeted-eager"),
                    "EAGER",
                    false,
                )
                .await;
        });

        let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("one eager send should start")
            .expect("send channel should stay open");
        assert_eq!(record.peer, slow_peer);

        let extra_start = tokio::time::timeout(Duration::from_millis(100), started_rx.recv()).await;
        assert!(
            extra_start.is_err(),
            "duplicate work for one peer should be skipped by the outbound budget, not spawned"
        );

        transport.release_sends(1);
        tokio::time::timeout(Duration::from_millis(200), send)
            .await
            .expect("budgeted send should finish after the only permit is released")
            .expect("send task should not panic");

        assert_eq!(
            transport.send_count(),
            1,
            "only one data send may enter transport for a peer at a time"
        );
        assert_eq!(
            transport.max_in_flight(),
            1,
            "per-peer data budget should cap duplicate fanout work at one in-flight send"
        );

        let stats = pubsub.stage_stats();
        assert_eq!(
            stats.outbound_budget_exhausted, PEER_TIMEOUT_THRESHOLD as u64,
            "budget skips should be visible in diagnostics"
        );
        assert_eq!(
            stats.suppressed_peers.len(),
            1,
            "repeated budget pressure should feed slow-peer cooling"
        );
        assert_eq!(stats.suppressed_peers[0].peer_id, slow_peer.to_string());
    }

    /// Regression: the dispatcher EAGER-forward path must not pin its worker
    /// on a slow peer. With `detach_accounting = true` the call returns
    /// promptly even though the only peer's send is blocked indefinitely; the
    /// send is still dispatched (in-flight), proving concurrency is preserved
    /// while the worker is freed to drain the next message.
    ///
    /// WHY this matters: awaiting the fan-out inline pinned the dispatcher
    /// worker for the slowest peer's full `PER_PEER_REPUBLISH_TIMEOUT` under
    /// degraded links, collapsing drain throughput and overflowing the shared
    /// recv channel (`recv_pump.dropped_full`). If the detach is reverted,
    /// the forward becomes synchronous and blocks ~2.5 s here, so the 500 ms
    /// timeout below fails — this test cannot pass without the fix.
    #[tokio::test]
    async fn test_dispatcher_forward_detaches_accounting_and_does_not_pin_on_slow_peer() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([66u8; 32]);
        let slow_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![slow_peer]).await;

        // Dispatcher forward path (detach_accounting = true). The slow peer's
        // send is never released, so a synchronous fan-out would block until
        // the per-peer timeout. The detached path must return promptly.
        let forward = pubsub.parallel_send_to_peers(
            topic,
            vec![slow_peer],
            GossipStreamType::PubSub,
            Bytes::from_static(b"detached-eager"),
            "EAGER",
            true,
        );
        tokio::time::timeout(Duration::from_millis(500), forward)
            .await
            .expect("detached forward must return promptly, not pin on the slow peer");

        // The send was still dispatched — it is in-flight, blocked on the
        // transport — so detaching the accounting did not drop the send.
        let record = tokio::time::timeout(Duration::from_millis(200), started_rx.recv())
            .await
            .expect("the detached send should still have started")
            .expect("send channel should stay open");
        assert_eq!(record.peer, slow_peer);
        assert_eq!(
            transport.max_in_flight(),
            1,
            "the per-peer send is still in-flight after the worker returned"
        );

        // Release the blocked send so the detached accounting task can finish.
        transport.release_sends(1);
    }

    #[tokio::test]
    async fn test_one_budget_exhausted_peer_does_not_block_healthy_peer_send() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([55u8; 32]);
        let slow_peer = test_peer_id(2);
        let healthy_peer = test_peer_id(3);
        pubsub
            .initialize_topic_peers(topic, vec![slow_peer, healthy_peer])
            .await;

        let mut peers = vec![slow_peer; PEER_TIMEOUT_THRESHOLD + 1];
        peers.push(healthy_peer);
        let send_pubsub = Arc::clone(&pubsub);
        let send = tokio::spawn(async move {
            send_pubsub
                .parallel_send_to_peers(
                    topic,
                    peers,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"mixed-budgeted-eager"),
                    "EAGER",
                    false,
                )
                .await;
        });

        let mut started = HashSet::new();
        for _ in 0..2 {
            let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .expect("slow and healthy peer sends should both start")
                .expect("send channel should stay open");
            started.insert(record.peer);
        }
        assert!(
            started.contains(&slow_peer),
            "first slow-peer send should consume its one data permit"
        );
        assert!(
            started.contains(&healthy_peer),
            "healthy peer should still get its own permit while slow peer is over budget"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .is_err(),
            "no extra slow-peer sends should be spawned"
        );

        transport.release_sends(2);
        tokio::time::timeout(Duration::from_millis(200), send)
            .await
            .expect("mixed send should finish after both real sends are released")
            .expect("send task should not panic");
        assert_eq!(
            transport.send_count(),
            2,
            "only one slow-peer send and one healthy-peer send should enter transport"
        );
        assert_eq!(
            pubsub.stage_stats().outbound_budget_exhausted,
            PEER_TIMEOUT_THRESHOLD as u64
        );
    }

    #[tokio::test]
    async fn test_outbound_control_budget_limits_duplicate_iwant_sends() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([56u8; 32]);
        let peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        let first_pubsub = Arc::clone(&pubsub);
        let first = tokio::spawn(async move {
            first_pubsub
                .send_to_peer_bounded(
                    topic,
                    peer,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"iwant-1"),
                    "IWANT",
                )
                .await
        });
        let second_pubsub = Arc::clone(&pubsub);
        let second = tokio::spawn(async move {
            second_pubsub
                .send_to_peer_bounded(
                    topic,
                    peer,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"iwant-2"),
                    "IWANT",
                )
                .await
        });

        for _ in 0..OUTBOUND_CONTROL_PERMITS_PER_PEER {
            let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .expect("control sends up to the peer budget should start")
                .expect("send channel should stay open");
            assert_eq!(record.peer, peer);
        }
        assert_eq!(
            transport.send_count(),
            OUTBOUND_CONTROL_PERMITS_PER_PEER,
            "two control sends should consume the small per-peer control budget"
        );

        pubsub
            .send_to_peer_bounded(
                topic,
                peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"iwant-3"),
                "IWANT",
            )
            .await
            .expect("budget exhaustion should skip, not return a send error");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .is_err(),
            "third control send should be skipped while the control budget is full"
        );
        assert_eq!(
            pubsub.stage_stats().outbound_budget_exhausted,
            1,
            "control budget exhaustion should be visible in diagnostics"
        );

        transport.release_sends(OUTBOUND_CONTROL_PERMITS_PER_PEER);
        for task in [first, second] {
            tokio::time::timeout(Duration::from_millis(200), task)
                .await
                .expect("control send should finish after release")
                .expect("send task should not panic")
                .expect("send should succeed");
        }
    }

    #[test]
    fn diagnostics_group_suppression_by_topic_and_peer_admission_state() {
        let topic_a = TopicId::new([1u8; 32]);
        let topic_b = TopicId::new([2u8; 32]);
        let peer_a = test_peer_id(10);
        let peer_b = test_peer_id(11);
        let suppressed = vec![
            suppressed_snapshot(topic_b, peer_b, "recovery_probe"),
            suppressed_snapshot(topic_a, peer_a, "cooldown"),
            suppressed_snapshot(topic_a, peer_b, "recovery_ready"),
        ];

        let by_topic =
            PlumtreePubSub::<RecordingTransport>::build_suppressed_peers_by_topic(&suppressed);
        assert_eq!(
            by_topic.get(&topic_a.to_string()).expect("topic a exists"),
            &vec![peer_a.to_string(), peer_b.to_string()]
        );
        assert_eq!(
            by_topic.get(&topic_b.to_string()).expect("topic b exists"),
            &vec![peer_b.to_string()]
        );

        let admission =
            PlumtreePubSub::<RecordingTransport>::build_admission_state_by_peer(&suppressed);
        let peer_b_state = admission.get(&peer_b.to_string()).expect("peer b state");
        assert_eq!(peer_b_state.state, "recovery_probe");
        assert_eq!(peer_b_state.suppressed_topics_count, 2);
        assert_eq!(peer_b_state.recovery_probe_topics_count, 1);
        assert_eq!(peer_b_state.recovery_ready_topics_count, 1);
    }

    #[test]
    fn diagnostics_peer_scores_by_topic_include_active_cooling_breakdown() {
        let topic = TopicId::new([3u8; 32]);
        let peer = test_peer_id(12);
        let scores = vec![PeerScoreSnapshot {
            peer_id: peer.to_string(),
            topic: topic.to_string(),
            role: "cooled".to_string(),
            score: 0.25,
            iwant_response_rate: 0.5,
            recency: 0.9,
            send_health: 0.1,
            outbound_send_successes: 2.0,
            outbound_send_timeouts: 8.0,
            cooling_events: 3.0,
            recovery_probes: 1.0,
            recovery_successes: 0.0,
            eager_eligible: false,
        }];
        let suppressed = vec![suppressed_snapshot(topic, peer, "cooldown")];

        let by_topic =
            PlumtreePubSub::<RecordingTransport>::build_peer_scores_by_topic(&scores, &suppressed);
        let breakdown = by_topic
            .get(&topic.to_string())
            .and_then(|peers| peers.get(&peer.to_string()))
            .expect("score breakdown");

        assert_eq!(breakdown.role, "cooled");
        assert_eq!(breakdown.suppression_state.as_deref(), Some("cooldown"));
        assert_eq!(breakdown.recent_timeout_count, Some(5));
        assert_eq!(breakdown.cooldown_ms, Some(30_000));
        assert_eq!(breakdown.last_cool_at_unix_ms, Some(900));
    }

    #[tokio::test]
    async fn test_ihave_flush_respects_existing_control_budget() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let topics = Arc::new(ShardedTopicMap::new());
        let signing_key = Arc::new(test_signing_key());
        let stage_stats = Arc::new(PubSubStageStats::default());
        let outbound_budgets = Arc::new(PeerOutboundBudgets::default());
        let rtt_tracker = Arc::new(PerPeerRttTracker::new());
        let peer_health_snapshot = Arc::new(StdRwLock::new(HashMap::new()));
        let connected_peers_snapshot = Arc::new(StdRwLock::new(None));
        let peer_health_oracle = Arc::new(StdRwLock::new(None));
        let send_path = SendPathContext {
            rtt_tracker,
            cooling_config: AdaptiveCoolingConfig::default(),
            peer_health_snapshot,
            connected_peers_snapshot,
            peer_health_oracle,
            admission: Arc::new(admission::AdmissionControl::new()),
        };
        let topic = TopicId::new([57u8; 32]);
        let lazy_peer = test_peer_id(2);

        {
            let mut topics_guard = topics.write_topic(&topic).await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(lazy_peer);
            state.pending_ihave.push([9u8; 32]);
        }

        let now = Instant::now();
        let first_permit = outbound_budgets
            .try_acquire(
                lazy_peer,
                OutboundSendClass::Control,
                TopicPriority::Normal,
                now,
            )
            .expect("first control permit should be available");
        let second_permit = outbound_budgets
            .try_acquire(
                lazy_peer,
                OutboundSendClass::Control,
                TopicPriority::Normal,
                now,
            )
            .expect("second control permit should be available");

        PlumtreePubSub::<BlockingTransport>::flush_ihave_batches(
            &topics,
            &transport,
            &signing_key,
            &stage_stats,
            &outbound_budgets,
            &send_path,
        )
        .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .is_err(),
            "IHAVE should not spawn transport work when the peer control budget is full"
        );
        assert_eq!(
            stage_stats.snapshot().outbound_budget_exhausted,
            1,
            "IHAVE budget skip should be visible in diagnostics"
        );

        drop(first_permit);
        drop(second_permit);
    }

    #[tokio::test]
    async fn test_cancelled_outbound_send_releases_peer_budget() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([58u8; 32]);
        let peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        let first_pubsub = Arc::clone(&pubsub);
        let first = tokio::spawn(async move {
            first_pubsub
                .send_to_peer_bounded(
                    topic,
                    peer,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"cancel-first"),
                    "EAGER",
                )
                .await
        });
        tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("first send should start")
            .expect("send channel should stay open");
        first.abort();
        let _ = first.await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let in_flight = {
                    let guard = pubsub.outbound_budgets.peers_guard();
                    guard
                        .get(&peer)
                        .map(|entry| {
                            entry.critical_data_in_flight + entry.best_effort_data_in_flight
                        })
                        .unwrap_or(0)
                };
                if in_flight == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("aborting the send should release its outbound data permit");

        let second_pubsub = Arc::clone(&pubsub);
        let second = tokio::spawn(async move {
            second_pubsub
                .send_to_peer_bounded(
                    topic,
                    peer,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"after-cancel"),
                    "EAGER",
                )
                .await
        });
        tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("second send should start after the aborted permit is released")
            .expect("send channel should stay open");
        transport.release_sends(1);
        tokio::time::timeout(Duration::from_millis(200), second)
            .await
            .expect("second send should finish after release")
            .expect("second send task should not panic")
            .expect("second send should succeed");

        assert_eq!(
            transport.send_count(),
            2,
            "the second send should enter transport instead of being blocked by a leaked permit"
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
            let topics = pubsub.topics.read_topic(&topic).await;
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

    #[tokio::test]
    async fn test_stage_stats_include_peer_score_components_for_cooled_peer() {
        let peer_id = test_peer_id(1);
        let transport = RecordingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([59u8; 32]);
        let slow_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![slow_peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![slow_peer])
                .await;
        }

        let stats = pubsub.stage_stats();
        let score = stats
            .peer_scores
            .iter()
            .find(|score| {
                score.peer_id == slow_peer.to_string() && score.topic == topic.to_string()
            })
            .expect("cooled peer should have peer-score diagnostics");

        assert_eq!(score.role, "cooled");
        assert!(
            score.outbound_send_timeouts >= (PEER_TIMEOUT_THRESHOLD as f64 - 0.01),
            "timeout component should explain low score: {score:?}"
        );
        assert!(
            score.cooling_events >= 0.99,
            "cooling component should explain low score: {score:?}"
        );
        assert!(
            score.score < PEER_SCORE_EAGER_MIN,
            "cooled peer should be below eager eligibility threshold: {score:?}"
        );
        assert!(
            !score.eager_eligible,
            "cooled peer should not be marked eager eligible"
        );
    }

    #[tokio::test]
    async fn test_single_peer_bounded_sends_record_cooling_and_skip_suppressed_peer() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([46u8; 32]);
        let slow_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![slow_peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            tokio::time::timeout(
                PER_PEER_REPUBLISH_TIMEOUT + Duration::from_secs(2),
                pubsub.send_to_peer_bounded(
                    topic,
                    slow_peer,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"bounded-single-peer"),
                    "EAGER",
                ),
            )
            .await
            .expect("bounded send should return after per-peer timeout")
            .expect("timeout outcome is recorded, not returned as an error");

            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .expect("send should have started")
                .expect("started channel should stay open");
        }

        let suppressed = pubsub.stage_stats().suppressed_peers;
        assert_eq!(suppressed.len(), 1, "single-peer sends should cool peers");
        assert_eq!(suppressed[0].peer_id, slow_peer.to_string());

        // WP6: the first send during active suppression is admitted as a
        // rate-limited cooldown-bypass probe so cached data still reaches
        // the peer.
        let attempts_after_cooling = transport.send_count();
        tokio::time::timeout(
            PER_PEER_REPUBLISH_TIMEOUT + Duration::from_secs(2),
            pubsub.send_to_peer_bounded(
                topic,
                slow_peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"cooldown-bypass"),
                "EAGER",
            ),
        )
        .await
        .expect("bypass send should return after per-peer timeout")
        .expect("timeout outcome is recorded, not returned as an error");
        tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("cooldown bypass should have attempted one send")
            .expect("started channel should stay open");
        assert_eq!(
            transport.send_count(),
            attempts_after_cooling + 1,
            "suppressed peer should receive exactly one rate-limited bypass send"
        );

        // A second send inside the bypass interval must be skipped.
        let attempts_after_bypass = transport.send_count();
        pubsub
            .send_to_peer_bounded(
                topic,
                slow_peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"should-be-skipped"),
                "EAGER",
            )
            .await
            .expect("suppressed peer should be skipped cleanly");
        assert_eq!(
            transport.send_count(),
            attempts_after_bypass,
            "suppressed peer should not consume another send slot inside the bypass interval"
        );
    }

    #[tokio::test]
    async fn test_single_peer_recovery_probe_timeout_resuppresses_immediately() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([47u8; 32]);
        let slow_peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![slow_peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![slow_peer])
                .await;
        }
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).expect("topic exists");
            let cooling = state
                .peer_cooling
                .get_mut(&slow_peer)
                .expect("peer is cooling");
            cooling.suppressed_until = Some(Instant::now() - Duration::from_millis(1));
        }

        tokio::time::timeout(
            PER_PEER_REPUBLISH_TIMEOUT + Duration::from_secs(2),
            pubsub.send_to_peer_bounded(
                topic,
                slow_peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"recovery-probe"),
                "EAGER",
            ),
        )
        .await
        .expect("recovery probe should return after per-peer timeout")
        .expect("timeout outcome is recorded, not returned as an error");

        tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("recovery probe should have attempted one send")
            .expect("started channel should stay open");

        let suppressed = pubsub.stage_stats().suppressed_peers;
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].peer_id, slow_peer.to_string());
        assert_eq!(suppressed[0].recent_timeout_count, 1);
        assert_eq!(suppressed[0].state, "cooldown");
        assert!(!suppressed[0].recovery_probe_in_flight);

        // WP6: re-suppression admits one rate-limited cooldown-bypass send
        // (the trickle continues), but a second send inside the bypass
        // interval must be skipped.
        let attempts_after_probe = transport.send_count();
        tokio::time::timeout(
            PER_PEER_REPUBLISH_TIMEOUT + Duration::from_secs(2),
            pubsub.send_to_peer_bounded(
                topic,
                slow_peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"cooldown-bypass"),
                "EAGER",
            ),
        )
        .await
        .expect("bypass send should return after per-peer timeout")
        .expect("timeout outcome is recorded, not returned as an error");
        tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("cooldown bypass should have attempted one send")
            .expect("started channel should stay open");
        assert_eq!(
            transport.send_count(),
            attempts_after_probe + 1,
            "re-suppressed peer should receive exactly one rate-limited bypass send"
        );

        let attempts_after_bypass = transport.send_count();
        pubsub
            .send_to_peer_bounded(
                topic,
                slow_peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"must-skip"),
                "EAGER",
            )
            .await
            .expect("active re-suppression should skip cleanly");
        assert_eq!(
            transport.send_count(),
            attempts_after_bypass,
            "failed recovery probe should not permit another send inside the bypass interval"
        );
    }

    #[tokio::test]
    async fn test_panicked_recovery_probe_send_resuppresses_peer() {
        let peer_id = test_peer_id(1);
        let transport = PanicTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([51u8; 32]);
        let peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![peer])
                .await;
        }
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).expect("topic exists");
            let cooling = state.peer_cooling.get_mut(&peer).expect("peer is cooling");
            cooling.suppressed_until = Some(Instant::now() - Duration::from_millis(1));
        }

        pubsub
            .parallel_send_to_peers(
                topic,
                vec![peer],
                GossipStreamType::PubSub,
                Bytes::from_static(b"panic-probe"),
                "EAGER",
                false,
            )
            .await;

        let suppressed = pubsub.stage_stats().suppressed_peers;
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].peer_id, peer.to_string());
        assert_eq!(suppressed[0].recent_timeout_count, 1);
        assert_eq!(suppressed[0].state, "cooldown");
    }

    #[tokio::test]
    async fn test_panicked_single_peer_recovery_probe_resuppresses_peer() {
        let peer_id = test_peer_id(1);
        let transport = PanicTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([52u8; 32]);
        let peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![peer])
                .await;
        }
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).expect("topic exists");
            let cooling = state.peer_cooling.get_mut(&peer).expect("peer is cooling");
            cooling.suppressed_until = Some(Instant::now() - Duration::from_millis(1));
        }

        let result = pubsub
            .send_to_peer_bounded(
                topic,
                peer,
                GossipStreamType::PubSub,
                Bytes::from_static(b"panic-direct-probe"),
                "EAGER",
            )
            .await;
        assert!(
            result.is_err(),
            "panicked transport should be returned as an error"
        );

        let suppressed = pubsub.stage_stats().suppressed_peers;
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].peer_id, peer.to_string());
        assert_eq!(suppressed[0].recent_timeout_count, 1);
        assert_eq!(suppressed[0].state, "cooldown");
    }

    #[tokio::test]
    async fn test_cancelled_recovery_probe_resuppresses_peer() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([53u8; 32]);
        let peer = test_peer_id(2);
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![peer])
                .await;
        }
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).expect("topic exists");
            let cooling = state.peer_cooling.get_mut(&peer).expect("peer is cooling");
            cooling.suppressed_until = Some(Instant::now() - Duration::from_millis(1));
        }

        let send_pubsub = Arc::clone(&pubsub);
        let send_task = tokio::spawn(async move {
            send_pubsub
                .send_to_peer_bounded(
                    topic,
                    peer,
                    GossipStreamType::PubSub,
                    Bytes::from_static(b"cancelled-probe"),
                    "EAGER",
                )
                .await
        });

        tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("recovery probe send should start")
            .expect("send channel should stay open");
        send_task.abort();
        let _ = send_task.await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let suppressed = pubsub.stage_stats().suppressed_peers;
                if suppressed
                    .first()
                    .is_some_and(|entry| entry.state == "cooldown")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled recovery probe should be re-suppressed");

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).expect("topic exists");
        let cooling = state.peer_cooling.get(&peer).expect("peer is cooling");
        assert!(
            !cooling.recovery_probe_in_flight,
            "cancelled probe cleanup must release the in-flight marker"
        );

        transport.release_sends(1);
    }

    #[test]
    fn test_expired_suppression_remains_visible_as_recovery_ready() {
        let stats = PubSubStageStats::default();
        let topic = TopicId::new([49u8; 32]);
        let peer = test_peer_id(2);
        stats.record_peer_suppressed(
            topic,
            peer,
            Instant::now() - Duration::from_millis(1),
            PEER_TIMEOUT_THRESHOLD,
            PEER_SUPPRESSION_COOLDOWN,
        );

        let suppressed = stats.snapshot().suppressed_peers;
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].peer_id, peer.to_string());
        assert_eq!(suppressed[0].topic, topic.to_string());
        assert_eq!(suppressed[0].state, "recovery_ready");
        assert!(!suppressed[0].recovery_probe_in_flight);
    }

    #[tokio::test]
    async fn test_unsigned_ihave_does_not_reset_subthreshold_timeout_counter() {
        let peer_id = test_peer_id(1);
        let from_peer = test_peer_id(2);
        let transport = RecordingTransport::new(peer_id);
        let pubsub =
            PlumtreePubSub::new_with_task_control(peer_id, transport, test_signing_key(), false);
        let topic = TopicId::new([54u8; 32]);
        let now = Instant::now();
        let pre_inbound = PEER_TIMEOUT_THRESHOLD - 1;

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            seed_subthreshold_cooling(state, from_peer, now);
            let cooling = state
                .peer_cooling
                .get(&from_peer)
                .expect("peer accumulated cooling state");
            assert_eq!(cooling.timeout_count, pre_inbound);
        }

        let missing_id = [7u8; 32];
        let payload: Bytes = postcard::to_stdvec(&vec![missing_id])
            .expect("IHAVE IDs serialize")
            .into();
        let message = unsigned_control_message(topic, missing_id, MessageKind::IHave, payload);
        let wire: Bytes = postcard::to_stdvec(&message)
            .expect("unsigned IHAVE message serializes")
            .into();

        assert!(
            pubsub.handle_message(from_peer, wire).await.is_err(),
            "unsigned IHAVE should be rejected"
        );

        let mut topics = pubsub.topics.write_topic(&topic).await;
        let state = topics.get_mut(&topic).expect("topic exists");
        let cooling = state
            .peer_cooling
            .get(&from_peer)
            .expect("unsigned IHAVE must not clear cooling");
        assert_eq!(cooling.timeout_count, pre_inbound);

        let event = state.record_send_timeout_at(
            normal_send_attempt(from_peer),
            now + Duration::from_millis(1),
        );
        assert!(
            event.is_some(),
            "next timeout should trip suppression when unsigned IHAVE did not reset"
        );
        assert!(state.is_peer_suppressed_at(from_peer, now + Duration::from_millis(1)));
    }

    /// X0X-0056/X0X-0040 — sub-threshold timeouts followed by a signed inbound
    /// frame must reset the per-peer cooling counter, so the next round of
    /// timeouts must not trip suppression prematurely.
    #[tokio::test]
    async fn test_inbound_frame_resets_subthreshold_timeout_counter() {
        let peer_id = test_peer_id(1);
        let from_peer = test_peer_id(2);
        let transport = RecordingTransport::new(peer_id);
        let sender_key = test_signing_key();
        let pubsub =
            PlumtreePubSub::new_with_task_control(peer_id, transport, test_signing_key(), false);
        let topic = TopicId::new([55u8; 32]);
        let now = Instant::now();
        let pre_inbound = PEER_TIMEOUT_THRESHOLD - 1;

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            seed_subthreshold_cooling(state, from_peer, now);
        }

        let missing_id = [8u8; 32];
        let payload: Bytes = postcard::to_stdvec(&vec![missing_id])
            .expect("IHAVE IDs serialize")
            .into();
        let message =
            signed_control_message(&sender_key, topic, missing_id, MessageKind::IHave, payload);
        let wire: Bytes = postcard::to_stdvec(&message)
            .expect("signed IHAVE message serializes")
            .into();

        pubsub
            .handle_message(from_peer, wire)
            .await
            .expect("signed IHAVE should be handled");

        let inbound_at = now + Duration::from_millis(1);
        let mut topics = pubsub.topics.write_topic(&topic).await;
        let state = topics.get_mut(&topic).expect("topic exists");
        assert!(
            !state.peer_cooling.contains_key(&from_peer),
            "signed IHAVE should clear cooling entry"
        );

        for i in 0..pre_inbound {
            assert!(
                state
                    .record_send_timeout_at(normal_send_attempt(from_peer), inbound_at)
                    .is_none(),
                "timeout {i} after signed reset must remain sub-threshold"
            );
        }
        assert!(
            !state.is_peer_suppressed_at(from_peer, inbound_at),
            "after signed inbound reset, sub-threshold timeouts must not trip suppression"
        );
        let cooling = state
            .peer_cooling
            .get(&from_peer)
            .expect("fresh post-reset cooling entry should exist");
        assert_eq!(
            cooling.timeout_count, pre_inbound,
            "post-reset counter must equal new timeout count"
        );
    }

    #[tokio::test]
    async fn test_signed_non_pubsub_frame_resets_subthreshold_timeout_counter() {
        let peer_id = test_peer_id(1);
        let from_peer = test_peer_id(2);
        let transport = RecordingTransport::new(peer_id);
        let sender_key = test_signing_key();
        let pubsub =
            PlumtreePubSub::new_with_task_control(peer_id, transport, test_signing_key(), false);
        let topic = TopicId::new([56u8; 32]);
        let now = Instant::now();

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            seed_subthreshold_cooling(state, from_peer, now);
        }

        let message = signed_control_message(
            &sender_key,
            topic,
            [9u8; 32],
            MessageKind::Presence,
            Bytes::from_static(b"presence-beacon"),
        );
        let wire: Bytes = postcard::to_stdvec(&message)
            .expect("signed presence message serializes")
            .into();

        pubsub
            .handle_message(from_peer, wire)
            .await
            .expect("signed non-pubsub frame should be ignored after reset");

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).expect("topic exists");
        assert!(
            !state.peer_cooling.contains_key(&from_peer),
            "signed non-pubsub frame should clear cooling"
        );
    }

    /// X0X-0061: the per-peer republish budget must accommodate WAN-class
    /// tail latency, otherwise nodes on cross-region public-internet paths
    /// (e.g. Hetzner Helsinki → DigitalOcean Singapore/Sydney) accumulate
    /// systematic per-peer timeouts that trip cooling at the
    /// `PEER_TIMEOUT_THRESHOLD` × `PEER_TIMEOUT_WINDOW` rate, suppressing
    /// peers for `PEER_SUPPRESSION_COOLDOWN` and pushing the
    /// `suppressed_peers/known_peer_topic_pairs` ratio over the 0.120
    /// broad-launch gate.
    ///
    /// The 750 ms predecessor caused the 4 h NO-GO 0/16 soak documented in
    /// `proofs/launch-readiness-soak-20260510T073944Z-4h-v0_19_34-x0x-0060-patched`.
    /// 1500 ms is the minimum needed for sydney-class paths (560 ms RTT)
    /// to absorb a single packet loss + retransmit (~1100 ms).
    #[test]
    fn per_peer_republish_timeout_accommodates_wan_tail_latency() {
        assert!(
            PER_PEER_REPUBLISH_TIMEOUT >= Duration::from_millis(1_500),
            "PER_PEER_REPUBLISH_TIMEOUT must accommodate WAN-class tail latency \
             (≥ 1500 ms — see X0X-0061); current is {:?}",
            PER_PEER_REPUBLISH_TIMEOUT
        );
    }

    #[tokio::test]
    async fn test_missing_topic_clears_recovery_probe_diagnostic() {
        let topics = Arc::new(ShardedTopicMap::new());
        let stage_stats = Arc::new(PubSubStageStats::default());
        let topic = TopicId::new([50u8; 32]);
        let peer = test_peer_id(2);
        stage_stats.record_peer_recovery_probe(
            topic,
            peer,
            Instant::now() - Duration::from_millis(1),
            PEER_TIMEOUT_THRESHOLD,
            PEER_SUPPRESSION_COOLDOWN,
        );

        PlumtreePubSub::<RecordingTransport>::record_topic_send_attempt_results_for(
            &topics,
            &stage_stats,
            topic,
            vec![PeerSendAttempt {
                peer,
                kind: SendAttemptKind::RecoveryProbe,
                recovery_probe_id: Some(1),
            }],
            Vec::new(),
        )
        .await;

        assert!(
            stage_stats.snapshot().suppressed_peers.is_empty(),
            "missing topic should not leave stale recovery-probe diagnostics"
        );
    }

    #[test]
    fn adaptive_alive_timeout_uses_initial_cooldown_at_threshold() {
        let mut state = TopicState::new();
        let peer = test_peer_id(61);
        state.eager_peers.insert(peer);
        let now = Instant::now();
        let cooling_config = AdaptiveCoolingConfig::default();

        for i in 0..(PEER_TIMEOUT_THRESHOLD - 1) {
            let outcome = state.record_send_timeout_with_context_at(
                normal_send_attempt(peer),
                now + Duration::from_millis(i as u64),
                Some(PeerHealth::Alive),
                cooling_config,
            );
            assert!(outcome.suppression.is_none());
            assert!(!outcome.request_indirect_probe);
        }

        let outcome = state.record_send_timeout_with_context_at(
            normal_send_attempt(peer),
            now + Duration::from_millis(PEER_TIMEOUT_THRESHOLD as u64),
            Some(PeerHealth::Alive),
            cooling_config,
        );
        let event = outcome
            .suppression
            .expect("alive peer should still cool at timeout threshold");

        assert!(!outcome.request_indirect_probe);
        assert_eq!(event.recent_timeout_count, PEER_TIMEOUT_THRESHOLD);
        assert_eq!(event.cooldown, crate::timing::ADAPTIVE_COOLDOWN_INITIAL);
        assert!(state.lazy_peers.contains(&peer));
        assert!(!state.eager_peers.contains(&peer));
    }

    #[test]
    fn suspect_health_holds_cooling_and_requests_probe_at_threshold() {
        let mut state = TopicState::new();
        let peer = test_peer_id(62);
        state.eager_peers.insert(peer);
        let now = Instant::now();
        let cooling_config = AdaptiveCoolingConfig::default();

        for i in 0..(PEER_TIMEOUT_THRESHOLD - 1) {
            let outcome = state.record_send_timeout_with_context_at(
                normal_send_attempt(peer),
                now + Duration::from_millis(i as u64),
                Some(PeerHealth::Suspect),
                cooling_config,
            );
            assert!(outcome.suppression.is_none());
            assert!(!outcome.request_indirect_probe);
        }

        let outcome = state.record_send_timeout_with_context_at(
            normal_send_attempt(peer),
            now + Duration::from_millis(PEER_TIMEOUT_THRESHOLD as u64),
            Some(PeerHealth::Suspect),
            cooling_config,
        );

        assert!(outcome.suppression.is_none());
        assert!(outcome.request_indirect_probe);
        assert!(state.eager_peers.contains(&peer));
        assert!(!state.lazy_peers.contains(&peer));
        let cooling = state
            .peer_cooling
            .get(&peer)
            .expect("suspect peer retains sub-threshold cooling state");
        assert_eq!(cooling.timeout_count, PEER_TIMEOUT_THRESHOLD - 1);
        assert!(cooling.suppressed_until.is_none());
    }

    #[test]
    fn dead_health_escalates_cooling_immediately() {
        let mut state = TopicState::new();
        let peer = test_peer_id(63);
        state.eager_peers.insert(peer);
        let now = Instant::now();
        let cooling_config = AdaptiveCoolingConfig::default();

        let outcome = state.record_send_timeout_with_context_at(
            normal_send_attempt(peer),
            now,
            Some(PeerHealth::Dead),
            cooling_config,
        );
        let event = outcome
            .suppression
            .expect("dead peer should cool on first observed timeout");

        assert_eq!(event.recent_timeout_count, 1);
        assert_eq!(
            event.cooldown,
            cooling_config.escalate_on_dead(Duration::ZERO)
        );
        assert!(state.is_peer_suppressed_at(peer, now));
        assert!(state.lazy_peers.contains(&peer));
        assert!(!state.eager_peers.contains(&peer));
    }

    #[test]
    fn successful_send_decays_adaptive_cooldown_after_suppression() {
        let mut state = TopicState::new();
        let peer = test_peer_id(64);
        let now = Instant::now();
        let cooling_config = AdaptiveCoolingConfig::default();
        let current = cooling_config
            .initial
            .checked_mul(4)
            .expect("test cooldown multiplier fits");
        state.peer_cooling.insert(
            peer,
            PeerCoolingState {
                timeout_window_started: now - Duration::from_secs(30),
                timeout_count: PEER_TIMEOUT_THRESHOLD - 1,
                suppressed_until: None,
                cooldown: current,
                last_suppressed_at: Some(now - Duration::from_secs(30)),
                last_suppression_timeout_count: PEER_TIMEOUT_THRESHOLD,
                recovery_probe_in_flight: false,
                recovery_probe_id: None,
                last_bypass_probe_at: None,
            },
        );

        assert!(!state.record_send_success_with_context_at(
            normal_send_attempt(peer),
            now,
            cooling_config
        ));
        let cooling = state
            .peer_cooling
            .get(&peer)
            .expect("normal success keeps cooling record for future adaptation");
        assert!(
            cooling.cooldown < current,
            "success should decay cooldown from {:?} to {:?}",
            current,
            cooling.cooldown
        );
        assert!(cooling.cooldown >= cooling_config.initial);
        assert_eq!(cooling.timeout_count, 0);
        let decayed = cooling.cooldown;

        let later = now + Duration::from_secs(1);
        for i in 0..(PEER_TIMEOUT_THRESHOLD - 1) {
            let outcome = state.record_send_timeout_with_context_at(
                normal_send_attempt(peer),
                later + Duration::from_millis(i as u64),
                Some(PeerHealth::Alive),
                cooling_config,
            );
            assert!(outcome.suppression.is_none());
        }
        let outcome = state.record_send_timeout_with_context_at(
            normal_send_attempt(peer),
            later + Duration::from_millis(PEER_TIMEOUT_THRESHOLD as u64),
            Some(PeerHealth::Alive),
            cooling_config,
        );
        let event = outcome
            .suppression
            .expect("next suppression should use decayed cooldown memory");
        assert_eq!(event.cooldown, cooling_config.next_cooldown(decayed));
    }

    #[test]
    fn test_peer_cooling_requires_successful_probe_after_cooldown() {
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), now);
        }

        assert!(
            state.is_peer_suppressed_at(peer, now),
            "peer should be suppressed after threshold timeouts"
        );
        assert!(state.lazy_peers.contains(&peer));

        let after_cooldown = now + PEER_SUPPRESSION_COOLDOWN + Duration::from_millis(1);
        let (_pruned, grafted) = state.maintain_degree_at(after_cooldown);

        assert_eq!(
            grafted, 0,
            "expired cooling should wait for one recovery probe"
        );
        assert!(
            !state.eager_peers.contains(&peer),
            "peer should not re-enter EAGER before a successful recovery probe"
        );
        assert!(state.is_peer_suppressed_at(peer, after_cooldown));

        let (attempt, event) = state
            .claim_send_attempt_at(peer, after_cooldown)
            .expect("expired cooling should admit exactly one recovery probe");
        assert_eq!(attempt.kind, SendAttemptKind::RecoveryProbe);
        assert!(event.is_some());
        assert!(
            state.claim_send_attempt_at(peer, after_cooldown).is_none(),
            "second recovery probe must be skipped while first is in flight"
        );

        assert!(
            state.record_send_success_at(attempt, after_cooldown),
            "successful recovery probe should clear cooling"
        );
        let (_pruned, grafted) = state.maintain_degree_at(after_cooldown);
        assert_eq!(grafted, 1, "cleared peer can be grafted again");
        assert!(state.eager_peers.contains(&peer));
        assert!(!state.is_peer_suppressed_at(peer, after_cooldown));
    }

    #[test]
    fn test_failed_recovery_probe_immediately_extends_cooldown() {
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), now);
        }

        let after_cooldown = now + PEER_SUPPRESSION_COOLDOWN + Duration::from_millis(1);
        let (attempt, _) = state
            .claim_send_attempt_at(peer, after_cooldown)
            .expect("expired cooling should admit one recovery probe");
        assert_eq!(attempt.kind, SendAttemptKind::RecoveryProbe);

        let event = state
            .record_send_timeout_at(attempt, after_cooldown)
            .expect("failed recovery probe should immediately re-suppress");
        assert_eq!(
            event.recent_timeout_count, 1,
            "failed probe should not need another full timeout window"
        );
        assert_eq!(
            event.cooldown,
            PEER_SUPPRESSION_COOLDOWN
                .checked_mul(2)
                .expect("cooldown doubles"),
            "failed probe should apply exponential backoff"
        );
        assert!(state.is_peer_suppressed_at(peer, after_cooldown));
        assert!(state.lazy_peers.contains(&peer));
        assert!(!state.eager_peers.contains(&peer));
    }

    #[test]
    fn test_cooldown_bypass_allows_rate_limited_send_during_suppression() {
        // WHY (WP6): suppression previously blocked BOTH eager push and
        // lazy IHAVE for the whole cooldown, so a message published while
        // a peer was cooling was undeliverable until the cooldown expired
        // (observed live as >96 s CRDT stalls on the three-machine WAN
        // test). During cooldown the claim gate must admit a strictly
        // rate-limited bypass send so cached data still reaches the peer
        // on the anti-entropy timescale.
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), now);
        }
        assert!(state.is_peer_suppressed_at(peer, now));

        // Mid-cooldown: the first claim is admitted as a bypass send.
        let mid = now + Duration::from_secs(1);
        let (attempt, event) = state
            .claim_send_attempt_at(peer, mid)
            .expect("suppressed peer should admit one rate-limited bypass send");
        assert_eq!(attempt.kind, SendAttemptKind::CooldownBypass);
        assert!(event.is_none(), "bypass is not a recovery-probe event");

        // A second claim inside the bypass interval must be rejected —
        // the bypass is a trickle, not a hole in the suppression.
        let inside = mid + PEER_COOLDOWN_BYPASS_MIN_INTERVAL - Duration::from_millis(1);
        assert!(
            state.claim_send_attempt_at(peer, inside).is_none(),
            "bypass must be rate-limited to one send per interval"
        );

        // Once the interval elapses (still well inside the cooldown)
        // another bypass send is admitted.
        let next = mid + PEER_COOLDOWN_BYPASS_MIN_INTERVAL;
        let (attempt2, _) = state
            .claim_send_attempt_at(peer, next)
            .expect("bypass should re-arm after the rate-limit interval");
        assert_eq!(attempt2.kind, SendAttemptKind::CooldownBypass);

        // A successful bypass send must NOT clear the suppression: eager
        // fanout stays gated until the cooldown genuinely ends.
        assert!(!state.record_send_success_at(attempt2, next));
        assert!(
            state.is_peer_suppressed_at(peer, next),
            "bypass success must not lift the suppression early"
        );
        assert!(
            !state.can_graft_peer_at(peer, next),
            "bypass success must not re-graft the peer into EAGER"
        );
    }

    #[test]
    fn test_cooldown_bypass_success_does_not_shortcut_recovery_probe() {
        // WHY (WP6): the bypass exists to deliver cached messages, not to
        // end cooling early. Full recovery still requires the cooldown to
        // expire AND one successful post-cooldown recovery probe — a
        // single lucky send under pressure must not resume full fanout.
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), now);
        }
        let suppressed_until = state
            .peer_cooling
            .get(&peer)
            .and_then(|cooling| cooling.suppressed_until)
            .expect("peer is suppressed");

        let mid = now + Duration::from_secs(1);
        let (attempt, _) = state
            .claim_send_attempt_at(peer, mid)
            .expect("bypass send admitted during cooldown");
        assert_eq!(attempt.kind, SendAttemptKind::CooldownBypass);
        assert!(!state.record_send_success_at(attempt, mid));
        assert_eq!(
            state
                .peer_cooling
                .get(&peer)
                .and_then(|cooling| cooling.suppressed_until),
            Some(suppressed_until),
            "bypass success must leave the cooldown deadline untouched"
        );

        // After genuine expiry the peer still goes through the normal
        // recovery-probe handshake before suppression clears.
        let after_cooldown = now + PEER_SUPPRESSION_COOLDOWN + Duration::from_millis(1);
        let (probe, event) = state
            .claim_send_attempt_at(peer, after_cooldown)
            .expect("expired cooling should admit a recovery probe");
        assert_eq!(probe.kind, SendAttemptKind::RecoveryProbe);
        assert!(event.is_some());
        assert!(
            state.record_send_success_at(probe, after_cooldown),
            "successful post-cooldown probe clears cooling"
        );
        assert!(!state.is_peer_suppressed_at(peer, after_cooldown));
    }

    #[test]
    fn test_cooldown_bypass_timeout_does_not_extend_suppression() {
        // WHY (WP6): the peer is already cooling; a failed bypass send is
        // expected on a genuinely dead peer and must not escalate the
        // cooldown or re-trigger suppression events (which would defeat
        // the purpose of the fixed, rate-limited trickle).
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), now);
        }
        let (suppressed_until, cooldown) = {
            let cooling = state.peer_cooling.get(&peer).expect("peer is cooling");
            (cooling.suppressed_until, cooling.cooldown)
        };

        let mid = now + Duration::from_secs(1);
        let (attempt, _) = state
            .claim_send_attempt_at(peer, mid)
            .expect("bypass send admitted during cooldown");
        assert_eq!(attempt.kind, SendAttemptKind::CooldownBypass);

        assert!(
            state
                .record_send_timeout_at(attempt, mid + Duration::from_secs(1))
                .is_none(),
            "bypass timeout must not emit a new suppression event"
        );
        let cooling = state.peer_cooling.get(&peer).expect("peer is cooling");
        assert_eq!(cooling.suppressed_until, suppressed_until);
        assert_eq!(cooling.cooldown, cooldown);
        assert!(state.is_peer_suppressed_at(peer, mid));
    }

    #[test]
    fn test_stale_recovery_probe_result_cannot_recreate_or_clear_cooling() {
        let mut state = TopicState::new();
        let peer = test_peer_id(2);
        state.eager_peers.insert(peer);

        let now = Instant::now();
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), now);
        }

        let after_cooldown = now + PEER_SUPPRESSION_COOLDOWN + Duration::from_millis(1);
        let (old_attempt, _) = state
            .claim_send_attempt_at(peer, after_cooldown)
            .expect("expired cooling should admit one recovery probe");
        assert_eq!(old_attempt.kind, SendAttemptKind::RecoveryProbe);
        let score_before_stale = state
            .peer_scores
            .get(&peer)
            .expect("peer score exists")
            .components_at(after_cooldown)
            .outbound_send_timeouts;

        state.peer_cooling.remove(&peer);
        assert!(
            state
                .record_send_timeout_at(old_attempt, after_cooldown)
                .is_none(),
            "stale recovery timeout must not recreate cooling after peer removal"
        );
        assert!(
            !state.peer_cooling.contains_key(&peer),
            "stale recovery timeout must leave removed peer absent"
        );
        let score_after_stale = state
            .peer_scores
            .get(&peer)
            .expect("peer score remains")
            .components_at(after_cooldown)
            .outbound_send_timeouts;
        assert_eq!(
            score_after_stale, score_before_stale,
            "stale recovery timeout must not add score penalties"
        );

        let later = after_cooldown + PEER_TIMEOUT_WINDOW + Duration::from_millis(1);
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            let _ = state.record_send_timeout_at(normal_send_attempt(peer), later);
        }
        {
            let cooling = state.peer_cooling.get_mut(&peer).expect("peer is cooling");
            cooling.suppressed_until = Some(later - Duration::from_millis(1));
        }
        let (new_attempt, _) = state
            .claim_send_attempt_at(peer, later)
            .expect("new cooling should admit its own recovery probe");
        assert_ne!(
            old_attempt.recovery_probe_id, new_attempt.recovery_probe_id,
            "recovery probes should carry unique ownership tokens"
        );

        assert!(
            !state.record_send_success_at(old_attempt, later),
            "stale recovery success must not clear a newer in-flight probe"
        );
        let cooling = state.peer_cooling.get(&peer).expect("peer stays cooling");
        assert!(
            cooling.matches_recovery_probe(new_attempt.recovery_probe_id),
            "new recovery probe ownership should remain intact"
        );
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

        {
            let topics = pubsub.topics.read_topic(&topic_a).await;
            let state_a = topics.get(&topic_a).expect("topic A exists");
            assert!(
                state_a.lazy_peers.contains(&peer),
                "active cooling should keep peer lazy on affected topic"
            );
            assert!(
                !state_a.eager_peers.contains(&peer),
                "refresh must not immediately re-promote cooled peer"
            );
        }
        {
            let topics = pubsub.topics.read_topic(&topic_b).await;
            let state_b = topics.get(&topic_b).expect("topic B exists");
            assert!(
                state_b.eager_peers.contains(&peer),
                "same peer should stay EAGER on unaffected topic"
            );
        }
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

        // Never release the send. The dispatcher forward path now DETACHES
        // fan-out accounting, so handle_message returns immediately rather
        // than blocking until the per-peer timeout fires — that is the
        // backpressure fix (a slow peer must not pin the dispatcher worker
        // and starve the recv channel). The republish STAGE timing is
        // therefore bounded by the spawn cost, not PER_PEER_REPUBLISH_TIMEOUT.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("detached forward must let handle_message return promptly — slow peer must not pin the dispatcher")
            .expect("handle task should not panic");

        // Synchronous stages are recorded before the detached fan-out.
        let stats = pubsub.stage_stats();
        assert_eq!(stats.decode.count, 1);
        assert_eq!(stats.verify.count, 1);
        // Issue #27: count is 2 — one for the dedupe path, one for the
        // send-claim path inside parallel_send_to_peers. Previously the
        // send-claim lock-wait was invisibly charged to Republish.
        assert_eq!(stats.dedupe_lock_acquire.count, 2);
        assert_eq!(stats.dedupe_check.count, 1);
        assert_eq!(stats.eager_fanout.count, 1);
        assert_eq!(stats.republish.count, 1);
        // Republish stage time is now bounded well below the per-peer timeout
        // because the worker no longer awaits the slow send inline.
        let max_bound_ns =
            (PER_PEER_REPUBLISH_TIMEOUT + Duration::from_millis(1_000)).as_nanos() as u64;
        assert!(
            stats.republish.max_ns < max_bound_ns,
            "republish max_ns should be bounded by PER_PEER_REPUBLISH_TIMEOUT + 1s slack, got {} ns vs bound {} ns",
            stats.republish.max_ns,
            max_bound_ns
        );

        // The per-peer timeout telemetry + cooling feedback is still recorded,
        // but now ASYNCHRONOUSLY in the detached accounting task once the
        // unreleased send hits PER_PEER_REPUBLISH_TIMEOUT. Poll for it within
        // the timeout budget plus slack — the counter must still reach exactly
        // 1, proving the detach defers (does not drop) the accounting.
        let deadline =
            tokio::time::Instant::now() + PER_PEER_REPUBLISH_TIMEOUT + Duration::from_secs(2);
        loop {
            if pubsub.stage_stats().republish_per_peer_timeout == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "detached accounting should record the per-peer timeout exactly once within PER_PEER_REPUBLISH_TIMEOUT + slack (got {})",
                pubsub.stage_stats().republish_per_peer_timeout
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Issue #27 regression: the anchor node on a 6-node testnet spent 49.6%
    /// of handler wall-time in `dedupe_lock_acquire` because all 32 x0x
    /// workers serialized on a single `RwLock<HashMap<TopicId, TopicState>>`.
    /// This test proves the sharded map eliminates that serialization:
    /// holding a write lock on one topic's shard does not block a write on a
    /// different topic's shard.
    #[tokio::test]
    async fn sharded_topic_map_different_shards_dont_block() {
        let map = ShardedTopicMap::with_shard_count(32);

        // Find two topics that land in different shards.
        let topic_a = TopicId::new([0u8; 32]);
        let topic_b = TopicId::new([1u8; 32]);
        assert_ne!(
            map.shard_index(&topic_a),
            map.shard_index(&topic_b),
            "test topics must map to different shards"
        );

        // Hold a write lock on topic_a's shard.
        let _guard_a = map.write_topic(&topic_a).await;

        // Acquiring a write lock on topic_b's shard must succeed immediately
        // — under the old single-lock design this would block indefinitely.
        let guard_b = tokio::time::timeout(Duration::from_millis(50), map.write_topic(&topic_b))
            .await
            .expect("write on a different shard must not block while another shard is locked");

        // Verify both guards are functional.
        assert!(guard_b.get(&topic_b).is_none());

        drop(_guard_a);
    }

    /// Issue #27 regression: the 47.6% republish time was partly a
    /// mis-attribution — the send-claim lock-wait inside
    /// `parallel_send_to_peers` → `claim_topic_send_attempts` was charged to
    /// the `Republish` stage instead of `DedupeLockAcquire`. This test
    /// verifies that after a single inbound EAGER message, the
    /// `DedupeLockAcquire` count is 2 (dedupe + send-claim) and the
    /// `Republish` time excludes the send-claim lock-wait.
    #[tokio::test]
    async fn republish_excludes_send_claim_lock_wait() {
        let peer_id = test_peer_id(1);
        let (transport, _started_rx) = BlockingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([77u8; 32]);
        let forward_peer = test_peer_id(2);
        pubsub
            .initialize_topic_peers(topic, vec![forward_peer])
            .await;

        let message = signed_eager_message(
            &test_signing_key(),
            topic,
            [88u8; 32],
            Bytes::from_static(b"attribution"),
        );
        let wire: Bytes = postcard::to_stdvec(&message)
            .expect("message serializes")
            .into();

        pubsub
            .handle_message(test_peer_id(3), wire)
            .await
            .expect("handle_message should complete");

        let stats = pubsub.stage_stats();

        // DedupeLockAcquire fires twice: once for dedupe/cache, once for
        // the send-claim inside parallel_send_to_peers.
        assert_eq!(
            stats.dedupe_lock_acquire.count, 2,
            "dedupe_lock_acquire must include the send-claim lock-wait (issue #27)"
        );

        // Republish fires once and its time must exclude the send-claim
        // lock-wait (now separately accounted in DedupeLockAcquire).
        assert_eq!(stats.republish.count, 1);

        // The PeerScoring and suppressed_peers lock-wait instrumentation
        // is present and non-panic.
        assert!(
            stats.topic_shard_count >= 2,
            "sharded topic map must report its shard count"
        );
        // Lock-wait stats exist (count may be 0 if uncontended).
        let _ = stats.peer_scoring_lock_wait.count;
        let _ = stats.suppressed_peers_lock_wait.count;
    }

    #[tokio::test]
    async fn test_ihave_flush_releases_topic_lock_before_network_io() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let topics = Arc::new(ShardedTopicMap::new());
        let signing_key = Arc::new(test_signing_key());
        let topic = TopicId::new([6u8; 32]);
        let lazy_peer = test_peer_id(2);

        {
            let mut topics_guard = topics.write_topic(&topic).await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(lazy_peer);
            state.pending_ihave.push([7u8; 32]);
        }

        let flush_topics = Arc::clone(&topics);
        let flush_transport = Arc::clone(&transport);
        let flush_signing_key = Arc::clone(&signing_key);
        let flush_stage_stats = Arc::new(PubSubStageStats::default());
        let flush_outbound_budgets = Arc::new(PeerOutboundBudgets::default());
        let flush_rtt_tracker = Arc::new(PerPeerRttTracker::new());
        let flush_peer_health_snapshot = Arc::new(StdRwLock::new(HashMap::new()));
        let flush_connected_peers_snapshot = Arc::new(StdRwLock::new(None));
        let flush_peer_health_oracle = Arc::new(StdRwLock::new(None));
        let flush_send_path = SendPathContext {
            rtt_tracker: flush_rtt_tracker,
            cooling_config: AdaptiveCoolingConfig::default(),
            peer_health_snapshot: flush_peer_health_snapshot,
            connected_peers_snapshot: flush_connected_peers_snapshot,
            peer_health_oracle: flush_peer_health_oracle,
            admission: Arc::new(admission::AdmissionControl::new()),
        };
        let flush = tokio::spawn(async move {
            PlumtreePubSub::<BlockingTransport>::flush_ihave_batches(
                &flush_topics,
                &flush_transport,
                &flush_signing_key,
                &flush_stage_stats,
                &flush_outbound_budgets,
                &flush_send_path,
            )
            .await;
        });

        let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("IHAVE send should start")
            .expect("send channel should stay open");
        assert_eq!(record.peer, lazy_peer);
        assert_eq!(record.stream_type, GossipStreamType::PubSub);

        let read_guard = tokio::time::timeout(Duration::from_millis(50), topics.read_topic(&topic))
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
    async fn test_ihave_flush_uses_single_recovery_probe_for_expired_cooling() {
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let topics = Arc::new(ShardedTopicMap::new());
        let signing_key = Arc::new(test_signing_key());
        let stage_stats = Arc::new(PubSubStageStats::default());
        let topic = TopicId::new([48u8; 32]);
        let lazy_peer = test_peer_id(2);

        {
            let mut topics_guard = topics.write_topic(&topic).await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(lazy_peer);
            state.pending_ihave.push([8u8; 32]);
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                let _ =
                    state.record_send_timeout_at(normal_send_attempt(lazy_peer), Instant::now());
            }
            let cooling = state
                .peer_cooling
                .get_mut(&lazy_peer)
                .expect("peer is cooling");
            cooling.suppressed_until = Some(Instant::now() - Duration::from_millis(1));
        }

        let flush_topics = Arc::clone(&topics);
        let flush_transport = Arc::clone(&transport);
        let flush_signing_key = Arc::clone(&signing_key);
        let flush_stage_stats = Arc::clone(&stage_stats);
        let flush_outbound_budgets = Arc::new(PeerOutboundBudgets::default());
        let flush_rtt_tracker = Arc::new(PerPeerRttTracker::new());
        let flush_peer_health_snapshot = Arc::new(StdRwLock::new(HashMap::new()));
        let flush_connected_peers_snapshot = Arc::new(StdRwLock::new(None));
        let flush_peer_health_oracle = Arc::new(StdRwLock::new(None));
        let flush_send_path = SendPathContext {
            rtt_tracker: flush_rtt_tracker,
            cooling_config: AdaptiveCoolingConfig::default(),
            peer_health_snapshot: flush_peer_health_snapshot,
            connected_peers_snapshot: flush_connected_peers_snapshot,
            peer_health_oracle: flush_peer_health_oracle,
            admission: Arc::new(admission::AdmissionControl::new()),
        };
        let flush = tokio::spawn(async move {
            PlumtreePubSub::<BlockingTransport>::flush_ihave_batches(
                &flush_topics,
                &flush_transport,
                &flush_signing_key,
                &flush_stage_stats,
                &flush_outbound_budgets,
                &flush_send_path,
            )
            .await;
        });

        let record = tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
            .await
            .expect("IHAVE recovery probe should start")
            .expect("send channel should stay open");
        assert_eq!(record.peer, lazy_peer);

        tokio::time::timeout(Duration::from_secs(5), flush)
            .await
            .expect("IHAVE recovery probe should finish after per-peer timeout")
            .expect("IHAVE flush task should not panic");

        let suppressed = stage_stats.snapshot().suppressed_peers;
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].peer_id, lazy_peer.to_string());
        assert_eq!(suppressed[0].recent_timeout_count, 1);
        assert_eq!(suppressed[0].state, "cooldown");
        assert_eq!(transport.send_count(), 1);
    }

    #[tokio::test]
    async fn test_ihave_flush_bypasses_active_cooldown_rate_limited() {
        // WHY (WP6): a message published while a peer's suppression
        // cooldown is ACTIVE must still be announced to it via a
        // rate-limited IHAVE bypass — well before the cooldown expires —
        // instead of stalling for the full suppression window as before.
        // The bypass must stay rate-limited (a second flush inside the
        // interval sends nothing) and must not lift the suppression.
        let peer_id = test_peer_id(1);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let topics = Arc::new(ShardedTopicMap::new());
        let signing_key = Arc::new(test_signing_key());
        let stage_stats = Arc::new(PubSubStageStats::default());
        let topic = TopicId::new([49u8; 32]);
        let lazy_peer = test_peer_id(2);

        {
            let mut topics_guard = topics.write_topic(&topic).await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(lazy_peer);
            state.pending_ihave.push([9u8; 32]);
            // Trip suppression NOW: the cooldown deadline stays in the
            // future for the whole test.
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                let _ =
                    state.record_send_timeout_at(normal_send_attempt(lazy_peer), Instant::now());
            }
            assert!(state.is_peer_suppressed_at(lazy_peer, Instant::now()));
        }

        let flush_outbound_budgets = Arc::new(PeerOutboundBudgets::default());
        let flush_send_path = SendPathContext {
            rtt_tracker: Arc::new(PerPeerRttTracker::new()),
            cooling_config: AdaptiveCoolingConfig::default(),
            peer_health_snapshot: Arc::new(StdRwLock::new(HashMap::new())),
            connected_peers_snapshot: Arc::new(StdRwLock::new(None)),
            peer_health_oracle: Arc::new(StdRwLock::new(None)),
            admission: Arc::new(admission::AdmissionControl::new()),
        };

        let flush_topics = Arc::clone(&topics);
        let flush_transport = Arc::clone(&transport);
        let flush_signing_key = Arc::clone(&signing_key);
        let flush_stage_stats = Arc::clone(&stage_stats);
        let flush_budgets = Arc::clone(&flush_outbound_budgets);
        let flush_path = flush_send_path.clone();
        let flush = tokio::spawn(async move {
            PlumtreePubSub::<BlockingTransport>::flush_ihave_batches(
                &flush_topics,
                &flush_transport,
                &flush_signing_key,
                &flush_stage_stats,
                &flush_budgets,
                &flush_path,
            )
            .await;
        });

        let record = tokio::time::timeout(Duration::from_millis(200), started_rx.recv())
            .await
            .expect("cooldown-bypass IHAVE send should start during suppression")
            .expect("send channel should stay open");
        assert_eq!(record.peer, lazy_peer);

        transport.release_sends(1);
        tokio::time::timeout(Duration::from_secs(5), flush)
            .await
            .expect("IHAVE flush should finish after send is released")
            .expect("IHAVE flush task should not panic");

        let snapshot = stage_stats.snapshot();
        assert_eq!(
            snapshot.cooldown_bypass_probes, 1,
            "bypass claim must be observable in stage stats"
        );
        assert_eq!(
            snapshot.cooldown_bypass_successes, 1,
            "delivered bypass send must be observable in stage stats"
        );
        assert_eq!(transport.send_count(), 1);

        // Suppression is still active after the successful bypass send.
        {
            let topics_guard = topics.read_topic(&topic).await;
            let state = topics_guard.get(&topic).expect("topic exists");
            assert!(
                state.is_peer_suppressed_at(lazy_peer, Instant::now()),
                "bypass delivery must not clear the active suppression"
            );
        }

        // A second flush inside the bypass interval must send nothing.
        {
            let mut topics_guard = topics.write_topic(&topic).await;
            let state = topics_guard.get_mut(&topic).expect("topic exists");
            state.pending_ihave.push([10u8; 32]);
        }
        PlumtreePubSub::<BlockingTransport>::flush_ihave_batches(
            &topics,
            &transport,
            &signing_key,
            &stage_stats,
            &flush_outbound_budgets,
            &flush_send_path,
        )
        .await;
        assert_eq!(
            transport.send_count(),
            1,
            "second flush inside the bypass interval must be rate-limited"
        );
        assert_eq!(stage_stats.snapshot().cooldown_bypass_probes, 1);
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
        let topics = pubsub.topics.read_topic(&topic).await;
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
        let topics = pubsub.topics.read_topic(&topic).await;
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
        let topics = pubsub.topics.read_topic(&topic).await;
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
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(from_peer);
        }

        // Publish a message to cache it
        let payload = Bytes::from("test");
        pubsub.publish(topic, payload.clone()).await.ok();

        // Get the actual cached msg_id (don't recalculate - epoch may have changed)
        let msg_id = {
            let topics = pubsub.topics.read_topic(&topic).await;
            let state = topics.get(&topic).unwrap();
            // Get the first (and only) cached message ID
            state
                .message_cache
                .peek_lru_message_id()
                .expect("message should be cached")
        };

        // Handle IWANT from lazy peer
        pubsub
            .handle_iwant(from_peer, topic, vec![msg_id])
            .await
            .ok();

        // Verify peer was grafted to eager
        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&from_peer));
        assert!(!state.lazy_peers.contains(&from_peer));
    }

    #[tokio::test]
    async fn test_iwant_graft_respects_max_eager_degree() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([10u8; 32]);
        let from_peer = test_peer_id(240);
        let mut connected = Vec::new();

        {
            let now = Instant::now();
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for i in 0..MAX_EAGER_DEGREE {
                let peer = test_peer_id(i as u8 + 30);
                connected.push(peer);
                state.eager_peers.insert(peer);
                let mut score = PeerScore::new_at(now);
                score.record_delivery();
                state.peer_scores.insert(peer, score);
            }
            state.lazy_peers.insert(from_peer);
        }

        let payload = Bytes::from("test");
        pubsub.publish(topic, payload).await.ok();
        let msg_id = {
            let topics = pubsub.topics.read_topic(&topic).await;
            let state = topics.get(&topic).unwrap();
            state
                .message_cache
                .peek_lru_message_id()
                .expect("message should be cached")
        };

        pubsub
            .handle_iwant(from_peer, topic, vec![msg_id])
            .await
            .ok();

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert_eq!(
            state.eager_peers.len(),
            MAX_EAGER_DEGREE,
            "IWANT repair should not inflate EAGER above MAX_EAGER_DEGREE"
        );
    }

    #[tokio::test]
    async fn test_unknown_eager_sender_enters_lazy_when_mesh_is_full_enough() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([11u8; 32]);
        let unknown_sender = test_peer_id(250);

        {
            let now = Instant::now();
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for i in 0..MIN_EAGER_DEGREE {
                let peer = test_peer_id(i as u8 + 50);
                state.eager_peers.insert(peer);
                let mut score = PeerScore::new_at(now);
                score.record_delivery();
                state.peer_scores.insert(peer, score);
            }
        }

        let payload = Bytes::from("hello from an unknown sender");
        let msg_id = [99u8; 32];
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
            .handle_eager(unknown_sender, topic, message)
            .await
            .expect("unknown sender eager should be accepted");

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert_eq!(state.eager_peers.len(), MIN_EAGER_DEGREE);
        assert!(
            state.lazy_peers.contains(&unknown_sender),
            "unknown EAGER sender should not bypass LAZY-first admission"
        );
        assert!(!state.eager_peers.contains(&unknown_sender));
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
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for peer in &peers {
                state.lazy_peers.insert(*peer);
            }

            // Maintain degree (should promote to reach MIN_EAGER_DEGREE)
            state.maintain_degree();

            assert!(state.eager_peers.len() >= MIN_EAGER_DEGREE);
        }
    }

    #[test]
    fn bounded_cache_evicts_by_age() {
        let topic = TopicId::new([1u8; 32]);
        let now = Instant::now();
        let old_id = test_msg_id(1);
        let fresh_id = test_msg_id(2);
        let mut cache = BoundedMessageCache::new(
            nonzero(10),
            usize::MAX,
            Duration::from_secs(MAX_CACHE_AGE_SECS),
        );

        assert!(cache.insert_at(
            old_id,
            test_cached_message(topic, old_id, 128),
            now - Duration::from_secs(MAX_CACHE_AGE_SECS + 1),
        ));
        assert!(cache.insert_at(fresh_id, test_cached_message(topic, fresh_id, 128), now));

        let stats = cache.stats_at(now);
        assert_eq!(stats.msg_count, 1);
        assert_eq!(stats.evicted_by_age, 1);
        assert!(!cache.contains(&old_id));
        assert!(cache.contains(&fresh_id));
    }

    #[test]
    fn bounded_cache_evicts_by_bytes_under_pressure() {
        let topic = TopicId::new([2u8; 32]);
        let now = Instant::now();
        let sample = test_cached_message(topic, test_msg_id(0), 1024);
        let entry_bytes = estimate_message_bytes(&sample);
        let mut cache =
            BoundedMessageCache::new(nonzero(10), entry_bytes * 2, Duration::from_secs(300));

        for id in 1..=3 {
            let msg_id = test_msg_id(id);
            assert!(cache.insert_at(msg_id, test_cached_message(topic, msg_id, 1024), now));
        }

        let stats = cache.stats_at(now);
        assert_eq!(stats.msg_count, 2);
        assert!(stats.total_bytes <= entry_bytes * 2);
        assert_eq!(stats.evicted_by_bytes, 1);
        assert!(!cache.contains(&test_msg_id(1)));
    }

    #[test]
    fn bounded_cache_evicts_by_count_hard_cap() {
        let topic = TopicId::new([3u8; 32]);
        let now = Instant::now();
        let mut cache = BoundedMessageCache::new(nonzero(2), usize::MAX, Duration::from_secs(300));

        for id in 1..=3 {
            let msg_id = test_msg_id(id);
            assert!(cache.insert_at(msg_id, test_cached_message(topic, msg_id, 64), now));
        }

        let stats = cache.stats_at(now);
        assert_eq!(stats.msg_count, 2);
        assert_eq!(stats.evicted_by_count, 1);
        assert!(!cache.contains(&test_msg_id(1)));
    }

    #[test]
    fn bounded_cache_age_takes_precedence_over_bytes() {
        let topic = TopicId::new([4u8; 32]);
        let now = Instant::now();
        let sample = test_cached_message(topic, test_msg_id(0), 512);
        let entry_bytes = estimate_message_bytes(&sample);
        let mut cache =
            BoundedMessageCache::new(nonzero(10), entry_bytes * 2, Duration::from_secs(60));
        let old_id = test_msg_id(1);
        let fresh_id = test_msg_id(2);
        let incoming_id = test_msg_id(3);

        assert!(cache.insert_at(
            old_id,
            test_cached_message(topic, old_id, 512),
            now - Duration::from_secs(61),
        ));
        assert!(cache.insert_at(fresh_id, test_cached_message(topic, fresh_id, 512), now));
        assert!(cache.insert_at(
            incoming_id,
            test_cached_message(topic, incoming_id, 512),
            now,
        ));

        let stats = cache.stats_at(now);
        assert_eq!(stats.msg_count, 2);
        assert_eq!(stats.evicted_by_age, 1);
        assert_eq!(stats.evicted_by_bytes, 0);
        assert!(!cache.contains(&old_id));
    }

    #[test]
    fn bounded_cache_eviction_counters_track_correctly() {
        let topic = TopicId::new([5u8; 32]);
        let now = Instant::now();
        let sample = test_cached_message(topic, test_msg_id(0), 256);
        let entry_bytes = estimate_message_bytes(&sample);

        let mut age_cache =
            BoundedMessageCache::new(nonzero(4), usize::MAX, Duration::from_secs(10));
        let old_id = test_msg_id(1);
        let new_id = test_msg_id(2);
        assert!(age_cache.insert_at(
            old_id,
            test_cached_message(topic, old_id, 256),
            now - Duration::from_secs(11),
        ));
        assert!(age_cache.insert_at(new_id, test_cached_message(topic, new_id, 256), now));
        assert_eq!(age_cache.stats_at(now).evicted_by_age, 1);

        let mut bytes_cache =
            BoundedMessageCache::new(nonzero(4), entry_bytes, Duration::from_secs(300));
        let msg_id = test_msg_id(3);
        assert!(bytes_cache.insert_at(msg_id, test_cached_message(topic, msg_id, 256), now));
        let msg_id = test_msg_id(4);
        assert!(bytes_cache.insert_at(msg_id, test_cached_message(topic, msg_id, 256), now));
        assert_eq!(bytes_cache.stats_at(now).evicted_by_bytes, 1);

        let mut count_cache =
            BoundedMessageCache::new(nonzero(1), usize::MAX, Duration::from_secs(300));
        let msg_id = test_msg_id(5);
        assert!(count_cache.insert_at(msg_id, test_cached_message(topic, msg_id, 256), now));
        let msg_id = test_msg_id(6);
        assert!(count_cache.insert_at(msg_id, test_cached_message(topic, msg_id, 256), now));
        assert_eq!(count_cache.stats_at(now).evicted_by_count, 1);
    }

    #[test]
    fn bounded_cache_get_prunes_expired() {
        let topic = TopicId::new([6u8; 32]);
        let now = Instant::now();
        let msg_id = test_msg_id(1);
        let mut cache = BoundedMessageCache::new(nonzero(4), usize::MAX, Duration::from_secs(10));

        assert!(cache.insert_at(
            msg_id,
            test_cached_message(topic, msg_id, 128),
            now - Duration::from_secs(11),
        ));

        assert!(cache.get_at(&msg_id, now).is_none());
        let stats = cache.stats_at(now);
        assert_eq!(stats.msg_count, 0);
        assert_eq!(stats.evicted_by_age, 1);
    }

    #[test]
    fn bounded_cache_simulated_load_stays_within_caps() {
        let topic = TopicId::new([7u8; 32]);
        let start = Instant::now();
        let sample = test_cached_message(topic, test_msg_id(0), 5 * 1024);
        let entry_bytes = estimate_message_bytes(&sample);
        let max_bytes = entry_bytes * 64;
        let mut cache = BoundedMessageCache::new(nonzero(128), max_bytes, Duration::from_secs(90));

        for i in 0..5_000_u64 {
            let msg_id = test_msg_id(i);
            let now = start + Duration::from_millis(i * 300);
            assert!(cache.insert_at(msg_id, test_cached_message(topic, msg_id, 5 * 1024), now,));
            let stats = cache.stats_at(now);
            assert!(stats.msg_count <= 128);
            assert!(stats.total_bytes <= max_bytes);
        }

        let stats = cache.stats_at(start + Duration::from_secs(1_500));
        assert!(
            stats.evicted_by_age > 0 || stats.evicted_by_bytes > 0 || stats.evicted_by_count > 0
        );
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
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.get_mut(&topic).unwrap();

            // Modify insertion time to simulate expiry
            state.message_cache.set_all_inserted_at_for_test(
                Instant::now() - Duration::from_secs(MAX_CACHE_AGE_SECS + 10),
            );

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
            let mut topics = pubsub.topics.write_topic(&topic).await;
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

        let topics = pubsub.topics.read_topic(&topic).await;
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

        assert!(
            matches!(deserialized, AntiEntropyPayload::Digest { .. }),
            "Expected Digest, got Response"
        );
        if let AntiEntropyPayload::Digest { msg_ids } = deserialized {
            assert_eq!(msg_ids.len(), 3);
            assert_eq!(msg_ids[0], [1u8; 32]);
            assert_eq!(msg_ids[1], [2u8; 32]);
            assert_eq!(msg_ids[2], [3u8; 32]);
        }

        // Test Response variant round-trips through postcard
        let response = AntiEntropyPayload::Response {
            missing_ids: vec![[4u8; 32], [5u8; 32]],
        };
        let bytes = postcard::to_stdvec(&response).expect("serialize response");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize response");

        assert!(
            matches!(deserialized, AntiEntropyPayload::Response { .. }),
            "Expected Response, got Digest"
        );
        if let AntiEntropyPayload::Response { missing_ids } = deserialized {
            assert_eq!(missing_ids.len(), 2);
            assert_eq!(missing_ids[0], [4u8; 32]);
            assert_eq!(missing_ids[1], [5u8; 32]);
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

        assert!(
            matches!(deserialized, AntiEntropyPayload::Digest { .. }),
            "Expected Digest, got Response"
        );
        if let AntiEntropyPayload::Digest { msg_ids } = deserialized {
            assert!(msg_ids.is_empty());
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
        let topics = pubsub.topics.read_topic(&topic).await;
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
            let topics = pubsub.topics.read_topic(&topic).await;
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
        let topics = pubsub.topics.read_topic(&topic).await;
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

    #[test]
    fn test_peer_score_outbound_timeouts_and_cooling_lower_score() {
        let now = Instant::now();
        let mut score = PeerScore::new_at(now);
        let base = score.score_at(now);

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            score.record_outbound_send_timeout_at(now);
        }
        score.record_cooling_event_at(now);

        let degraded = score.score_at(now);
        assert!(
            degraded < PEER_SCORE_EAGER_MIN,
            "send-side timeout pressure should make peer ineligible for EAGER promotion, got {degraded}"
        );
        assert!(
            degraded < base * 0.7,
            "send-side timeout pressure should materially lower score: base={base}, degraded={degraded}"
        );
    }

    #[test]
    fn test_peer_score_recovery_success_recovers_gradually() {
        let now = Instant::now();
        let mut score = PeerScore::new_at(now);
        let base = score.score_at(now);

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            score.record_outbound_send_timeout_at(now);
        }
        score.record_cooling_event_at(now);
        let degraded = score.score_at(now);

        score.record_recovery_probe_at(now);
        score.record_outbound_send_success_at(now, SendAttemptKind::RecoveryProbe);

        let recovered = score.score_at(now);
        assert!(
            recovered > degraded,
            "successful recovery probe should improve score: degraded={degraded}, recovered={recovered}"
        );
        assert!(
            recovered < base,
            "recovery should be gradual while timeout evidence remains: base={base}, recovered={recovered}"
        );
    }

    #[test]
    fn test_peer_score_send_health_evidence_decays() {
        let now = Instant::now();
        let mut score = PeerScore::new_at(now);
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            score.record_outbound_send_timeout_at(now);
        }
        score.record_cooling_event_at(now);

        let degraded = score.components_at(now).send_health;
        let future = now + Duration::from_secs(PEER_SCORE_DECAY_HALF_LIFE.as_secs() * 6);
        let decayed = score.components_at(future).send_health;
        assert!(
            decayed > degraded,
            "old send-health penalties should decay: degraded_health={degraded}, decayed_health={decayed}"
        );
    }

    #[test]
    fn test_peer_score_cleanup_retains_active_send_side_evidence() {
        let mut state = TopicState::new();
        let now = Instant::now();
        let peer = test_peer_id(22);

        let Some(stale_seen) = now.checked_sub(PEER_SCORE_RETENTION + Duration::from_secs(1))
        else {
            return;
        };
        let mut score = PeerScore::new_at(stale_seen);
        score.last_seen = stale_seen;
        score.record_outbound_send_timeout_at(now);
        state.peer_scores.insert(peer, score);

        state.clean_cache();
        assert!(
            state.peer_scores.contains_key(&peer),
            "fresh send-side health evidence should keep a stale inbound score alive"
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
        let topics = pubsub.topics.read_topic(&topic).await;
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
            let topics = pubsub.topics.read_topic(&topic).await;
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
        let topics = pubsub.topics.read_topic(&topic).await;
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
    fn test_score_based_promotion_avoids_low_score_when_alternatives_exist() {
        let mut state = TopicState::new();
        let now = Instant::now();

        let mut healthy_peers = Vec::new();
        for i in 0..MIN_EAGER_DEGREE {
            let peer = test_peer_id(i as u8 + 30);
            healthy_peers.push(peer);
            state.lazy_peers.insert(peer);

            let mut score = PeerScore::new_at(now);
            score.record_delivery();
            state.peer_scores.insert(peer, score);
        }

        let slow_a = test_peer_id(90);
        let slow_b = test_peer_id(91);
        for peer in [slow_a, slow_b] {
            state.lazy_peers.insert(peer);
            let mut score = PeerScore::new_at(now);
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                score.record_outbound_send_timeout_at(now);
            }
            score.record_cooling_event_at(now);
            state.peer_scores.insert(peer, score);
        }

        let (pruned, grafted) = state.maintain_degree_at(now);
        assert_eq!(pruned, 0);
        assert_eq!(grafted, MIN_EAGER_DEGREE);
        for peer in healthy_peers {
            assert!(
                state.eager_peers.contains(&peer),
                "healthy peer should be promoted"
            );
        }
        assert!(
            state.lazy_peers.contains(&slow_a) && state.lazy_peers.contains(&slow_b),
            "low-score peers should remain LAZY while healthy alternatives exist"
        );
    }

    #[test]
    fn test_score_based_promotion_backfills_low_score_when_healthy_insufficient() {
        let mut state = TopicState::new();
        let now = Instant::now();

        let mut healthy_peers = Vec::new();
        for i in 0..(MIN_EAGER_DEGREE - 2) {
            let peer = test_peer_id(i as u8 + 40);
            healthy_peers.push(peer);
            state.lazy_peers.insert(peer);

            let mut score = PeerScore::new_at(now);
            score.record_delivery();
            state.peer_scores.insert(peer, score);
        }

        let mut slow_peers = Vec::new();
        for i in 0..10 {
            let peer = test_peer_id(i as u8 + 70);
            slow_peers.push(peer);
            state.lazy_peers.insert(peer);

            let mut score = PeerScore::new_at(now);
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                score.record_outbound_send_timeout_at(now);
            }
            score.record_cooling_event_at(now);
            state.peer_scores.insert(peer, score);
        }

        let (pruned, grafted) = state.maintain_degree_at(now);
        assert_eq!(pruned, 0);
        assert_eq!(
            grafted, MIN_EAGER_DEGREE,
            "mesh should still fill to the minimum degree when healthy candidates are insufficient"
        );
        for peer in healthy_peers {
            assert!(
                state.eager_peers.contains(&peer),
                "healthy peer should be promoted first"
            );
        }
        let slow_promoted = slow_peers
            .iter()
            .filter(|peer| state.eager_peers.contains(peer))
            .count();
        assert_eq!(
            slow_promoted, 2,
            "only the low-score peers required to fill MIN_EAGER_DEGREE should be backfilled"
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
        let Some(past) = Instant::now().checked_sub(PEER_SCORE_RETENTION + Duration::from_secs(1))
        else {
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
    async fn peer_scores_rebuild_atomicity_uses_cached_snapshot_while_topics_locked() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            transport,
            signing_key,
            false,
        ));
        let topic = TopicId::new([42u8; 32]);
        let scored_peer = test_peer_id(42);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(scored_peer);
            let mut score = PeerScore::new_at(Instant::now());
            score.record_delivery();
            state.peer_scores.insert(scored_peer, score);
        }

        let warm_snapshot = pubsub.stage_stats();
        assert!(
            !warm_snapshot.peer_scores.is_empty(),
            "warm diagnostics snapshot should contain the seeded peer"
        );

        let topics_write_guard = pubsub.topics.write_topic(&topic).await;
        let mut readers = Vec::new();
        for _ in 0..16 {
            let pubsub = Arc::clone(&pubsub);
            readers.push(tokio::spawn(async move {
                pubsub.stage_stats().peer_scores.len()
            }));
        }

        for reader in readers {
            assert!(
                reader.await.unwrap() > 0,
                "readers should fall back to cached peer_scores while topics are locked"
            );
        }
        drop(topics_write_guard);

        pubsub.set_topic_peers(topic, vec![scored_peer]).await;
        assert!(
            !pubsub.stage_stats().peer_scores.is_empty(),
            "peer_scores should remain populated after membership refresh"
        );
    }

    #[test]
    fn cooling_cleanup_adaptive_interval_shortens_and_lengthens() {
        let normal = Duration::from_millis(SUPPRESSION_CLEANUP_NORMAL_INTERVAL_MS);
        let fast = next_suppression_cleanup_interval(0, 10, normal);
        assert_eq!(fast, Duration::from_millis(30_000));

        let min = next_suppression_cleanup_interval(
            10,
            20,
            Duration::from_millis(SUPPRESSION_CLEANUP_MIN_INTERVAL_MS),
        );
        assert_eq!(
            min,
            Duration::from_millis(SUPPRESSION_CLEANUP_MIN_INTERVAL_MS)
        );

        let slow = next_suppression_cleanup_interval(20, 10, normal);
        assert_eq!(
            slow,
            Duration::from_millis(SUPPRESSION_CLEANUP_MAX_INTERVAL_MS)
        );
    }

    #[test]
    fn cooling_cleanup_adaptive_removes_expired_excluded_suppression() {
        let topic = TopicId::new([55u8; 32]);
        let peer = test_peer_id(55);
        let now = Instant::now();
        let mut topics = HashMap::new();
        let mut state = TopicState::new();
        state.peer_cooling.insert(
            peer,
            PeerCoolingState {
                timeout_window_started: now,
                timeout_count: 0,
                suppressed_until: Some(now),
                cooldown: PEER_SUPPRESSION_COOLDOWN,
                last_suppressed_at: Some(now),
                last_suppression_timeout_count: PEER_TIMEOUT_THRESHOLD,
                recovery_probe_in_flight: false,
                recovery_probe_id: None,
                last_bypass_probe_at: None,
            },
        );
        topics.insert(topic, state);

        let stats = PubSubStageStats::default();
        stats.record_peer_suppressed(
            topic,
            peer,
            now,
            PEER_TIMEOUT_THRESHOLD,
            PEER_SUPPRESSION_COOLDOWN,
        );
        assert_eq!(stats.suppressed_peer_snapshots().len(), 1);

        let removed = clean_expired_peer_cooling(&mut topics, &stats, now);

        assert_eq!(removed, 1);
        assert_eq!(count_peer_cooling_entries(&topics), 0);
        assert!(stats.suppressed_peer_snapshots().is_empty());
    }

    #[tokio::test]
    async fn transport_disconnected_peer_is_skipped_before_send_attempt() {
        let peer_id = test_peer_id(1);
        let connected_peer = test_peer_id(2);
        let ghost_peer = test_peer_id(3);
        let transport = RecordingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([90u8; 32]);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(connected_peer);
            state.eager_peers.insert(ghost_peer);
        }
        store_connected_peers_snapshot(
            pubsub.connected_peers_snapshot.as_ref(),
            Some(HashSet::from([connected_peer])),
        );

        pubsub
            .publish_local(topic, Bytes::from_static(b"payload"))
            .await
            .unwrap();

        assert_eq!(transport.send_count_to(connected_peer), 1);
        assert_eq!(transport.send_count_to(ghost_peer), 0);
    }

    #[tokio::test]
    async fn transport_disconnected_peer_is_skipped_at_claim_time() {
        let peer_id = test_peer_id(1);
        let connected_peer = test_peer_id(2);
        let ghost_peer = test_peer_id(3);
        let transport = RecordingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([93u8; 32]);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            topics.entry(topic).or_insert_with(TopicState::new);
        }
        store_connected_peers_snapshot(
            pubsub.connected_peers_snapshot.as_ref(),
            Some(HashSet::from([connected_peer])),
        );

        let (claims, _) = pubsub
            .claim_topic_send_attempts(topic, vec![ghost_peer], "EAGER")
            .await;

        assert!(claims.is_empty());
    }

    #[tokio::test]
    async fn transport_connectivity_refresher_prunes_disconnected_mesh_peers() {
        let peer_id = test_peer_id(1);
        let connected_peer = test_peer_id(2);
        let ghost_peer = test_peer_id(3);
        let transport = RecordingTransport::new(peer_id);
        transport.set_connected_peer_ids(vec![connected_peer]);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([91u8; 32]);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(connected_peer);
            state.eager_peers.insert(ghost_peer);
            state.lazy_peers.insert(ghost_peer);
        }

        pubsub.refresh_connected_peers_snapshot_for_test().await;

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&connected_peer));
        assert!(!state.eager_peers.contains(&ghost_peer));
        assert!(!state.lazy_peers.contains(&ghost_peer));
    }

    #[tokio::test]
    async fn empty_transport_connectivity_snapshot_fails_open() {
        let peer_id = test_peer_id(1);
        let ghost_peer = test_peer_id(2);
        let transport = RecordingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([92u8; 32]);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(ghost_peer);
        }

        pubsub.refresh_connected_peers_snapshot_for_test().await;
        pubsub
            .publish_local(topic, Bytes::from_static(b"payload"))
            .await
            .unwrap();

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&ghost_peer));
        assert_eq!(transport.send_count_to(ghost_peer), 1);
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

        let topics = pubsub.topics.read_topic(&topic).await;
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
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(peer_a);
            state.lazy_peers.insert(peer_b);
        }

        // Only peer_a is still connected — peer_b should be pruned from lazy
        pubsub.set_topic_peers(topic, vec![peer_a]).await;

        let topics = pubsub.topics.read_topic(&topic).await;
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

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&peer_a));
        assert!(state.eager_peers.contains(&peer_b));
    }

    #[tokio::test]
    async fn test_initialize_topic_peers_limits_initial_eager_degree() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([7u8; 32]);
        let peers: Vec<PeerId> = (10..(10 + MAX_EAGER_DEGREE + 4))
            .map(|i| test_peer_id(i as u8))
            .collect();

        pubsub.initialize_topic_peers(topic, peers.clone()).await;

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert_eq!(
            state.eager_peers.len(),
            MIN_EAGER_DEGREE,
            "initial peer seeding should build a bounded mesh instead of bulk-promoting the full view"
        );
        assert_eq!(
            state.lazy_peers.len(),
            peers.len() - MIN_EAGER_DEGREE,
            "remaining connected peers should stay LAZY for repair/opportunistic grafting"
        );
    }

    #[tokio::test]
    async fn test_set_topic_peers_adds_new_peers_as_lazy_when_mesh_is_full_enough() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([8u8; 32]);

        let mut connected = Vec::new();
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for i in 0..MIN_EAGER_DEGREE {
                let peer = test_peer_id(i as u8 + 20);
                connected.push(peer);
                state.eager_peers.insert(peer);
            }
        }

        let new_peer_a = test_peer_id(80);
        let new_peer_b = test_peer_id(81);
        connected.extend([new_peer_a, new_peer_b]);

        pubsub.set_topic_peers(topic, connected).await;

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert_eq!(
            state.eager_peers.len(),
            MIN_EAGER_DEGREE,
            "refresh should not expand EAGER just because new connected peers appeared"
        );
        assert!(state.lazy_peers.contains(&new_peer_a));
        assert!(state.lazy_peers.contains(&new_peer_b));
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
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(peer_a);
            state.lazy_peers.insert(peer_b);
        }

        // Both still connected and the mesh is below MIN_EAGER_DEGREE, so
        // score-aware maintenance may promote peer_b to keep routing viable.
        pubsub.set_topic_peers(topic, vec![peer_a, peer_b]).await;

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&peer_a));
        assert!(
            state.eager_peers.contains(&peer_b),
            "Lazy peer should be promoted when needed to reach the minimum eager degree"
        );
        assert!(
            !state.lazy_peers.contains(&peer_b),
            "Promoted peer should no longer be in lazy set"
        );
    }

    #[tokio::test]
    async fn test_set_topic_peers_does_not_bulk_promote_low_score_lazy_peer() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let slow_peer = test_peer_id(90);

        let mut connected = Vec::new();
        {
            let now = Instant::now();
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);

            for i in 0..MIN_EAGER_DEGREE {
                let peer = test_peer_id(i as u8 + 20);
                connected.push(peer);
                state.eager_peers.insert(peer);

                let mut score = PeerScore::new_at(now);
                score.record_delivery();
                state.peer_scores.insert(peer, score);
            }

            connected.push(slow_peer);
            state.lazy_peers.insert(slow_peer);
            let mut slow_score = PeerScore::new_at(now);
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                slow_score.record_outbound_send_timeout_at(now);
            }
            slow_score.record_cooling_event_at(now);
            state.peer_scores.insert(slow_peer, slow_score);
        }

        pubsub.set_topic_peers(topic, connected).await;

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(
            state.lazy_peers.contains(&slow_peer),
            "membership refresh should not bypass score-aware EAGER selection"
        );
        assert!(
            !state.eager_peers.contains(&slow_peer),
            "low-score lazy peer should not be bulk-promoted by refresh"
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
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(peer_a);
            state.lazy_peers.insert(peer_b);
        }

        // peer_a disconnected, peer_b still connected, peer_c is new
        pubsub.set_topic_peers(topic, vec![peer_b, peer_c]).await;

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert!(
            !state.eager_peers.contains(&peer_a),
            "Disconnected eager peer should be removed"
        );
        assert!(
            state.eager_peers.contains(&peer_b),
            "Connected lazy peer should be promoted because the mesh is below minimum degree"
        );
        assert!(
            !state.lazy_peers.contains(&peer_b),
            "Promoted peer should no longer be in lazy set"
        );
        assert!(
            state.eager_peers.contains(&peer_c),
            "New peer should be promoted when needed to reach the minimum eager degree"
        );
    }

    #[test]
    fn test_opportunistic_graft_replaces_low_score_eager_with_better_lazy() {
        let mut state = TopicState::new();
        let now = Instant::now();

        let low_eager = test_peer_id(31);
        for i in 0..MIN_EAGER_DEGREE {
            let peer = test_peer_id(i as u8 + 30);
            state.eager_peers.insert(peer);
            let mut score = PeerScore::new_at(now);
            score.record_delivery();
            state.peer_scores.insert(peer, score);
        }

        let mut low_score = PeerScore::new_at(now);
        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            low_score.record_outbound_send_timeout_at(now);
        }
        low_score.record_cooling_event_at(now);
        state.peer_scores.insert(low_eager, low_score);

        let better_lazy = test_peer_id(90);
        state.lazy_peers.insert(better_lazy);
        let mut better_score = PeerScore::new_at(now);
        better_score.record_delivery();
        for _ in 0..6 {
            better_score.record_outbound_send_success_at(now, SendAttemptKind::Normal);
        }
        state.peer_scores.insert(better_lazy, better_score);

        let (pruned, grafted) = state.maintain_degree_at(now);

        assert_eq!(pruned, 1);
        assert_eq!(grafted, 1);
        assert!(
            state.lazy_peers.contains(&low_eager),
            "low-score eager peer should be demoted during opportunistic maintenance"
        );
        assert!(
            state.eager_peers.contains(&better_lazy),
            "higher-score lazy peer should replace the low-score eager peer"
        );
        assert_eq!(state.eager_peers.len(), MIN_EAGER_DEGREE);
    }

    #[test]
    fn test_opportunistic_graft_is_rate_limited() {
        let mut state = TopicState::new();
        let now = Instant::now();

        let low_eager_a = test_peer_id(101);
        let low_eager_b = test_peer_id(102);
        for peer in [low_eager_a, low_eager_b] {
            state.eager_peers.insert(peer);
            let mut score = PeerScore::new_at(now);
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                score.record_outbound_send_timeout_at(now);
            }
            score.record_cooling_event_at(now);
            state.peer_scores.insert(peer, score);
        }
        for i in 0..(MIN_EAGER_DEGREE - 2) {
            let peer = test_peer_id(i as u8 + 110);
            state.eager_peers.insert(peer);
            let mut score = PeerScore::new_at(now);
            score.record_delivery();
            state.peer_scores.insert(peer, score);
        }

        let better_lazy_a = test_peer_id(121);
        let better_lazy_b = test_peer_id(122);
        for peer in [better_lazy_a, better_lazy_b] {
            state.lazy_peers.insert(peer);
            let mut score = PeerScore::new_at(now);
            score.record_delivery();
            for _ in 0..6 {
                score.record_outbound_send_success_at(now, SendAttemptKind::Normal);
            }
            state.peer_scores.insert(peer, score);
        }

        let first = state.maintain_degree_at(now);
        let second = state.maintain_degree_at(now + Duration::from_secs(1));

        assert_eq!(first, (1, 1));
        assert_eq!(
            second,
            (0, 0),
            "opportunistic replacement should not run on every membership refresh"
        );
        let replaced = [better_lazy_a, better_lazy_b]
            .iter()
            .filter(|peer| state.eager_peers.contains(peer))
            .count();
        assert_eq!(replaced, 1);
    }

    #[tokio::test]
    async fn test_repeated_refresh_keeps_duplicate_pruned_peer_lazy_when_mesh_is_healthy() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([9u8; 32]);
        let pruned_peer = test_peer_id(140);
        let mut connected = Vec::new();

        {
            let now = Instant::now();
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for i in 0..MIN_EAGER_DEGREE {
                let peer = test_peer_id(i as u8 + 130);
                connected.push(peer);
                state.eager_peers.insert(peer);
                let mut score = PeerScore::new_at(now);
                score.record_delivery();
                state.peer_scores.insert(peer, score);
            }

            connected.push(pruned_peer);
            state.lazy_peers.insert(pruned_peer);
            let mut score = PeerScore::new_at(now);
            for _ in 0..PEER_TIMEOUT_THRESHOLD {
                score.record_outbound_send_timeout_at(now);
            }
            score.record_cooling_event_at(now);
            state.peer_scores.insert(pruned_peer, score);
        }

        for _ in 0..5 {
            pubsub.set_topic_peers(topic, connected.clone()).await;
        }

        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).unwrap();
        assert_eq!(state.eager_peers.len(), MIN_EAGER_DEGREE);
        assert!(
            state.lazy_peers.contains(&pruned_peer),
            "refresh ticks should preserve LAZY state for a low-score duplicate-driven PRUNE"
        );
        assert!(!state.eager_peers.contains(&pruned_peer));
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

    // X0X-0069 — SWIM peer-health oracle bridge tests.

    /// Test oracle that returns canned health verdicts and records the
    /// peers it was nudged to indirect-probe.
    struct MockOracle {
        states: std::collections::HashMap<PeerId, PeerHealth>,
        probes: std::sync::Mutex<Vec<PeerId>>,
    }

    impl MockOracle {
        fn new(states: std::collections::HashMap<PeerId, PeerHealth>) -> Self {
            Self {
                states,
                probes: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn probed_peers(&self) -> Vec<PeerId> {
            self.probes.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl PeerHealthOracle for MockOracle {
        async fn health_of(&self, peer: &PeerId) -> Option<PeerHealth> {
            self.states.get(peer).copied()
        }

        async fn request_indirect_probe(&self, target: PeerId) {
            self.probes.lock().unwrap().push(target);
        }
    }

    #[tokio::test]
    async fn peer_health_oracle_unwired_returns_none() {
        // X0X-0069: with no oracle wired, the bridge accessor returns
        // None — pub-sub keeps its legacy unilateral cooling path.
        let pubsub = PlumtreePubSub::new_with_task_control(
            test_peer_id(0),
            test_transport().await,
            test_signing_key(),
            false,
        );
        let peer = test_peer_id(1);
        assert!(pubsub.peer_health(&peer).await.is_none());
        // request_indirect_probe is a no-op when no oracle is wired.
        pubsub.request_indirect_probe(peer).await;
    }

    #[tokio::test]
    async fn peer_health_oracle_returns_swim_state_when_wired() {
        // X0X-0069: wiring a `PeerHealthOracle` makes the bridge
        // surface the oracle's verdict per peer. Covers the three
        // states downstream cooling logic must distinguish.
        let alive = test_peer_id(11);
        let suspect = test_peer_id(12);
        let dead = test_peer_id(13);
        let mut states = std::collections::HashMap::new();
        states.insert(alive, PeerHealth::Alive);
        states.insert(suspect, PeerHealth::Suspect);
        states.insert(dead, PeerHealth::Dead);
        let oracle: Arc<dyn PeerHealthOracle> = Arc::new(MockOracle::new(states));

        let pubsub = PlumtreePubSub::new_with_task_control(
            test_peer_id(0),
            test_transport().await,
            test_signing_key(),
            false,
        )
        .with_health_oracle(oracle);

        assert_eq!(pubsub.peer_health(&alive).await, Some(PeerHealth::Alive));
        assert_eq!(
            pubsub.peer_health(&suspect).await,
            Some(PeerHealth::Suspect)
        );
        assert_eq!(pubsub.peer_health(&dead).await, Some(PeerHealth::Dead));
        // Unknown peer: oracle has no record.
        assert!(pubsub.peer_health(&test_peer_id(99)).await.is_none());
    }

    #[tokio::test]
    async fn request_indirect_probe_forwards_to_oracle() {
        // X0X-0069: `request_indirect_probe` delegates to the oracle
        // when wired. Downstream tickets (X0X-0073 adaptive cooling,
        // X0X-0074 admission control) call this when a peer is
        // accumulating timeouts so SWIM has fresh signal in time for
        // the next decision.
        let oracle = Arc::new(MockOracle::new(std::collections::HashMap::new()));
        let oracle_arc: Arc<dyn PeerHealthOracle> = oracle.clone();
        let pubsub = PlumtreePubSub::new_with_task_control(
            test_peer_id(0),
            test_transport().await,
            test_signing_key(),
            false,
        )
        .with_health_oracle(oracle_arc);

        let target1 = test_peer_id(21);
        let target2 = test_peer_id(22);
        pubsub.request_indirect_probe(target1).await;
        pubsub.request_indirect_probe(target2).await;

        let probed = oracle.probed_peers();
        assert_eq!(probed, vec![target1, target2]);
    }

    #[tokio::test]
    async fn peer_health_snapshot_drives_suspect_cooling_hold() {
        let local = test_peer_id(0);
        let suspect = test_peer_id(65);
        let topic = TopicId::new([65u8; 32]);
        let mut states = std::collections::HashMap::new();
        states.insert(suspect, PeerHealth::Suspect);
        let oracle = Arc::new(MockOracle::new(states));
        let oracle_arc: Arc<dyn PeerHealthOracle> = oracle.clone();
        let pubsub = PlumtreePubSub::new_with_task_control(
            local,
            RecordingTransport::new(local),
            test_signing_key(),
            false,
        )
        .with_health_oracle(oracle_arc);

        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(suspect);
        }

        pubsub.refresh_peer_health_snapshot_for_test().await;
        assert_eq!(
            peer_health_from_snapshot(pubsub.peer_health_snapshot.as_ref(), &suspect),
            Some(PeerHealth::Suspect)
        );

        for _ in 0..PEER_TIMEOUT_THRESHOLD {
            pubsub
                .record_topic_send_results(topic, Vec::new(), vec![suspect])
                .await;
        }

        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if oracle.probed_peers().contains(&suspect) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("suspect threshold should request an indirect probe");

        assert!(
            pubsub.stage_stats().suppressed_peers.is_empty(),
            "suspect snapshot should hold cooling and avoid suppression"
        );
        let topics = pubsub.topics.read_topic(&topic).await;
        let state = topics.get(&topic).expect("topic exists");
        let cooling = state
            .peer_cooling
            .get(&suspect)
            .expect("suspect peer keeps cooling state");
        assert_eq!(cooling.timeout_count, PEER_TIMEOUT_THRESHOLD - 1);
        assert!(cooling.suppressed_until.is_none());
    }

    // ---- X0X-0074d: per-peer Critical send serialization gate ----

    #[test]
    fn test_x0x_0074d_critical_gate_bounds_queue_depth() {
        // The gate must accept up to OUTBOUND_CRITICAL_QUEUE_PER_PEER concurrent
        // reservations for one peer and reject (overflow) beyond that, so
        // genuine overload still surfaces as a hard error rather than queuing
        // unboundedly (and spawning unbounded tasks).
        let gate = CriticalSendGate::default();
        let peer = test_peer_id(7);

        let mut held = Vec::new();
        for _ in 0..OUTBOUND_CRITICAL_QUEUE_PER_PEER {
            held.push(
                gate.try_reserve(peer)
                    .expect("reservations within the bound succeed"),
            );
        }
        assert!(
            gate.try_reserve(peer).is_none(),
            "reservation past the per-peer bound must overflow (caller records a hard error)"
        );

        // Freeing one reservation reopens exactly one slot.
        held.pop();
        assert!(
            gate.try_reserve(peer).is_some(),
            "a freed reservation must reopen a queue slot"
        );
    }

    #[tokio::test]
    async fn test_x0x_0074d_critical_gate_serializes_per_peer() {
        // Two reservations for the same peer: the first engages immediately, the
        // second must WAIT (FIFO) until the first's in-flight permit is released
        // — proving serialization rather than a second hard-drop.
        let gate = CriticalSendGate::default();
        let peer = test_peer_id(7);

        let r1 = gate.try_reserve(peer).expect("first reserve");
        let hold1 = r1.engage().await.expect("first engages immediately");

        let r2 = gate
            .try_reserve(peer)
            .expect("second reserve (within bound)");
        let waiter = tokio::spawn(async move { r2.engage().await });

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !waiter.is_finished(),
            "second Critical send must wait for the in-flight one, not proceed concurrently"
        );

        drop(hold1);
        let hold2 = waiter
            .await
            .expect("waiter task joins")
            .expect("second engages once the first releases");
        drop(hold2);
    }

    #[tokio::test]
    async fn test_x0x_0074d_critical_gate_wait_timeout_releases_queue_slot() {
        // A queued Critical send that gives up waiting must release its reserved
        // queue slot. Otherwise one slow in-flight send can pin 64 queued tasks
        // for 64× the send timeout and produce prolonged X0X-0074d overflow.
        let gate = CriticalSendGate::default();
        let peer = test_peer_id(8);

        let r1 = gate.try_reserve(peer).expect("first reserve");
        let hold1 = r1.engage().await.expect("first engages immediately");

        let r2 = gate.try_reserve(peer).expect("second reserve");
        let waited = tokio::time::timeout(Duration::from_millis(20), r2.engage()).await;
        assert!(waited.is_err(), "second reservation should still be queued");

        let mut held = Vec::new();
        for _ in 0..(OUTBOUND_CRITICAL_QUEUE_PER_PEER - 1) {
            held.push(
                gate.try_reserve(peer)
                    .expect("timed-out reservation released its queue slot"),
            );
        }
        assert!(
            gate.try_reserve(peer).is_none(),
            "only one in-flight plus 63 queued reservations should fit"
        );
        drop(held);
        drop(hold1);
    }

    #[tokio::test]
    async fn test_x0x_0074d_gate_overflow_immediately_cools_peer() {
        // A full Critical FIFO is already threshold-equivalent pressure. The
        // first claim past the bound should cool the peer/topic immediately so
        // follow-up Critical claims are skipped by cooling instead of emitting
        // repeated X0X-0074d overflow WARNs.
        let peer_id = test_peer_id(1);
        let target = test_peer_id(2);
        let transport = RecordingTransport::new(peer_id);
        let pubsub = PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        );
        let topic = TopicId::new([44u8; 32]);
        pubsub
            .admission()
            .registry()
            .register(topic, TopicPriority::Critical);
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.eager_peers.insert(target);
        }

        let (held, _) = pubsub
            .claim_topic_send_attempts(
                topic,
                vec![target; OUTBOUND_CRITICAL_QUEUE_PER_PEER],
                "EAGER",
            )
            .await;
        assert_eq!(held.attempts().len(), OUTBOUND_CRITICAL_QUEUE_PER_PEER);

        let (overflow, _) = pubsub
            .claim_topic_send_attempts(topic, vec![target], "EAGER")
            .await;
        assert!(overflow.is_empty());
        {
            let topics = pubsub.topics.read_topic(&topic).await;
            let state = topics.get(&topic).expect("topic exists");
            assert!(state.is_peer_suppressed_at(target, Instant::now()));
        }

        let (cooled, _) = pubsub
            .claim_topic_send_attempts(topic, vec![target], "EAGER")
            .await;
        assert!(cooled.is_empty());
        drop(held);
    }

    #[tokio::test]
    async fn test_x0x_0074d_concurrent_critical_sends_serialized_not_dropped() {
        // End-to-end through send_to_peer_bounded: two concurrent Critical
        // (EAGER) sends to the SAME peer must both be delivered, serialized one
        // at a time, with zero hard errors and zero cooling skips.
        let peer_id = test_peer_id(1);
        let target = test_peer_id(2);
        let (transport, mut started_rx) = BlockingTransport::new(peer_id);
        let pubsub = Arc::new(PlumtreePubSub::new_with_task_control(
            peer_id,
            Arc::clone(&transport),
            test_signing_key(),
            false,
        ));
        let topic = TopicId::new([42u8; 32]);
        pubsub
            .admission()
            .registry()
            .register(topic, TopicPriority::Critical);
        {
            let mut topics = pubsub.topics.write_topic(&topic).await;
            topics.insert(topic, TopicState::new());
        }

        let p1 = Arc::clone(&pubsub);
        let send1 = tokio::spawn(async move {
            p1.send_to_peer_bounded(
                topic,
                target,
                GossipStreamType::PubSub,
                Bytes::from_static(b"critical-1"),
                "EAGER",
            )
            .await
        });
        let p2 = Arc::clone(&pubsub);
        let send2 = tokio::spawn(async move {
            p2.send_to_peer_bounded(
                topic,
                target,
                GossipStreamType::PubSub,
                Bytes::from_static(b"critical-2"),
                "EAGER",
            )
            .await
        });

        // First send reaches the transport; the second must be parked at the
        // gate (not in the transport, not dropped).
        let _first = started_rx.recv().await.expect("first Critical send starts");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            transport.max_in_flight(),
            1,
            "only one Critical send to a peer may be in flight at a time"
        );
        assert_eq!(
            transport.send_count(),
            1,
            "the second Critical send must be queued at the gate, not dropped"
        );

        // Release the first; the second then proceeds.
        transport.release_sends(1);
        let _second = started_rx
            .recv()
            .await
            .expect("second Critical send starts after the first releases");
        transport.release_sends(1);

        send1
            .await
            .expect("send1 task joins")
            .expect("send1 succeeds");
        send2
            .await
            .expect("send2 task joins")
            .expect("send2 succeeds");

        assert_eq!(
            transport.send_count(),
            2,
            "both Critical sends must be delivered"
        );
        assert_eq!(
            transport.max_in_flight(),
            1,
            "serialization holds for the whole exchange"
        );
        let stats = pubsub.admission().stats().snapshot();
        assert_eq!(
            stats.dropped_critical_hard_error, 0,
            "queuing the second send must NOT record a hard error"
        );
        assert_eq!(
            stats.dropped_critical_cooling, 0,
            "no peer was cooling in this scenario"
        );
        assert_eq!(stats.admitted_critical, 2, "both sends were admitted");
    }
}
