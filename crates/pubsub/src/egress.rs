//! Optional serialized-byte enforcement for Leaf PubSub nodes.
//!
//! Disabled configuration preserves the historical send path. Enabled callers
//! reserve the final serialized frame before acquiring transport admission.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const RECOVERY_RESERVE_PERCENT: u64 = 25;
const DEFAULT_MAX_WAITERS: usize = 256;
const DEFAULT_MAX_WAITERS_PER_PEER: usize = 8;
const DEFAULT_MAX_INTENTS: usize = 1024;
const MAX_PURPOSE_ROWS: usize = 1024;
const RECOVERY_INTENT_MAX_AGE: Duration = Duration::from_secs(super::MAX_CACHE_AGE_SECS);

/// Leaf-only serialized PubSub egress policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafEgressConfig {
    /// Early degradation rate for relayed EAGER traffic. Zero disables it.
    pub soft_bytes_per_second: u64,
    /// Sustained hard rate. Zero disables enforcement completely.
    pub hard_bytes_per_second: u64,
    /// Short-spike capacity. Must cover the largest accepted frame.
    pub burst_bytes: u64,
    /// Largest final serialized frame accepted while enforcement is enabled.
    pub max_serialized_frame_bytes: usize,
}

impl LeafEgressConfig {
    fn validate(self) -> Option<Self> {
        if self.hard_bytes_per_second == 0 {
            return None;
        }
        let maximum = u64::try_from(self.max_serialized_frame_bytes).ok()?;
        (maximum > 0
            && self.burst_bytes >= maximum
            && (self.soft_bytes_per_second == 0
                || self.soft_bytes_per_second <= self.hard_bytes_per_second))
            .then_some(self)
    }
}

/// Stable identity of one peer/frame recovery operation. It contains no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecoveryIntentKey {
    pub(crate) peer: [u8; 32],
    pub(crate) family: [u8; 32],
    pub(crate) operation: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReserveError {
    Disabled,
    Oversized,
    Deferred,
    WaiterLimit,
    IntentLimit,
    Reconfigured,
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Disabled => "limiter disabled",
            Self::Oversized => "serialized PubSub frame exceeds configured Leaf maximum",
            Self::Deferred => "serialized PubSub byte budget deferred send",
            Self::WaiterLimit => "serialized PubSub byte-budget waiter limit reached",
            Self::IntentLimit => "serialized PubSub recovery-intent limit reached",
            Self::Reconfigured => "serialized PubSub limiter was reconfigured",
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Intent {
    bytes: u64,
    created: Instant,
    order: u64,
    charged: u64,
}

#[derive(Debug)]
struct State {
    config: Option<LeafEgressConfig>,
    generation: u64,
    tokens: u64,
    remainder: u128,
    soft_tokens: u64,
    soft_remainder: u128,
    last_refill: Instant,
    intents: HashMap<RecoveryIntentKey, Intent>,
    waiters_by_peer: HashMap<[u8; 32], usize>,
    next_order: u64,
}

#[derive(Debug, Default)]
struct EgressCounters {
    demanded_bytes: AtomicU64,
    charged_bytes: AtomicU64,
    sent_bytes: AtomicU64,
    send_failures: AtomicU64,
    data_deferred: AtomicU64,
    recovery_waited: AtomicU64,
    budget_timeouts: AtomicU64,
    queue_overflow: AtomicU64,
    invariant_violations: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
struct PurposeCounters {
    demanded_bytes: u64,
    charged_bytes: u64,
    deferred: u64,
    sent_bytes: u64,
    send_failures: u64,
}

/// Per-topic and per-wire-purpose Leaf accounting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LeafEgressPurposeSnapshot {
    /// Transport topic identifier associated with this accounting row.
    pub topic: [u8; 32],
    /// Wire-purpose label, such as `EAGER`, `IHAVE`, or `IWANT`.
    pub purpose: String,
    /// Final serialized bytes presented to this row's limiter accounting.
    pub demanded_bytes: u64,
    /// Bytes for which the hard bucket issued a reservation.
    pub charged_bytes: u64,
    /// Attempts deferred before a transport send.
    pub deferred: u64,
    /// Charged bytes whose transport send completed successfully.
    pub sent_bytes: u64,
    /// Charged sends that failed, timed out, or were invalidated.
    pub send_failures: u64,
}

/// Cumulative Leaf egress enforcement counters.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LeafEgressSnapshot {
    /// Final serialized bytes presented to the limiter.
    pub demanded_bytes: u64,
    /// Bytes for which the aggregate hard bucket issued a reservation.
    pub charged_bytes: u64,
    /// Charged bytes whose transport send completed successfully.
    pub sent_bytes: u64,
    /// Charged sends that failed, timed out, or were invalidated.
    pub send_failures: u64,
    /// EAGER data attempts converted to lazy recovery.
    pub data_deferred: u64,
    /// Recovery/control attempts that entered the bounded waiter lane.
    pub recovery_waited: u64,
    /// Waiter attempts that exhausted their unchanged caller deadline.
    pub budget_timeouts: u64,
    /// Recovery intents refused because bounded metadata was full.
    pub queue_overflow: u64,
    /// Enabled sends that reached the final transport fence unreserved.
    pub invariant_violations: u64,
    /// Current coalesced recovery intents awaiting enough tokens.
    pub pending_recovery_intents: usize,
    /// Bounded per-topic and per-wire-purpose accounting rows.
    pub by_topic_and_purpose: Vec<LeafEgressPurposeSnapshot>,
}

