//! X0X-0074 — substrate-level admission control with topic priority.
//!
//! Rejects low-priority anti-entropy work before it enters the per-peer
//! outbound pipeline, so high-priority DM / control-plane traffic keeps
//! moving under load. Implements the mechanism the Hunt 12f forecast
//! (`docs/design/hunt-12f-stale-release-fast-drop.md` §147) called for:
//!
//! > "a real PubSub admission control path for known low-priority topics
//! > (x0x/release, discovery anti-entropy, identity anti-entropy),
//! > preferably before subscriber-channel enqueue."
//!
//! ## Decision matrix (as implemented 0.5.48+)
//!
//! For peer `P` carrying current SWIM health `H`, current cooled state
//! `C` (from the per-topic cooling map), and per-peer bulk-queue depth
//! `Q_bulk`:
//!
//! - **Critical** → always admit at the admission layer and use a dedicated
//!   per-peer outbound Data lane. Normal/Bulk work cannot occupy that lane,
//!   so Critical only fails to claim a permit when another Critical send to
//!   the same peer is already in flight; that records
//!   `dropped_critical_hard_error`, which must remain zero in production.
//!   Two benign skip reasons are counted separately so the hard-error
//!   contract holds: peer cooling/suppression → `dropped_critical_cooling`,
//!   and a Critical *control* send whose entire target set is
//!   transport-disconnected → `dropped_critical_no_target`.
//! - **Normal** → admit unless `H ∈ {Dead, Suspect}`. The Suspect drop
//!   matches the X0X-0074 ticket text ("admitted unless peer is under
//!   suspicion or peer score below threshold"); the score check is
//!   deferred until X0X-0071 lands. Cooled peers still receive Normal
//!   admissions — dropping Normal there would starve presence and
//!   named-group fanout.
//! - **Bulk** → admit only when `H = Alive` AND `C = false` AND
//!   `Q_bulk < per_peer_bulk_slack_threshold`. Otherwise drop with the
//!   appropriate `AdmissionDropReason` so the anti-entropy retry path
//!   re-attempts when capacity returns.
//!
//! ## Telemetry
//!
//! Drop counters are atomic per (priority, reason) and snapshot into the
//! `PubSubStageStatsSnapshot.admission` block. Per-peer bulk-queue depths
//! flow into `AdmissionStateSnapshot.priority_queue_depths` so the
//! X0X-0075 visibility surface attributes pressure to specific peers
//! under load. `dropped_critical_hard_error` is treated as a soak-
//! blocking violation by the broad-launch gate.
//!
//! ## Why not just rely on cooling?
//!
//! Cooling reacts to per-peer send-side timeouts after they happen.
//! Admission control prevents low-priority work from ever entering the
//! pipeline so the queue doesn't fill up in the first place. The two
//! compose: admission relieves pressure, cooling handles peers that
//! still time out at the reduced load.
//!
//! ## X0X-0074c — Dedicated Critical Data lane
//!
//! X0X-0074b relieved only the Bulk-holds-permit slice: Critical could evict
//! an in-flight Bulk send, but still hard-dropped when the single Data permit
//! was held by Normal or another Critical send. Production soak telemetry showed
//! most `dropped_critical_hard_error` growth came from that Critical/Normal
//! contention, not Bulk backpressure. The outbound-budget layer now splits Data
//! permits per peer into one Critical lane plus one best-effort lane shared by
//! Normal/Bulk. This preserves the old best-effort bound while ensuring Normal
//! and Bulk cannot starve Critical.

