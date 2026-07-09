//! X0X-0071 — libp2p gossipsub v1.1 style per-peer scoring with decay.
//!
//! Even with SWIM suspicion (X0X-0069) and adaptive cooling (X0X-0073),
//! peer state is categorical: Alive / Suspect / Dead. That can't capture
//! a peer that is slow on one dimension but fine on others. libp2p
//! gossipsub v1.1 solves this with a weighted multi-parameter score that
//! decays over time — production-grade peer measurement.
//!
//! ## Scored parameters (subset of the libp2p P1-P7 set)
//!
//! - **P1 — time in mesh**: small positive weight. Rewards peers that
//!   have stuck around. Capped at [`PeerScoringConfig::p1_time_cap_secs`]
//!   so it can't dominate. Derived from a join timestamp — not decayed.
//! - **P2 — first message deliveries**: positive weight, decays.
//!   Rewards peers that deliver messages we haven't seen yet.
//! - **P3b — mesh message delivery deficit**: negative weight, decays,
//!   *sticky* on prune (a prune bumps the deficit so a flapping peer
//!   carries the penalty across re-graft). Penalises peers that are in
//!   our mesh but under-deliver.
//! - **P4 — invalid messages**: heavy negative weight, **squared**,
//!   decays. One bad message is cheap; a pattern is very expensive.
//! - **P7 — behavioural penalty**: negative weight, **squared**,
//!   decays. Catch-all for protocol violations (IWANT floods, graft
//!   backoff violations, etc.).
//!
//! P5 (application-specific) and P6 (IP colocation) are intentionally
//! out of scope for this MVP.
//!
//! ## Scoping: per `(TopicId, PeerId)`
//!
//! P1/P2/P3b are mesh-membership signals — a peer is in *our mesh for a
//! topic*, delivers novel messages *on a topic*, is pruned *from a
//! topic's mesh*. A peer that is an excellent forwarder on topic A may
//! be absent from topic B entirely. The engine therefore keys every
//! score by `(TopicId, PeerId)`, matching the per-`TopicState` legacy
//! `PeerScore` map. A global per-`PeerId` score would blend behaviour
//! across topics and make later mesh-selection wiring unsound.
//!
//! ## Composite score
//!
//! ```text
//! score = w1 · min(time_in_mesh_secs, p1_time_cap)
//!       + w2 · first_deliveries
//!       + w3b · delivery_deficit
//!       + w4 · invalid_messages²
//!       + w7 · behavioural_penalty²
//! ```
//!
//! `w3b`, `w4`, `w7` are negative; `w1`, `w2` positive.
//!
//! ## Thresholds
//!
//! Three thresholds let consumers gate behaviour by score:
//! - `gossip_threshold` (default −100): below this, stop gossiping
//!   IHAVE to the peer.
//! - `publish_threshold` (default −1000): below this, stop selecting
//!   the peer for EAGER publish fan-out.
//! - `graylist_threshold` (default −10000): below this, drop the peer
//!   entirely.
//!
//! ## Decay
//!
//! P2/P3b/P4/P7 decay by `decay_per_sec` (default 0.97) every second —
//! `value · 0.97^elapsed_secs`. Applied lazily: every method that
//! touches a peer entry first decays it to "now", and
//! [`PeerScoring::tick_decay`] decays *every* entry so idle-peer
//! snapshots stay fresh. This matches libp2p's continuous decay model.
//!
//! ## MVP scope: populated telemetry, no enforcement
//!
//! This ships the engine **plus real production wiring**: the inbound
//! EAGER path (`PlumtreePubSub::handle_eager`) records P1 (mesh join),
//! P2 (first delivery), P3b (prune on duplicate), and P4 (invalid
//! signature) as they happen, so `PubSubStageStatsSnapshot.peer_scores_v2`
//! is genuinely populated on `/diagnostics/gossip` in production.
//!
//! What is **not** in this MVP:
//! - **Threshold enforcement.** The `gossip_threshold` /
//!   `publish_threshold` / `graylist_threshold` are computed and
//!   surfaced but NOT yet consulted by mesh-selection or admission —
//!   that changes runtime policy and needs its own validation
//!   (X0X-0071b).
//! - **`record_delivery_deficit` / `record_behavioral_penalty`.** These
//!   API entry points exist for P3b-by-amount and P7, but have no
//!   production call site yet — there is no single clean signal for
//!   them without deeper changes. Reserved for X0X-0071b.
//!
//! Reference: <https://github.com/libp2p/specs/blob/master/pubsub/gossipsub/gossipsub-v1.1.md>

