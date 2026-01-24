# Phase 3.1: Upgrade ant-quic Dependency ✅

## Overview
Upgrade ant-quic from 0.19 to 0.20 to access native transport routing infrastructure:
- **TransportRegistry** (`src/transport/provider.rs`) - Multi-transport collection
- **ConnectionRouter** (`src/connection_router.rs`) - Engine selection (QUIC vs Constrained)
- **LinkTransport** trait (`src/link_transport.rs`) - Stable abstraction for overlays
- **SharedTransport** (`src/link_transport_impl.rs`) - Protocol handler multiplexing

## Tasks

### Task 1: Bump ant-quic version in workspace Cargo.toml ✅
- **Files**: `Cargo.toml`
- **Description**: Change `ant-quic = "0.19"` to `ant-quic = "0.20"` in workspace dependencies
- **Tests**: `cargo check` passes
- **Status**: completed

### Task 2: Run cargo update to refresh lock file ✅
- **Files**: `Cargo.lock`
- **Description**: Run `cargo update -p ant-quic` to update the lock file
- **Tests**: Lock file updated, no conflicts
- **Status**: completed

### Task 3: Fix compilation errors from API changes ✅
- **Files**: `crates/transport/src/udp_transport_adapter.rs`
- **Description**: Fixed `peer_conn.remote_addr` which is now `TransportAddr` instead of `SocketAddr`
- **Change**: Added `.to_synthetic_socket_addr()` conversion for peer tracking
- **Tests**: `cargo check --all-features --all-targets` passes
- **Status**: completed

### Task 4: Fix clippy warnings ✅
- **Files**: `crates/transport/src/multiplexer.rs`
- **Description**: Removed unnecessary `.clone()` on Copy types in proptest
- **Tests**: `cargo clippy --all-features -- -D warnings` passes
- **Status**: completed

### Task 5: Verify all tests pass ✅
- **Files**: All test files
- **Description**: Run full test suite to ensure no regressions
- **Tests**: 363 tests pass
- **Status**: completed

### Task 6: Update ant-quic re-exports ✅
- **Files**: `crates/transport/src/lib.rs`
- **Description**: Added re-exports for new ant-quic transport types
- **New exports**: `TransportAddr`, `AntTransportType`, `AntTransportCapabilities`, `TransportProvider`, `AntTransportRegistry`, `AntLoRaParams`
- **Status**: completed

## Acceptance Criteria ✅
- [x] ant-quic 0.20 in Cargo.toml
- [x] Zero compilation errors
- [x] Zero clippy warnings
- [x] All 363 tests pass
- [x] New ant-quic types re-exported

## Summary of Changes

### API Changes in ant-quic 0.20
1. `PeerConnection.remote_addr` is now `TransportAddr` instead of `SocketAddr`
2. `TransportAddr` provides multi-transport addressing (UDP, BLE, LoRa, Serial, etc.)
3. New `TransportRegistry` for managing multiple transports
4. New `TransportProvider` trait for pluggable transport implementations

### Files Modified
- `Cargo.toml` - Version bump
- `Cargo.lock` - Updated dependencies
- `crates/transport/src/udp_transport_adapter.rs` - TransportAddr conversion
- `crates/transport/src/multiplexer.rs` - Remove unnecessary clone
- `crates/transport/src/lib.rs` - New re-exports