use saorsa_gossip_types::{
    AdmissionDecision, AdmissionDropReason, PeerHealth, PeerId, TopicId, TopicPriority,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Default Bulk-queue slack threshold per peer. When the per-peer Bulk
/// queue depth is at or above this value, new Bulk admissions are
/// dropped with `BulkBackpressure`. Calibrated against the 4 h cert
/// soak's window-3 inflection point — that's where suppressed_peers
/// growth crosses the 0.10 ratio that flags overload.
pub const DEFAULT_PER_PEER_BULK_SLACK_THRESHOLD: usize = 64;

/// Default Critical-queue cap per peer. Used only for telemetry — a
/// Critical message is never dropped, but its queue depth is recorded
/// so operators can see when Critical traffic is backed up against
/// transport.
pub const DEFAULT_PER_PEER_CRITICAL_QUEUE_CAP: usize = 256;

/// Per-peer admission counter snapshot.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct PerPeerAdmissionCounts {
    /// Active Bulk-queue depth (estimate; reset to 0 between snapshots
    /// when the pipeline drains).
    pub bulk_queue_depth: usize,
    /// Active Normal-queue depth (currently always 0 — Normal traffic
    /// does not maintain per-peer queue state).
    pub normal_queue_depth: usize,
    /// Active Critical-queue depth (currently always 0 — Critical
    /// traffic does not maintain per-peer queue state).
    pub critical_queue_depth: usize,
}

/// Atomic per-(priority, reason) drop counters.
///
/// Cheap to read for the diagnostic snapshot path; updated from the
/// admission decision call site under a single relaxed `fetch_add`.
#[derive(Debug, Default)]
pub struct AdmissionStats {
    admitted_critical: AtomicU64,
    admitted_normal: AtomicU64,
    admitted_bulk: AtomicU64,
    dropped_bulk_peer_dead: AtomicU64,
    dropped_bulk_peer_suspect: AtomicU64,
    dropped_bulk_peer_cooled: AtomicU64,
    dropped_bulk_backpressure: AtomicU64,
    dropped_normal_peer_dead: AtomicU64,
    dropped_normal_peer_suspect: AtomicU64,
    /// Hard error: a Critical message was dropped. Must stay zero in
    /// production; surfaced as a violation on `/diagnostics/gossip`.
    dropped_critical_hard_error: AtomicU64,
    /// X0X-0074d: a Critical send was skipped because the target peer is
    /// actively cooling/suppressed (or has a recovery probe in flight) —
    /// NOT a budget/gate violation. Legitimate, transient backpressure;
    /// tracked separately so `dropped_critical_hard_error` can hold at zero.
    dropped_critical_cooling: AtomicU64,
    /// Critical control send found no transport-connected target; indicates
    /// unreachable peers in the send set, not local overload or loss to live
    /// peers. Tracked separately so `dropped_critical_hard_error` can hold at
    /// zero when the only "failures" are sends toward disconnected peers
    /// (e.g. unreachable bootstrap entries in a small topology).
    dropped_critical_no_target: AtomicU64,
}

/// JSON-friendly snapshot of admission counters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AdmissionStatsSnapshot {
    /// Critical-priority messages admitted to the per-peer pipeline.
    pub admitted_critical: u64,
    /// Normal-priority messages admitted.
    pub admitted_normal: u64,
    /// Bulk-priority messages admitted.
    pub admitted_bulk: u64,
    /// Bulk admissions dropped because the peer is `Dead` per the SWIM
    /// oracle.
    pub dropped_bulk_peer_dead: u64,
    /// Bulk admissions dropped because the peer is `Suspect` (SWIM
    /// indirect probes pending).
    pub dropped_bulk_peer_suspect: u64,
    /// Bulk admissions dropped because the peer is currently in
    /// post-timeout cooling.
    pub dropped_bulk_peer_cooled: u64,
    /// Bulk admissions dropped because the per-peer bulk-queue depth
    /// exceeded the slack threshold (the real pressure-relief path).
    pub dropped_bulk_backpressure: u64,
    /// Normal admissions dropped because the peer is `Dead`.
    pub dropped_normal_peer_dead: u64,
    /// Normal admissions dropped because the peer is `Suspect` (SWIM
    /// indirect probes pending). Per the X0X-0074 ticket, Normal
    /// traffic backs off under suspicion so the SWIM round can clear
    /// or confirm the peer without the application piling on.
    pub dropped_normal_peer_suspect: u64,
    /// **Hard error**: a Critical admission was dropped. Must remain
    /// zero in production; non-zero is a soak-blocking violation.
    pub dropped_critical_hard_error: u64,
    /// X0X-0074d: Critical sends skipped because the peer is actively
    /// cooling/suppressed (or has a recovery probe in flight). Legitimate
    /// transient backpressure, distinct from the hard-error budget/gate
    /// violation — does NOT block the broad-launch soak gate.
    #[serde(default)]
    pub dropped_critical_cooling: u64,
    /// Critical control send found no transport-connected target; indicates
    /// unreachable peers in the send set, not local overload or loss to live
    /// peers. Distinct from the hard-error violation — does NOT block the
    /// zero-growth soak gate. Appended at the END of the struct so
    /// positional (bincode-style) serialization of the preceding fields is
    /// unchanged.
    #[serde(default)]
    pub dropped_critical_no_target: u64,
}

