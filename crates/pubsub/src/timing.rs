//! X0X-0073 — adaptive cooling primitives.
//!
//! Per-peer EWMA of observed send durations + adaptive cooldown
//! configuration. Replaces the previous fixed `PER_PEER_REPUBLISH_TIMEOUT
//! = 2500ms` + `PEER_SUPPRESSION_COOLDOWN = 120000ms` values with bounds
//! that adapt to the peer's actual round-trip behaviour.
//!
//! Reference: libp2p gossipsub v1.1 (1-minute recommended backoff,
//! 0.97/sec decay factor). The numbers chosen here are tighter than
//! gossipsub's defaults because x0x's mesh is small (6-8 peers) and
//! cross-region latency is the dominant variable — per-peer adaptation
//! matters more than fleet-wide tuning.
//!
//! This crate ships the primitives. Downstream integration in
//! `record_send_timeout_at` consumes them so:
//! - The per-peer timeout becomes `max(RTT_MULT × p95_ewma, RTT_MIN_FLOOR)`,
//!   floored to keep brief spikes from cooling intra-region paths.
//! - The initial cooldown drops from 120 s to 30 s; exponential
//!   escalation continues from there capped at 5 min (down from 30 min).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use saorsa_gossip_types::PeerId;

/// Initial cooldown on a peer's first suppression. Lower than the
/// legacy 120 s baseline so transient cross-Pacific slowness recovers
/// in tens of seconds, not minutes.
pub const ADAPTIVE_COOLDOWN_INITIAL: Duration = Duration::from_secs(30);

/// Hard cap on cooldown after consecutive escalations. libp2p's
/// recommended bound is "tens of minutes"; 5 min keeps escalation
/// firm enough to deter genuine bad actors while letting a genuinely
/// healthy-but-slow peer recover within one soak window.
pub const ADAPTIVE_COOLDOWN_MAX: Duration = Duration::from_secs(300);

/// Minimum per-peer send timeout. Floors the adaptive computation so
/// intra-region peers (typical RTT ~50 ms) keep a generous budget even
/// when their EWMA is small.
pub const PER_PEER_TIMEOUT_FLOOR: Duration = Duration::from_millis(1_500);

/// Maximum per-peer send timeout. Caps the adaptive computation so a
/// pathologically slow peer doesn't pull the whole fan-out into long
/// stalls (the cooldown layer handles peers that consistently exceed
/// the floor).
pub const PER_PEER_TIMEOUT_CEILING: Duration = Duration::from_secs(10);

/// Per-peer timeout multiplier over observed RTT EWMA. 2.5× gives
/// roughly p95 headroom over the smoothed mean for typical jitter
/// profiles (verified against helsinki↔singapore p95/mean ratio in the
/// 2026-05-12 4h cert soak).
pub const PER_PEER_TIMEOUT_MULTIPLIER: f64 = 2.5;

/// Maximum samples retained per peer for the EWMA. Older samples
/// affect the EWMA via the decay factor, not by eviction; this cap
/// just bounds memory.
pub const PER_PEER_RTT_SAMPLE_CAP: u32 = 32;

/// EWMA decay factor applied to each new sample. 0.20 weights the new
/// observation as 20 % of the updated mean (so the EWMA converges to a
/// step change in ~5 samples), matching gossipsub-class smoothing.
pub const PER_PEER_RTT_EWMA_ALPHA: f64 = 0.20;

/// Per-peer EWMA tracker for observed send durations.
///
/// Used by adaptive cooling to compute a per-peer send timeout that
/// scales with the observed RTT instead of using a single fleet-wide
/// constant. `PER_PEER_RTT_EWMA_ALPHA` weights new samples; the
/// `sample_count` is purely for diagnostics (no bounded buffer is
/// kept — EWMA encodes history implicitly).
#[derive(Debug, Default)]
pub struct PerPeerRttTracker {
    inner: Mutex<HashMap<PeerId, PerPeerRttEntry>>,
}

/// Snapshot of one peer's observed RTT statistics.
#[derive(Debug, Clone, Copy, Default)]
struct PerPeerRttEntry {
    ewma_ms: f64,
    sample_count: u32,
    last_sample_at: Option<Instant>,
}

