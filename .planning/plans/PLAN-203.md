# Phase 2.3: Plumb Multiplexer into GossipTransport

## Overview
Create MultiplexedGossipTransport that implements GossipTransport and routes operations through the TransportMultiplexer. Wire this into GossipRuntime while maintaining backward compatibility with single-transport mode.

## Technical Decisions
- Breakdown approach: By layer (MultiplexedGossipTransport → Runtime wiring → Tests → Examples)
- Task size: Small (~50 lines per task)
- Testing strategy: Unit tests for types + logic + integration + property tests
- Dependencies: Uses TransportMultiplexer from Phase 2.2
- Pattern: Follow existing GossipTransport trait implementation from UdpTransportAdapter

## Task Complexity Summary

| Task | Est. Lines | Files | Complexity | Model |
|------|-----------|-------|------------|-------|
| Task 1 | ~40 | 1 | simple | haiku |
| Task 2 | ~80 | 1 | standard | sonnet |
| Task 3 | ~15 | 1 | simple | haiku |
| Task 4 | ~50 | 1 | standard | sonnet |
| Task 5 | ~40 | 1 | standard | sonnet |
| Task 6 | ~80 | 1 | standard | sonnet |
| Task 7 | ~100 | 1 | standard | sonnet |
| Task 8 | ~60 | 1 | standard | sonnet |
| Task 9 | ~10 | 0 | simple | haiku |