impl AdmissionStats {
    /// Build a JSON-friendly snapshot. Cheap; uses relaxed reads.
    #[must_use]
    pub fn snapshot(&self) -> AdmissionStatsSnapshot {
        AdmissionStatsSnapshot {
            admitted_critical: self.admitted_critical.load(Ordering::Relaxed),
            admitted_normal: self.admitted_normal.load(Ordering::Relaxed),
            admitted_bulk: self.admitted_bulk.load(Ordering::Relaxed),
            dropped_bulk_peer_dead: self.dropped_bulk_peer_dead.load(Ordering::Relaxed),
            dropped_bulk_peer_suspect: self.dropped_bulk_peer_suspect.load(Ordering::Relaxed),
            dropped_bulk_peer_cooled: self.dropped_bulk_peer_cooled.load(Ordering::Relaxed),
            dropped_bulk_backpressure: self.dropped_bulk_backpressure.load(Ordering::Relaxed),
            dropped_normal_peer_dead: self.dropped_normal_peer_dead.load(Ordering::Relaxed),
            dropped_normal_peer_suspect: self.dropped_normal_peer_suspect.load(Ordering::Relaxed),
            dropped_critical_hard_error: self.dropped_critical_hard_error.load(Ordering::Relaxed),
            dropped_critical_cooling: self.dropped_critical_cooling.load(Ordering::Relaxed),
            dropped_critical_no_target: self.dropped_critical_no_target.load(Ordering::Relaxed),
        }
    }