impl PerPeerRttTracker {
    /// Construct an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observed RTT sample for `peer`.
    pub fn record(&self, peer: PeerId, observed: Duration) {
        let observed_ms = observed.as_secs_f64() * 1_000.0;
        let now = Instant::now();
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = guard.entry(peer).or_default();
        if entry.sample_count == 0 {
            entry.ewma_ms = observed_ms;
        } else {
            entry.ewma_ms = PER_PEER_RTT_EWMA_ALPHA * observed_ms
                + (1.0 - PER_PEER_RTT_EWMA_ALPHA) * entry.ewma_ms;
        }
        entry.sample_count = entry
            .sample_count
            .saturating_add(1)
            .min(PER_PEER_RTT_SAMPLE_CAP);
        entry.last_sample_at = Some(now);
    }

    /// Current EWMA in milliseconds for `peer`, or `None` if no
    /// sample has been recorded.
    #[must_use]
    pub fn ewma_ms(&self, peer: &PeerId) -> Option<f64> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(peer).map(|e| e.ewma_ms)
    }

    /// Sample count for `peer` (diagnostic). Caps at
    /// `PER_PEER_RTT_SAMPLE_CAP` to prevent display overflow.
    #[must_use]
    pub fn sample_count(&self, peer: &PeerId) -> u32 {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(peer).map(|e| e.sample_count).unwrap_or(0)
    }

    /// Compute the adaptive per-peer send timeout. Returns the legacy
    /// `legacy_floor` when no samples exist (cold-start). Otherwise
    /// returns `max(MULTIPLIER × ewma, PER_PEER_TIMEOUT_FLOOR)`,
    /// capped at `PER_PEER_TIMEOUT_CEILING`.
    #[must_use]
    pub fn adaptive_timeout(&self, peer: &PeerId, legacy_floor: Duration) -> Duration {
        let Some(ewma_ms) = self.ewma_ms(peer) else {
            return legacy_floor;
        };
        let scaled_ms = ewma_ms * PER_PEER_TIMEOUT_MULTIPLIER;
        let scaled = Duration::from_millis(scaled_ms.max(0.0) as u64);
        scaled
            .max(PER_PEER_TIMEOUT_FLOOR)
            .min(PER_PEER_TIMEOUT_CEILING)
    }
}

/// Adaptive cooldown configuration.
///
/// Replaces the legacy fixed `PEER_SUPPRESSION_COOLDOWN = 120s` +
/// `PEER_SUPPRESSION_BACKOFF_MAX = 1800s`. Default values track
/// X0X-0073's calibrated bounds; consumers can override via
/// `AdaptiveCoolingConfig::with_initial` / `::with_max` builders for
/// testing or tuning.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveCoolingConfig {
    /// Initial cooldown on a peer's first suppression event.
    pub initial: Duration,
    /// Hard cap on cooldown after consecutive escalations.
    pub max: Duration,
    /// Multiplier applied on each consecutive cool (current × this).
    pub escalation_factor: u32,
}

impl Default for AdaptiveCoolingConfig {
    fn default() -> Self {
        Self {
            initial: ADAPTIVE_COOLDOWN_INITIAL,
            max: ADAPTIVE_COOLDOWN_MAX,
            escalation_factor: 2,
        }
    }
}

impl AdaptiveCoolingConfig {
    /// Override the initial cooldown.
    #[must_use]
    pub const fn with_initial(mut self, initial: Duration) -> Self {
        self.initial = initial;
        self
    }

    /// Override the cooldown cap.
    #[must_use]
    pub const fn with_max(mut self, max: Duration) -> Self {
        self.max = max;
        self
    }

