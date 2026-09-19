//! Deterministic injected-clock regressions for recovery byte fairness.

use super::*;

fn fair_key(scope: u8, seed: u64) -> RecoveryIntentKey {
    let bytes = seed.to_le_bytes();
    let mut identity = [0_u8; 32];
    identity[..bytes.len()].copy_from_slice(&bytes);
    RecoveryIntentKey {
        peer: identity,
        scope: [scope; 32],
        family: identity,
        operation: identity,
    }
}

fn limiter(hard: u64, burst: u64) -> Arc<LeafEgressLimiter> {
    let limiter = Arc::new(LeafEgressLimiter::disabled());
    assert!(limiter.configure(Some(LeafEgressConfig {
        soft_bytes_per_second: 0,
        hard_bytes_per_second: hard,
        burst_bytes: burst,
        max_serialized_frame_bytes: usize::try_from(burst).unwrap_or(usize::MAX),
        policy: BytePolicy::ShedNormal,
    })));
    limiter
}

fn credit(limiter: &LeafEgressLimiter, key: RecoveryIntentKey) -> u64 {
    limiter
        .lock_state()
        .intents
        .get(&key)
        .map_or(0, |intent| intent.charged)
}

fn drain(limiter: &LeafEgressLimiter, start: Instant, frame: usize) {
    for index in 0..8 {
        assert!(limiter
            .try_reserve_recovery_at_class(
                fair_key(1, 1000 + index),
                frame,
                1,
                start,
                RecoveryClass::CriticalEager,
            )
            .is_ok());
    }
}

#[test]
fn sustained_critical_backlog_gives_ordinary_one_slot_in_eight() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let ordinary = fair_key(9, 9000);
    let critical = fair_key(1, 2000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(ordinary, 256, 1, start),
        Err(ReserveError::Deferred)
    ));
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            critical,
            4096,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::Deferred)
    ));
    assert!(matches!(
        limiter.try_reserve_recovery_at(ordinary, 256, 1, start + Duration::from_secs(1),),
        Err(ReserveError::Deferred)
    ));
    assert_eq!(credit(&limiter, ordinary), 128);
    for _ in 0..32 {
        assert!(matches!(
            limiter.try_reserve_recovery_at(ordinary, 256, 1, start + Duration::from_secs(1),),
            Err(ReserveError::Deferred)
        ));
    }
    assert_eq!(credit(&limiter, ordinary), 128);
    assert!(limiter
        .try_reserve_recovery_at(ordinary, 256, 1, start + Duration::from_secs(2),)
        .is_ok());
    assert_eq!(limiter.snapshot().charged_bytes, 4096 + 2048);
}

#[test]
fn field_rate_maximum_ordinary_never_blocks_critical_budget() {
    const HARD: u64 = 131_072;
    const BURST: u64 = 4 * 1024 * 1024;
    const STEP: Duration = Duration::from_millis(125);
    let limiter = limiter(HARD, BURST);
    let start = Instant::now();
    drain(&limiter, start, 512 * 1024);
    let ordinary = fair_key(2, 20_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(ordinary, BURST as usize, 1, start),
        Err(ReserveError::Deferred)
    ));
    let mut now = start;
    let mut ordinary_done = false;
    for sequence in 0..400_u64 {
        let critical = fair_key(1, 30_000 + sequence);
        let registered = now;
        assert!(matches!(
            limiter.try_reserve_recovery_at_class(
                critical,
                1024,
                1,
                now,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        ));
        let mut critical_done = false;
        for _ in 0..32 {
            now += STEP;
            if !ordinary_done
                && limiter
                    .try_reserve_recovery_at(ordinary, BURST as usize, 1, now)
                    .is_ok()
            {
                ordinary_done = true;
            }
            if limiter
                .try_reserve_recovery_at_class(critical, 1024, 1, now, RecoveryClass::CriticalEager)
                .is_ok()
            {
                critical_done = true;
                break;
            }
        }
        let critical_delay = now.saturating_duration_since(registered);
        let service_nanos = (1024_u64 * 1_000_000_000).div_ceil(HARD * 7 / 8);
        let conservative_bound = STEP + STEP + Duration::from_nanos(service_nanos);
        assert!(critical_done, "critical frame {sequence} missed its budget");
        assert!(critical_delay <= conservative_bound);
        assert!(critical_delay <= Duration::from_secs(4));
        if ordinary_done {
            break;
        }
    }
    assert!(
        ordinary_done,
        "4 MiB ordinary frame made no bounded progress"
    );
    let elapsed = now.saturating_duration_since(start);
    assert!(elapsed <= Duration::from_secs(40));
    assert!(limiter.snapshot().charged_bytes <= BURST + HARD * elapsed.as_secs() + HARD);
}