    fn record_admit(&self, priority: TopicPriority) {
        match priority {
            TopicPriority::Critical => &self.admitted_critical,
            TopicPriority::Normal => &self.admitted_normal,
            TopicPriority::Bulk => &self.admitted_bulk,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// X0X-0074 hard error: a Critical admission could not actually
    /// claim an outbound budget permit. Per the ticket, Critical must
    /// always reach the per-peer pipeline; a non-zero value here is a
    /// soak-blocking violation. Call sites (the publish path) must
    /// emit this whenever they short-circuit a Critical admission
    /// downstream of [`AdmissionControl::admit`].
    pub fn record_critical_hard_error(&self) {
        self.dropped_critical_hard_error
            .fetch_add(1, Ordering::Relaxed);
    }

    /// X0X-0074d: record a Critical send skipped because the peer is
    /// actively cooling/suppressed (or has a recovery probe in flight).
    /// Distinct from [`Self::record_critical_hard_error`] — this is
    /// legitimate transient backpressure and does NOT block the soak gate.
    pub fn record_critical_cooling(&self) {
        self.dropped_critical_cooling
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a Critical control send that found no transport-connected
    /// target: every candidate peer was skipped as transport-disconnected
    /// before an attempt could be claimed. Indicates unreachable peers in
    /// the send set, not local overload or loss to live peers — distinct
    /// from [`Self::record_critical_hard_error`] and does NOT block the
    /// soak gate.
    pub fn record_critical_no_target(&self) {
        self.dropped_critical_no_target
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_drop(&self, priority: TopicPriority, reason: AdmissionDropReason) {
        let counter = match (priority, reason) {
            (TopicPriority::Critical, _) => &self.dropped_critical_hard_error,
            (TopicPriority::Normal, AdmissionDropReason::PeerDead) => {
                &self.dropped_normal_peer_dead
            }
            (TopicPriority::Normal, AdmissionDropReason::PeerSuspect) => {
                &self.dropped_normal_peer_suspect
            }
            (TopicPriority::Bulk, AdmissionDropReason::PeerDead) => &self.dropped_bulk_peer_dead,
            (TopicPriority::Bulk, AdmissionDropReason::PeerSuspect) => {
                &self.dropped_bulk_peer_suspect
            }
            (TopicPriority::Bulk, AdmissionDropReason::PeerCooled) => {
                &self.dropped_bulk_peer_cooled
            }
            (TopicPriority::Bulk, AdmissionDropReason::BulkBackpressure) => {
                &self.dropped_bulk_backpressure
            }
            // Normal traffic doesn't currently drop on Cooled or
            // Backpressure — Normal peers continue to receive non-Bulk
            // traffic so essential overlay flow keeps moving. Record
            // these unexpected cases against bulk-backpressure for
            // visibility if the rules evolve.
            (TopicPriority::Normal, _) => &self.dropped_bulk_backpressure,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Per-peer bulk-queue depth state, used to gate Bulk admission on
/// backpressure.
#[derive(Debug, Default)]
struct PerPeerQueueState {
    bulk_depth: usize,
}

/// Admission-control configuration.
#[derive(Debug, Clone, Copy)]
pub struct AdmissionConfig {
    /// Bulk admissions are dropped with `BulkBackpressure` when the
    /// per-peer bulk-queue depth is at or above this value.
    pub per_peer_bulk_slack_threshold: usize,
    /// Telemetry-only Critical queue cap. Used to size the
    /// `priority_queue_depths` snapshot output; not enforced.
    pub per_peer_critical_queue_cap: usize,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            per_peer_bulk_slack_threshold: DEFAULT_PER_PEER_BULK_SLACK_THRESHOLD,
            per_peer_critical_queue_cap: DEFAULT_PER_PEER_CRITICAL_QUEUE_CAP,
        }
    }
}

/// Topic → priority registry. Topics not in the registry default to
/// `TopicPriority::Normal`.
#[derive(Debug, Default)]
pub struct TopicPriorityRegistry {
    inner: Mutex<HashMap<TopicId, TopicPriority>>,
}

impl TopicPriorityRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `topic` with `priority`. Overwrites any previous entry.
    pub fn register(&self, topic: TopicId, priority: TopicPriority) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.insert(topic, priority);
    }

    /// Resolve `topic` to its registered priority, or
    /// `TopicPriority::Normal` if unregistered.
    #[must_use]
    pub fn priority_for(&self, topic: &TopicId) -> TopicPriority {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(topic).copied().unwrap_or_default()
    }

    /// Snapshot the registry contents (useful for diagnostics).
    #[must_use]
    pub fn snapshot(&self) -> HashMap<TopicId, TopicPriority> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    }
}

/// Admission-control engine.
///
/// Holds the topic registry, decision configuration, atomic counters,
/// and per-peer bulk-queue state. The hot path
/// [`AdmissionControl::admit`] performs a single sync lookup per call,
/// no async / no awaits.
#[derive(Debug, Default)]
pub struct AdmissionControl {
    registry: TopicPriorityRegistry,
    config: AdmissionConfig,
    stats: AdmissionStats,
    per_peer: Mutex<HashMap<PeerId, PerPeerQueueState>>,
}

impl AdmissionControl {
    /// Construct with the default configuration and an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the admission configuration.
    #[must_use]
    pub fn with_config(mut self, config: AdmissionConfig) -> Self {
        self.config = config;
        self
    }

    /// Borrow the registry so applications can register topic priorities
    /// at startup.
    #[must_use]
    pub fn registry(&self) -> &TopicPriorityRegistry {
        &self.registry
    }

    /// Borrow the atomic counters for snapshotting.
    #[must_use]
    pub fn stats(&self) -> &AdmissionStats {
        &self.stats
    }

    /// Apply the admission rules for `(topic, peer, health)`.
    ///
    /// Returns [`AdmissionDecision::Admit`] when the message should
    /// enter the per-peer outbound pipeline, or
    /// [`AdmissionDecision::Drop`] with the reason that fired.
    /// Counter updates are atomic; per-peer state is updated under a
    /// short sync lock.
    pub fn admit(
        &self,
        topic: &TopicId,
        peer: &PeerId,
        health: Option<PeerHealth>,
        is_peer_cooled: bool,
    ) -> AdmissionDecision {
        let priority = self.registry.priority_for(topic);

        match priority {
            TopicPriority::Critical => {
                // Always admit. Critical never drops on health or
                // backpressure; the only "drop" path is a hard error
                // upstream (queue overflow at the transport layer).
                self.stats.record_admit(priority);
                self.note_admission_locked(peer, priority);
                AdmissionDecision::Admit
            }
            TopicPriority::Normal => {
                // Per X0X-0074 ticket: "admitted unless peer is under
                // suspicion (X0X-0069) or peer score below threshold
                // (X0X-0071)". Dead and Suspect both back off so the
                // SWIM round can clear or confirm the peer without the
                // application piling on. Score-threshold check is
                // deferred until X0X-0071 lands and a score is plumbed
                // through.
                if matches!(health, Some(PeerHealth::Dead)) {
                    self.stats
                        .record_drop(priority, AdmissionDropReason::PeerDead);
                    return AdmissionDecision::Drop {
                        reason: AdmissionDropReason::PeerDead,
                    };
                }
                if matches!(health, Some(PeerHealth::Suspect)) {
                    self.stats
                        .record_drop(priority, AdmissionDropReason::PeerSuspect);
                    return AdmissionDecision::Drop {
                        reason: AdmissionDropReason::PeerSuspect,
                    };
                }
                self.stats.record_admit(priority);
                self.note_admission_locked(peer, priority);
                AdmissionDecision::Admit
            }
            TopicPriority::Bulk => self.admit_bulk(peer, health, is_peer_cooled, priority),
        }
    }

    fn admit_bulk(
        &self,
        peer: &PeerId,
        health: Option<PeerHealth>,
        is_peer_cooled: bool,
        priority: TopicPriority,
    ) -> AdmissionDecision {
        if matches!(health, Some(PeerHealth::Dead)) {
            self.stats
                .record_drop(priority, AdmissionDropReason::PeerDead);
            return AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerDead,
            };
        }
        if matches!(health, Some(PeerHealth::Suspect)) {
            self.stats
                .record_drop(priority, AdmissionDropReason::PeerSuspect);
            return AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerSuspect,
            };
        }
        if is_peer_cooled {
            self.stats
                .record_drop(priority, AdmissionDropReason::PeerCooled);
            return AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerCooled,
            };
        }

