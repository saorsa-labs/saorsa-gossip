# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-01-24

### Changed

- **Transport Layer Simplification** (Milestone 3)
  - Upgraded ant-quic from 0.19 to 0.20 with native multi-transport capabilities
  - Removed custom `TransportMultiplexer` in favor of ant-quic's `TransportRegistry`
  - Removed custom `MultiplexedGossipTransport` in favor of direct `UdpTransportAdapter`
  - Removed `BleTransportAdapter` stub - use ant-quic's native BLE transport
  - Simplified `GossipTransport` trait: removed `send_with_request()` method
  - Runtime now uses `UdpTransportAdapter` directly

### Removed

- `TransportMultiplexer`, `TransportRegistry`, `TransportRequest` (use ant-quic native)
- `MultiplexedGossipTransport` wrapper
- `BleTransportAdapter` stub
- `transport_benchmark.rs` example
- ~4,130 lines of redundant code

### Documentation

- Updated README.md transport section
- Updated benchmarks.md to reflect simplified architecture
- Added ADR-011: Transport Layer Simplification

## [0.3.0] - 2026-01-23

### Added

- **Transport Multiplexing Architecture**
  - `TransportAdapter` trait for pluggable transport backends
  - `TransportMultiplexer` for capability-based routing
  - `MultiplexedGossipTransport` implementing `GossipTransport` trait
  - `TransportCapability` enum: `LowLatencyControl`, `BulkTransfer`, `Broadcast`, `OfflineReady`
  - `TransportDescriptor` enum: `Udp`, `Ble`, `Lora`, `Custom`

- **Transport Request Builder**
  - `TransportRequest` with fluent API: `require()`, `exclude()`, `prefer()`
  - Convenience constructors: `low_latency_control()`, `bulk_transfer()`, `offline_ready()`
  - `send_with_request()` method on `GossipTransport`

- **Configuration & Context**
  - `GossipContext` struct for simplified runtime configuration
  - `GossipContextBuilder` with builder pattern
  - Integration with `GossipRuntimeBuilder`

- **Transport Hints in Peer Discovery**
  - `TransportHint` struct for multi-transport peer discovery
  - Extended `CoordinatorAdvert` with `transport_hints` field
  - Helper methods: `has_transport()`, `available_transports()`, `add_transport_hint()`

- **BLE Transport Stub**
  - `BleTransportAdapter` simulating constrained BLE characteristics
  - 512-byte MTU, 50-150ms latency simulation
  - `MtuExceeded` error variant for MTU enforcement

- **Transport Error Handling**
  - `TransportError` enum with thiserror integration
  - Variants: `ConnectionFailed`, `SendFailed`, `ReceiveFailed`, `PeerNotFound`, `Timeout`, `MtuExceeded`, `NotConnected`, `Internal`

- **Benchmarking**
  - `--multiplexed` flag for `transport_benchmark.rs`
  - `--ble` flag for BLE transport simulation
  - Per-transport statistics display
  - `docs/benchmarks.md` guide

- **Property-Based Tests**
  - 9 proptest tests for capability routing invariants
  - Idempotency, preference overwriting, capability consistency tests

### Changed

- Renamed `AntQuicTransport` to `UdpTransportAdapter`
- Moved transport file from `ant_quic_transport.rs` to `udp_transport_adapter.rs`
- Membership module now requests `LowLatencyControl` capability
- PubSub module now requests `BulkTransfer` for large messages

### Fixed

- N/A (feature release)

## [0.2.2] - 2026-01-15

### Added

- Complete Milestone 1: Production Bug Fixes & Improvements
- `GossipCacheAdapter` wrapping `Arc<BootstrapCache>` with gossip-specific advert storage
- Complete FOAF discovery with query forwarding and response aggregation
- Complete HyParView shuffle protocol implementation

### Fixed

- PeerId-to-public-key binding verification in `handle_advert()`
- FOAF response structural validation
- `select_best_from_adverts()` logic bug
- Silent serialization failures now logged
- Lock contention warnings added

## [0.2.1] - 2026-01-10

### Added

- Initial public release
- HyParView membership protocol
- Plumtree PubSub protocol
- SWIM failure detection
- Delta-CRDT synchronization (OR-Set, LWW-Register, Rga)
- Presence beacons and coordinator discovery
