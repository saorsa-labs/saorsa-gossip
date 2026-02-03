# Consensus Review: Coordinator Roles as Hints

**Date:** 2026-01-31
**Reviewer:** Claude (Manual Review)
**Status:** ✅ APPROVED - ALL ISSUES RESOLVED

## Executive Summary

The changes correctly implement the "hints only" philosophy for coordinator roles. All nodes are now treated equally by code, with roles only influencing sort order during selection. The documentation updates consistently reinforce the "measure, don't trust" principle.

## Changes Reviewed

| File | Lines Changed | Purpose |
|------|--------------|---------|
| `crates/coordinator/src/bootstrap.rs` | +22, -2 | Use all adverts, sort by hint weight |
| `crates/coordinator/src/handler.rs` | +18, -3 | Get all adverts, sort by hint weight |
| `crates/coordinator/src/gossip_cache.rs` | +2, -4 | Remove capability setting (hints only) |
| `crates/coordinator/src/lib.rs` | +5 | Doc: roles are hints, measure don't trust |
| `DESIGN.md` | +2 | Add "Measure, Don't Trust" principle |
| `README.md` | +1, -1 | Update coordinator description |
| `docs/adr/ADR-004-seedless-bootstrap.md` | +3 | Add hint/trust anchor clarification |
| `docs/adr/ADR-009-peer-scoring.md` | +5, -3 | Update bootstrap to use hints |

## Review Findings

### ✅ PASS: Coordinator Adverts No Longer Gate Selection

**Before:** `get_adverts_by_role(|a| a.roles.coordinator)` filtered to only coordinators
**After:** `get_all_adverts()` returns all peers, roles only influence sort order

**Evidence:**
- `handler.rs:142`: Changed from `get_adverts_by_role(|advert| advert.roles.coordinator)` to `get_all_adverts()`
- `bootstrap.rs:257`: Changed from `get_adverts_by_role(|a| a.roles.coordinator)` to `get_all_adverts()`

### ✅ PASS: Bootstrap Selection Uses All Peers

**Before:** Only peers with `roles.coordinator == true` were considered
**After:** All peers are considered; roles affect sort priority

**Evidence:**
- `bootstrap.rs:286-299`: New `sort_adverts_by_hint()` method
- `handler.rs:148-158`: Inline sorting by role weight + score

### ✅ PASS: Roles Only Influence Sort Order

The sort algorithm counts role hints and uses them as primary sort key:
```rust
let a_hint = a.roles.coordinator as u8
    + a.roles.relay as u8
    + a.roles.rendezvous as u8
    + a.roles.reflector as u8;
// ... sort by hint count, then by score
```

This is exactly correct - roles are hints that influence ordering, not gates that block selection.

### ✅ PASS: Documentation Updates

All documentation consistently uses "measure, don't trust" language:

1. **DESIGN.md**: Added core principle #7: "Measure, Don't Trust: Capability claims are hints; peers validate reachability and behavior before relying on them"

2. **README.md**: Updated coordinator description: "Publishes Coordinator Adverts (ML-DSA signed) as *hints*. Peers validate reachability and success rates before selecting coordinators/relays ('measure, don't trust')."

3. **lib.rs doc comment**: "Coordinators are self-elected nodes that *attempt* to provide... Roles are **hints**, not trusted claims."

4. **ADR-004**: "Important: Roles in adverts are *hints*, not trust anchors. Peers validate reachability and observed behavior before relying on a node."

5. **ADR-009**: Updated `select_bootstrap_peers` to show hint-based approach with comments "roles are hints only"

### ✅ PASS: GossipCacheAdapter Change

**Before:** `add_from_connection(ant_peer_id, addresses, Some(capabilities))`
**After:** `add_from_connection(ant_peer_id, addresses, None)`

This correctly removes the automatic capability setting. Capabilities should be measured, not trusted from adverts.

### ⚠️ NOTE: Duplicate Sort Logic

The same sorting logic appears in two places:
1. `bootstrap.rs:286-299` (`sort_adverts_by_hint`)
2. `handler.rs:148-158` (inline sort)

**Recommendation:** Consider extracting to a shared helper function in a future cleanup. Not blocking.

### ⚠️ NOTE: Pre-existing Clippy Issues

The coordinator crate has pre-existing clippy issues in test modules (`publisher.rs`, `topic.rs`) that are missing `#[allow(clippy::expect_used, clippy::unwrap_used)]` attributes. These are **not related to this change set** and should be addressed separately.

## Security Analysis

| Check | Status | Notes |
|-------|--------|-------|
| No new panic paths | ✅ | No new `.unwrap()` or `.expect()` in production code |
| Signature verification unchanged | ✅ | Advert signatures still required |
| Rate limiting preserved | ✅ | Query deduplication still in place |
| TTL checks preserved | ✅ | Expiry checks unchanged |

## Quality Assessment

| Metric | Score |
|--------|-------|
| Code Quality | A |
| Documentation | A+ |
| Security | A |
| Test Coverage | B (pre-existing tests pass) |
| Consistency | A |

## Verdict

**APPROVED** ✅

The changes correctly implement the "coordinator roles as hints" philosophy:
- All peers are now considered for selection (no filtering by role)
- Roles only affect sort priority (hints for ordering)
- Documentation consistently reinforces "measure, don't trust"
- No security regressions

## Additional Fixes Applied During Review

During the review, the following issues were discovered and fixed:

### 1. Removed dead code after hints-only refactor

**Problem:** `select_best_coordinator()` and `get_addr_for_method()` became unused after switching to advert-based selection.

**Fix:** Removed these methods entirely from `bootstrap.rs`.

### 2. Fixed bootstrap flow to use gossip cache directly

**Problem:** `find_coordinator()` was calling ant-quic's `select_coordinators()` which filters by `supports_coordination` capability. Since we're no longer setting capabilities (hints-only), this returned empty results.

**Fix:** Changed to use `cache.get_all_adverts()` with hint-based sorting, which aligns with the "all peers equal, roles are hints" philosophy.

### 3. Marked internal conversion functions as test-only

**Problem:** `roles_to_capabilities()` and `nat_class_to_nat_type()` became dead code in production after removing capability setting.

**Fix:** Added `#[cfg(test)]` attribute and doc comments explaining they're retained for tests.

### 4. Clippy fix: `&mut Vec` → `&mut [_]`

**Problem:** `sort_adverts_by_hint` took `&mut Vec<CoordinatorAdvert>` instead of slice.

**Fix:** Changed to `&mut [crate::CoordinatorAdvert]`.

## Final Test Results

```
test result: ok. 90 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Recommended Next Steps

1. ✅ Tests pass: `cargo test -p saorsa-gossip-coordinator` (90 passed)
2. ✅ Clippy clean: `cargo clippy -p saorsa-gossip-coordinator -- -D warnings`
3. Bump version in `Cargo.toml` (0.4.1 → 0.4.2)
4. Update CHANGELOG.md
5. Commit with message: `feat(coordinator): treat coordinator roles as hints`
6. Publish to crates.io
