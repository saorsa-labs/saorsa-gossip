# PLAN-206: Transport Hints in Peer Adverts

## Phase: 2.6 - Transport Hints in Peer Adverts
## Milestone: 2 - Transport Multi-Plexing Refactor

---

## Overview

Extend `CoordinatorAdvert` to include transport capability hints so peers can discover which transports other peers support. This enables BLE bridge discovery for Communitas integration.

## Success Criteria

- CoordinatorAdvert includes transport_hints field
- CBOR serialization handles transport hints correctly
- GossipCacheAdapter preserves hints when caching
- Advert round-trip tests pass
- Zero compilation warnings, all tests pass

## Dependencies

- Phase 2.5: TransportDescriptor enum exists in crates/transport/src/multiplexer.rs
- CoordinatorAdvert: Already has CBOR serialization support

---

## Tasks

### Task 1: Create TransportHint type in coordinator crate
**File**: `crates/coordinator/src/lib.rs`
**Lines**: ~50 new

Create a `TransportHint` struct that encapsulates transport availability info:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportHint {
    /// Transport type identifier (e.g., "udp", "ble", "lora")
    pub transport_type: String,
    /// Address or connection info for this transport (optional)
    pub endpoint: Option<String>,
    /// Whether this transport is currently available
    pub available: bool,
}
```

Use String for transport_type to avoid cross-crate dependency on TransportDescriptor.

### Task 2: Extend CoordinatorAdvert with transport_hints field
**File**: `crates/coordinator/src/lib.rs`
**Lines**: Modify struct at ~229

Add field to CoordinatorAdvert:
```rust
/// Transport capability hints for multi-transport discovery
#[serde(default)]
pub transport_hints: Vec<TransportHint>,
```

Update constructor to accept hints parameter.

### Task 3: Update CoordinatorAdvert::new() constructor
**File**: `crates/coordinator/src/lib.rs`

Update the `new()` method signature and implementation to include transport_hints.
Add a builder method `with_transport_hints()` for backward compatibility.

### Task 4: Update CBOR serialization tests
**File**: `crates/coordinator/src/lib.rs`
**Lines**: ~400-600 (test module)

Update existing CBOR tests to verify transport_hints serialization:
- Test empty hints (backward compatibility)
- Test with multiple hints
- Test round-trip preserves hints

### Task 5: Add TransportHint helper methods
**File**: `crates/coordinator/src/lib.rs`

Add helper methods to CoordinatorAdvert:
- `has_transport(&self, transport_type: &str) -> bool`
- `available_transports(&self) -> Vec<&TransportHint>`
- `add_transport_hint(&mut self, hint: TransportHint)`

### Task 6: Update GossipCacheAdapter to preserve hints
**File**: `crates/coordinator/src/gossip_cache.rs`

Verify that GossipCacheAdapter correctly stores and retrieves adverts with transport hints. The existing implementation should work since it stores full CoordinatorAdvert objects.

### Task 7: Add integration tests for transport hints
**File**: `crates/coordinator/src/lib.rs`

Add tests:
- test_advert_with_transport_hints
- test_advert_empty_transport_hints_backward_compat
- test_advert_transport_hints_cbor_roundtrip
- test_has_transport_helper

---

## Files Modified

| File | Change Type | Description |
|------|-------------|-------------|
| crates/coordinator/src/lib.rs | Modify | Add TransportHint, extend CoordinatorAdvert |
| crates/coordinator/src/gossip_cache.rs | Verify | Ensure hints preserved (likely no changes) |

---

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

Expected: 325+ tests passing, 0 warnings