#[test]
fn ordinary_scopes_rotate_on_partial_slots() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let a1 = fair_key(2, 50_000);
    let a2 = fair_key(2, 50_001);
    let b = fair_key(3, 60_000);
    for key in [a1, a2, b] {
        assert!(matches!(
            limiter.try_reserve_recovery_at(key, 512, 1, start),
            Err(ReserveError::Deferred)
        ));
    }
    let critical = fair_key(1, 70_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            critical,
            4096,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::Deferred)
    ));
    assert!(matches!(
        limiter.try_reserve_recovery_at(a1, 512, 1, start + Duration::from_secs(1),),
        Err(ReserveError::Deferred)
    ));
    assert_eq!(credit(&limiter, a1), 128);
    assert!(matches!(
        limiter.try_reserve_recovery_at(a1, 512, 1, start + Duration::from_secs(2)),
        Err(ReserveError::Deferred)
    ));
    assert_eq!(credit(&limiter, b), 128);
    assert_eq!(credit(&limiter, a2), 0);
}

#[test]
fn partial_escrow_replacement_cancel_and_reconfigure_are_non_refunding() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let original = fair_key(2, 90_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(original, 512, 1, start),
        Err(ReserveError::Deferred)
    ));
    let critical = fair_key(1, 95_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            critical,
            4096,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::Deferred)
    ));
    assert!(matches!(
        limiter.try_reserve_recovery_at(original, 512, 1, start + Duration::from_secs(1),),
        Err(ReserveError::Deferred)
    ));
    let inherited = credit(&limiter, original);
    assert_eq!(inherited, 128);
    let scope_order_before: Vec<_> = limiter
        .lock_state()
        .ordinary_scopes
        .iter()
        .copied()
        .collect();
    let mut replacement = fair_key(2, 90_001);
    replacement.family = original.family;
    assert!(matches!(
        limiter.try_reserve_recovery_at(replacement, 1024, 1, start + Duration::from_secs(1)),
        Err(ReserveError::Deferred)
    ));
    assert_eq!(credit(&limiter, replacement), inherited);
    assert_eq!(
        limiter
            .lock_state()
            .ordinary_scopes
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        scope_order_before
    );
    let before_cancel = limiter.lock_state().tokens;
    limiter.cancel_intent(replacement, 1);
    assert_eq!(limiter.lock_state().tokens, before_cancel);
    let stale = fair_key(3, 100_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(stale, 1024, 1, start + Duration::from_secs(1)),
        Err(ReserveError::Deferred)
    ));
    assert!(limiter.configure(Some(LeafEgressConfig {
        soft_bytes_per_second: 0,
        hard_bytes_per_second: 2048,
        burst_bytes: 4096,
        max_serialized_frame_bytes: 4096,
        policy: BytePolicy::ShedNormal,
    })));
    assert!(matches!(
        limiter.try_reserve_recovery_at(stale, 1024, 1, start + Duration::from_secs(2)),
        Err(ReserveError::Reconfigured)
    ));
    assert!(limiter.lock_state().ordinary_scopes.is_empty());
}

#[test]
fn owner_observation_renews_lease_and_abandoned_partial_expires() {
    let limiter = limiter(8, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let owned = fair_key(2, 110_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(owned, 4096, 1, start),
        Err(ReserveError::Deferred)
    ));
    let critical = fair_key(1, 115_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            critical,
            4096,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::Deferred)
    ));
    let lease_secs = RECOVERY_INTENT_MAX_AGE.as_secs();
    let mut previous_credit = 0;
    let mut last_observed = start;
    for cycle in 1..=3 {
        let second = cycle * (lease_secs - 1);
        last_observed = start + Duration::from_secs(second);
        assert!(matches!(
            limiter.try_reserve_recovery_at(owned, 4096, 1, last_observed),
            Err(ReserveError::Deferred)
        ));
        assert!(matches!(
            limiter.try_reserve_recovery_at_class(
                critical,
                4096,
                1,
                last_observed,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        ));
        assert!(limiter.lock_state().intents.contains_key(&owned));
        let current_credit = credit(&limiter, owned);
        assert!(current_credit > previous_credit);
        previous_credit = current_credit;
    }
    let pump = fair_key(9, 120_000);
    let _ = limiter.try_reserve_data_at(
        pump,
        1,
        false,
        last_observed + RECOVERY_INTENT_MAX_AGE + Duration::from_secs(1),
    );
    assert!(!limiter.lock_state().intents.contains_key(&owned));
    assert!(limiter.lock_state().ordinary_scopes.is_empty());
    assert!(limiter.lock_state().ordinary_scope_counts.is_empty());
}