use saorsa_gossip_types::{PeerId, TopicId};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Default P1 (time in mesh) weight — small positive.
pub const W_P1_TIME_IN_MESH: f64 = 0.01;
/// Default P2 (first message deliveries) weight — positive.
pub const W_P2_FIRST_DELIVERY: f64 = 1.0;
/// Default P3b (mesh delivery deficit) weight — negative.
pub const W_P3B_DELIVERY_DEFICIT: f64 = -1.0;
/// Default P4 (invalid messages) weight — heavy negative, applied to
/// the squared count.
pub const W_P4_INVALID_MESSAGE: f64 = -100.0;
/// Default P7 (behavioural penalty) weight — negative, applied to the
/// squared penalty.
pub const W_P7_BEHAVIORAL: f64 = -10.0;

/// Default per-second decay factor for P2/P3b/P4/P7. Matches libp2p
/// gossipsub v1.1's recommended decay.
pub const SCORE_DECAY_PER_SEC: f64 = 0.97;

/// Default cap on the P1 time-in-mesh contribution, in seconds. Without
/// a cap a long-lived peer's P1 term would swamp the negative terms.
pub const P1_TIME_IN_MESH_CAP_SECS: f64 = 3600.0;

/// Default gossip threshold — below this score, stop gossiping IHAVE to
/// the peer.
pub const GOSSIP_THRESHOLD: f64 = -100.0;
/// Default publish threshold — below this score, stop selecting the
/// peer for EAGER publish fan-out.
pub const PUBLISH_THRESHOLD: f64 = -1000.0;
/// Default graylist threshold — below this score, drop the peer
/// entirely.
pub const GRAYLIST_THRESHOLD: f64 = -10000.0;

/// Tunable weights, decay, and thresholds for [`PeerScoring`].
#[derive(Debug, Clone, Copy)]
pub struct PeerScoringConfig {
    /// P1 time-in-mesh weight.
    pub w_p1: f64,
    /// P2 first-delivery weight.
    pub w_p2: f64,
    /// P3b delivery-deficit weight (negative).
    pub w_p3b: f64,
    /// P4 invalid-message weight (negative; applied to the squared count).
    pub w_p4: f64,
    /// P7 behavioural-penalty weight (negative; applied to the squared
    /// penalty).
    pub w_p7: f64,
    /// Per-second decay factor for P2/P3b/P4/P7.
    pub decay_per_sec: f64,
    /// Cap on the P1 time-in-mesh contribution, in seconds.
    pub p1_time_cap_secs: f64,
    /// Below this score, stop gossiping to the peer.
    pub gossip_threshold: f64,
    /// Below this score, stop selecting the peer for publish fan-out.
    pub publish_threshold: f64,
    /// Below this score, drop the peer entirely.
    pub graylist_threshold: f64,
}

impl Default for PeerScoringConfig {
    fn default() -> Self {
        Self {
            w_p1: W_P1_TIME_IN_MESH,
            w_p2: W_P2_FIRST_DELIVERY,
            w_p3b: W_P3B_DELIVERY_DEFICIT,
            w_p4: W_P4_INVALID_MESSAGE,
            w_p7: W_P7_BEHAVIORAL,
            decay_per_sec: SCORE_DECAY_PER_SEC,
            p1_time_cap_secs: P1_TIME_IN_MESH_CAP_SECS,
            gossip_threshold: GOSSIP_THRESHOLD,
            publish_threshold: PUBLISH_THRESHOLD,
            graylist_threshold: GRAYLIST_THRESHOLD,
        }
    }
}

