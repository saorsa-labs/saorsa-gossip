# PLAN-207: Benchmarking & BLE Stub

## Phase: 2.7 - Benchmarking & BLE Stub
## Milestone: 2 - Transport Multi-Plexing Refactor

---

## Overview

Extend the transport benchmark to support dual-transport testing and create a BLE transport adapter stub that simulates constrained link characteristics. This enables comparison between broadband (UDP/QUIC) and constrained (BLE) transports.

## Success Criteria

- transport_benchmark.rs supports --ble flag for simulated constrained transport
- Per-transport metrics (throughput, latency) are reported
- BleTransportAdapter stub implements TransportAdapter trait
- Property-based tests cover routing edge cases
- Benchmark documentation with methodology
- Zero compilation warnings, all tests pass

## Dependencies

- Phase 2.3: MultiplexedGossipTransport with --multiplexed flag
- Phase 2.5: TransportRequest for capability-based routing
- Existing transport_benchmark.rs example

---

## Tasks

### Task 1: Create BleTransportAdapter stub
**File**: `crates/transport/src/ble_transport_adapter.rs`
**Lines**: ~150 new

Create a stub BLE transport adapter that simulates constrained link behavior:
- MTU limit: 512 bytes
- Simulated latency: 50-150ms
- No bulk transfer capability
- Implements TransportAdapter trait

### Task 2: Register BLE stub in multiplexer
**File**: `crates/transport/src/multiplexed_transport.rs`

Add method to register BLE transport adapter:
```rust
pub async fn with_ble_transport(self, ble_adapter: Arc<dyn TransportAdapter>) -> Self
```

### Task 3: Extend transport_benchmark.rs with --ble flag
**File**: `examples/transport_benchmark.rs`

Add CLI flag for BLE simulation:
- `--ble` enables BLE transport stub
- Reports separate metrics for UDP and BLE
- Shows throughput comparison

### Task 4: Add per-transport metrics collection
**File**: `examples/transport_benchmark.rs`

Collect and report:
- Messages sent/received per transport
- Average latency per transport
- Throughput (bytes/sec) per transport
- Failure counts per transport

### Task 5: Add property-based tests for routing
**File**: `crates/transport/src/multiplexer.rs`

Add proptest tests:
- Random capability combinations
- Edge case transport selection
- Fallback behavior verification

### Task 6: Add benchmark documentation
**File**: `docs/benchmarks.md`

Document:
- Benchmark methodology
- How to run benchmarks
- Expected results and interpretation
- Transport comparison notes

---

## Files Modified

| File | Change Type | Description |
|------|-------------|-------------|
| crates/transport/src/ble_transport_adapter.rs | New | BLE stub implementation |
| crates/transport/src/lib.rs | Modify | Export BleTransportAdapter |
| crates/transport/src/multiplexed_transport.rs | Modify | Add BLE registration |
| examples/transport_benchmark.rs | Modify | Add --ble flag, per-transport metrics |
| crates/transport/src/multiplexer.rs | Modify | Add property-based tests |
| docs/benchmarks.md | New | Benchmark documentation |

---

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo run --example transport_benchmark -- --help
cargo run --example transport_benchmark -- --ble
```

Expected: 340+ tests passing, 0 warnings
