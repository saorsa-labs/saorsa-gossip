# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.26] - 2026-05-03

### Fixed

- PubSub slow-peer cooling now also applies to single-peer recovery/control sends
  (IWANT, anti-entropy, and EAGER replies to IWANT). These paths previously used
  the bounded send timeout but did not update the per-topic cooling state, so a
  cooled peer could keep consuming 750 ms send slots outside normal EAGER fanout.

## [0.5.25] - 2026-05-03

### Added

- PubSub sender-side slow-peer cooling for PlumTree fanout. Peers that repeatedly
  hit the per-peer send timeout on a topic are temporarily demoted from EAGER to
  LAZY and skipped during the cooldown, freeing dispatcher capacity without a
  daemon restart.
- `PubSubStageStatsSnapshot::suppressed_peers` diagnostics showing cooled
  peer/topic entries, cooldown expiry, recent timeout rate, and affected topic
  count.

### Fixed

- Periodic topic peer refresh no longer immediately re-promotes a peer that is
  actively cooling after send-side timeouts.
- Cooled peers are re-admitted after cooldown expiry through the normal PlumTree
  maintenance/refresh path instead of requiring operator restart.

## [0.5.23] - 2026-04-27

### Fixed

- Presence beacon fanout now sends to peers concurrently with a bounded timeout,
  preventing one slow or wedged peer from delaying delivery to the rest of the
  mesh.
- Increased the per-peer presence beacon send timeout to 15 seconds now that
  fanout is concurrent, reducing false timeouts on high-latency live links
  without reintroducing head-of-line blocking.

## [0.5.22] - 2026-04-26

### Fixed

- Presence beacon sends are bounded by a timeout so a stalled peer cannot wedge
  the beacon task indefinitely.
- Presence beacon fanout snapshots groups and broadcast peers before network I/O,
  so no broadcast peer lock is held across awaited sends.
- Added `PresenceManager::replace_broadcast_peers()` for authoritative fanout
  refresh and stale-peer pruning by callers.

## [0.5.20] - 2026-04-23

### Changed

- Bumped `ant-quic` to `0.27.4`, picking up the dual-stack CPU-spin fix in
  `DualStackSocket::create_io_poller` (AND-combine v4/v6 writability). No
  saorsa-gossip source changes; all 11 workspace crates re-published at
  0.5.20 for a consistent lockstep upgrade across `saorsa-gossip-{types,
  transport, membership, pubsub, presence, crdt-sync, groups, identity,
  coordinator, rendezvous, runtime}`.

## [0.5.14] - 2026-04-09

### Changed

- Updated `ant-quic` to `0.26.1`
- Saorsa Gossip now relies on ant-quic's built-in first-party mDNS discovery and additive UPnP handling for zero-config transport connectivity

### Documentation

- Updated transport documentation to reflect that local discovery and router-assisted reachability live in ant-quic rather than in gossip-specific code
- Refreshed ADR-011 to note the expanded ant-quic transport responsibilities in the current stack

## [0.5.11] - 2026-04-01

### Changed

- Updated `ant-quic` to 0.24.5 (NAT traversal coordination now uses PeerId-based lookups instead of SocketAddr)
- Updated `saorsa-pqc` to 0.5

## [0.5.3] - 2026-03-07

### Fixed

- **Best-effort EAGER forwarding**: A single `send_to_peer` failure no longer aborts the
  entire forwarding loop. Failures are logged as warnings and forwarding continues to
  remaining peers.
- **Serialization error propagation**: Re-serialization failures in `handle_eager` now
  propagate as `Err` rather than being silently swallowed as `Ok(())`.
- **Serialization hoisted outside loop**: `postcard::to_stdvec` is now called once before
  the per-peer forwarding loop instead of once per peer.

### Added

- **`set_topic_peers`** on `PubSub` trait and `PlumtreePubSub`: atomically replaces topic
  peer membership by pruning disconnected peers from eager/lazy sets and adding newly
  connected peers as eager peers. Respects existing PRUNE decisions (lazy peers that are
  still connected remain lazy). Default trait implementation falls back to
  `initialize_topic_peers` (add-only); override for full prune-and-replace semantics.

## [0.4.3] - 2026-02-01

### Changed

- Updated ant-quic dependency to 0.21.0 (from 0.20.1)
  - Adapted to API change: `recv()` no longer takes a Duration parameter

## [0.4.2] - 2026-01-31

### Changed

- **Coordinator roles are now hints, not filters** ("measure, don't trust")
  - Bootstrap selection uses all peers; roles only influence sort order
  - `find_coordinator()` now uses gossip cache adverts directly
  - `handle_find_query()` returns all adverts sorted by hint weight
  - Removed capability-based filtering from ant-quic integration

### Documentation

- Added core principle #7: "Measure, Don't Trust" to DESIGN.md
- Updated coordinator crate description in README.md
- Updated ADR-004 (Seedless Bootstrap) with hints clarification
- Updated ADR-009 (Peer Scoring) with hint-based bootstrap example
- Updated module docs in `lib.rs` to clarify roles are hints

### Removed

- `select_best_coordinator()` and `get_addr_for_method()` methods (replaced by hint-sorted advert selection)
- Capability setting in `insert_advert()` (capabilities are now measured, not trusted)

## [0.4.1] - 2026-01-30

### Changed

- Updated ant-quic dependency to 0.20.1 (from 0.20.0)
  - Includes latest bug fixes and improvements from ant-quic

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