/// WHY: the recovery escrow may exceed the fixed 25% floor so a frame larger
/// than it can accumulate, but it must not be able to escrow the whole burst.
/// `data_hard_denied` denies whenever `tokens - bytes < reserve`, so an intent
/// whose remaining demand approaches `burst_bytes` would make that true for
/// every data frame, for as long as it is outstanding — and since every
/// observation renews `last_observed`, a polling owner can hold it there.
/// The ceiling guarantees data keeps a share of the burst.
///
/// NOTE: this bounds the *reserve*. It does not change `charge_ready_recovery`,
/// which still drains refilled tokens into outstanding intents; that is a
/// separate mechanism governed by the recovery quantum and slot fairness.
#[test]
fn a_large_recovery_intent_cannot_zero_the_data_lane() -> Result<(), &'static str> {
    const HARD: u64 = 131_072;
    const BURST: u64 = 4 * 1024 * 1024;
    let limiter = limiter(HARD, BURST);
    let start = Instant::now();
    // Spend the bucket so the whale below registers an intent it cannot
    // immediately charge, which is what puts escrow into the reserve.
    drain(&limiter, start, 512 * 1024);
    let whale = fair_key(9, 20_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(whale, BURST as usize, 1, start),
        Err(ReserveError::Deferred)
    ));

    let mut state = limiter.lock_state();
    let config = state.config.ok_or("configured")?;
    let outstanding = state
        .intents
        .get(&whale)
        .map(|intent| intent.bytes - intent.charged)
        .ok_or("the whale intent must be outstanding for this to be a real test")?;
    assert!(
        outstanding > BURST / 2,
        "precondition: the whale escrows more than the ceiling ({outstanding})"
    );

    let reserve = LeafEgressLimiter::recovery_reserve(&state, &config);
    assert!(
        reserve <= BURST / 2,
        "one intent must not escrow the whole burst: reserve {reserve} of {BURST}"
    );
    // Data is therefore still admissible once the bucket holds more than the
    // capped reserve. Without the cap the reserve would be the whale's full
    // outstanding demand and this would be denied at any bucket level.
    state.tokens = BURST;
    assert!(
        !LeafEgressLimiter::data_hard_denied(&state, &config, 64 * 1024),
        "data must be admissible once the bucket exceeds the capped reserve"
    );
    Ok(())
}

/// WHY: `last_observed` is renewed on every observation, so the idle-age sweep
/// alone can never evict an intent whose owner keeps re-requesting it. The
/// absolute lifetime is what bounds that.
///
/// The hard rate is 1 B/s so the intent can never finish charging and leave on
/// its own — completion, not eviction, would otherwise be what clears it, and
/// the test would pass with or without the fix.
#[test]
fn a_continuously_renewed_intent_still_ages_out() -> Result<(), &'static str> {
    const BURST: u64 = 4 * 1024 * 1024;
    let limiter = limiter(1, BURST);
    let start = Instant::now();
    drain(&limiter, start, 512 * 1024);
    let key = fair_key(4, 30_000);
    assert!(matches!(
        limiter.try_reserve_recovery_at(key, BURST as usize, 1, start),
        Err(ReserveError::Deferred)
    ));
    assert!(limiter.lock_state().intents.contains_key(&key));

    // Re-observe well inside the idle window, so the idle sweep never fires.
    let step = RECOVERY_INTENT_MAX_AGE / 2;
    let mut now = start;
    while now.duration_since(start) < RECOVERY_INTENT_MAX_LIFETIME + step {
        now += step;
        let _ = limiter.try_reserve_recovery_at(key, BURST as usize, 1, now);
    }
    // The owner is still asking, so an intent for this key still exists — but
    // it must be a *fresh* one. Eviction at the lifetime ceiling drops the old
    // intent and with it the escrow it had accumulated, so a polling owner
    // cannot hold escrowed tokens for the life of the process.
    let created_at = limiter
        .lock_state()
        .intents
        .get(&key)
        .map(|intent| intent.created_at)
        .ok_or("the owner is still requesting, so an intent exists")?;
    assert!(
        created_at > start,
        "renewal must not hold one intent, and its escrow, past the absolute lifetime"
    );
    Ok(())
}

// ---- Fair-share admission displacement (#504 review) ------------------
// The tests below pin the revised displacement contract (review r2):
// a single peer fairness dimension that cannot ping-pong, pending-only
// candidate counts, youngest-victim tiebreak, a per-class rate budget,
// bounded front-of-rotation promotion, class isolation, bookkeeping that
// matches a manual recount, and Sybil-churn bounds on incumbent service.

use std::collections::{HashMap, HashSet};

/// One intent identity with an explicit peer and a scope derived from a
/// counter, so tests can shape per-peer and per-scope bucket counts.
fn pinned_key(peer: u8, scope_index: &mut u32) -> RecoveryIntentKey {
    *scope_index += 1;
    let mut bytes = [0_u8; 32];
    bytes[..4].copy_from_slice(&scope_index.to_le_bytes());
    RecoveryIntentKey {
        peer: [peer; 32],
        scope: bytes,
        family: bytes,
        operation: bytes,
    }
}