impl PeerScoringConfig {
    /// Override the decay factor (clamped to `(0.0, 1.0]` — a factor of
    /// 0 or below would erase all history every tick).
    #[must_use]
    pub fn with_decay_per_sec(mut self, decay: f64) -> Self {
        self.decay_per_sec = if decay.is_finite() && decay > 0.0 && decay <= 1.0 {
            decay
        } else {
            SCORE_DECAY_PER_SEC
        };
        self
    }

    /// Override the three thresholds at once.
    #[must_use]
    pub fn with_thresholds(mut self, gossip: f64, publish: f64, graylist: f64) -> Self {
        self.gossip_threshold = gossip;
        self.publish_threshold = publish;
        self.graylist_threshold = graylist;
        self
    }
}

/// Per-peer raw score state. The decaying components are stored as the
/// running decayed value plus a `last_decay_at` so decay can be applied
/// lazily on the next touch.
#[derive(Debug, Clone)]
struct PeerScoreState {
    /// When the peer joined the mesh — basis for P1. `None` until
    /// [`PeerScoring::note_mesh_join`] is called.
    joined_mesh_at: Option<Instant>,
    /// P2 — decayed running count of first-message deliveries.
    first_deliveries: f64,
    /// P3b — decayed running mesh-delivery deficit (non-negative;
    /// larger = more under-delivery).
    delivery_deficit: f64,
    /// P4 — decayed running count of invalid messages.
    invalid_messages: f64,
    /// P7 — decayed running behavioural penalty.
    behavioral_penalty: f64,
    /// Timestamp the decaying components were last brought current.
    last_decay_at: Instant,
}

impl PeerScoreState {
    fn new(now: Instant) -> Self {
        Self {
            joined_mesh_at: None,
            first_deliveries: 0.0,
            delivery_deficit: 0.0,
            invalid_messages: 0.0,
            behavioral_penalty: 0.0,
            last_decay_at: now,
        }
    }

    /// Apply `decay_per_sec ^ elapsed` to the four decaying components.
    /// Idempotent for `now <= last_decay_at` (elapsed clamps to 0).
    fn decay_to(&mut self, now: Instant, decay_per_sec: f64) {
        let elapsed = now.saturating_duration_since(self.last_decay_at);
        if elapsed.is_zero() {
            return;
        }
        let factor = decay_per_sec.powf(elapsed.as_secs_f64());
        self.first_deliveries *= factor;
        self.delivery_deficit *= factor;
        self.invalid_messages *= factor;
        self.behavioral_penalty *= factor;
        self.last_decay_at = now;
    }

    /// Composite score for this peer under `config`, evaluated at `now`.
    fn score(&self, now: Instant, config: &PeerScoringConfig) -> f64 {
        let p1 = match self.joined_mesh_at {
            Some(joined) => now
                .saturating_duration_since(joined)
                .as_secs_f64()
                .min(config.p1_time_cap_secs),
            None => 0.0,
        };
        config.w_p1 * p1
            + config.w_p2 * self.first_deliveries
            + config.w_p3b * self.delivery_deficit
            + config.w_p4 * self.invalid_messages * self.invalid_messages
            + config.w_p7 * self.behavioral_penalty * self.behavioral_penalty
    }
}

/// JSON-friendly per-`(topic, peer)` score snapshot for
/// `/diagnostics/gossip`.
#[derive(Debug, Clone, Serialize)]
pub struct PeerScoreV2Snapshot {
    /// Topic this score is scoped to, formatted the same way as logs.
    pub topic_id: String,
    /// Peer identifier, formatted the same way as logs.
    pub peer_id: String,
    /// Composite score (sum of the weighted P-terms).
    pub score: f64,
    /// P1 — capped time-in-mesh contribution, in seconds.
    pub p1_time_in_mesh_secs: f64,
    /// P2 — decayed first-message-delivery count.
    pub p2_first_deliveries: f64,
    /// P3b — decayed mesh-delivery deficit.
    pub p3b_delivery_deficit: f64,
    /// P4 — decayed invalid-message count.
    pub p4_invalid_messages: f64,
    /// P7 — decayed behavioural penalty.
    pub p7_behavioral_penalty: f64,
    /// Whether the score is below `graylist_threshold`.
    pub graylisted: bool,
    /// Whether the score is at or above `publish_threshold` (eligible
    /// for EAGER publish fan-out).
    pub publishable: bool,
    /// Whether the score is at or above `gossip_threshold` (eligible
    /// for IHAVE gossip).
    pub gossiped: bool,
}

