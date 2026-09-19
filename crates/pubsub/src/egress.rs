//! Optional serialized-byte enforcement for Leaf PubSub nodes.
//!
//! Disabled configuration preserves the historical send path. Enabled callers
//! reserve the final serialized frame before acquiring transport admission.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const RECOVERY_RESERVE_PERCENT: u64 = 25;
/// Ceiling on the recovery escrow, as a percentage of `burst_bytes`. Without
/// it a single large — or continuously renewed — recovery intent escrows the
/// whole burst and denies every data frame for as long as it is outstanding.
const RECOVERY_RESERVE_MAX_PERCENT: u64 = 50;
const DEFAULT_MAX_WAITERS: usize = 256;
const DEFAULT_MAX_WAITERS_PER_PEER: usize = 8;
const DEFAULT_MAX_INTENTS: usize = 1024;
const CRITICAL_MAX_WAITERS: usize = 16;
const CRITICAL_MAX_INTENTS: usize = 64;
const ORDINARY_MAX_WAITERS: usize = DEFAULT_MAX_WAITERS - CRITICAL_MAX_WAITERS;
const ORDINARY_MAX_INTENTS: usize = DEFAULT_MAX_INTENTS - CRITICAL_MAX_INTENTS;
const ORDINARY_MAX_WAITERS_PER_PEER: usize = DEFAULT_MAX_WAITERS_PER_PEER - 1;
const MAX_PURPOSE_ROWS: usize = 1024;
const RECOVERY_INTENT_MAX_AGE: Duration = Duration::from_secs(super::MAX_CACHE_AGE_SECS);
/// Absolute ceiling on one intent's lifetime, independent of renewal. Without
/// it a continuously re-observed intent renews `last_observed` on every poll
/// and never ages out, holding its escrow indefinitely.
const RECOVERY_INTENT_MAX_LIFETIME: Duration =
    Duration::from_secs(super::MAX_CACHE_AGE_SECS.saturating_mul(2));
const RECOVERY_CRITICAL_SLOTS: u8 = 7;
const RECOVERY_TOTAL_SLOTS: u8 = RECOVERY_CRITICAL_SLOTS + 1;
const RECOVERY_MAX_QUANTUM_BYTES: u64 = 16 * 1024;

/// What the Leaf serialized-byte budget is allowed to *do* once it is exceeded.
///
/// #504: a configured budget is an observation instrument by default. Operators
/// have to ask for shedding explicitly, because dropping gossip is a behaviour
/// change that has to be attributable to a deliberate decision rather than to
/// the side effect of setting a non-zero rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BytePolicy {
    /// Account and meter every send, but never deny one. The default: a
    /// non-zero `hard_bytes_per_second` alone must never start dropping
    /// traffic.
    #[default]
    ObserveOnly,
    /// Shed *forwarded* Normal and Bulk traffic once the budget is exhausted.
    ///
    /// Never sheds: `TopicPriority::Critical` topics (DM inbox and the control
    /// plane), locally originated publishes, own-inbox delivery, and targeted
    /// sends. Those classes are either the node's own speech or the traffic
    /// whose loss is a hard error, so a byte budget is not permitted to be the
    /// thing that silences them.
    ShedNormal,
}

/// Leaf-only serialized PubSub egress policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafEgressConfig {
    /// Early degradation rate for relayed EAGER traffic. Zero disables it.
    pub soft_bytes_per_second: u64,
    /// Sustained hard rate. Zero disables accounting completely.
    ///
    /// A non-zero value turns the budget *on as a meter*. It does not by
    /// itself authorise shedding — see [`LeafEgressConfig::policy`].
    pub hard_bytes_per_second: u64,
    /// Short-spike capacity. Must cover the largest accepted frame.
    pub burst_bytes: u64,
    /// Largest final serialized frame accepted while enforcement is enabled.
    pub max_serialized_frame_bytes: usize,
    /// Whether an exhausted budget may actually deny a send. Defaults to
    /// [`BytePolicy::ObserveOnly`].
    pub policy: BytePolicy,
}