/// One intent identity with an explicit peer and a fixed scope, for maps
/// that are peer-balanced but scope-imbalanced.
fn fixed_scope_key(peer: u8, scope: [u8; 32], seed: u64) -> RecoveryIntentKey {
    let mut bytes = [0_u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    RecoveryIntentKey {
        peer: [peer; 32],
        scope,
        family: bytes,
        operation: bytes,
    }
}

/// The rotation bookkeeping must equal a manual recount of pending
/// Ordinary intents, and the rotation must hold each scope at most once.
fn assert_rotation_bookkeeping_matches(limiter: &LeafEgressLimiter) {
    let state = limiter.lock_state();
    let mut expected: HashMap<[u8; 32], usize> = HashMap::new();
    for (key, intent) in &state.intents {
        if intent.class == RecoveryClass::Ordinary && intent.charged < intent.bytes {
            *expected.entry(key.scope).or_insert(0) += 1;
        }
    }
    assert_eq!(state.ordinary_scope_counts, expected);
    assert_eq!(
        state.ordinary_scopes.len(),
        state.ordinary_scope_counts.len(),
        "rotation length must equal the number of pending scopes"
    );
    let mut seen = HashSet::new();
    for scope in &state.ordinary_scopes {
        assert!(seen.insert(*scope), "scope appears twice in the rotation");
    }
}

/// Fill both classes to their caps: Ordinary with a dominant peer, then
/// Critical with 64 single-intent peers. Tokens are drained first so
/// nothing charges and every intent stays pending at zero credit.
fn pinned_imbalanced_classes(
    limiter: &LeafEgressLimiter,
    start: Instant,
) -> Vec<RecoveryIntentKey> {
    drain(limiter, start, 512);
    let mut scope_index: u32 = 0;
    let mut dominant = Vec::new();
    for _ in 0..600 {
        let key = pinned_key(0x10, &mut scope_index);
        dominant.push(key);
        assert!(matches!(
            limiter.try_reserve_recovery_at(key, 512, 1, start),
            Err(ReserveError::Deferred)
        ));
    }
    for _ in 0..359 {
        let key = pinned_key(0x20, &mut scope_index);
        assert!(matches!(
            limiter.try_reserve_recovery_at(key, 512, 1, start),
            Err(ReserveError::Deferred)
        ));
    }
    assert!(matches!(
        limiter.try_reserve_recovery_at(pinned_key(0x30, &mut scope_index), 512, 1, start),
        Err(ReserveError::Deferred)
    ));
    for index in 0..64 {
        let peer = 0x40 + index as u8;
        assert!(matches!(
            limiter.try_reserve_recovery_at_class(
                pinned_key(peer, &mut scope_index),
                512,
                1,
                start,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        ));
    }
    assert_eq!(limiter.lock_state().intents.len(), 1024);
    dominant
}

/// WHY: admission was first-come-first-served up to the class caps, so a
/// saturated class refused a newly observed eligible target with
/// `IntentLimit` regardless of byte budget — metadata starvation of new
/// targets under sustained load. Displacement admits the under-
/// represented peer by removing the most-represented peer's least-work
/// intent, and the newcomer takes over the FRONT of the ordinary
/// rotation: admission without prompt service would leave it waiting out
/// the full rotation behind the incumbents that saturated the class
/// (measured at position 959 of 960 with zero charged bytes after 120
/// simulated seconds, which is why front insertion is part of the fix).
#[test]
fn fair_displacement_admits_under_represented_peer_with_timely_service() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    let dominant = pinned_imbalanced_classes(&limiter, start);

    let mut newcomer_scope: u32 = 100_000;
    let newcomer = pinned_key(0x50, &mut newcomer_scope);
    assert!(matches!(
        limiter.try_reserve_recovery_at(newcomer, 128, 1, start),
        Err(ReserveError::Deferred)
    ));
    let snapshot = limiter.snapshot();
    assert_eq!(snapshot.intent_displaced, 1);
    assert_eq!(snapshot.queue_overflow, 0);
    assert_eq!(limiter.lock_state().intents.len(), 1024);

    // The victim is the dominant peer's least-work intent: everything was
    // charged zero at the same instant, so the YOUNGEST registration loses
    // (Reverse(created_at), then Reverse(order)).
    assert!(!limiter.lock_state().intents.contains_key(&dominant[599]));
    let dominant_peer = [0x10_u8; 32];
    let newcomer_peer = [0x50_u8; 32];
    let mut per_peer: HashMap<[u8; 32], usize> = HashMap::new();
    {
        let state = limiter.lock_state();
        for (key, intent) in &state.intents {
            if intent.class == RecoveryClass::Ordinary {
                *per_peer.entry(key.peer).or_insert(0) += 1;
            }
        }
    }
    assert_eq!(per_peer.get(&dominant_peer), Some(&599));
    assert_eq!(per_peer.get(&newcomer_peer), Some(&1));

    // The newcomer's scope took over the front of the rotation, and the
    // rotation bookkeeping still matches a manual recount.
    let (deque_len, position, intents) = limiter.ordinary_rotation_probe_for_test(newcomer.scope);
    assert_eq!(
        position,
        Some(0),
        "displacement hands over the rotation front"
    );
    assert_eq!(deque_len, 960);
    assert_eq!(intents, vec![(128, 0)]);
    assert_rotation_bookkeeping_matches(&limiter);

    // Timely service: a 128 B frame against a 128 B ordinary slot must
    // complete on the first slot visit, not after a 960-scope rotation.
    let mut completed_at = None;
    for second in 1..=8_u64 {
        if limiter
            .try_reserve_recovery_at(newcomer, 128, 1, start + Duration::from_secs(second))
            .is_ok()
        {
            completed_at = Some(second);
            break;
        }
    }
    assert_eq!(
        completed_at,
        Some(1),
        "a displacement-admitted frame must be served on the next slot"
    );
}

/// WHY: displacement is strictly imbalance-driven. In a balanced full
/// class every peer already holds its fair share, so a newcomer — from a
/// brand-new peer or from an at-fair-share incumbent — must still be
/// refused, `intent_displaced` must stay at zero, and the refusal must
/// surface in `queue_overflow` exactly as before.
#[test]
fn balanced_full_class_refuses_newcomers_without_displacing() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let mut scope_index: u32 = 0;
    for index in 0..64_u16 {
        let peer = 0x60 + u8::try_from(index).unwrap_or(0xff);
        assert!(matches!(
            limiter.try_reserve_recovery_at_class(
                pinned_key(peer, &mut scope_index),
                512,
                1,
                start,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        ));
    }
    assert_eq!(limiter.lock_state().intents.len(), 64);

    // A brand-new peer: n_new = 0 and n_max = 1, and 0 + 1 < 1 is false.
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            pinned_key(0xa0, &mut scope_index),
            512,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::IntentLimit)
    ));
    // An incumbent already at the fair share: n_new = 1 and 1 + 1 < 1 is
    // false, so the incumbent cannot displace anyone either.
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            pinned_key(0x60, &mut scope_index),
            512,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::IntentLimit)
    ));
    let snapshot = limiter.snapshot();
    assert_eq!(snapshot.intent_displaced, 0);
    assert_eq!(snapshot.queue_overflow, 2);
}