    /// Compute the next cooldown for a peer that was just suppressed.
    /// `previous` is the last cooldown applied (or
    /// `Duration::default()` if first cool). Returns the new cooldown,
    /// capped at `self.max`.
    #[must_use]
    pub fn next_cooldown(&self, previous: Duration) -> Duration {
        if previous.is_zero() {
            return self.initial.min(self.max);
        }
        previous
            .checked_mul(self.escalation_factor)
            .unwrap_or(self.max)
            .min(self.max)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn p(seed: u8) -> PeerId {
        let mut b = [0u8; 32];
        b[0] = seed;
        PeerId::new(b)
    }

    #[test]
    fn rtt_tracker_records_first_sample_directly() {
        let t = PerPeerRttTracker::new();
        t.record(p(1), Duration::from_millis(200));
        let ewma = t.ewma_ms(&p(1)).expect("recorded");
        assert!(
            (ewma - 200.0).abs() < 0.01,
            "first sample becomes the EWMA seed (got {ewma})"
        );
        assert_eq!(t.sample_count(&p(1)), 1);
    }

    #[test]
    fn rtt_tracker_ewma_smooths_toward_new_samples() {
        let t = PerPeerRttTracker::new();
        t.record(p(2), Duration::from_millis(100));
        t.record(p(2), Duration::from_millis(500));
        let ewma = t.ewma_ms(&p(2)).expect("recorded");
        // 0.20 × 500 + 0.80 × 100 = 180
        assert!(
            (ewma - 180.0).abs() < 0.01,
            "second sample weighted by alpha (got {ewma})"
        );
    }

    #[test]
    fn rtt_tracker_sample_count_caps_at_limit() {
        let t = PerPeerRttTracker::new();
        for _ in 0..(PER_PEER_RTT_SAMPLE_CAP + 10) {
            t.record(p(3), Duration::from_millis(100));
        }
        assert_eq!(t.sample_count(&p(3)), PER_PEER_RTT_SAMPLE_CAP);
    }

    #[test]
    fn adaptive_timeout_returns_legacy_floor_when_no_samples() {
        let t = PerPeerRttTracker::new();
        let legacy = Duration::from_millis(2_500);
        assert_eq!(t.adaptive_timeout(&p(99), legacy), legacy);
    }

    #[test]
    fn adaptive_timeout_scales_by_multiplier_when_samples_exist() {
        let t = PerPeerRttTracker::new();
        t.record(p(4), Duration::from_millis(800));
        // 800 ms × 2.5 = 2000 ms → above the 1500 ms floor.
        let adaptive = t.adaptive_timeout(&p(4), Duration::from_millis(2_500));
        assert_eq!(adaptive, Duration::from_millis(2_000));
    }

    #[test]
    fn adaptive_timeout_floors_at_per_peer_minimum() {
        let t = PerPeerRttTracker::new();
        // Intra-region: 50 ms × 2.5 = 125 ms → floored to 1500 ms.
        t.record(p(5), Duration::from_millis(50));
        let adaptive = t.adaptive_timeout(&p(5), Duration::from_millis(2_500));
        assert_eq!(adaptive, PER_PEER_TIMEOUT_FLOOR);
    }

    #[test]
    fn adaptive_timeout_ceilings_at_per_peer_maximum() {
        let t = PerPeerRttTracker::new();
        // Pathological: 5000 ms × 2.5 = 12500 ms → capped to 10s.
        t.record(p(6), Duration::from_millis(5_000));
        let adaptive = t.adaptive_timeout(&p(6), Duration::from_millis(2_500));
        assert_eq!(adaptive, PER_PEER_TIMEOUT_CEILING);
    }

    #[test]
    fn adaptive_cooldown_first_returns_initial() {
        let cfg = AdaptiveCoolingConfig::default();
        assert_eq!(cfg.next_cooldown(Duration::ZERO), ADAPTIVE_COOLDOWN_INITIAL);
    }

    #[test]
    fn adaptive_cooldown_escalates_on_consecutive_cool() {
        let cfg = AdaptiveCoolingConfig::default();
        let first = cfg.next_cooldown(Duration::ZERO);
        let second = cfg.next_cooldown(first);
        assert_eq!(second, first.checked_mul(2).unwrap());
    }

    #[test]
    fn adaptive_cooldown_caps_at_max() {
        let cfg = AdaptiveCoolingConfig::default()
            .with_initial(Duration::from_secs(60))
            .with_max(Duration::from_secs(120));
        let first = cfg.next_cooldown(Duration::ZERO); // 60s
        let second = cfg.next_cooldown(first); // 120s
        let third = cfg.next_cooldown(second); // would be 240s, capped to 120s
        assert_eq!(third, Duration::from_secs(120));
    }

    #[test]
    fn adaptive_cooldown_handles_arithmetic_overflow_gracefully() {
        let cfg = AdaptiveCoolingConfig {
            initial: Duration::from_secs(30),
            max: Duration::from_secs(300),
            escalation_factor: 2,
        };
        let mut cd = cfg.next_cooldown(Duration::ZERO);
        for _ in 0..1_000 {
            cd = cfg.next_cooldown(cd);
        }
        assert_eq!(cd, cfg.max, "no overflow after extreme escalation");
    }

    #[test]
    fn rtt_tracker_unknown_peer_returns_none_and_zero() {
        let t: PerPeerRttTracker = PerPeerRttTracker::new();
        assert!(t.ewma_ms(&p(123)).is_none());
        assert_eq!(t.sample_count(&p(123)), 0);
    }
}