impl Default for LeafEgressConfig {
    /// Accounting off (`hard_bytes_per_second == 0`) and enforcement opt-in.
    ///
    /// Exists so a consumer can write `LeafEgressConfig { .. Default::default() }`
    /// and pick up new fields without a source break, and so the default of
    /// every field is the inert one.
    fn default() -> Self {
        Self {
            soft_bytes_per_second: 0,
            hard_bytes_per_second: 0,
            burst_bytes: 0,
            max_serialized_frame_bytes: 0,
            policy: BytePolicy::ObserveOnly,
        }
    }
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
    pub(crate) scope: [u8; 32],
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
    last_observed: Instant,
    /// When this intent first entered the map. `last_observed` is renewed on
    /// every observation, so an owner that keeps re-requesting the same frame
    /// can hold its escrow forever; the absolute lifetime bounds that.
    created_at: Instant,
    order: u64,
    charged: u64,
    class: RecoveryClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryClass {
    Ordinary,
    CriticalEager,
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
    waiters_by_peer: HashMap<[u8; 32], (usize, usize)>,
    next_order: u64,
    recovery_quantum: u64,
    recovery_slot: u8,
    recovery_slot_remaining: u64,
    ordinary_scopes: VecDeque<[u8; 32]>,
    ordinary_scope_counts: HashMap<[u8; 32], usize>,
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
    shed_suppressed: AtomicU64,
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
    /// Sends the budget would have denied but that policy protected: either
    /// [`BytePolicy::ObserveOnly`] is in force, or the traffic is
    /// Critical-class/local-origin under [`BytePolicy::ShedNormal`]. This is
    /// the headroom an operator would buy by enabling shedding, and it is the
    /// only signal that an ObserveOnly budget is being exceeded at all.
    ///
    /// It is a *lower bound*, not an exact count. "Would have been denied" is
    /// evaluated with the data-path predicate (the hard bucket and the
    /// recovery escrow) plus the frame-size cap. It does not model the
    /// recovery path's waiter-slot and intent-limit refusals, nor the relay
    /// soft bucket, so a send those would have denied while the hard bucket
    /// had room is not counted. Read it as "at least this much pressure",
    /// which is what it is used for; it is not a shed-count forecast.
    pub shed_suppressed: u64,
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
    ordinary_waiters: Arc<Semaphore>,
    critical_waiters: Arc<Semaphore>,
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
                recovery_quantum: 1,
                recovery_slot: 0,
                recovery_slot_remaining: 1,
                ordinary_scopes: VecDeque::new(),
                ordinary_scope_counts: HashMap::new(),
            }),
            ordinary_waiters: Arc::new(Semaphore::new(ORDINARY_MAX_WAITERS)),
            critical_waiters: Arc::new(Semaphore::new(CRITICAL_MAX_WAITERS)),
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

    /// Whether an exhausted budget is permitted to actually deny a send.
    ///
    /// #504: enforcement is opt-in. `enabled()` only means "accounting is on";
    /// a configured non-zero hard rate must never start shedding gossip by
    /// itself, because that would make a behaviour change an accident of
    /// configuration rather than a decision. Callers still consult per-message
    /// protection (Critical-class, local origin) on top of this.
    pub(crate) fn enforcing(&self) -> bool {
        self.lock_state()
            .config
            .is_some_and(|config| config.policy == BytePolicy::ShedNormal)
    }

    /// Charge a send that policy protects from shedding, and mint its
    /// reservation.
    ///
    /// A protected send must still carry a *real* reservation. The transport
    /// fence (`validate_reservation`) rejects an unreserved send whenever a
    /// config exists, so admitting a protected peer with `None` would convert
    /// "never shed" into a hard error one stage later — the frame would never
    /// reach the wire, the peer would be booked as timed out, and the lazy
    /// recovery custody would be skipped too.
    ///
    /// Overspend is the point: the bucket is a meter for this send, so the
    /// full demand is charged and the bucket is allowed to floor at zero
    /// rather than deny. This never waits and never registers a recovery
    /// intent, so a protected send cannot stall on the waiter loop or leave
    /// escrowed residue behind.
    ///
    /// `max_serialized_frame_bytes` is deliberately bypassed too. It is a
    /// byte-budget ceiling, not a protocol limit, so honouring it here would
    /// reintroduce exactly the drop this path exists to prevent — an oversized
    /// DM would be silently discarded rather than delivered. An oversized
    /// protected frame is counted in `shed_suppressed` like any other overrun,
    /// so the cap being exceeded stays visible.
    ///
    /// Returns `None` only when the limiter is disabled, where an unreserved
    /// send is what the fence expects. A send that the budget *would* have
    /// denied is counted in `shed_suppressed`, so an operator can see how much
    /// headroom enabling `ShedNormal` would actually buy.
    pub(crate) fn reserve_protected(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        relayed: bool,
    ) -> Option<ByteReservation> {
        let mut state = self.lock_state();
        self.refill(&mut state, Instant::now());
        let config = state.config?;
        let bytes = u64::try_from(frame_bytes).unwrap_or(u64::MAX);
        self.counters
            .demanded_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        if frame_bytes > config.max_serialized_frame_bytes
            || Self::data_hard_denied(&state, &config, bytes)
        {
            self.counters
                .shed_suppressed
                .fetch_add(1, Ordering::Relaxed);
        }
        state.tokens = state.tokens.saturating_sub(bytes);
        // The soft bucket meters relayed EAGER only, matching
        // `try_reserve_data_at`; charging it for our own traffic would
        // understate the relay headroom.
        if relayed && config.soft_bytes_per_second > 0 {
            state.soft_tokens = state.soft_tokens.saturating_sub(bytes);
        }
        self.counters
            .charged_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        Some(ByteReservation {
            key,
            generation: state.generation,
        })
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
        state.ordinary_scopes.clear();
        state.ordinary_scope_counts.clear();
        state.recovery_quantum = validated.map_or(1, |config| {
            (config.hard_bytes_per_second / 8).clamp(1, RECOVERY_MAX_QUANTUM_BYTES)
        });
        state.recovery_slot = 0;
        state.recovery_slot_remaining = state.recovery_quantum;
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
            shed_suppressed: self.counters.shed_suppressed.load(Ordering::Relaxed),
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
        let expired: Vec<_> = state
            .intents
            .iter()
            .filter(|(_, intent)| {
                now.saturating_duration_since(intent.last_observed) >= RECOVERY_INTENT_MAX_AGE
                    || now.saturating_duration_since(intent.created_at)
                        >= RECOVERY_INTENT_MAX_LIFETIME
            })
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            if let Some(intent) = state.intents.remove(&key) {
                if intent.class == RecoveryClass::Ordinary && intent.charged < intent.bytes {
                    Self::remove_ordinary_pending(state, key.scope);
                }
            }
        }
        self.charge_ready_recovery(state);
    }

    fn add_ordinary_pending(state: &mut State, scope: [u8; 32]) {
        let count = state.ordinary_scope_counts.entry(scope).or_default();
        if *count == 0 {
            state.ordinary_scopes.push_back(scope);
        }
        *count = count.saturating_add(1);
    }

    fn remove_ordinary_pending(state: &mut State, scope: [u8; 32]) {
        let remove_scope = if let Some(count) = state.ordinary_scope_counts.get_mut(&scope) {
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if remove_scope {
            state.ordinary_scope_counts.remove(&scope);
            state
                .ordinary_scopes
                .retain(|candidate| *candidate != scope);
        }
    }

    fn advance_recovery_slot(state: &mut State) {
        state.recovery_slot = (state.recovery_slot + 1) % RECOVERY_TOTAL_SLOTS;
        state.recovery_slot_remaining = state.recovery_quantum;
    }

    /// Escrow recovery bytes in persistent 7:1 critical/ordinary byte slots.
    /// Idle-class slots are donated immediately and never accrue future credit.
    fn charge_ready_recovery(&self, state: &mut State) {
        while state.tokens > 0 {
            let has_critical = state.intents.values().any(|intent| {
                intent.class == RecoveryClass::CriticalEager && intent.charged < intent.bytes
            });
            let has_ordinary = !state.ordinary_scopes.is_empty();
            let class = match (has_critical, has_ordinary) {
                (false, false) => break,
                (true, false) => RecoveryClass::CriticalEager,
                (false, true) => RecoveryClass::Ordinary,
                (true, true) if state.recovery_slot < RECOVERY_CRITICAL_SLOTS => {
                    RecoveryClass::CriticalEager
                }
                (true, true) => RecoveryClass::Ordinary,
            };
            let ordinary_scope = (class == RecoveryClass::Ordinary)
                .then(|| state.ordinary_scopes.front().copied())
                .flatten();
            let head = state
                .intents
                .iter()
                .filter(|(key, intent)| {
                    intent.class == class
                        && intent.charged < intent.bytes
                        && ordinary_scope.is_none_or(|scope| key.scope == scope)
                })
                .min_by_key(|(_, intent)| intent.order)
                .map(|(key, intent)| (*key, intent.bytes - intent.charged));
            let Some((key, remaining)) = head else {
                if class == RecoveryClass::Ordinary {
                    if let Some(scope) = ordinary_scope {
                        state.ordinary_scope_counts.remove(&scope);
                        state.ordinary_scopes.pop_front();
                    }
                    continue;
                }
                break;
            };
            let delta = remaining
                .min(state.tokens)
                .min(state.recovery_slot_remaining);
            if delta == 0 {
                Self::advance_recovery_slot(state);
                continue;
            }
            state.tokens -= delta;
            state.recovery_slot_remaining -= delta;
            self.counters
                .charged_bytes
                .fetch_add(delta, Ordering::Relaxed);
            if let Some(intent) = state.intents.get_mut(&key) {
                intent.charged = intent.charged.saturating_add(delta);
            }
            if class == RecoveryClass::Ordinary && delta == remaining {
                Self::remove_ordinary_pending(state, key.scope);
            }
            let scope_empty = ordinary_scope
                .is_some_and(|scope| !state.ordinary_scope_counts.contains_key(&scope));
            if state.recovery_slot_remaining == 0 {
                if class == RecoveryClass::Ordinary && !scope_empty {
                    if let Some(scope) = state.ordinary_scopes.pop_front() {
                        state.ordinary_scopes.push_back(scope);
                    }
                }
                Self::advance_recovery_slot(state);
            }
        }
    }

    /// Tokens recovery has escrowed and data may not spend.
    ///
    /// Once recovery declares its exact serialized demand, data may still use
    /// surplus tokens but cannot repeatedly consume the tokens that operation
    /// needs. The escrow may grow past the fixed floor so a frame larger than
    /// it can accumulate without globally stopping unrelated data.
    ///
    /// It is capped, though: an intent whose remaining demand approaches
    /// `burst_bytes` would otherwise deny *every* data frame for as long as it
    /// is outstanding, and an owner that keeps re-observing the intent renews
    /// `last_observed` and so can hold that state indefinitely. The ceiling
    /// guarantees data always keeps a share of the burst.
    fn recovery_reserve(state: &State, config: &LeafEgressConfig) -> u64 {
        let floor = config.burst_bytes.saturating_mul(RECOVERY_RESERVE_PERCENT) / 100;
        let ceiling = config
            .burst_bytes
            .saturating_mul(RECOVERY_RESERVE_MAX_PERCENT)
            / 100;
        state
            .intents
            .values()
            .filter(|intent| intent.charged < intent.bytes)
            .map(|intent| intent.bytes - intent.charged)
            .max()
            .unwrap_or(floor)
            .max(floor)
            .min(ceiling.max(floor))
    }

    /// Whether the hard bucket would refuse `bytes` of data right now.
    fn data_hard_denied(state: &State, config: &LeafEgressConfig, bytes: u64) -> bool {
        let reserve = Self::recovery_reserve(state, config);
        state.tokens < bytes || state.tokens.saturating_sub(bytes) < reserve
    }

    pub(crate) fn try_reserve_data(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        relayed: bool,
    ) -> Result<ByteReservation, ReserveError> {
        self.try_reserve_data_at(key, frame_bytes, relayed, Instant::now())
    }

    fn try_reserve_data_at(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        relayed: bool,
        now: Instant,
    ) -> Result<ByteReservation, ReserveError> {
        let mut state = self.lock_state();
        self.refill(&mut state, now);
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
        let hard_denied = Self::data_hard_denied(&state, &config, bytes);
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

    fn waiter_permit(
        self: &Arc<Self>,
        peer: [u8; 32],
        class: RecoveryClass,
    ) -> Result<WaiterPermit, ReserveError> {
        let semaphore = match class {
            RecoveryClass::Ordinary => &self.ordinary_waiters,
            RecoveryClass::CriticalEager => &self.critical_waiters,
        };
        let global = Arc::clone(semaphore)
            .try_acquire_owned()
            .map_err(|_| ReserveError::WaiterLimit)?;
        {
            let mut state = self.lock_state();
            // Read before inserting: `entry().or_default()` on the refusal
            // path would leave a zero row behind for every peer that ever hit
            // the limit, keyed by peer and never swept.
            let counts = state
                .waiters_by_peer
                .get(&peer)
                .copied()
                .unwrap_or_default();
            let class_count = match class {
                RecoveryClass::Ordinary => counts.0,
                RecoveryClass::CriticalEager => counts.1,
            };
            let class_limit = match class {
                RecoveryClass::Ordinary => ORDINARY_MAX_WAITERS_PER_PEER,
                RecoveryClass::CriticalEager => 1,
            };
            if class_count >= class_limit || counts.0 + counts.1 >= DEFAULT_MAX_WAITERS_PER_PEER {
                return Err(ReserveError::WaiterLimit);
            }
            let counts = state.waiters_by_peer.entry(peer).or_default();
            match class {
                RecoveryClass::Ordinary => counts.0 += 1,
                RecoveryClass::CriticalEager => counts.1 += 1,
            }
        }
        Ok(WaiterPermit {
            limiter: Arc::clone(self),
            peer,
            class,
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
        self.reserve_recovery_class(
            key,
            frame_bytes,
            deadline,
            keep_intent_on_timeout,
            RecoveryClass::Ordinary,
        )
        .await
    }

    pub(crate) async fn reserve_critical_eager(
        self: &Arc<Self>,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        deadline: Duration,
    ) -> Result<ByteReservation, ReserveError> {
        self.reserve_recovery_class(
            key,
            frame_bytes,
            deadline,
            false,
            RecoveryClass::CriticalEager,
        )
        .await
    }

    async fn reserve_recovery_class(
        self: &Arc<Self>,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        deadline: Duration,
        keep_intent_on_timeout: bool,
        class: RecoveryClass,
    ) -> Result<ByteReservation, ReserveError> {
        if !self.enabled() {
            return Err(ReserveError::Disabled);
        }
        let _waiter = self.waiter_permit(key.peer, class)?;
        self.counters
            .recovery_waited
            .fetch_add(1, Ordering::Relaxed);
        let generation = self.lock_state().generation;
        let attempt = async {
            loop {
                match self.try_reserve_recovery_at_class(
                    key,
                    frame_bytes,
                    generation,
                    Instant::now(),
                    class,
                ) {
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
        self.try_reserve_recovery_at_class(
            key,
            frame_bytes,
            generation,
            now,
            RecoveryClass::Ordinary,
        )
    }

    fn try_reserve_recovery_at_class(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
        generation: u64,
        now: Instant,
        class: RecoveryClass,
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
            if class == RecoveryClass::CriticalEager {
                let critical_count = state
                    .intents
                    .values()
                    .filter(|intent| intent.class == RecoveryClass::CriticalEager)
                    .count();
                if state
                    .intents
                    .get(&key)
                    .is_some_and(|intent| intent.class != RecoveryClass::CriticalEager)
                {
                    if critical_count >= CRITICAL_MAX_INTENTS {
                        self.counters.queue_overflow.fetch_add(1, Ordering::Relaxed);
                        return Err(ReserveError::IntentLimit);
                    }
                    let was_pending_ordinary = state.intents.get(&key).is_some_and(|intent| {
                        intent.class == RecoveryClass::Ordinary && intent.charged < intent.bytes
                    });
                    if was_pending_ordinary {
                        Self::remove_ordinary_pending(&mut state, key.scope);
                    }
                    if let Some(intent) = state.intents.get_mut(&key) {
                        intent.class = RecoveryClass::CriticalEager;
                    }
                }
            }
            if state
                .intents
                .get(&key)
                .is_some_and(|intent| intent.bytes != bytes)
            {
                return Err(ReserveError::Oversized);
            }
            if let Some(intent) = state.intents.get_mut(&key) {
                intent.last_observed = now;
            }
        } else {
            // A changed payload for the same peer/frame family replaces the
            // prior family member. Preserve its queue position, lease, and any
            // escrowed credit so periodic digest churn cannot repeatedly burn
            // a freshly charged intent and starve the family indefinitely.
            // Excess credit above the new size is abandoned, never refunded.
            let inherited = state
                .intents
                .iter()
                .find(|(candidate, _)| candidate.family == key.family)
                .map(|(candidate, intent)| (*candidate, *intent));
            let (order, last_observed, created_at, credit, preserved_scope_position) =
                match inherited {
                    Some((previous_key, previous)) => {
                        state.intents.remove(&previous_key);
                        let credit = previous.charged.min(bytes);
                        let preserve_scope_position = previous.class == RecoveryClass::Ordinary
                            && class == RecoveryClass::Ordinary
                            && previous_key.scope == key.scope
                            && previous.charged < previous.bytes
                            && credit < bytes;
                        if previous.class == RecoveryClass::Ordinary
                            && previous.charged < previous.bytes
                            && !preserve_scope_position
                        {
                            Self::remove_ordinary_pending(&mut state, previous_key.scope);
                        }
                        (
                            previous.order,
                            now,
                            previous.created_at,
                            credit,
                            preserve_scope_position,
                        )
                    }
                    None => {
                        let order = state.next_order;
                        state.next_order = state.next_order.saturating_add(1);
                        (order, now, now, 0, false)
                    }
                };
            let class_count = state
                .intents
                .values()
                .filter(|intent| intent.class == class)
                .count();
            let class_limit = match class {
                RecoveryClass::Ordinary => ORDINARY_MAX_INTENTS,
                RecoveryClass::CriticalEager => CRITICAL_MAX_INTENTS,
            };
            if state.intents.len() >= DEFAULT_MAX_INTENTS || class_count >= class_limit {
                self.counters.queue_overflow.fetch_add(1, Ordering::Relaxed);
                return Err(ReserveError::IntentLimit);
            }
            state.intents.insert(
                key,
                Intent {
                    bytes,
                    last_observed,
                    created_at,
                    order,
                    charged: credit,
                    class,
                },
            );
            if class == RecoveryClass::Ordinary && credit < bytes && !preserved_scope_position {
                Self::add_ordinary_pending(&mut state, key.scope);
            }
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
            if let Some(intent) = state.intents.remove(&key) {
                if intent.class == RecoveryClass::Ordinary && intent.charged < intent.bytes {
                    Self::remove_ordinary_pending(&mut state, key.scope);
                }
            }
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

    #[cfg(test)]
    pub(crate) fn try_reserve_critical_for_test(
        &self,
        key: RecoveryIntentKey,
        frame_bytes: usize,
    ) -> Result<ByteReservation, ReserveError> {
        let generation = self.lock_state().generation;
        self.try_reserve_recovery_at_class(
            key,
            frame_bytes,
            generation,
            Instant::now(),
            RecoveryClass::CriticalEager,
        )
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
            .map(|intent| (intent.bytes, intent.last_observed))
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
    class: RecoveryClass,
    _global: OwnedSemaphorePermit,
}

impl Drop for WaiterPermit {
    fn drop(&mut self) {
        let mut state = self.limiter.lock_state();
        if let Some(counts) = state.waiters_by_peer.get_mut(&self.peer) {
            match self.class {
                RecoveryClass::Ordinary => counts.0 = counts.0.saturating_sub(1),
                RecoveryClass::CriticalEager => counts.1 = counts.1.saturating_sub(1),
            }
            if counts.0 + counts.1 == 0 {
                state.waiters_by_peer.remove(&self.peer);
            }
        }
    }
}

#[cfg(test)]
#[path = "egress_fairness_tests.rs"]
mod egress_fairness_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> RecoveryIntentKey {
        RecoveryIntentKey {
            peer: [seed; 32],
            scope: [0; 32],
            family: [seed; 32],
            operation: [seed; 32],
        }
    }

    fn indexed_key(index: usize) -> RecoveryIntentKey {
        let bytes = index.to_le_bytes();
        let mut operation = [0; 32];
        operation[..bytes.len()].copy_from_slice(&bytes);
        RecoveryIntentKey {
            peer: operation,
            scope: [0; 32],
            family: operation,
            operation,
        }
    }

    #[test]
    fn ordinary_metadata_saturation_preserves_critical_capacity() -> Result<(), ReserveError> {
        let limiter = limiter();
        let ordinary_waiters: Vec<_> = (0..ORDINARY_MAX_WAITERS)
            .map(|index| limiter.waiter_permit(indexed_key(index).peer, RecoveryClass::Ordinary))
            .collect::<Result<Vec<_>, _>>()?;
        assert!(matches!(
            limiter.waiter_permit([250; 32], RecoveryClass::Ordinary),
            Err(ReserveError::WaiterLimit)
        ));
        let critical_waiters: Vec<_> = (0..CRITICAL_MAX_WAITERS)
            .map(|index| {
                limiter.waiter_permit(indexed_key(1000 + index).peer, RecoveryClass::CriticalEager)
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(limiter.ordinary_waiters.available_permits(), 0);
        assert_eq!(limiter.critical_waiters.available_permits(), 0);
        assert!(matches!(
            limiter.waiter_permit([251; 32], RecoveryClass::CriticalEager),
            Err(ReserveError::WaiterLimit)
        ));
        drop(critical_waiters);
        drop(ordinary_waiters);

        let peer = [252; 32];
        let per_peer: Vec<_> = (0..ORDINARY_MAX_WAITERS_PER_PEER)
            .map(|_| limiter.waiter_permit(peer, RecoveryClass::Ordinary))
            .collect::<Result<Vec<_>, _>>()?;
        let critical = limiter.waiter_permit(peer, RecoveryClass::CriticalEager)?;
        assert!(matches!(
            limiter.waiter_permit(peer, RecoveryClass::Ordinary),
            Err(ReserveError::WaiterLimit)
        ));
        assert!(matches!(
            limiter.waiter_permit(peer, RecoveryClass::CriticalEager),
            Err(ReserveError::WaiterLimit)
        ));
        drop(critical);
        drop(per_peer);

        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(limiter.try_reserve_recovery(key(2), 1024).is_ok());
        let now = Instant::now();
        for index in 0..ORDINARY_MAX_INTENTS {
            assert_eq!(
                limiter.try_reserve_recovery_at(indexed_key(index + 10), 4096, 1, now),
                Err(ReserveError::Deferred)
            );
        }
        assert_eq!(limiter.lock_state().intents.len(), ORDINARY_MAX_INTENTS);
        let critical_key = indexed_key(ORDINARY_MAX_INTENTS + 20);
        assert_eq!(
            limiter.try_reserve_recovery_at_class(
                critical_key,
                896,
                1,
                now,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        );
        assert_eq!(limiter.lock_state().intents.len(), ORDINARY_MAX_INTENTS + 1);
        assert!(limiter
            .try_reserve_recovery_at_class(
                critical_key,
                896,
                1,
                now + Duration::from_secs(8),
                RecoveryClass::CriticalEager,
            )
            .is_ok());
        assert_eq!(limiter.lock_state().intents.len(), ORDINARY_MAX_INTENTS);
        Ok(())
    }

    #[test]
    fn ordinary_intent_promotion_respects_critical_intent_cap() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(limiter.try_reserve_recovery(key(2), 1024).is_ok());
        let now = Instant::now();
        let ordinary = indexed_key(5000);
        assert_eq!(
            limiter.try_reserve_recovery_at(ordinary, 4096, 1, now),
            Err(ReserveError::Deferred)
        );
        for index in 0..CRITICAL_MAX_INTENTS {
            assert_eq!(
                limiter.try_reserve_recovery_at_class(
                    indexed_key(6000 + index),
                    4096,
                    1,
                    now,
                    RecoveryClass::CriticalEager,
                ),
                Err(ReserveError::Deferred)
            );
        }
        assert_eq!(
            limiter.try_reserve_recovery_at_class(
                ordinary,
                4096,
                1,
                now,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::IntentLimit)
        );
        assert_eq!(
            limiter.lock_state().intents.get(&ordinary).map(|i| i.class),
            Some(RecoveryClass::Ordinary)
        );
    }

    fn limiter() -> Arc<LeafEgressLimiter> {
        let limiter = Arc::new(LeafEgressLimiter::disabled());
        assert!(limiter.configure(Some(LeafEgressConfig {
            soft_bytes_per_second: 64,
            hard_bytes_per_second: 128,
            burst_bytes: 4096,
            max_serialized_frame_bytes: 4096,
            policy: BytePolicy::ShedNormal,
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

    #[test]
    fn continuous_data_cannot_spend_critical_escrow() {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(limiter.try_reserve_recovery(key(2), 1024).is_ok());
        let critical = key(4);
        let start = Instant::now();
        assert_eq!(
            limiter.try_reserve_recovery_at_class(
                critical,
                512,
                1,
                start,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        );
        for second in 1..4 {
            assert_eq!(
                limiter.try_reserve_data_at(
                    indexed_key(8000 + second),
                    1,
                    false,
                    start + Duration::from_secs(second as u64),
                ),
                Err(ReserveError::Deferred)
            );
            assert_eq!(
                limiter.try_reserve_recovery_at_class(
                    critical,
                    512,
                    1,
                    start + Duration::from_secs(second as u64),
                    RecoveryClass::CriticalEager,
                ),
                Err(ReserveError::Deferred)
            );
        }
        assert!(limiter
            .try_reserve_recovery_at_class(
                critical,
                512,
                1,
                start + Duration::from_secs(4),
                RecoveryClass::CriticalEager,
            )
            .is_ok());
        assert_eq!(limiter.snapshot().charged_bytes, 4096 + 512);
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
        assert_eq!(
            limiter.ordinary_waiters.available_permits(),
            ORDINARY_MAX_WAITERS
        );
        assert_eq!(
            limiter.critical_waiters.available_permits(),
            CRITICAL_MAX_WAITERS
        );
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
    async fn aborted_waiter_cancels_generation_owned_intent(
    ) -> Result<(), tokio::time::error::Elapsed> {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        let recovery = key(8);
        let task_limiter = Arc::clone(&limiter);
        let task = tokio::spawn(async move {
            task_limiter
                .reserve_recovery(recovery, 4096, Duration::from_secs(30), false)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while limiter.ordinary_waiters.available_permits() == ORDINARY_MAX_WAITERS {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        task.abort();
        let _ = task.await;
        assert!(limiter.intent(recovery).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn aborted_critical_waiter_releases_reserved_metadata(
    ) -> Result<(), tokio::time::error::Elapsed> {
        let limiter = limiter();
        assert!(limiter.try_reserve_data(key(1), 3072, false).is_ok());
        assert!(limiter.try_reserve_recovery(key(3), 1024).is_ok());
        let recovery = key(9);
        let task_limiter = Arc::clone(&limiter);
        let task = tokio::spawn(async move {
            task_limiter
                .reserve_critical_eager(recovery, 4096, Duration::from_secs(30))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while limiter.critical_waiters.available_permits() == CRITICAL_MAX_WAITERS {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            limiter.critical_waiters.available_permits(),
            CRITICAL_MAX_WAITERS - 1
        );
        task.abort();
        let _ = task.await;
        assert!(limiter.intent(recovery).is_none());
        assert_eq!(
            limiter.critical_waiters.available_permits(),
            CRITICAL_MAX_WAITERS
        );
        Ok(())
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
            policy: BytePolicy::ShedNormal,
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
            policy: BytePolicy::ShedNormal,
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
            scope: [7; 32],
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