/// WHY: the strict-imbalance rule bounds total displacements. Each
/// admission moves one intent from the fullest peer to the hungry one,
/// so the gap shrinks by two per admission and displacement stops — and
/// stays stopped — once the newcomer would no longer be strictly below
/// the fullest bucket. A displaced peer re-registering while it holds the
/// maximum is refused, which is what makes ping-pong impossible rather
/// than merely unlikely. Attempts are spaced 2 s of virtual time apart —
/// the displacement rate limit admits one displacement per 2 s — and the
/// byte rate is 1 B/s so nothing ever charges and the pending-count
/// arithmetic stays exact.
#[test]
fn alternating_hungry_peers_displace_boundedly_without_thrash() {
    let limiter = limiter(1, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let mut scope_index: u32 = 0;
    let mut dominant = Vec::new();
    let mut now = start;
    for _ in 0..60 {
        let key = pinned_key(0x10, &mut scope_index);
        dominant.push(key);
        assert!(matches!(
            limiter.try_reserve_recovery_at_class(key, 4096, 1, now, RecoveryClass::CriticalEager,),
            Err(ReserveError::Deferred)
        ));
    }
    for _ in 0..4 {
        assert!(matches!(
            limiter.try_reserve_recovery_at_class(
                pinned_key(0x20, &mut scope_index),
                4096,
                1,
                now,
                RecoveryClass::CriticalEager,
            ),
            Err(ReserveError::Deferred)
        ));
    }

    let mut admissions = 0;
    loop {
        assert!(admissions < 200, "displacement must converge, not loop");
        now += Duration::from_secs(2);
        match limiter.try_reserve_recovery_at_class(
            pinned_key(0x20, &mut scope_index),
            4096,
            1,
            now,
            RecoveryClass::CriticalEager,
        ) {
            Err(ReserveError::Deferred) => admissions += 1,
            Err(ReserveError::IntentLimit) => break,
            other => panic!("hungry peer admission must defer or refuse, got {other:?}"),
        }
    }
    // From (60, 4): the i-th admission displaces while (4 + i) < (61 - i),
    // which holds through i = 28, leaving both peers at 32. One attempt
    // per 2 s of virtual time is exactly the sustained rate limit.
    assert_eq!(admissions, 28);
    let snapshot = limiter.snapshot();
    assert_eq!(snapshot.intent_displaced, 28);
    assert_eq!(snapshot.displacement_rate_limited, 0);
    assert_eq!(limiter.lock_state().intents.len(), 64);

    // The displaced peer cannot displace anything back while it holds the
    // maximum, and repeated polls of the refused key displace nothing new.
    // Re-poll at the same instant: the virtual clock must stay under the
    // 60 s idle age so the never-renewed fill is not swept mid-test.
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            dominant[59],
            4096,
            1,
            now,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::IntentLimit)
    ));
    assert_eq!(limiter.snapshot().intent_displaced, 28);
    assert_eq!(limiter.snapshot().displacement_rate_limited, 0);
}