/// Unforgeable ownership of one charged peer/frame reservation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ByteReservation {
    key: RecoveryIntentKey,
    generation: u64,
}

impl ByteReservation {
    pub(crate) fn matches(&self, key: RecoveryIntentKey, generation: u64) -> bool {
        self.key == key && self.generation == generation
    }
}

/// One aggregate token bucket plus bounded waiter and persistent-intent state.
#[derive(Debug)]
pub(crate) struct LeafEgressLimiter {
    state: Mutex<State>,
    waiters: Arc<Semaphore>,
    counters: EgressCounters,
    purpose_counters: Mutex<HashMap<([u8; 32], &'static str), PurposeCounters>>,
}

impl LeafEgressLimiter {
    pub(crate) fn disabled() -> Self {
        Self {
            state: Mutex::new(State {
                config: None,
                generation: 0,
                tokens: 0,
                remainder: 0,
                soft_tokens: 0,
                soft_remainder: 0,
                last_refill: Instant::now(),
                intents: HashMap::new(),
                waiters_by_peer: HashMap::new(),
                next_order: 0,
            }),
            waiters: Arc::new(Semaphore::new(DEFAULT_MAX_WAITERS)),
            counters: EgressCounters::default(),
            purpose_counters: Mutex::new(HashMap::new()),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.lock_state().config.is_some()
    }

    pub(crate) fn configure(&self, requested: Option<LeafEgressConfig>) -> bool {
        let validated = match requested {
            Some(candidate) if candidate.hard_bytes_per_second == 0 => None,
            Some(candidate) => {
                let Some(validated) = candidate.validate() else {
                    return false;
                };
                Some(validated)
            }
            None => None,
        };
        let mut state = self.lock_state();
        if state.config == validated {
            return true;
        }
        state.config = validated;
        state.generation = state.generation.wrapping_add(1);
        state.tokens = validated.map_or(0, |config| config.burst_bytes);
        state.soft_tokens = state.tokens;
        state.remainder = 0;
        state.soft_remainder = 0;
        state.last_refill = Instant::now();
        state.intents.clear();
        true
    }

    pub(crate) fn snapshot(&self) -> LeafEgressSnapshot {
        let mut by_topic_and_purpose: Vec<_> = self
            .purpose_counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(&(topic, purpose), counters)| LeafEgressPurposeSnapshot {
                topic,
                purpose: purpose.to_string(),
                demanded_bytes: counters.demanded_bytes,
                charged_bytes: counters.charged_bytes,
                deferred: counters.deferred,
                sent_bytes: counters.sent_bytes,
                send_failures: counters.send_failures,
            })
            .collect();
        by_topic_and_purpose.sort_by(|left, right| {
            left.topic
                .cmp(&right.topic)
                .then_with(|| left.purpose.cmp(&right.purpose))
        });
        LeafEgressSnapshot {
            demanded_bytes: self.counters.demanded_bytes.load(Ordering::Relaxed),
            charged_bytes: self.counters.charged_bytes.load(Ordering::Relaxed),
            sent_bytes: self.counters.sent_bytes.load(Ordering::Relaxed),
            send_failures: self.counters.send_failures.load(Ordering::Relaxed),
            data_deferred: self.counters.data_deferred.load(Ordering::Relaxed),
            recovery_waited: self.counters.recovery_waited.load(Ordering::Relaxed),
            budget_timeouts: self.counters.budget_timeouts.load(Ordering::Relaxed),
            queue_overflow: self.counters.queue_overflow.load(Ordering::Relaxed),
            invariant_violations: self.counters.invariant_violations.load(Ordering::Relaxed),
            pending_recovery_intents: self.lock_state().intents.len(),
            by_topic_and_purpose,
        }
    }

