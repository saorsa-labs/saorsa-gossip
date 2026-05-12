# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.42] - 2026-05-12

X0X-0069 (MVP) — SWIM peer-health oracle bridge between the membership
crate's `SwimDetector` and the pub-sub crate's per-topic cooling
decisions. Layer 3 of x0x's SOTA-Borrow Phase 2 fleet-survival
portfolio.

### Why

Pub-sub's per-topic cooling (saorsa-gossip-pubsub 0.5.36+'s
`PeerCoolingState`) currently treats every per-peer timeout as evidence
the peer is failing, with no awareness of global SWIM membership state.
A cross-region peer transiently slow under load gets cooled for the full
`PEER_SUPPRESSION_COOLDOWN` (2 minutes), even when the membership
crate's `SwimDetector` already knows the peer is responsive on other
paths. Lifeguard (Dadgar/Hendrickson 2018) reference: SWIM Suspicion
prevents false-positive failure marks under sustained packet loss
(12 vs 2 stable members in their experiments).

This release ships the **bridge surface** so downstream cooling
decisions can consult SWIM. The actual threshold-decision integration
(use the bridge to hold cooling on Suspect peers, escalate on Dead
peers) is filed as X0X-0069b follow-up, intentionally scoped out so
this release does not change cooling behaviour for callers that don't
opt in.

### Added

- `saorsa_gossip_types::PeerHealth` enum (`Alive` / `Suspect` / `Dead`)
  maps SWIM states onto pub-sub's vocabulary without exposing the
  membership crate's internal `SwimPeerEntry`.
- `saorsa_gossip_types::PeerHealthOracle` async trait — minimal
  read-only surface (`health_of`, `request_indirect_probe`) for the
  cross-crate bridge.
- `impl<T: GossipTransport + 'static> PeerHealthOracle for SwimDetector<T>`
  in `saorsa-gossip-membership` — maps `PeerState` →`PeerHealth`,
  delegates indirect-probe requests to the existing
  `SwimDetector::request_indirect_probes` async path.
- `PlumtreePubSub::with_health_oracle(oracle)` builder method that
  installs the oracle on the pub-sub instance.
- `PlumtreePubSub::peer_health(peer)` — async accessor exposing the
  oracle verdict per peer; downstream tickets (X0X-0073 adaptive
  cooling, X0X-0071 P1-P7 scoring, X0X-0074 admission control) read
  via this single bridge surface.
- `PlumtreePubSub::request_indirect_probe(target)` — best-effort
  nudge for SWIM to issue indirect probes, called by accumulating-
  timeout paths to keep oracle signal fresh.
- `GossipRuntimeBuilder::peer_health_oracle(oracle)` wires the oracle
  through the runtime so the pub-sub instance picks it up
  automatically; if the runtime owns its own `SwimDetector`, wiring
  is one builder call.
- New tests in `crates/pubsub`:
  - `peer_health_oracle_unwired_returns_none`
  - `peer_health_oracle_returns_swim_state_when_wired`
  - `request_indirect_probe_forwards_to_oracle`

### Scope notes

This is the **MVP** for X0X-0069. The bridge plumbing ships now so
downstream tickets unblock immediately. The actual cooling-decision
integration (consult oracle in `record_send_timeout_at` threshold-
crossing, hold cooling for `Suspect`, escalate for `Dead`) lands as
X0X-0069b once a downstream consumer needs it.

### Reference

- Reference: x0x ticket X0X-0069
- SOTA reference: SWIM paper (Das/Gupta/Motivala 2002), Lifeguard
  extensions (Dadgar/Hendrickson 2018)
- See x0x `docs/design/sota-borrow-phase-2-fleet-survival.md` for
  the portfolio context

## [0.5.41] - 2026-05-12

X0X-0068 — Bounded discovery cache by age + bytes, with per-topic telemetry.
Layer 1 of the SOTA-Borrow Phase 2 fleet-survival portfolio (see x0x
`docs/design/sota-borrow-phase-2-fleet-survival.md`).

### Why

The 2026-05-12 4h cert soak on x0x's 6-node bootstrap mesh showed the
classic "load grows with state" pattern: `continuous_max_pp_to` climbed
0→1835 across 6 windows, and `recv_pump.dropped_full` ticked > 0 at
window 11. saorsa-gossip's per-topic message cache was bounded by count
(`MAX_CACHE_SIZE = 2_048`) but not by bytes or age. With 11-16 KB group
cards on `x0x.discovery.groups`, worst-case per-topic state is ≈32 MB
before signature overhead; multiplied across active topics, ≈100 MB+
state per anti-entropy reconciliation cycle. Cross-Pacific paths
(helsinki↔singapore, helsinki↔sydney) can't sustain that bandwidth and
cool repeatedly.

### Added

- `BoundedMessageCache` replaces the bare `LruCache` for per-topic
  message storage. Bounds applied in priority order:
  1. **Age cap** — `MAX_CACHE_AGE_SECS = 60`. Entries older than this
     are evicted on every cache touch.
  2. **Bytes cap** — `MAX_CACHE_BYTES_PER_TOPIC = 16 MB`. LRU eviction
     when the per-topic byte total would exceed the cap (with
     `MESSAGE_CRYPTO_OVERHEAD_BYTES = 5_500` + `MESSAGE_HEADER_OVERHEAD_BYTES
     = 256` factored into the byte estimate).
  3. **Count cap** — existing `MAX_CACHE_SIZE = 2_048` retained as the
     hard upper bound.
- `CacheStatsSnapshot { msg_count, total_bytes, oldest_age_secs,
  evicted_by_age, evicted_by_bytes, evicted_by_count }` exposed per
  topic via `TopicCacheStatsSnapshot { topic, cache }`.