/// WHY: the class partitions exist so ordinary pressure cannot silence
/// critical recovery and vice versa; displacement must respect the same
/// boundary. An ordinary displacement may only remove an ordinary
/// intent, and a refused critical admission (balanced critical class)
/// must remove nothing at all.
#[test]
fn ordinary_displacement_never_crosses_class_boundaries() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    let dominant = pinned_imbalanced_classes(&limiter, start);
    let mut critical_keys = Vec::new();
    {
        let state = limiter.lock_state();
        for key in state.intents.keys() {
            if state
                .intents
                .get(key)
                .is_some_and(|intent| intent.class == RecoveryClass::CriticalEager)
            {
                critical_keys.push(*key);
            }
        }
    }
    assert_eq!(critical_keys.len(), 64);

    // Ordinary overflow displaces an ordinary victim only.
    let mut newcomer_scope: u32 = 200_000;
    assert!(matches!(
        limiter.try_reserve_recovery_at(pinned_key(0x50, &mut newcomer_scope), 128, 1, start),
        Err(ReserveError::Deferred)
    ));
    assert_eq!(limiter.snapshot().intent_displaced, 1);
    {
        let state = limiter.lock_state();
        for key in &critical_keys {
            assert!(
                state.intents.contains_key(key),
                "critical intent must survive"
            );
        }
    }

    // A refused critical newcomer (critical is full of single-intent
    // peers, so no imbalance) must not remove any ordinary intent.
    let ordinary_before = {
        let state = limiter.lock_state();
        state
            .intents
            .values()
            .filter(|intent| intent.class == RecoveryClass::Ordinary)
            .count()
    };
    assert!(matches!(
        limiter.try_reserve_recovery_at_class(
            pinned_key(0xa0, &mut newcomer_scope),
            512,
            1,
            start,
            RecoveryClass::CriticalEager,
        ),
        Err(ReserveError::IntentLimit)
    ));
    let ordinary_after = {
        let state = limiter.lock_state();
        state
            .intents
            .values()
            .filter(|intent| intent.class == RecoveryClass::Ordinary)
            .count()
    };
    assert_eq!(ordinary_before, ordinary_after);
    assert!(!limiter.lock_state().intents.contains_key(&dominant[599]));
}

/// WHY (review P0): two fairness dimensions have no joint potential
/// function, so they could evict each other's owners in a cycle. The
/// schedule below is the reviewer's probe: eight peers hold 96 intents
/// each on one hot scope and two peers hold 96 each on unique scopes —
/// a peer-balanced map. Under the old peer+scope rules, an N
/// registration (fair-share peer, fresh scope) evicted a hot-scope
/// intent V via the scope dimension, and V's re-poll evicted N via the
/// peer dimension: 40 cycles, 80 displacements, zero progress. With the
/// single peer dimension both attempts are clean refusals — nothing is
/// evicted, no escrow burns, and charged work never regresses.
#[test]
fn fair_admission_cannot_ping_pong_across_dimensions() {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    drain(&limiter, start, 512);
    let hot_scope = [0xaa_u8; 32];
    let mut seed: u64 = 0;
    for peer in 0x10_u8..0x18 {
        for _ in 0..96 {
            seed += 1;
            assert!(matches!(
                limiter.try_reserve_recovery_at(
                    fixed_scope_key(peer, hot_scope, seed),
                    512,
                    1,
                    start
                ),
                Err(ReserveError::Deferred)
            ));
        }
    }
    let mut scope_index: u32 = 300_000;
    for peer in [0x18_u8, 0x19] {
        for _ in 0..96 {
            let key = pinned_key(peer, &mut scope_index);
            assert!(matches!(
                limiter.try_reserve_recovery_at(key, 512, 1, start),
                Err(ReserveError::Deferred)
            ));
        }
    }
    assert_eq!(limiter.lock_state().intents.len(), 960);

    // Alternate the two halves of the old cycle: N registers from a
    // fair-share peer with a fresh scope, then a hot-scope peer
    // re-registers a fresh operation on the hot scope. Every peer holds
    // 96 pending intents, so every attempt must be refused.
    let mut charged_before = limiter.snapshot().charged_bytes;
    for cycle in 0..40 {
        let newcomer = pinned_key(0x18, &mut scope_index);
        assert!(
            matches!(
                limiter.try_reserve_recovery_at(newcomer, 128, 1, start),
                Err(ReserveError::IntentLimit)
            ),
            "cycle {cycle}: a fair-share peer must not displace anyone"
        );
        seed += 1;
        let hot = fixed_scope_key(0x10, hot_scope, seed);
        assert!(
            matches!(
                limiter.try_reserve_recovery_at(hot, 128, 1, start),
                Err(ReserveError::IntentLimit)
            ),
            "cycle {cycle}: a max-holder must not displace anyone"
        );
        let charged_now = limiter.snapshot().charged_bytes;
        assert!(
            charged_now >= charged_before,
            "cycle {cycle}: charged work must never regress (churn burns escrow)"
        );
        charged_before = charged_now;
    }
    let snapshot = limiter.snapshot();
    assert_eq!(snapshot.intent_displaced, 0);
    assert_eq!(snapshot.displacement_rate_limited, 0);
    assert_eq!(snapshot.queue_overflow, 80);
    assert_eq!(limiter.lock_state().intents.len(), 960);
}

