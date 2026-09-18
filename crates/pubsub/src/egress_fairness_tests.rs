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
