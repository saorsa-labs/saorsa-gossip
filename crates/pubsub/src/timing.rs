//! X0X-0073 — adaptive cooling primitives.
//!
//! Per-peer sliding-window p95 of observed send durations + adaptive
//! cooldown configuration with success decay. Replaces the previous
//! fixed `PER_PEER_REPUBLISH_TIMEOUT = 2500ms` +
//! `PEER_SUPPRESSION_COOLDOWN = 120000ms` values with bounds that adapt
//! to the peer's actual round-trip behaviour.
//!
//! Reference: libp2p gossipsub v1.1 (1-minute recommended backoff,
//! 0.97/sec success decay). The numbers chosen here are tighter than
//! gossipsub's defaults because x0x's mesh is small (6-8 peers) and
//! cross-region latency is the dominant variable — per-peer adaptation
//! matters more than fleet-wide tuning.
//!
//! What gets recorded: the *full send duration* (enqueue-to-ack), not
//! the raw transport RTT. The send-path measurement includes queueing,
//! decode, dispatch and any application-layer hops, all of which the
//! per-peer timeout must accommodate.
//!
//! This crate ships the primitives. Downstream integration in
//! `record_send_timeout_at` consumes them so:
//! - The per-peer timeout becomes `max(RTT_MULT × p95, RTT_MIN_FLOOR)`,
//!   capped at `RTT_MAX_CEILING`, with `legacy_floor` only used as the
//!   cold-start fallback (also subject to the ceiling).
//! - The initial cooldown drops from 120 s to 30 s; exponential
//!   escalation continues from there capped at 5 min (down from 30 min).
//! - Successful sends decay the cooldown back toward `initial` at
//!   0.97/sec (libp2p-compatible).
//! - A `Dead` verdict from the SWIM oracle escalates the current
//!   cooldown immediately (2× the current value, capped at max).

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

/// Cooldown success-decay factor, applied per second of elapsed time
/// since the last suppression. 0.97 matches libp2p gossipsub v1.1.
pub const ADAPTIVE_COOLDOWN_DECAY_PER_SEC: f64 = 0.97;

/// Minimum per-peer send timeout. Floors the adaptive computation so
/// intra-region peers (typical RTT ~50 ms) keep a generous budget even
/// when their p95 is small.
pub const PER_PEER_TIMEOUT_FLOOR: Duration = Duration::from_millis(1_500);

/// Maximum per-peer send timeout. Caps the adaptive computation so a
/// pathologically slow peer doesn't pull the whole fan-out into long
/// stalls (the cooldown layer handles peers that consistently exceed
/// the floor).
pub const PER_PEER_TIMEOUT_CEILING: Duration = Duration::from_secs(10);

/// Per-peer timeout multiplier over observed p95. 2.5× gives headroom
/// over the observed tail for typical jitter profiles (verified against
/// helsinki↔singapore p95 in the 2026-05-12 4h cert soak).
pub const PER_PEER_TIMEOUT_MULTIPLIER: f64 = 2.5;

/// Number of recent samples retained per peer for the p95 estimate.
pub const PER_PEER_RTT_SAMPLE_CAP: usize = 32;

/// EWMA decay factor applied to each new sample (kept alongside the
/// p95 ring buffer as a diagnostic mean). 0.20 weights the new
/// observation as 20 % of the updated mean (so the EWMA converges to a
/// step change in ~5 samples).
pub const PER_PEER_RTT_EWMA_ALPHA: f64 = 0.20;

/// Per-peer sliding-window p95 tracker for observed send durations.
///
/// Maintains a ring buffer of the last `PER_PEER_RTT_SAMPLE_CAP`
/// samples per peer and computes p95 on demand. Also exposes an EWMA
/// mean for diagnostics (separate from the timeout calculation, which
/// uses p95 only).
#[derive(Debug, Default)]
pub struct PerPeerRttTracker {
    inner: Mutex<HashMap<PeerId, PerPeerRttEntry>>,
}