/// WHY: displacement must not strand the victim. Its owner observes
/// displacement exactly as it observes expiry — the key is gone from the
/// map, so the next poll re-registers through the ordinary back-of-queue
/// path — and both the re-registered operation and the victim peer's
/// remaining intents must still be able to complete.
#[test]
fn displaced_incumbent_re_enters_and_still_completes() -> Result<(), &'static str> {
    let limiter = limiter(1024, 4096);
    let start = Instant::now();
    let dominant = pinned_imbalanced_classes(&limiter, start);
    let mut newcomer_scope: u32 = 400_000;
    assert!(matches!(
        limiter.try_reserve_recovery_at(pinned_key(0x50, &mut newcomer_scope), 128, 1, start),
        Err(ReserveError::Deferred)
    ));
    assert!(!limiter.lock_state().intents.contains_key(&dominant[599]));

    // While the map is still saturated the victim's re-poll is a clean
    // refusal, identical to what an owner sees after expiry into a full
    // map: its peer holds the maximum, so the strict-imbalance rule that
    // admitted the newcomer now protects the newcomer. It must be a
    // refusal, never a panic or an invariant violation.
    assert!(matches!(
        limiter.try_reserve_recovery_at(dominant[599], 512, 1, start),
        Err(ReserveError::IntentLimit)
    ));
    assert_eq!(limiter.snapshot().queue_overflow, 1);

    // Once the never-renewed background ages out at the idle deadline,
    // the victim is re-admitted on its next poll — through the ordinary
    // back-of-queue path, never retaking the front the newcomer earned —
    // and both it and the peer's continuously polled surviving intent
    // complete.
    let sweep = RECOVERY_INTENT_MAX_AGE.as_secs();
    let mut re_admitted_at = None;
    let mut victim_done = None;
    let mut survivor_done = None;
    for second in 1..=(sweep + 20) {
        let now = start + Duration::from_secs(second);
        if re_admitted_at.is_none() || victim_done.is_none() {
            let outcome = limiter.try_reserve_recovery_at(dominant[599], 512, 1, now);
            // Admission is either a deferral or — if the emptied rotation
            // and freshly refilled bucket can serve it at once — a
            // completed reservation. Both are re-admission.
            if re_admitted_at.is_none() && !matches!(outcome, Err(ReserveError::IntentLimit)) {
                re_admitted_at = Some(second);
                if let Err(ReserveError::Deferred) = outcome {
                    let (deque_len, position, intents) =
                        limiter.ordinary_rotation_probe_for_test(dominant[599].scope);
                    assert_eq!(position, Some(deque_len - 1));
                    assert_eq!(intents, vec![(512, 0)]);
                }
            }
            if outcome.is_ok() {
                victim_done = Some(second);
            }
        }
        if survivor_done.is_none()
            && limiter
                .try_reserve_recovery_at(dominant[1], 512, 1, now)
                .is_ok()
        {
            survivor_done = Some(second);
        }
    }
    assert_eq!(
        re_admitted_at,
        Some(sweep),
        "the victim is re-admitted as soon as the idle sweep frees capacity"
    );
    let victim_done = victim_done.ok_or("re-admitted victim must complete")?;
    let survivor_done = survivor_done.ok_or("displaced peer's survivor must complete")?;
    assert!(victim_done >= sweep);
    assert!(victim_done <= sweep + 20 && survivor_done <= sweep + 20);
    assert_rotation_bookkeeping_matches(&limiter);
    Ok(())
}

/// WHY (review P1): displacement is a cost the displaced peer pays, and
/// fresh PeerIds are free, so an attacker that registers one fresh peer
/// with a maximum-size frame per second can otherwise invoke displacement
/// on every poll and burn incumbent service. The defense under test is the
/// pair of bounds from the revised design: the per-class token bucket (8
/// burst, then one displacement per 2 s of virtual time) and the
/// single-outstanding front-of-rotation promotion. The same incumbent
/// schedule runs twice — once with the attack, once without — and the
/// attacked arm must keep at least half of the untouched arm's completed
/// intents.
#[test]
fn sybil_fresh_peer_churn_is_rate_limited_and_bounded() {
    const HARD: u64 = 8192;
    const BURST: u64 = 65_536;
    const WINDOW: u32 = 120;
    fn run_schedule(attack: bool) -> (usize, u64, u64) {
        let limiter = limiter(HARD, BURST);
        // The incumbent peer fills the ordinary class with one-slot
        // frames; the first ~64 of them consume the burst and complete,
        // so the loop keeps admitting until the class is genuinely full.
        let mut scope_index: u32 = 0;
        while limiter.lock_state().intents.len() < ORDINARY_MAX_INTENTS {
            let _ = limiter.try_reserve_recovery(pinned_key(0x10, &mut scope_index), 1024);
        }
        for second in 0..WINDOW {
            limiter.refill_after_for_test(Duration::from_secs(1));
            if attack {
                // One fresh PeerId per second, maximum-size frame: it can
                // never complete within the window, so every admission is
                // pure cost to the incumbents.
                let mut identity = [0_u8; 32];
                identity[..4].copy_from_slice(&(0x5100_0000_u32 + second).to_le_bytes());
                let _ = limiter.try_reserve_recovery(
                    RecoveryIntentKey {
                        peer: identity,
                        scope: identity,
                        family: identity,
                        operation: identity,
                    },
                    BURST as usize,
                );
            }
        }
        // Completed intents stay in the map until claimed, so a scan
        // counts every incumbent that reached full charge.
        let completed = {
            let state = limiter.lock_state();
            state
                .intents
                .values()
                .filter(|intent| {
                    intent.class == RecoveryClass::Ordinary
                        && intent.charged >= intent.bytes
                        && intent.bytes == 1024
                })
                .count()
        };
        let snapshot = limiter.snapshot();
        (
            completed,
            snapshot.intent_displaced,
            snapshot.displacement_rate_limited,
        )
    }

    let (clean, clean_displaced, _) = run_schedule(false);
    let (attacked, displaced, rate_limited) = run_schedule(true);
    assert_eq!(
        clean_displaced, 0,
        "no fresh peers: nothing to displace for"
    );
    assert!(
        attacked * 2 >= clean,
        "incumbent completions under attack ({attacked}) must be >= 50% of the untouched figure ({clean})"
    );
    // 8-token burst plus one per 2 s over the window.
    assert!(
        displaced <= DISPLACEMENT_TOKEN_CAPACITY + u64::from(WINDOW) / 2,
        "displacements ({displaced}) exceed the rate budget"
    );
    assert!(
        rate_limited >= 1,
        "the rate limiter must engage under churn"
    );
}