/// libp2p-style per-`(topic, peer)` scoring engine.
///
/// All mutating methods decay the touched `(topic, peer)` state to "now"
/// before applying the update, so the running values are always
/// current. The hot path is a single `Mutex<HashMap>` lookup per call —
/// no async.
#[derive(Debug)]
pub struct PeerScoring {
    config: PeerScoringConfig,
    inner: Mutex<HashMap<(TopicId, PeerId), PeerScoreState>>,
    /// Lock-wait timing for `inner` (issue #27 instrumentation).
    lock_wait: crate::StageTimingStats,
}

impl Default for PeerScoring {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerScoring {
    /// Construct with the default libp2p-class config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: PeerScoringConfig::default(),
            inner: Mutex::new(HashMap::new()),
            lock_wait: crate::StageTimingStats::default(),
        }
    }
    /// Construct with a custom config.
    #[must_use]
    pub fn with_config(config: PeerScoringConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(HashMap::new()),
            lock_wait: crate::StageTimingStats::default(),
        }
    }

    /// Borrow the active config (weights + thresholds).
    #[must_use]
    pub fn config(&self) -> &PeerScoringConfig {
        &self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(TopicId, PeerId), PeerScoreState>> {
        let started = Instant::now();
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.lock_wait.record(started.elapsed());
        guard
    }

    /// Snapshot of the `inner` lock-wait timing (issue #27 instrumentation).
    #[must_use]
    pub fn lock_wait_snapshot(&self) -> crate::StageTimingStatsSnapshot {
        self.lock_wait.snapshot()
    }

    /// Mark that `peer` has joined our mesh for `topic` — starts the P1
    /// clock. Idempotent: a second call does not reset the join
    /// timestamp, so a peer's accumulated time-in-mesh survives
    /// transient churn.
    pub fn note_mesh_join(&self, topic: TopicId, peer: PeerId) {
        let now = Instant::now();
        let mut guard = self.lock();
        let entry = guard
            .entry((topic, peer))
            .or_insert_with(|| PeerScoreState::new(now));
        if entry.joined_mesh_at.is_none() {
            entry.joined_mesh_at = Some(now);
        }
    }

    /// P2 — record that `peer` delivered a message on `topic` that we
    /// had not seen before. Positive signal.
    pub fn record_first_delivery(&self, topic: TopicId, peer: PeerId) {
        let now = Instant::now();
        let mut guard = self.lock();
        let entry = guard
            .entry((topic, peer))
            .or_insert_with(|| PeerScoreState::new(now));
        entry.decay_to(now, self.config.decay_per_sec);
        entry.first_deliveries += 1.0;
    }

    /// P3b — record that `peer` was pruned from our mesh for `topic`. A
    /// prune bumps the delivery deficit so a flapping peer carries the
    /// penalty across a later re-graft (the "sticky on prune" libp2p
    /// rule).
    pub fn record_mesh_pruned(&self, topic: TopicId, peer: PeerId) {
        let now = Instant::now();
        let mut guard = self.lock();
        let entry = guard
            .entry((topic, peer))
            .or_insert_with(|| PeerScoreState::new(now));
        entry.decay_to(now, self.config.decay_per_sec);
        entry.delivery_deficit += 1.0;
    }

    /// P3b — record `amount` of mesh-delivery deficit for `peer` on
    /// `topic` (how far below the expected delivery rate it ran this
    /// window). Negative `amount` is clamped to 0 — deficit never goes
    /// negative.
    pub fn record_delivery_deficit(&self, topic: TopicId, peer: PeerId, amount: f64) {
        let now = Instant::now();
        let add = if amount.is_finite() && amount > 0.0 {
            amount
        } else {
            0.0
        };
        let mut guard = self.lock();
        let entry = guard
            .entry((topic, peer))
            .or_insert_with(|| PeerScoreState::new(now));
        entry.decay_to(now, self.config.decay_per_sec);
        entry.delivery_deficit += add;
    }

    /// P4 — record that `peer` sent an invalid message on `topic` (bad
    /// signature, malformed payload, unsupported version). Heavy,
    /// squared penalty.
    pub fn record_invalid_message(&self, topic: TopicId, peer: PeerId) {
        let now = Instant::now();
        let mut guard = self.lock();
        let entry = guard
            .entry((topic, peer))
            .or_insert_with(|| PeerScoreState::new(now));
        entry.decay_to(now, self.config.decay_per_sec);
        entry.invalid_messages += 1.0;
    }

    /// P7 — record `amount` of behavioural penalty for `peer` on
    /// `topic` (IWANT flood, graft-backoff violation, etc.). Negative
    /// `amount` is clamped to 0.
    pub fn record_behavioral_penalty(&self, topic: TopicId, peer: PeerId, amount: f64) {
        let now = Instant::now();
        let add = if amount.is_finite() && amount > 0.0 {
            amount
        } else {
            0.0
        };
        let mut guard = self.lock();
        let entry = guard
            .entry((topic, peer))
            .or_insert_with(|| PeerScoreState::new(now));
        entry.decay_to(now, self.config.decay_per_sec);
        entry.behavioral_penalty += add;
    }

    /// Decay every tracked peer's P2/P3b/P4/P7 components to `now`.
    /// Call from a background task so idle-peer snapshots stay current
    /// even when no events are arriving for them.
    pub fn tick_decay(&self, now: Instant) {
        let decay = self.config.decay_per_sec;
        let mut guard = self.lock();
        for entry in guard.values_mut() {
            entry.decay_to(now, decay);
        }
    }

    /// Current composite score for `peer` on `topic`, or `0.0` if the
    /// `(topic, peer)` pair is not tracked. Decays the entry's state to
    /// "now" before evaluating.
    #[must_use]
    pub fn score(&self, topic: &TopicId, peer: &PeerId) -> f64 {
        let now = Instant::now();
        let decay = self.config.decay_per_sec;
        let mut guard = self.lock();
        match guard.get_mut(&(*topic, *peer)) {
            Some(entry) => {
                entry.decay_to(now, decay);
                entry.score(now, &self.config)
            }
            None => 0.0,
        }
    }

    /// `true` if `score` is at or above the gossip threshold — the peer
    /// is healthy enough to receive IHAVE gossip.
    #[must_use]
    pub fn is_gossiped_by(&self, score: f64) -> bool {
        score >= self.config.gossip_threshold
    }

    /// `true` if `score` is at or above the publish threshold — the
    /// peer is healthy enough for EAGER publish fan-out.
    #[must_use]
    pub fn is_publishable_by(&self, score: f64) -> bool {
        score >= self.config.publish_threshold
    }

    /// `true` if `score` is below the graylist threshold — the peer
    /// should be dropped entirely.
    #[must_use]
    pub fn is_graylisted(&self, score: f64) -> bool {
        score < self.config.graylist_threshold
    }

    /// Snapshot every tracked `(topic, peer)` score, sorted
    /// lowest-score-first (the worst peers — graylist candidates —
    /// appear at the top). Decays each entry to "now" so the snapshot
    /// reflects current state without requiring a prior `tick_decay`.
    #[must_use]
    pub fn snapshot(&self) -> Vec<PeerScoreV2Snapshot> {
        let now = Instant::now();
        let decay = self.config.decay_per_sec;
        let mut guard = self.lock();
        let mut rows: Vec<PeerScoreV2Snapshot> = guard
            .iter_mut()
            .map(|((topic, peer), entry)| {
                entry.decay_to(now, decay);
                let score = entry.score(now, &self.config);
                let p1 = match entry.joined_mesh_at {
                    Some(joined) => now
                        .saturating_duration_since(joined)
                        .as_secs_f64()
                        .min(self.config.p1_time_cap_secs),
                    None => 0.0,
                };
                PeerScoreV2Snapshot {
                    topic_id: topic.to_string(),
                    peer_id: peer.to_string(),
                    score,
                    p1_time_in_mesh_secs: p1,
                    p2_first_deliveries: entry.first_deliveries,
                    p3b_delivery_deficit: entry.delivery_deficit,
                    p4_invalid_messages: entry.invalid_messages,
                    p7_behavioral_penalty: entry.behavioral_penalty,
                    graylisted: self.is_graylisted(score),
                    publishable: self.is_publishable_by(score),
                    gossiped: self.is_gossiped_by(score),
                }
            })
            .collect();
        // Lowest score first — worst peers (graylist candidates) on top.
        // Tie-break by (topic, peer) for a deterministic ordering.
        rows.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.topic_id.cmp(&b.topic_id))
                .then_with(|| a.peer_id.cmp(&b.peer_id))
        });
        rows
    }

    /// Number of tracked `(topic, peer)` score entries (diagnostic).
    #[must_use]
    pub fn tracked_peer_count(&self) -> usize {
        self.lock().len()
    }

    /// Drop every score entry for `peer` across all topics — call when
    /// the peer disconnects so the map doesn't grow unbounded.
    pub fn forget_peer(&self, peer: &PeerId) {
        self.lock().retain(|(_, tracked), _| tracked != peer);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn p(seed: u8) -> PeerId {
        PeerId::new([seed; 32])
    }

    fn t(seed: u8) -> TopicId {
        TopicId::new([seed; 32])
    }

    #[test]
    fn unknown_peer_scores_zero() {
        // Why: a peer we've never seen has no evidence either way —
        // its score must be exactly 0, not a default penalty or bonus,
        // so a fresh peer isn't graylisted on sight.
        let scoring = PeerScoring::new();
        assert_eq!(scoring.score(&t(1), &p(1)), 0.0);
        assert_eq!(scoring.tracked_peer_count(), 0);
    }

    #[test]
    fn first_deliveries_raise_score_positively() {
        // Why: P2 rewards peers that deliver novel messages. Three
        // first-deliveries with default w_p2=1.0 should produce a
        // score near +3 (P1 ~0 since note_mesh_join not called).
        let scoring = PeerScoring::new();
        for _ in 0..3 {
            scoring.record_first_delivery(t(1), p(1));
        }
        let score = scoring.score(&t(1), &p(1));
        assert!(
            (score - 3.0).abs() < 0.1,
            "3 first-deliveries × w_p2=1.0 ≈ +3 (got {score})"
        );
    }

    #[test]
    fn invalid_messages_apply_squared_heavy_penalty() {
        // Why: P4 is squared so a single bad message is cheap but a
        // pattern is catastrophic. 3 invalid messages with w_p4=-100
        // → -100 × 3² = -900.
        let scoring = PeerScoring::new();
        for _ in 0..3 {
            scoring.record_invalid_message(t(1), p(1));
        }
        let score = scoring.score(&t(1), &p(1));
        assert!(
            (score - (-900.0)).abs() < 1.0,
            "3 invalid msgs squared × w_p4=-100 ≈ -900 (got {score})"
        );
    }

    #[test]
    fn behavioral_penalty_is_squared_and_negative() {
        // Why: P7 mirrors P4's squared shape for protocol-violation
        // catch-all. penalty=4.0 with w_p7=-10 → -10 × 4² = -160.
        let scoring = PeerScoring::new();
        scoring.record_behavioral_penalty(t(1), p(1), 4.0);
        let score = scoring.score(&t(1), &p(1));
        assert!(
            (score - (-160.0)).abs() < 1.0,
            "behavioural penalty 4² × w_p7=-10 ≈ -160 (got {score})"
        );
    }

    #[test]
    fn prune_is_sticky_on_delivery_deficit() {
        // Why P3b: a flapping peer that gets pruned must carry the
        // deficit penalty into its next graft, so churn doesn't reset
        // the score. Two prunes → deficit 2 → w_p3b=-1 → score -2.
        let scoring = PeerScoring::new();
        scoring.record_mesh_pruned(t(1), p(1));
        scoring.record_mesh_pruned(t(1), p(1));
        let score = scoring.score(&t(1), &p(1));
        assert!(
            (score - (-2.0)).abs() < 0.1,
            "2 prunes × w_p3b=-1 ≈ -2 (got {score})"
        );
    }

    #[test]
    fn scores_are_isolated_per_topic() {
        // Why (X0X-0071 reviewer P2 regression guard): P1/P2/P3b/P4 are
        // mesh-membership signals scoped to a topic. A peer that is a
        // bad actor on topic A must NOT have that penalty leak onto its
        // score for topic B. Under the original global `PeerId` keying
        // this test fails — both topics would read the same blended
        // score.
        let scoring = PeerScoring::new();
        let peer = p(1);
        let topic_a = t(10);
        let topic_b = t(20);

        // Peer behaves badly on topic A only.
        for _ in 0..3 {
            scoring.record_invalid_message(topic_a, peer);
        }
        // ...and well on topic B only.
        for _ in 0..2 {
            scoring.record_first_delivery(topic_b, peer);
        }

        let score_a = scoring.score(&topic_a, &peer);
        let score_b = scoring.score(&topic_b, &peer);
        assert!(
            score_a < -800.0,
            "topic A score reflects 3 invalid msgs only (got {score_a})"
        );
        assert!(
            (score_b - 2.0).abs() < 0.1,
            "topic B score reflects 2 first-deliveries only — NOT blended \
             with topic A's penalty (got {score_b})"
        );
        // Two distinct entries, not one shared one.
        assert_eq!(scoring.tracked_peer_count(), 2);
    }

    #[test]
    fn negative_deficit_and_penalty_amounts_are_clamped_to_zero() {
        // Why: a buggy caller passing a negative amount must not be
        // able to *raise* a peer's score by feeding negative penalty.
        let scoring = PeerScoring::new();
        scoring.record_delivery_deficit(t(1), p(1), -50.0);
        scoring.record_behavioral_penalty(t(1), p(1), -50.0);
        assert_eq!(
            scoring.score(&t(1), &p(1)),
            0.0,
            "negative penalty amounts clamp to 0 — cannot game the score"
        );
    }

    #[test]
    fn decay_reduces_penalty_components_toward_zero() {
        // Why: decay is what lets a peer recover. After a behavioural
        // penalty, ticking decay forward must shrink the penalty's
        // magnitude. We can't sleep a real second in a unit test, so
        // we drive decay via a manufactured future `Instant`.
        let scoring = PeerScoring::new();
        scoring.record_invalid_message(t(1), p(1));
        let before = scoring.score(&t(1), &p(1));
        assert!(before < 0.0, "invalid message → negative score");

        // Decay 10 seconds into the future: 0.97^10 ≈ 0.737, applied to
        // the invalid_messages count BEFORE squaring, so the score
        // magnitude shrinks to ≈ 0.737² ≈ 0.543 of |before|.
        let future = Instant::now() + Duration::from_secs(10);
        scoring.tick_decay(future);
        // score() uses Instant::now() which is < `future`, so it will
        // not decay further — read the snapshot which also decays to
        // now (a no-op past `future` already applied).
        let snap = scoring.snapshot();
        let row = snap.iter().find(|r| r.peer_id == p(1).to_string()).unwrap();
        assert!(
            row.p4_invalid_messages < 1.0 && row.p4_invalid_messages > 0.5,
            "0.97^10 ≈ 0.737 decay on the invalid-message count (got {})",
            row.p4_invalid_messages
        );
    }

    #[test]
    fn threshold_helpers_classify_scores_correctly() {
        // Why: the three thresholds are the consumer-facing contract,
        // and they nest (gossip −100 > publish −1000 > graylist
        // −10000). Verify each band lands a score in the right bucket.
        let scoring = PeerScoring::new();
        // Healthy: score 0 (fresh peer) is above all thresholds.
        assert!(scoring.is_gossiped_by(0.0));
        assert!(scoring.is_publishable_by(0.0));
        assert!(!scoring.is_graylisted(0.0));
        // Below gossip (−100) but above publish (−1000): IHAVE gossip
        // stops, but the peer is still a valid publish-fanout target.
        assert!(!scoring.is_gossiped_by(-150.0));
        assert!(scoring.is_publishable_by(-150.0));
        assert!(!scoring.is_graylisted(-150.0));
        // Below publish (−1000) but above graylist (−10000): no gossip,
        // no publish fanout, but not yet dropped.
        assert!(!scoring.is_gossiped_by(-1500.0));
        assert!(!scoring.is_publishable_by(-1500.0));
        assert!(!scoring.is_graylisted(-1500.0));
        // Below graylist threshold (−10000): graylisted, drop the peer.
        assert!(!scoring.is_gossiped_by(-20_000.0));
        assert!(!scoring.is_publishable_by(-20_000.0));
        assert!(scoring.is_graylisted(-20_000.0));
    }

    #[test]
    fn snapshot_sorts_lowest_score_first_and_carries_topic() {
        // Why: operators (and X0X-0071b's wiring) want the worst peers
        // — graylist candidates — at the top of the diagnostic list,
        // and each row must name the topic it is scoped to.
        let scoring = PeerScoring::new();
        scoring.record_first_delivery(t(1), p(1)); // +1 — best
        scoring.record_invalid_message(t(1), p(2)); // -100 — worst
        scoring.record_mesh_pruned(t(1), p(3)); // -1 — middle

        let snap = scoring.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].peer_id, p(2).to_string(), "worst peer first");
        assert_eq!(snap[1].peer_id, p(3).to_string(), "middle peer second");
        assert_eq!(snap[2].peer_id, p(1).to_string(), "best peer last");
        assert!(snap[0].score < snap[1].score);
        assert!(snap[1].score < snap[2].score);
        assert!(
            snap.iter().all(|r| r.topic_id == t(1).to_string()),
            "every snapshot row carries the topic it is scoped to"
        );
    }

    #[test]
    fn note_mesh_join_is_idempotent() {
        // Why: P1's value is "time in mesh". A second join call must
        // NOT reset the clock — a peer that briefly re-grafts keeps
        // its accrued tenure.
        let scoring = PeerScoring::new();
        scoring.note_mesh_join(t(1), p(1));
        let joined_first = {
            let guard = scoring.lock();
            guard
                .get(&(t(1), p(1)))
                .and_then(|e| e.joined_mesh_at)
                .unwrap()
        };
        scoring.note_mesh_join(t(1), p(1));
        let joined_second = {
            let guard = scoring.lock();
            guard
                .get(&(t(1), p(1)))
                .and_then(|e| e.joined_mesh_at)
                .unwrap()
        };
        assert_eq!(
            joined_first, joined_second,
            "second note_mesh_join must not reset the P1 clock"
        );
    }

    #[test]
    fn forget_peer_removes_score_state_across_all_topics() {
        // Why: forget_peer is the disconnect hook — a disconnected peer
        // must leave no score state behind on ANY topic, or the map
        // grows unbounded.
        let scoring = PeerScoring::new();
        scoring.record_first_delivery(t(1), p(1));
        scoring.record_first_delivery(t(2), p(1));
        scoring.record_first_delivery(t(1), p(2));
        assert_eq!(scoring.tracked_peer_count(), 3);
        scoring.forget_peer(&p(1));
        assert_eq!(
            scoring.tracked_peer_count(),
            1,
            "p(1) forgotten on every topic; p(2) untouched"
        );
        assert_eq!(scoring.score(&t(1), &p(1)), 0.0);
        assert_eq!(scoring.score(&t(2), &p(1)), 0.0);
        assert!(scoring.score(&t(1), &p(2)) > 0.0, "p(2) still tracked");
    }

    #[test]
    fn config_decay_clamp_rejects_invalid_factors() {
        // Why: a decay factor of 0 would erase all history every tick;
        // > 1 would amplify penalties forever. Both fall back to the
        // libp2p default.
        assert_eq!(
            PeerScoringConfig::default()
                .with_decay_per_sec(0.0)
                .decay_per_sec,
            SCORE_DECAY_PER_SEC
        );
        assert_eq!(
            PeerScoringConfig::default()
                .with_decay_per_sec(2.5)
                .decay_per_sec,
            SCORE_DECAY_PER_SEC
        );
        assert_eq!(
            PeerScoringConfig::default()
                .with_decay_per_sec(0.5)
                .decay_per_sec,
            0.5
        );
    }
}