/// One peer's sample buffer + EWMA mean.
#[derive(Debug, Clone)]
struct PerPeerRttEntry {
    ring: [u32; PER_PEER_RTT_SAMPLE_CAP],
    next: usize,
    filled: usize,
    ewma_ms: f64,
    last_sample_at: Option<Instant>,
}

impl Default for PerPeerRttEntry {
    fn default() -> Self {
        Self {
            ring: [0u32; PER_PEER_RTT_SAMPLE_CAP],
            next: 0,
            filled: 0,
            ewma_ms: 0.0,
            last_sample_at: None,
        }
    }
}

impl PerPeerRttEntry {
    fn record(&mut self, observed_ms: u32, now: Instant) {
        self.ring[self.next] = observed_ms;
        self.next = (self.next + 1) % PER_PEER_RTT_SAMPLE_CAP;
        if self.filled < PER_PEER_RTT_SAMPLE_CAP {
            self.filled += 1;
        }
        let observed_f = f64::from(observed_ms);
        if self.ewma_ms == 0.0 && self.filled == 1 {
            self.ewma_ms = observed_f;
        } else {
            self.ewma_ms = PER_PEER_RTT_EWMA_ALPHA * observed_f
                + (1.0 - PER_PEER_RTT_EWMA_ALPHA) * self.ewma_ms;
        }
        self.last_sample_at = Some(now);
    }

    fn p95_ms(&self) -> Option<f64> {
        if self.filled == 0 {
            return None;
        }
        let mut sorted: Vec<u32> = self.ring[..self.filled].to_vec();
        sorted.sort_unstable();
        // p95 index: ceil(0.95 × N) - 1, clamped to last element.
        let idx = ((self.filled as f64) * 0.95).ceil() as usize;
        let idx = idx.saturating_sub(1).min(self.filled - 1);
        Some(f64::from(sorted[idx]))
    }
}

impl PerPeerRttTracker {
    /// Construct an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observed send-duration sample for `peer`.
    pub fn record(&self, peer: PeerId, observed: Duration) {
        let observed_ms =
            u32::try_from(observed.as_millis().min(u128::from(u32::MAX))).unwrap_or(u32::MAX);
        let now = Instant::now();
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.entry(peer).or_default().record(observed_ms, now);
    }