        let mut guard = match self.per_peer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = guard.entry(*peer).or_default();
        if entry.bulk_depth >= self.config.per_peer_bulk_slack_threshold {
            drop(guard);
            self.stats
                .record_drop(priority, AdmissionDropReason::BulkBackpressure);
            return AdmissionDecision::Drop {
                reason: AdmissionDropReason::BulkBackpressure,
            };
        }
        entry.bulk_depth = entry.bulk_depth.saturating_add(1);
        drop(guard);
        self.stats.record_admit(priority);
        AdmissionDecision::Admit
    }

    fn note_admission_locked(&self, _peer: &PeerId, _priority: TopicPriority) {
        // Normal + Critical do not currently maintain per-peer queue
        // state — depth telemetry for those classes is reported as
        // zero. Bulk depth is managed inside `admit_bulk` so the
        // backpressure check is consistent with the admit decision.
    }

    /// Signal that a previously admitted Bulk message has either been
    /// sent or dropped downstream. Call once per Bulk admission to keep
    /// the backpressure estimate accurate.
    pub fn release_bulk(&self, peer: &PeerId) {
        let mut guard = match self.per_peer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = guard.get_mut(peer) {
            entry.bulk_depth = entry.bulk_depth.saturating_sub(1);
        }
    }

    /// Snapshot per-peer Bulk queue depths (Normal + Critical depths
    /// are reported as zero for now — they don't maintain per-peer
    /// queue state).
    #[must_use]
    pub fn per_peer_snapshot(&self) -> HashMap<PeerId, PerPeerAdmissionCounts> {
        let guard = match self.per_peer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .iter()
            .map(|(peer, state)| {
                (
                    *peer,
                    PerPeerAdmissionCounts {
                        bulk_queue_depth: state.bulk_depth,
                        normal_queue_depth: 0,
                        critical_queue_depth: 0,
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn t(seed: u8) -> TopicId {
        TopicId::new([seed; 32])
    }

    fn p(seed: u8) -> PeerId {
        PeerId::new([seed; 32])
    }

    #[test]
    fn unregistered_topic_defaults_to_normal_priority() {
        // Why: applications that forget to classify a topic should not
        // get treated as bulk (silent drop) or critical (no
        // protection). Default Normal keeps overlay traffic flowing.
        let reg = TopicPriorityRegistry::new();
        assert_eq!(reg.priority_for(&t(1)), TopicPriority::Normal);
    }

    #[test]
    fn critical_topic_always_admits_even_when_peer_dead() {
        // Why: Critical traffic (DM inbox, control plane) must never
        // be dropped on health/backpressure. Dropping a critical
        // message is a hard error surfaced as a violation.
        let admission = AdmissionControl::new();
        admission.registry().register(t(1), TopicPriority::Critical);

        let decision = admission.admit(&t(1), &p(2), Some(PeerHealth::Dead), true);
        assert_eq!(decision, AdmissionDecision::Admit);
        let snap = admission.stats().snapshot();
        assert_eq!(snap.admitted_critical, 1);
        assert_eq!(snap.dropped_critical_hard_error, 0);
    }

    #[test]
    fn normal_drops_under_suspicion_and_when_dead() {
        // Why: per the X0X-0074 ticket, Normal traffic is "admitted
        // unless peer is under suspicion (X0X-0069) or peer score
        // below threshold (X0X-0071)". Both Dead and Suspect back off;
        // Alive and cooled-without-health-signal admit (the
        // score-threshold check arrives with X0X-0071). This replaces
        // the prior `normal_drops_only_when_peer_is_dead` test that
        // contradicted the ticket (reviewer P2, 2026-05-12).
        let admission = AdmissionControl::new();
        admission.registry().register(t(1), TopicPriority::Normal);

        assert_eq!(
            admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false),
            AdmissionDecision::Admit
        );
        assert_eq!(
            admission.admit(&t(1), &p(3), Some(PeerHealth::Suspect), false),
            AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerSuspect
            },
            "Suspect must back off Normal traffic per X0X-0074 ticket"
        );
        assert_eq!(
            admission.admit(&t(1), &p(4), None, true),
            AdmissionDecision::Admit,
            "cooled-without-health-signal still gets Normal admission"
        );
        assert_eq!(
            admission.admit(&t(1), &p(5), Some(PeerHealth::Dead), false),
            AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerDead
            }
        );

        let snap = admission.stats().snapshot();
        assert_eq!(snap.admitted_normal, 2);
        assert_eq!(snap.dropped_normal_peer_suspect, 1);
        assert_eq!(snap.dropped_normal_peer_dead, 1);
    }

    #[test]
    fn bulk_admits_only_when_alive_and_under_slack_threshold() {
        // Why: Bulk is anti-entropy/manifests — drop early on any
        // health signal or backpressure so the pipeline doesn't fill
        // with low-priority work.
        let admission = AdmissionControl::new();
        admission.registry().register(t(1), TopicPriority::Bulk);

        assert_eq!(
            admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false),
            AdmissionDecision::Admit
        );
        assert_eq!(
            admission.admit(&t(1), &p(3), Some(PeerHealth::Suspect), false),
            AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerSuspect
            }
        );
        assert_eq!(
            admission.admit(&t(1), &p(4), Some(PeerHealth::Dead), false),
            AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerDead
            }
        );
        assert_eq!(
            admission.admit(&t(1), &p(5), Some(PeerHealth::Alive), true),
            AdmissionDecision::Drop {
                reason: AdmissionDropReason::PeerCooled
            }
        );

        let snap = admission.stats().snapshot();
        assert_eq!(snap.admitted_bulk, 1);
        assert_eq!(snap.dropped_bulk_peer_suspect, 1);
        assert_eq!(snap.dropped_bulk_peer_dead, 1);
        assert_eq!(snap.dropped_bulk_peer_cooled, 1);
    }

    #[test]
    fn bulk_drops_with_backpressure_when_per_peer_queue_full() {
        // Why: this is the actual pressure-relief mechanism — when one
        // peer's bulk queue fills, further bulk admissions to that peer
        // are dropped so critical/normal traffic keeps moving. The
        // backpressure boundary is the per-peer slack threshold.
        let admission = AdmissionControl::new().with_config(AdmissionConfig {
            per_peer_bulk_slack_threshold: 3,
            per_peer_critical_queue_cap: 256,
        });
        admission.registry().register(t(1), TopicPriority::Bulk);

        for _ in 0..3 {
            assert_eq!(
                admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false),
                AdmissionDecision::Admit
            );
        }
        // Fourth admission to same peer exceeds slack — drop.
        let dropped = admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false);
        assert_eq!(
            dropped,
            AdmissionDecision::Drop {
                reason: AdmissionDropReason::BulkBackpressure
            }
        );

        // Different peer still admits (per-peer slack, not fleet-wide).
        assert_eq!(
            admission.admit(&t(1), &p(3), Some(PeerHealth::Alive), false),
            AdmissionDecision::Admit
        );

        let snap = admission.stats().snapshot();
        assert_eq!(snap.admitted_bulk, 4);
        assert_eq!(snap.dropped_bulk_backpressure, 1);

        // release_bulk decrements depth, re-opening capacity.
        admission.release_bulk(&p(2));
        assert_eq!(
            admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false),
            AdmissionDecision::Admit,
            "release_bulk restores admission capacity"
        );
    }

    #[test]
    fn registry_overwrites_previous_priority() {
        // Why: x0x topics can be re-classified at runtime if the
        // operator changes config. Overwrites must take effect on the
        // next admit call.
        let reg = TopicPriorityRegistry::new();
        reg.register(t(7), TopicPriority::Bulk);
        assert_eq!(reg.priority_for(&t(7)), TopicPriority::Bulk);
        reg.register(t(7), TopicPriority::Critical);
        assert_eq!(reg.priority_for(&t(7)), TopicPriority::Critical);
    }

    #[test]
    fn per_peer_snapshot_reports_bulk_depths_only() {
        let admission = AdmissionControl::new();
        admission.registry().register(t(1), TopicPriority::Bulk);
        admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false);
        admission.admit(&t(1), &p(2), Some(PeerHealth::Alive), false);
        admission.admit(&t(1), &p(3), Some(PeerHealth::Alive), false);

        let snap = admission.per_peer_snapshot();
        assert_eq!(snap.get(&p(2)).map(|c| c.bulk_queue_depth), Some(2));
        assert_eq!(snap.get(&p(3)).map(|c| c.bulk_queue_depth), Some(1));
    }
}