## Tasks

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 1: Create MultiplexedGossipTransport struct</n>
  <activeForm>Creating MultiplexedGossipTransport struct</activeForm>
  <files>
    crates/transport/src/multiplexed_transport.rs (NEW)
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends></depends>
  <action>
    Create a new file crates/transport/src/multiplexed_transport.rs with:

    1. Module documentation explaining the multiplexed transport
    2. MultiplexedGossipTransport struct with fields:
       - multiplexer: Arc<TransportMultiplexer>
       - local_peer_id: PeerId
    3. Constructor `new(multiplexer: Arc<TransportMultiplexer>, local_peer_id: PeerId) -> Self`
    4. Builder pattern method `from_adapter` that creates a multiplexer with a single transport

    Follow patterns from udp_transport_adapter.rs for struct organization.

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Use TransportResult for fallible operations
    - Add doc comments for all public items
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo check -p saorsa-gossip-transport
  </verify>
  <done>
    - MultiplexedGossipTransport struct exists with proper fields
    - Constructor and builder methods available
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 2: Implement GossipTransport trait</n>
  <activeForm>Implementing GossipTransport trait for MultiplexedGossipTransport</activeForm>
  <files>
    crates/transport/src/multiplexed_transport.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 1</depends>
  <action>
    Implement the GossipTransport trait for MultiplexedGossipTransport:

    1. `dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()>`
       - Select transport for low-latency control
       - Delegate to selected adapter's dial

    2. `dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId>`
       - Use default transport for bootstrap
       - Return peer ID from adapter

    3. `listen(&self, bind: SocketAddr) -> Result<()>`
       - Listen on default transport
       - May need to listen on all transports

    4. `close(&self) -> Result<()>`
       - Close all registered transports

    5. `send_to_peer(&self, peer: PeerId, stream_type: GossipStreamType, data: Bytes) -> Result<()>`
       - Use select_transport_for_stream to pick adapter
       - Delegate send to selected adapter

    6. `receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)>`
       - Receive from default/primary transport
       - Or multiplex receive from all transports (advanced)

    Requirements:
    - NO .unwrap() or .expect()
    - Map TransportError to anyhow::Error as needed
    - Log transport selection decisions at debug level
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo check -p saorsa-gossip-transport
  </verify>
  <done>
    - All 6 GossipTransport methods implemented
    - Transport selection uses multiplexer
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 3: Add re-exports to lib.rs</n>
  <activeForm>Adding re-exports to transport lib.rs</activeForm>
  <files>
    crates/transport/src/lib.rs
  </files>
  <estimated_lines>~15</estimated_lines>
  <depends>Task 2</depends>
  <action>
    Update crates/transport/src/lib.rs to:

    1. Add module declaration: `mod multiplexed_transport;`
    2. Add public re-export: `pub use multiplexed_transport::MultiplexedGossipTransport;`
    3. Update module documentation to mention MultiplexedGossipTransport

    Requirements:
    - Keep existing exports intact
    - Follow existing re-export pattern
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - MultiplexedGossipTransport publicly exported
    - All existing tests still pass
    - No warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 4: Update GossipRuntimeBuilder</n>
  <activeForm>Updating GossipRuntimeBuilder for multiplexer</activeForm>
  <files>
    crates/runtime/src/runtime.rs
  </files>
  <estimated_lines>~50</estimated_lines>
  <depends>Task 3</depends>
  <action>
    Update GossipRuntimeBuilder in crates/runtime/src/runtime.rs:

    1. Add optional field for additional transports or multiplexer config
    2. Add builder method: `with_multiplexer(self, multiplexer: Arc<TransportMultiplexer>) -> Self`
    3. In build(), if multiplexer provided, wrap in MultiplexedGossipTransport
    4. If no multiplexer, create default with just UdpTransportAdapter (backward compat)

    Requirements:
    - Maintain backward compatibility - existing code without multiplexer still works
    - NO .unwrap() or .expect()
    - Add import for MultiplexedGossipTransport
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo check -p saorsa-gossip-runtime
  </verify>
  <done>
    - Builder accepts optional multiplexer
    - Default behavior unchanged (backward compatible)
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 5: Update GossipRuntime transport field</n>
  <activeForm>Updating GossipRuntime transport field type</activeForm>
  <files>
    crates/runtime/src/runtime.rs
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends>Task 4</depends>
  <action>
    Update GossipRuntime struct in crates/runtime/src/runtime.rs:

    1. Change field type from `Arc<UdpTransportAdapter>` to `Arc<dyn GossipTransport>`
    2. Update all usages of runtime.transport to work with trait object
    3. Verify membership, pubsub, presence, coordinator all receive Arc<dyn GossipTransport>
    4. Ensure Clone still works (Arc<dyn T> is Clone if T: ?Sized)

    Key changes:
    - Line ~145: `pub transport: Arc<dyn GossipTransport>`
    - Update transport creation in build() to produce trait object

    Requirements:
    - NO .unwrap() or .expect()
    - All existing functionality preserved
    - Subsystems should already accept trait objects
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo test -p saorsa-gossip-runtime
  </verify>
  <done>
    - GossipRuntime.transport is Arc<dyn GossipTransport>
    - All subsystems work with trait object
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 6: Unit tests for MultiplexedGossipTransport</n>
  <activeForm>Adding unit tests for MultiplexedGossipTransport</activeForm>
  <files>
    crates/transport/src/multiplexed_transport.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 2</depends>
  <action>
    Add unit tests to crates/transport/src/multiplexed_transport.rs:

    1. Test struct creation:
       - test_multiplexed_transport_creation
       - test_from_adapter_creates_single_transport

    2. Test method delegation:
       - test_local_peer_id_returns_correct_id
       - test_send_selects_transport_by_stream_type
       - test_dial_uses_default_transport

    3. Use MockTransportAdapter from multiplexer.rs tests
       - Reuse the mock pattern for consistent testing

    Requirements:
    - Tests can use .unwrap()/.expect() (only production code forbids them)
    - Cover happy paths and error cases
    - Follow existing test patterns from multiplexer.rs
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - 5+ unit tests for MultiplexedGossipTransport
    - Tests cover creation and delegation
    - All tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 7: Integration tests for multi-transport</n>
  <activeForm>Adding integration tests for multi-transport scenarios</activeForm>
  <files>
    crates/transport/src/multiplexed_transport.rs
  </files>
  <estimated_lines>~100</estimated_lines>
  <depends>Task 6</depends>
  <action>
    Add integration tests for multi-transport scenarios:

    1. test_multi_transport_routing:
       - Register UDP and mock BLE transports
       - Send message for LowLatencyControl → should use UDP
       - Send message for Broadcast → should use mock if capable

    2. test_fallback_when_preferred_unavailable:
       - Register only UDP transport
       - Request transport with BLE preference
       - Should fallback to UDP

    3. test_backward_compatibility_single_transport:
       - Create MultiplexedGossipTransport with single adapter
       - All operations work as expected

    4. test_close_closes_all_transports:
       - Register multiple transports
       - Call close()
       - All transports should be closed

    Requirements:
    - Use mock transports to avoid real network
    - Tests can use .unwrap()/.expect()
    - Add proptest for routing edge cases if time permits
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - 4+ integration tests for multi-transport
    - Routing logic verified
    - Backward compatibility verified
    - All tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 8: Update transport_benchmark.rs example</n>
  <activeForm>Updating transport_benchmark.rs example</activeForm>
  <files>
    examples/transport_benchmark.rs
  </files>
  <estimated_lines>~60</estimated_lines>
  <depends>Task 5</depends>
  <action>
    Update examples/transport_benchmark.rs to show multiplexed transport usage:

    1. Add import for MultiplexedGossipTransport, TransportMultiplexer
    2. Add optional --multiplexed CLI flag
    3. When flag set:
       - Create TransportMultiplexer
       - Register UdpTransportAdapter
       - Wrap in MultiplexedGossipTransport
       - Run same benchmark
    4. Show per-transport metrics if available

    Requirements:
    - Existing benchmark mode unchanged
    - New multiplexed mode optional
    - Clear output showing which mode is active
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy --example transport_benchmark -- -D warnings
    cargo build --example transport_benchmark
  </verify>
  <done>
    - Example updated with multiplexed mode
    - Existing functionality preserved
    - Example compiles and runs
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 9: Verify all tests pass</n>
  <activeForm>Verifying all tests pass</activeForm>
  <files></files>
  <estimated_lines>~10</estimated_lines>
  <depends>Task 1, Task 2, Task 3, Task 4, Task 5, Task 6, Task 7, Task 8</depends>
  <action>
    Run full verification suite:

    1. cargo fmt --all -- --check
    2. cargo clippy --workspace --all-targets -- -D warnings
    3. cargo test --workspace
    4. Verify 268+ tests passing
    5. Verify zero warnings

    If any failures, fix them before marking complete.
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
  </verify>
  <done>
    - All tests pass (268+)
    - Zero clippy warnings
    - Zero fmt issues
    - Phase 2.3 complete
  </done>
</task>

## Exit Criteria
- [ ] All 9 tasks complete
- [ ] MultiplexedGossipTransport implemented and exported
- [ ] GossipRuntime uses trait object for transport
- [ ] Backward compatibility maintained
- [ ] All tests passing (268+)
- [ ] Zero clippy warnings
- [ ] Example updated with multiplexed mode
- [ ] Code reviewed via /review