    /// Current EWMA mean in milliseconds for `peer` (diagnostic), or
    /// `None` if no sample has been recorded.
    #[must_use]
    pub fn ewma_ms(&self, peer: &PeerId) -> Option<f64> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(peer).map(|e| e.ewma_ms)
    }

    /// Current sliding-window p95 in milliseconds for `peer`, or
    /// `None` if no sample has been recorded.
    #[must_use]
    pub fn p95_ms(&self, peer: &PeerId) -> Option<f64> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(peer).and_then(|e| e.p95_ms())
    }

    /// Sample count for `peer`. Caps at `PER_PEER_RTT_SAMPLE_CAP`
    /// (older samples are overwritten in the ring).
    #[must_use]
    pub fn sample_count(&self, peer: &PeerId) -> u32 {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // SAFETY: filled is bounded by PER_PEER_RTT_SAMPLE_CAP (32),
        // always fits in u32.
        guard.get(peer).map_or(0, |e| e.filled as u32)
    }

    /// Compute the adaptive per-peer send timeout from observed p95.
    ///
    /// Cold start (no samples): returns `legacy_floor`, capped at
    /// `PER_PEER_TIMEOUT_CEILING`. With samples: returns
    /// `max(MULTIPLIER × p95, PER_PEER_TIMEOUT_FLOOR)`, capped at
    /// `PER_PEER_TIMEOUT_CEILING`. The ceiling is the absolute upper
    /// bound; passing a `legacy_floor` larger than the ceiling will
    /// not extend the returned timeout.
    #[must_use]
    pub fn adaptive_timeout(&self, peer: &PeerId, legacy_floor: Duration) -> Duration {
        let Some(p95_ms) = self.p95_ms(peer) else {
            return legacy_floor.min(PER_PEER_TIMEOUT_CEILING);
        };
        let scaled_ms = p95_ms * PER_PEER_TIMEOUT_MULTIPLIER;
        // SAFETY: scaled_ms is bounded above by p95 * 2.5 ≤ u32::MAX * 2.5,
        // which fits in u64 with margin.
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
/// `AdaptiveCoolingConfig::with_initial` / `::with_max` /
/// `::with_escalation_factor` builders for testing or tuning.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveCoolingConfig {
    /// Initial cooldown on a peer's first suppression event.
    pub initial: Duration,
    /// Hard cap on cooldown after consecutive escalations.
    pub max: Duration,
    /// Multiplier applied on each consecutive cool (current × this).
    /// Clamped to ≥ 1 internally — passing 0 collapses to no
    /// escalation rather than a zero cooldown.
    escalation_factor: u32,
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

    /// Override the escalation factor. Values below 1 are clamped to 1
    /// (no escalation) so a misconfiguration cannot produce a zero
    /// cooldown after the first event.
    #[must_use]
    pub const fn with_escalation_factor(mut self, factor: u32) -> Self {
        self.escalation_factor = if factor < 1 { 1 } else { factor };
        self
    }

    /// Current escalation factor (after clamp).
    #[must_use]
    pub const fn escalation_factor(&self) -> u32 {
        if self.escalation_factor < 1 {
            1
        } else {
            self.escalation_factor
        }
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
        let factor = self.escalation_factor();
        previous
            .checked_mul(factor)
            .unwrap_or(self.max)
            .min(self.max)
    }

    /// Apply 0.97/sec success decay toward `self.initial`. Call this
    /// after a successful send; `current` is the active cooldown for
    /// the peer and `elapsed` is the time since the last suppression.
    /// Returns the new (smaller) cooldown, never below `self.initial`.
    #[must_use]
    pub fn decay_on_success(&self, current: Duration, elapsed: Duration) -> Duration {
        if current <= self.initial || elapsed.is_zero() {
            return current.max(self.initial);
        }
        let secs = elapsed.as_secs_f64();
        let factor = ADAPTIVE_COOLDOWN_DECAY_PER_SEC.powf(secs);
        let above_initial = current.saturating_sub(self.initial).as_secs_f64();
        let new_above = above_initial * factor;
        let decayed = self.initial + Duration::from_secs_f64(new_above);
        decayed.max(self.initial).min(self.max)
    }

    /// Apply Dead-peer escalation: jump straight to 2× the current
    /// cooldown (or 2× `self.initial` if not yet cooled), capped at
    /// `self.max`. Use when the SWIM oracle returns `Dead`, which is
    /// a stronger signal than a single send-timeout event.
    #[must_use]
    pub fn escalate_on_dead(&self, current: Duration) -> Duration {
        let base = if current.is_zero() {
            self.initial
        } else {
            current
        };
        base.checked_mul(2).unwrap_or(self.max).min(self.max)
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
        let p95 = t.p95_ms(&p(1)).expect("recorded");
        assert!(
            (p95 - 200.0).abs() < 0.01,
            "single-sample p95 equals the sample (got {p95})"
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
    fn rtt_tracker_p95_picks_top_5_percent_value() {
        // Why: timeout must absorb the tail, not the mean. With the
        // full 32-sample window: 30 samples at 100 ms + 2 outliers at
        // 1000 ms (6.25 % tail, above the 5 % cutoff) — p95 must
        // surface the outlier value so the per-peer timeout
        // accommodates it. (A 1-in-21 outlier sits at p99, not p95,
        // so does NOT show up in p95 — this is correct semantics.)
        let t = PerPeerRttTracker::new();
        for _ in 0..30 {
            t.record(p(11), Duration::from_millis(100));
        }
        t.record(p(11), Duration::from_millis(1_000));
        t.record(p(11), Duration::from_millis(1_000));
        let p95 = t.p95_ms(&p(11)).expect("recorded");
        assert!(
            (p95 - 1_000.0).abs() < 0.01,
            "tail sample drives p95 (got {p95})"
        );
        let ewma = t.ewma_ms(&p(11)).expect("recorded");
        // Why the bound: 30×100 saturates the EWMA near 100, then two
        // spikes at 1000 push it to ~424 ms. The point is that p95
        // (1000) is well above the EWMA (≪ p95) so the timeout
        // calculation uses the tail signal, not the mean.
        assert!(
            ewma < p95,
            "EWMA mean stays below p95 when tail is sparse (ewma={ewma}, p95={p95})"
        );
    }

    #[test]
    fn rtt_tracker_p95_window_evicts_oldest_first() {
        // Why: a single past spike must not stay in p95 forever. After
        // PER_PEER_RTT_SAMPLE_CAP fresh low-latency samples, the old
        // outlier must be gone from the ring.
        let t = PerPeerRttTracker::new();
        t.record(p(12), Duration::from_millis(1_000)); // outlier
        for _ in 0..PER_PEER_RTT_SAMPLE_CAP {
            t.record(p(12), Duration::from_millis(50));
        }
        let p95 = t.p95_ms(&p(12)).expect("recorded");
        assert!(
            p95 < 200.0,
            "old outlier evicted by ring window (got {p95})"
        );
    }

    #[test]
    fn rtt_tracker_sample_count_caps_at_limit() {
        let t = PerPeerRttTracker::new();
        for _ in 0..(PER_PEER_RTT_SAMPLE_CAP + 10) {
            t.record(p(3), Duration::from_millis(100));
        }
        assert_eq!(t.sample_count(&p(3)), PER_PEER_RTT_SAMPLE_CAP as u32);
    }

    #[test]
    fn adaptive_timeout_returns_legacy_floor_when_no_samples() {
        let t = PerPeerRttTracker::new();
        let legacy = Duration::from_millis(2_500);
        assert_eq!(t.adaptive_timeout(&p(99), legacy), legacy);
    }

    #[test]
    fn adaptive_timeout_caps_oversize_legacy_floor_at_ceiling() {
        // Why: the public API contract says the returned timeout is
        // bounded by PER_PEER_TIMEOUT_CEILING. A misconfigured caller
        // passing a 30 s legacy_floor must not get 30 s back.
        let t = PerPeerRttTracker::new();
        let oversized = Duration::from_secs(30);
        assert_eq!(
            t.adaptive_timeout(&p(98), oversized),
            PER_PEER_TIMEOUT_CEILING,
            "ceiling clamps legacy_floor at cold start"
        );
    }

    #[test]
    fn adaptive_timeout_scales_by_multiplier_from_p95() {
        // Why: timeout = 2.5 × p95 when above the floor. Fill the ring
        // with 30×100ms + 2×800ms (6.25% tail → p95 = 800); 800 × 2.5
        // = 2000 ms, above the 1500 ms floor.
        let t = PerPeerRttTracker::new();
        for _ in 0..30 {
            t.record(p(4), Duration::from_millis(100));
        }
        t.record(p(4), Duration::from_millis(800));
        t.record(p(4), Duration::from_millis(800));
        let adaptive = t.adaptive_timeout(&p(4), Duration::from_millis(2_500));
        assert_eq!(adaptive, Duration::from_millis(2_000));
    }

    #[test]
    fn adaptive_timeout_floors_at_per_peer_minimum() {
        let t = PerPeerRttTracker::new();
        // Intra-region: 50 ms p95 × 2.5 = 125 ms → floored to 1500 ms.
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
        let cfg = AdaptiveCoolingConfig::default();
        let mut cd = cfg.next_cooldown(Duration::ZERO);
        for _ in 0..1_000 {
            cd = cfg.next_cooldown(cd);
        }
        assert_eq!(cd, cfg.max, "no overflow after extreme escalation");
    }

    #[test]
    fn adaptive_cooldown_zero_escalation_factor_clamps_to_one() {
        // Why: a misconfigured factor of 0 would, without the clamp,
        // produce a zero cooldown after the first event — defeating
        // the entire cooling mechanism. The builder clamps to ≥ 1.
        let cfg = AdaptiveCoolingConfig::default().with_escalation_factor(0);
        let first = cfg.next_cooldown(Duration::ZERO); // initial
        let second = cfg.next_cooldown(first); // factor=1 → unchanged
        assert_eq!(second, first, "clamp prevents zero-cooldown collapse");
        assert_eq!(cfg.escalation_factor(), 1);
    }

    #[test]
    fn adaptive_cooldown_decay_returns_initial_when_already_at_floor() {
        let cfg = AdaptiveCoolingConfig::default();
        let decayed = cfg.decay_on_success(cfg.initial, Duration::from_secs(60));
        assert_eq!(decayed, cfg.initial);
    }

    #[test]
    fn adaptive_cooldown_decay_reduces_proportionally() {
        // Why: 0.97/sec for 10 s reduces the "above-initial" portion
        // to 0.97^10 ≈ 0.737 of its prior value. Starting at 120s
        // (90s above initial=30s): after 10s decay, expect
        // 30s + 90s × 0.737 ≈ 96.3s.
        let cfg = AdaptiveCoolingConfig::default();
        let decayed = cfg.decay_on_success(Duration::from_secs(120), Duration::from_secs(10));
        let secs = decayed.as_secs_f64();
        assert!(
            (secs - 96.3).abs() < 1.0,
            "decay tracks 0.97^elapsed (got {secs}s)"
        );
    }

    #[test]
    fn adaptive_cooldown_decay_returns_initial_after_long_elapsed() {
        let cfg = AdaptiveCoolingConfig::default();
        let decayed = cfg.decay_on_success(cfg.max, Duration::from_secs(1_000));
        assert_eq!(decayed, cfg.initial, "long elapsed decays to floor");
    }

    #[test]
    fn adaptive_cooldown_decay_never_below_initial() {
        let cfg = AdaptiveCoolingConfig::default();
        for elapsed in [0u64, 1, 60, 600, 3_600] {
            let decayed = cfg.decay_on_success(cfg.initial, Duration::from_secs(elapsed));
            assert!(decayed >= cfg.initial, "decay clamp at elapsed={elapsed}");
        }
    }

    #[test]
    fn adaptive_cooldown_dead_escalation_doubles_current() {
        // Why: SWIM Dead verdict is stronger than a single timeout —
        // escalate at 2× immediately rather than waiting for the
        // next suppression event.
        let cfg = AdaptiveCoolingConfig::default();
        let dead = cfg.escalate_on_dead(Duration::from_secs(60));
        assert_eq!(dead, Duration::from_secs(120));
    }

    #[test]
    fn adaptive_cooldown_dead_escalation_from_zero_uses_initial() {
        // Why: if SWIM declares Dead before any cooling has happened,
        // start from `initial` so the escalation is applied to a real
        // baseline, not zero.
        let cfg = AdaptiveCoolingConfig::default();
        let dead = cfg.escalate_on_dead(Duration::ZERO);
        assert_eq!(dead, cfg.initial.checked_mul(2).unwrap());
    }

    #[test]
    fn adaptive_cooldown_dead_escalation_caps_at_max() {
        let cfg = AdaptiveCoolingConfig::default();
        let dead = cfg.escalate_on_dead(cfg.max);
        assert_eq!(dead, cfg.max, "dead escalation honours cap");
    }

    #[test]
    fn rtt_tracker_unknown_peer_returns_none_and_zero() {
        let t: PerPeerRttTracker = PerPeerRttTracker::new();
        assert!(t.ewma_ms(&p(123)).is_none());
        assert!(t.p95_ms(&p(123)).is_none());
        assert_eq!(t.sample_count(&p(123)), 0);
    }
}