/// WHY (review r2, residual b): the fresh-PeerId churn bound above never
/// re-polls incumbent keys, so it cannot see the degradation incumbents
/// suffer while actually polling (renewal and claim). This schedule keeps
/// the production 7:1 critical/ordinary split with a permanent critical
/// backlog, fills the ordinary class from ONE incumbent peer, and then
/// re-polls the rotation-front cohort every second while the attacker
/// registers one fresh PeerId with a maximum-size frame per second.
/// Measured on this schedule (virtual clock, no sleeps): 63 rotation-head
/// completions clean vs 61 under attack (96.8%) across 47 displacements —
/// milder than the reviewer's probe because these single-visit incumbent
/// frames keep the displacement victims (youngest pending, lowest charge)
/// behind the completing cohort. The residual is real but
/// schedule-dependent; this test records the number honestly and keeps a
/// deliberately weak bound so it guards the mechanism without pretending
/// the known issue is solved.
#[test]
fn re_polling_incumbents_under_fresh_peer_attack_still_complete() {
    const HARD: u64 = 8192;
    const BURST: u64 = 65_536;
    const WINDOW: u32 = 120;
    const COHORT: usize = 128;
    fn run_schedule(attack: bool) -> (usize, u64) {
        let limiter = limiter(HARD, BURST);
        let mut scope_index: u32 = 0;
        // Incumbent peer fills the ordinary class first; the very first
        // registration consumes the whole burst and completes, so the
        // loop keeps admitting until the class is genuinely full.
        let mut keys = Vec::new();
        while limiter.lock_state().intents.len() < ORDINARY_MAX_INTENTS {
            let key = pinned_key(0x10, &mut scope_index);
            keys.push(key);
            let _ = limiter.try_reserve_recovery(key, 1024);
        }
        // Permanent critical backlog added second (tokens are drained by
        // now, so nothing self-completes): 64 maximum-size intents that
        // cannot finish inside the window, forcing the production 7:1
        // slot split.
        while limiter.lock_state().intents.len() < DEFAULT_MAX_INTENTS {
            let _ = limiter.try_reserve_recovery_at_class(
                pinned_key(0x60, &mut scope_index),
                BURST as usize,
                1,
                Instant::now(),
                RecoveryClass::CriticalEager,
            );
        }
        let cohort: Vec<_> = keys[..COHORT].to_vec();
        let mut completions = 0;
        for second in 0..WINDOW {
            limiter.refill_after_for_test(Duration::from_secs(1));
            if attack {
                let mut identity = [0_u8; 32];
                identity[..4].copy_from_slice(&(0x7700_0000_u32 + second).to_le_bytes());
                let _ = limiter.try_reserve_recovery(
                    RecoveryIntentKey {
                        peer: identity,
                        scope: identity,
                        family: identity,
                        operation: identity,
                    },
                    BURST as usize,
                );
            }
            for key in &cohort {
                if limiter.try_reserve_recovery(*key, 1024).is_ok() {
                    completions += 1;
                }
            }
        }
        let snapshot = limiter.snapshot();
        (completions, snapshot.intent_displaced)
    }

    let (clean, clean_displaced) = run_schedule(false);
    let (attacked, attacked_displaced) = run_schedule(true);
    assert_eq!(
        clean_displaced, 0,
        "no fresh peers: nothing to displace for"
    );
    assert!(
        attacked >= 1,
        "incumbents must still complete under attack (clean {clean}, attacked {attacked})"
    );
    assert!(
        attacked * 4 >= clean,
        "rotation-head completions collapsed: clean {clean}, attacked {attacked}, \
         displaced {attacked_displaced} — update the recorded numbers if intentional"
    );
}