    pub(crate) fn record_purpose_demand(
        &self,
        topic: [u8; 32],
        purpose: &'static str,
        bytes: usize,
        charged: bool,
    ) {
        let mut counters = self
            .purpose_counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if counters.len() >= MAX_PURPOSE_ROWS && !counters.contains_key(&(topic, purpose)) {
            self.counters.queue_overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let entry = counters.entry((topic, purpose)).or_default();
        entry.demanded_bytes = entry
            .demanded_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if charged {
            entry.charged_bytes = entry
                .charged_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        } else {
            entry.deferred = entry.deferred.saturating_add(1);
        }
    }

    pub(crate) fn validate_frame(&self, bytes: usize) -> Result<(), ReserveError> {
        let state = self.lock_state();
        let Some(config) = state.config else {
            return Err(ReserveError::Disabled);
        };
        (bytes <= config.max_serialized_frame_bytes)
            .then_some(())
            .ok_or(ReserveError::Oversized)
    }

    fn refill(&self, state: &mut State, now: Instant) {
        let Some(config) = state.config else { return };
        let elapsed = now.saturating_duration_since(state.last_refill);
        let scaled = elapsed
            .as_nanos()
            .saturating_mul(u128::from(config.hard_bytes_per_second))
            .saturating_add(state.remainder);
        state.tokens = state
            .tokens
            .saturating_add(u64::try_from(scaled / 1_000_000_000).unwrap_or(u64::MAX))
            .min(config.burst_bytes);
        state.remainder = scaled % 1_000_000_000;
        if config.soft_bytes_per_second > 0 {
            let scaled = elapsed
                .as_nanos()
                .saturating_mul(u128::from(config.soft_bytes_per_second))
                .saturating_add(state.soft_remainder);
            state.soft_tokens = state
                .soft_tokens
                .saturating_add(u64::try_from(scaled / 1_000_000_000).unwrap_or(u64::MAX))
                .min(config.burst_bytes);
            state.soft_remainder = scaled % 1_000_000_000;
        }
        state.last_refill = now;
        state.intents.retain(|_, intent| {
            now.saturating_duration_since(intent.created) < RECOVERY_INTENT_MAX_AGE
        });
        self.charge_ready_recovery(state);
    }

    /// Convert the oldest affordable intents into charged credits. A caller
    /// need not still be polling for the queue to advance: any refill pump
    /// can escrow its bytes, after which later intents may progress. Removing
    /// an abandoned charged credit never refunds bytes.
    fn charge_ready_recovery(&self, state: &mut State) {
        loop {
            let head = state
                .intents
                .iter()
                .filter(|(_, intent)| intent.charged < intent.bytes)
                .min_by_key(|(_, intent)| intent.order)
                .map(|(key, intent)| (*key, intent.bytes - intent.charged));
            let Some((key, delta)) = head else { break };
            if state.tokens < delta {
                break;
            }
            state.tokens -= delta;
            self.counters
                .charged_bytes
                .fetch_add(delta, Ordering::Relaxed);
            if let Some(intent) = state.intents.get_mut(&key) {
                intent.charged = intent.charged.saturating_add(delta);
            }
        }
    }

    pub(crate) fn try_reserve_data(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        relayed: bool,
    ) -> Result<ByteReservation, ReserveError> {
        let mut state = self.lock_state();
        self.refill(&mut state, Instant::now());
        let Some(config) = state.config else {
            return Err(ReserveError::Disabled);
        };
        if frame_bytes > config.max_serialized_frame_bytes {
            return Err(ReserveError::Oversized);
        }
        let bytes = u64::try_from(frame_bytes).map_err(|_| ReserveError::Oversized)?;
        self.counters
            .demanded_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        let fixed_reserve = config.burst_bytes.saturating_mul(RECOVERY_RESERVE_PERCENT) / 100;
        // Once recovery declares its exact serialized demand, data may still
        // use surplus tokens but cannot repeatedly consume the tokens that
        // operation needs. This lets a frame larger than the fixed 25% floor
        // accumulate without globally stopping unrelated data.
        let reserve = state
            .intents
            .values()
            .filter(|intent| intent.charged < intent.bytes)
            .map(|intent| intent.bytes)
            .max()
            .unwrap_or(fixed_reserve)
            .max(fixed_reserve);
        let hard_denied = state.tokens < bytes || state.tokens.saturating_sub(bytes) < reserve;
        let soft_denied = relayed && config.soft_bytes_per_second > 0 && state.soft_tokens < bytes;
        if hard_denied || soft_denied {
            self.counters.data_deferred.fetch_add(1, Ordering::Relaxed);
            return Err(ReserveError::Deferred);
        }
        state.tokens -= bytes;
        if relayed && config.soft_bytes_per_second > 0 {
            state.soft_tokens -= bytes;
        }
        self.counters
            .charged_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        Ok(ByteReservation {
            key,
            generation: state.generation,
        })
    }

    fn waiter_permit(self: &Arc<Self>, peer: [u8; 32]) -> Result<WaiterPermit, ReserveError> {
        let global = Arc::clone(&self.waiters)
            .try_acquire_owned()
            .map_err(|_| ReserveError::WaiterLimit)?;
        {
            let mut state = self.lock_state();
            let count = state.waiters_by_peer.entry(peer).or_default();
            if *count >= DEFAULT_MAX_WAITERS_PER_PEER {
                return Err(ReserveError::WaiterLimit);
            }
            *count += 1;
        }
        Ok(WaiterPermit {
            limiter: Arc::clone(self),
            peer,
            _global: global,
        })
    }

    pub(crate) async fn reserve_recovery(
        self: &Arc<Self>,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        deadline: Duration,
        keep_intent_on_timeout: bool,
    ) -> Result<ByteReservation, ReserveError> {
        if !self.enabled() {
            return Err(ReserveError::Disabled);
        }
        let _waiter = self.waiter_permit(key.peer)?;
        self.counters
            .recovery_waited
            .fetch_add(1, Ordering::Relaxed);
        let generation = self.lock_state().generation;
        let attempt = async {
            loop {
                match self.try_reserve_recovery_at(key, frame_bytes, generation, Instant::now()) {
                    Ok(reservation) => return Ok(reservation),
                    Err(ReserveError::Deferred) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let mut intent_guard = IntentGuard {
            limiter: Arc::clone(self),
            key,
            generation,
            armed: true,
        };
        match tokio::time::timeout(deadline, attempt).await {
            Ok(Ok(reservation)) => {
                intent_guard.armed = false;
                Ok(reservation)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                self.counters
                    .budget_timeouts
                    .fetch_add(1, Ordering::Relaxed);
                if keep_intent_on_timeout {
                    // The caller explicitly retains executor ownership and
                    // will retry/cancel this stable key.
                    intent_guard.armed = false;
                } else {
                    self.cancel_intent(key, generation);
                }
                Err(ReserveError::Deferred)
            }
        }
    }

    /// Register or service one persistent recovery intent without waiting.
    ///
    /// Periodic and explicitly retried owners use this entry point so a peer
    /// that lacks tokens cannot delay a different peer whose frame is ready.
    pub(crate) fn try_reserve_recovery(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
    ) -> Result<ByteReservation, ReserveError> {
        let generation = self.lock_state().generation;
        self.try_reserve_recovery_at(key, frame_bytes, generation, Instant::now())
    }

    fn try_reserve_recovery_at(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        generation: u64,
        now: Instant,
    ) -> Result<ByteReservation, ReserveError> {
        let mut state = self.lock_state();
        self.refill(&mut state, now);
        if state.generation != generation {
            return Err(ReserveError::Reconfigured);
        }
        let Some(config) = state.config else {
            return Err(ReserveError::Disabled);
        };
        if frame_bytes > config.max_serialized_frame_bytes {
            return Err(ReserveError::Oversized);
        }
        let bytes = u64::try_from(frame_bytes).map_err(|_| ReserveError::Oversized)?;
        if state.intents.contains_key(&key) {
            if state
                .intents
                .get(&key)
                .is_some_and(|intent| intent.bytes != bytes)
            {
                return Err(ReserveError::Oversized);
            }
        } else {
            // A changed payload for the same peer/frame family replaces the
            // prior family member. Preserve its queue position, age, and any
            // escrowed credit so periodic digest churn cannot repeatedly burn
            // a freshly charged intent and starve the family indefinitely.
            // Excess credit above the new size is abandoned, never refunded.
            let inherited = state
                .intents
                .iter()
                .find(|(candidate, _)| candidate.family == key.family)
                .map(|(candidate, intent)| (*candidate, *intent));
            let (order, created, credit) = match inherited {
                Some((previous_key, previous)) => {
                    state.intents.remove(&previous_key);
                    (
                        previous.order,
                        previous.created,
                        previous.charged.min(bytes),
                    )
                }
                None => {
                    let order = state.next_order;
                    state.next_order = state.next_order.wrapping_add(1);
                    (order, now, 0)
                }
            };
            if state.intents.len() >= DEFAULT_MAX_INTENTS {
                self.counters.queue_overflow.fetch_add(1, Ordering::Relaxed);
                return Err(ReserveError::IntentLimit);
            }
            state.intents.insert(
                key,
                Intent {
                    bytes,
                    created,
                    order,
                    charged: credit,
                },
            );
            self.counters
                .demanded_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
        self.charge_ready_recovery(&mut state);
        if !state
            .intents
            .get(&key)
            .is_some_and(|intent| intent.charged >= intent.bytes)
        {
            return Err(ReserveError::Deferred);
        }
        state.intents.remove(&key);
        Ok(ByteReservation { key, generation })
    }

    fn cancel_intent(&self, key: RecoveryIntentKey, generation: u64) {
        let mut state = self.lock_state();
        if state.generation == generation {
            state.intents.remove(&key);
        }
    }

    #[cfg(test)]
    pub(crate) fn refill_after_for_test(&self, elapsed: Duration) {
        let now = Instant::now();
        let mut state = self.lock_state();
        state.last_refill = now.checked_sub(elapsed).unwrap_or(now);
        self.refill(&mut state, now);
    }

    #[cfg(test)]
    pub(crate) fn cancel_intent_for_test(&self, key: RecoveryIntentKey) {
        let generation = self.lock_state().generation;
        self.cancel_intent(key, generation);
    }

    pub(crate) fn validate_reservation(
        &self,
        key: Option<RecoveryIntentKey>,
        reservation: Option<ByteReservation>,
    ) -> bool {
        let state = self.lock_state();
        if state.config.is_none() {
            return reservation.is_none();
        }
        let valid = key
            .zip(reservation)
            .is_some_and(|(key, owned)| owned.matches(key, state.generation));
        if !valid {
            self.counters
                .invariant_violations
                .fetch_add(1, Ordering::Relaxed);
        }
        valid
    }

    pub(crate) fn record_send_outcome(
        &self,
        topic: [u8; 32],
        purpose: &'static str,
        frame_bytes: usize,
        sent: bool,
    ) {
        if !self.enabled() {
            return;
        }
        if sent {
            self.counters.sent_bytes.fetch_add(
                u64::try_from(frame_bytes).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        } else {
            self.counters.send_failures.fetch_add(1, Ordering::Relaxed);
        }
        let mut counters = self
            .purpose_counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if counters.len() >= MAX_PURPOSE_ROWS && !counters.contains_key(&(topic, purpose)) {
            self.counters.queue_overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let entry = counters.entry((topic, purpose)).or_default();
        if sent {
            entry.sent_bytes = entry
                .sent_bytes
                .saturating_add(u64::try_from(frame_bytes).unwrap_or(u64::MAX));
        } else {
            entry.send_failures = entry.send_failures.saturating_add(1);
        }
    }

    #[cfg(test)]
    fn intent(&self, key: RecoveryIntentKey) -> Option<(u64, Instant)> {
        self.lock_state()
            .intents
            .get(&key)
            .map(|intent| (intent.bytes, intent.created))
    }

    #[cfg(test)]
    pub(crate) fn refill_one_second_for_test(&self) {
        self.lock_state().last_refill = Instant::now() - Duration::from_secs(1);
    }
}

struct IntentGuard {
    limiter: Arc<LeafEgressLimiter>,
    key: RecoveryIntentKey,
    generation: u64,
    armed: bool,
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        if self.armed {
            self.limiter.cancel_intent(self.key, self.generation);
        }
    }
}

struct WaiterPermit {
    limiter: Arc<LeafEgressLimiter>,
    peer: [u8; 32],
    _global: OwnedSemaphorePermit,
}

impl Drop for WaiterPermit {
    fn drop(&mut self) {
        let mut state = self.limiter.lock_state();
        if let Some(count) = state.waiters_by_peer.get_mut(&self.peer) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.waiters_by_peer.remove(&self.peer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> RecoveryIntentKey {
        RecoveryIntentKey {
            peer: [seed; 32],
            family: [seed; 32],
            operation: [seed; 32],
        }
    }

    fn limiter() -> Arc<LeafEgressLimiter> {
        let limiter = Arc::new(LeafEgressLimiter::disabled());
        assert!(limiter.configure(Some(LeafEgressConfig {
            soft_bytes_per_second: 64,
            hard_bytes_per_second: 128,
            burst_bytes: 4096,
            max_serialized_frame_bytes: 4096,
        })));
        limiter
    }

    #[test]
    fn concurrent_data_reservations_preserve_recovery_capacity() {
        let limiter = limiter();
        let mut workers = Vec::new();
        for seed in 0..16 {
            let limiter = Arc::clone(&limiter);
            workers.push(std::thread::spawn(move || {
                limiter.try_reserve_data(key(seed), 512, false)
            }));
        }
        let admitted = workers
            .into_iter()
            .filter_map(|worker| worker.join().ok().and_then(Result::ok))
            .count();
        assert_eq!(admitted, 6);
        assert_eq!(limiter.snapshot().charged_bytes, 3072);
    }

    #[test]
    fn pending_recovery_above_fixed_reserve_accumulates_under_data_demand() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        let recovery = key(2);
        let created = Instant::now();
        assert_eq!(
            limiter.try_reserve_recovery_at(recovery, 1536, 1, created),
            Err(ReserveError::Deferred)
        );
        {
            let mut state = limiter.lock_state();
            state.last_refill = Instant::now() - Duration::from_secs(4);
        }
        assert_eq!(
            limiter.try_reserve_data(key(3), 1, false),
            Err(ReserveError::Deferred),
            "continuous data must leave the pending recovery's exact token requirement"
        );
        assert!(limiter.try_reserve_recovery(recovery, 1536).is_ok());
    }

    #[tokio::test]
    async fn timeout_releases_waiter_and_owned_retry_reaches_32_second_eligibility(
    ) -> Result<(), &'static str> {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(limiter.try_reserve_recovery(key(3), 1024).is_ok());
        let recovery = key(2);
        assert_eq!(
            limiter
                .reserve_recovery(recovery, 4096, Duration::ZERO, true)
                .await,
            Err(ReserveError::Deferred)
        );
        assert_eq!(limiter.waiters.available_permits(), DEFAULT_MAX_WAITERS);
        assert!(limiter.lock_state().waiters_by_peer.is_empty());
        let (_, created) = limiter.intent(recovery).ok_or("persistent intent")?;
        assert!(limiter
            .try_reserve_recovery_at(recovery, 4096, 1, created + Duration::from_secs(31))
            .is_err());
        assert!(limiter
            .try_reserve_recovery_at(recovery, 4096, 1, created + Duration::from_secs(32))
            .is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn aborted_waiter_cancels_generation_owned_intent() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        let recovery = key(8);
        let task_limiter = Arc::clone(&limiter);
        let task = tokio::spawn(async move {
            task_limiter
                .reserve_recovery(recovery, 4096, Duration::from_secs(30), false)
                .await
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        assert!(limiter.intent(recovery).is_none());
    }

    #[test]
    fn older_large_recovery_precedes_recurring_small_recovery() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        let large = key(9);
        let small = key(10);
        let now = Instant::now();
        assert_eq!(
            limiter.try_reserve_recovery_at(large, 1536, 1, now),
            Err(ReserveError::Deferred)
        );
        assert_eq!(
            limiter.try_reserve_recovery_at(small, 1, 1, now),
            Err(ReserveError::Deferred)
        );
        assert!(limiter
            .try_reserve_recovery_at(large, 1536, 1, now + Duration::from_secs(4))
            .is_ok());
        assert!(limiter
            .try_reserve_recovery_at(small, 1, 1, now + Duration::from_secs(5))
            .is_ok());
    }

    #[test]
    fn abandoned_head_is_escrowed_and_does_not_block_following_recovery() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        let abandoned = key(11);
        let follower = key(12);
        let now = Instant::now();
        assert_eq!(
            limiter.try_reserve_recovery_at(abandoned, 1536, 1, now),
            Err(ReserveError::Deferred)
        );
        assert_eq!(
            limiter.try_reserve_recovery_at(follower, 1, 1, now),
            Err(ReserveError::Deferred)
        );
        assert!(limiter
            .try_reserve_recovery_at(follower, 1, 1, now + Duration::from_secs(5))
            .is_ok());
        assert!(
            limiter.intent(abandoned).is_some(),
            "abandoned charged credit remains bounded metadata until expiry/cancellation"
        );
        let tokens_before_cancel = limiter.lock_state().tokens;
        limiter.cancel_intent(abandoned, 1);
        assert_eq!(limiter.lock_state().tokens, tokens_before_cancel);
    }

    #[test]
    fn purpose_rows_are_bounded_and_overflow_remains_aggregate_visible() {
        let limiter = limiter();
        for index in 0..=MAX_PURPOSE_ROWS {
            let mut topic = [0; 32];
            topic[..8].copy_from_slice(&(index as u64).to_le_bytes());
            limiter.record_purpose_demand(topic, "EAGER", 1, false);
        }
        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.by_topic_and_purpose.len(), MAX_PURPOSE_ROWS);
        assert_eq!(snapshot.queue_overflow, 1);
    }

    #[test]
    fn identical_configuration_is_noop_and_generation_invalidates_old_reservation(
    ) -> Result<(), &'static str> {
        let limiter = limiter();
        let reservation = limiter
            .try_reserve_data(key(3), 1, false)
            .map_err(|_| "reservation")?;
        let config = limiter.lock_state().config.ok_or("config")?;
        assert!(limiter.configure(Some(config)));
        assert!(limiter.validate_reservation(Some(key(3)), Some(reservation)));
        let old = limiter
            .try_reserve_data(key(4), 1, false)
            .map_err(|_| "reservation")?;
        assert!(limiter.configure(None));
        assert!(!limiter.validate_reservation(Some(key(4)), Some(old)));
        Ok(())
    }

    #[test]
    fn invalid_reconfiguration_preserves_active_config() {
        let limiter = limiter();
        assert!(!limiter.configure(Some(LeafEgressConfig {
            soft_bytes_per_second: 129,
            hard_bytes_per_second: 128,
            burst_bytes: 4096,
            max_serialized_frame_bytes: 4096,
        })));
        assert!(limiter.enabled());
    }

    #[test]
    fn zero_hard_rate_explicitly_disables_existing_policy() {
        let limiter = limiter();
        assert!(limiter.configure(Some(LeafEgressConfig {
            soft_bytes_per_second: 0,
            hard_bytes_per_second: 0,
            burst_bytes: 0,
            max_serialized_frame_bytes: 0,
        })));
        assert!(!limiter.enabled());
        assert_eq!(
            limiter.try_reserve_data(key(1), 1, false),
            Err(ReserveError::Disabled)
        );
    }

    fn digest_key(operation: u8) -> RecoveryIntentKey {
        RecoveryIntentKey {
            peer: [7; 32],
            family: [7; 32],
            operation: [operation; 32],
        }
    }

    #[test]
    fn changing_digest_churn_obtains_reservations_without_starving_other_families() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        // Drain the remaining burst floor so digest intents start at zero.
        assert!(limiter.try_reserve_recovery(key(2), 1024).is_ok());
        assert_eq!(limiter.lock_state().tokens, 0);
        let now = Instant::now();
        assert_eq!(
            limiter.try_reserve_recovery_at(digest_key(1), 128, 1, now),
            Err(ReserveError::Deferred)
        );
        // One frame of refill (128 bytes at 128 B/s) per interval, with the
        // digest payload changing each interval: a new operation key in the
        // same family. The round that charges the predecessor must hand its
        // escrow to the replacement instead of burning it, so every churn
        // round actually obtains a reservation rather than starving forever.
        for round in 1u8..=4 {
            let at = now + Duration::from_secs(u64::from(round));
            assert!(
                limiter
                    .try_reserve_recovery_at(digest_key(round + 1), 128, 1, at)
                    .is_ok(),
                "replaced digest must obtain a reservation at one-frame refill rate"
            );
        }
        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.charged_bytes, 3072 + 1024 + 4 * 128);
        assert_eq!(snapshot.pending_recovery_intents, 0);
        // A different family is not starved by the churn above.
        assert!(limiter
            .try_reserve_recovery_at(key(9), 64, 1, now + Duration::from_secs(6))
            .is_ok());
    }

    #[test]
    fn family_replacement_resize_charges_only_the_size_delta() {
        let grown = limiter();
        assert!(grown.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(grown.try_reserve_recovery(key(2), 1024).is_ok());
        assert_eq!(grown.lock_state().tokens, 0);
        let now = Instant::now();
        assert_eq!(
            grown.try_reserve_recovery_at(digest_key(1), 128, 1, now),
            Err(ReserveError::Deferred)
        );
        // Larger replacement: one frame of refill escrows the predecessor's
        // 128 bytes; the successor still needs one more frame for the delta.
        assert_eq!(
            grown.try_reserve_recovery_at(digest_key(2), 256, 1, now + Duration::from_secs(1)),
            Err(ReserveError::Deferred)
        );
        assert_eq!(grown.snapshot().charged_bytes, 3072 + 1024 + 128);
        assert!(grown
            .try_reserve_recovery_at(digest_key(2), 256, 1, now + Duration::from_secs(2))
            .is_ok());
        assert_eq!(grown.snapshot().charged_bytes, 3072 + 1024 + 256);
        assert_eq!(grown.lock_state().tokens, 0);

        // Smaller replacement: one exact frame of refill (128 bytes at 128 B/s)
        // escrows the predecessor's full 128 bytes, leaving zero tokens; the
        // 64-byte successor is fully covered by the carried credit, spends
        // nothing extra, and the abandoned 64 bytes are never refunded.
        let shrunk = limiter();
        let shrunk_now = Instant::now();
        assert!(shrunk.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(shrunk.try_reserve_recovery(key(2), 1024).is_ok());
        assert_eq!(
            shrunk.try_reserve_recovery_at(digest_key(1), 128, 1, shrunk_now),
            Err(ReserveError::Deferred)
        );
        assert!(shrunk
            .try_reserve_recovery_at(digest_key(2), 64, 1, shrunk_now + Duration::from_secs(1))
            .is_ok());
        assert_eq!(shrunk.snapshot().charged_bytes, 3072 + 1024 + 128);
        assert_eq!(shrunk.lock_state().tokens, 0);
    }

    #[test]
    fn family_replacement_records_full_demand_but_charges_only_unpaid_bytes() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(limiter.try_reserve_recovery(key(2), 1024).is_ok());
        let now = Instant::now();
        assert_eq!(
            limiter.try_reserve_recovery_at(digest_key(1), 128, 1, now),
            Err(ReserveError::Deferred)
        );
        assert_eq!(
            limiter.try_reserve_recovery_at(digest_key(2), 256, 1, now + Duration::from_secs(1)),
            Err(ReserveError::Deferred)
        );
        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.demanded_bytes, 3072 + 1024 + 128 + 256);
        assert_eq!(snapshot.charged_bytes, 3072 + 1024 + 128);
    }
}