- `PubSubStats.topic_caches: Vec<TopicCacheStatsSnapshot>` lets
  consumers see the caps engaging in production.
- `PubSubCacheConfig { max_messages_per_topic, max_bytes_per_topic,
  max_age }` — operator-tunable. `GossipRuntimeConfig::pubsub_cache`
  + `GossipRuntimeConfig::pubsub_cache(...)` builder method wire it
  through.
- `PlumtreePubSub::new_with_cache_config` /
  `::new_with_task_control_and_cache_config` constructors that take
  `PubSubCacheConfig`. The existing zero-argument constructor remains
  backwards compatible (uses `PubSubCacheConfig::default()`).
- New tests in `crates/pubsub`:
  - `bounded_cache_evicts_by_age`
  - `bounded_cache_evicts_by_bytes_under_pressure`
  - `bounded_cache_evicts_by_count_hard_cap`
  - `bounded_cache_age_takes_precedence_over_bytes`
  - `bounded_cache_eviction_counters_track_correctly`
  - `bounded_cache_get_prunes_expired`
  - `bounded_cache_simulated_load_stays_within_caps`

### Reference

- Reference: x0x ticket X0X-0068
- SOTA reference: High-Scalability gossip protocol guide
- Hunt 12f forecast (§147 in x0x `docs/design/hunt-12f-stale-release-fast-drop.md`)

## [0.5.36] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.12 (X0X-0037 — duplicate-safe
ACK-v2 timeout retry: B3 envelope with request_id + receiver dedupe cache +
sender single retry). No source changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.11"` → `"0.27.12"`.
- **Workspace version**: 0.5.35 → 0.5.36 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.35] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.11 (X0X-0036 part 2 —
ACK-v2 priority + per-stage diagnostics + 500 ms receiver-side bounded
response write timeout). No source changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.10"` → `"0.27.11"`.
- **Workspace version**: 0.5.34 → 0.5.35 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.34] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.10 (X0X-0036 part 1 —
probe scavenger priority + single-flight + global cap). No source
changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.9"` → `"0.27.10"`.
- **Workspace version**: 0.5.33 → 0.5.34 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.33] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.9 (X0X-0035 fix —
ACK-v2 / relay-CONNECT-UDP bidi accept-race resolved via prefix-peek
demux). No source changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.8"` → `"0.27.9"`.
- **Workspace version**: 0.5.32 → 0.5.33 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.32] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.8 (X0X-0034 fix — bidi ACK
protocol + supersede-race grace window). No source changes in saorsa-gossip
itself; this is purely a transport-layer pin update so downstream consumers
(x0x) can pull a single coordinated upstream version.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.5"` → `"0.27.8"`.
  All 11 saorsa-gossip-* crates inherit via `workspace = true`.
- **Workspace version**: 0.5.31 → 0.5.32 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo nextest run --all-features` — full workspace passes.

## [0.5.30] - 2026-05-03

### Changed

- PlumTree topic-peer refresh now admits newly connected peers as LAZY first,
  then uses score-aware mesh maintenance to fill only the bounded EAGER degree.
- IWANT repair and first-contact EAGER senders now go through the same bounded
  score-aware maintenance path instead of directly expanding EAGER membership.
- Added rate-limited opportunistic grafting so a substantially better LAZY peer
  can replace one low-score EAGER peer while the mesh remains within degree
  bounds.
- ADR-009 now documents how PlumTree `MIN_EAGER_DEGREE`, `MAX_EAGER_DEGREE`,
  LAZY peers, and opportunistic grafting map to Gossipsub-style mesh parameters.

## [0.5.29] - 2026-05-03

### Added

- PubSub peer-score diagnostics now include per-topic score components for
  EAGER/LAZY/cooled peers, including decayed send successes, timeouts, cooling
  events, recovery probes, and recovery successes.

### Changed

- PlumTree mesh selection now folds send-side health into peer scores so
  outbound timeouts and cooling events reduce future EAGER eligibility, while
  successful recovery sends rebuild trust gradually.
- Topic peer refresh now uses the same score-aware degree maintenance as the
  background maintainer instead of bulk-promoting connected LAZY peers.

### Fixed

- Score cleanup now retains fresh send-side evidence for the full cooling/backoff
  horizon, preventing long-backoff peers from looking freshly healthy too early.

### Compatibility

- `PubSubStageStatsSnapshot` has a new public `peer_scores` field. Consumers
  that construct this diagnostic snapshot directly with a struct literal may
  need to add the field; serde/JSON consumers remain additive-compatible.

## [0.5.28] - 2026-05-03

### Added

- PubSub outbound sends now enforce a per-peer concurrency budget: one EAGER/data
  send plus a small control-plane budget for IHAVE, IWANT, and anti-entropy.
- `PubSubStageStatsSnapshot::outbound_budget_exhausted` exposes skipped sends
  caused by a peer already consuming its outbound PubSub permits.

### Fixed

- Repeated outbound budget pressure now feeds the existing slow-peer cooling
  path, so one overloaded peer cannot keep spawning fresh send tasks while
  waiting for timeout-based suppression.
- Outbound send permits are released on completion, timeout, panic, and caller
  cancellation, preventing leaked peer budget after aborted sends.

## [0.5.27] - 2026-05-03

### Fixed

- PubSub slow-peer recovery now uses a single post-cooldown probe per
  peer/topic. A failed probe immediately re-suppresses the peer with
  exponential backoff instead of allowing another full timeout window.
- GRAFT and degree maintenance no longer re-admit peers whose cooldown has
  expired until a recovery probe succeeds.
- Recovery-probe diagnostics now distinguish `cooldown`, `recovery_ready`, and
  `recovery_probe` states, and clear stale probe diagnostics when topics are
  reaped or disappear before result recording.

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
