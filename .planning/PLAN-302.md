# Phase 3.2: Flatten Transport Layer onto ant-quic

## Overview
Add deprecation warnings to custom transport layer components that duplicate ant-quic's native functionality. Archive the BLE stub. Prepare for Phase 3.3 which will wire the runtime directly to ant-quic.

## Current State Analysis
- `multiplexer.rs` (1501 lines) - TransportMultiplexer, TransportRegistry, TransportRequest
- `multiplexed_transport.rs` (1174 lines) - MultiplexedGossipTransport wrapper
- `ble_transport_adapter.rs` (432 lines) - BLE stub (never used in production)
- `udp_transport_adapter.rs` (1101 lines) - UdpTransportAdapter (the actual transport)

## Tasks

### Task 1: Add deprecation to TransportMultiplexer
- **Files**: `crates/transport/src/multiplexer.rs`
- **Description**: Add `#[deprecated]` attribute to TransportMultiplexer with note about ant-quic's TransportRegistry
- **Tests**: Compilation passes (deprecation warnings are OK)
- **Status**: pending

### Task 2: Add deprecation to TransportRequest
- **Files**: `crates/transport/src/multiplexer.rs`
- **Description**: Add `#[deprecated]` attribute to TransportRequest - ant-quic handles routing internally
- **Tests**: Compilation passes
- **Status**: pending

### Task 3: Add deprecation to TransportRegistry (ours)
- **Files**: `crates/transport/src/multiplexer.rs`
- **Description**: Add `#[deprecated]` attribute to our TransportRegistry in favor of AntTransportRegistry
- **Tests**: Compilation passes
- **Status**: pending

### Task 4: Add deprecation to MultiplexedGossipTransport
- **Files**: `crates/transport/src/multiplexed_transport.rs`
- **Description**: Add `#[deprecated]` - will be replaced with direct ant-quic adapter in Phase 3.3
- **Tests**: Compilation passes
- **Status**: pending

### Task 5: Archive BLE stub to examples
- **Files**: `crates/transport/src/ble_transport_adapter.rs`, `examples/ble_transport_stub.rs`
- **Description**: Move BLE stub to examples/ as reference implementation, remove from lib exports
- **Tests**: All tests pass
- **Status**: pending

### Task 6: Update lib.rs exports with deprecation notices
- **Files**: `crates/transport/src/lib.rs`
- **Description**: Update module docs to explain the migration path to ant-quic native transport
- **Tests**: `cargo doc` builds without errors
- **Status**: pending

### Task 7: Verify tests still pass
- **Files**: All test files
- **Description**: Run full test suite, fix any issues from deprecation warnings
- **Tests**: All 363+ tests pass
- **Status**: pending

## Acceptance Criteria
- [ ] TransportMultiplexer marked deprecated
- [ ] TransportRequest marked deprecated
- [ ] TransportRegistry (ours) marked deprecated
- [ ] MultiplexedGossipTransport marked deprecated
- [ ] BLE stub moved to examples/
- [ ] lib.rs docs updated with migration guide
- [ ] All tests pass
- [ ] Zero compilation errors

## Notes
- We're adding deprecation warnings but NOT removing code yet
- This maintains backward compatibility for existing consumers
- Phase 3.3 will wire runtime directly to ant-quic's transport layer
- The actual code removal will happen in a future release after deprecation period
