# Phase 2.2: Implement TransportMultiplexer

## Overview
Create the transport registry and multiplexer that routes messages to different transports based on capabilities. This enables multi-transport support (UDP, BLE, LoRa) with intelligent routing.

## Technical Decisions
- Breakdown approach: By feature - complete multiplexer fully with small tasks
- Task size: Small - ~50 lines per task
- Testing strategy: Unit tests for types + routing logic + integration tests + property tests
- Dependencies: Uses TransportAdapter from Phase 2.1
- Order: Foundation first (Types → Registry → Routing → Tests)

## Task Complexity Summary

| Task | Est. Lines | Files | Complexity | Model |
|------|-----------|-------|------------|-------|
| Task 1 | ~40 | 1 | simple | haiku |
| Task 2 | ~35 | 1 | simple | haiku |
| Task 3 | ~60 | 1 | standard | sonnet |
| Task 4 | ~50 | 1 | standard | sonnet |
| Task 5 | ~70 | 1 | standard | sonnet |
| Task 6 | ~20 | 1 | simple | haiku |
| Task 7 | ~100 | 1 | standard | sonnet |
| Task 8 | ~10 | 0 | simple | haiku |

## Tasks

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 1: Create TransportCapability enum</n>
  <activeForm>Creating TransportCapability enum</activeForm>
  <files>
    crates/transport/src/multiplexer.rs (NEW)
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends></depends>
  <action>
    Create a new file crates/transport/src/multiplexer.rs with:

    1. Module documentation explaining transport multiplexing
    2. TransportCapability enum with variants:
       - LowLatencyControl: For membership/SWIM (< 100ms preferred)
       - BulkTransfer: For CRDT deltas and large payloads
       - Broadcast: For multicast-capable transports
       - OfflineReady: For transports with local persistence
    3. Derive Debug, Clone, Copy, PartialEq, Eq, Hash
    4. Implement Display trait for human-readable output

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Add module-level lint attributes matching lib.rs
    - Follow existing enum patterns (see GossipStreamType)
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportCapability enum exists with 4+ variants
    - Derives and Display implemented
    - File compiles with zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 2: Create TransportDescriptor enum</n>
  <activeForm>Creating TransportDescriptor enum</activeForm>
  <files>
    crates/transport/src/multiplexer.rs
  </files>
  <estimated_lines>~35</estimated_lines>
  <depends>Task 1</depends>
  <action>
    Add TransportDescriptor enum to multiplexer.rs:

    1. TransportDescriptor enum with variants:
       - Udp: UDP/QUIC transport (default)
       - Ble: Bluetooth Low Energy (constrained)
       - Lora: LoRa radio (very constrained)
       - Custom(String): For extensibility
    2. Derive Debug, Clone, PartialEq, Eq, Hash
    3. Implement Display trait
    4. Add method: capabilities() -> HashSet<TransportCapability>
       - Udp: LowLatencyControl, BulkTransfer
       - Ble: LowLatencyControl (no bulk due to MTU)
       - Lora: OfflineReady (no realtime)
       - Custom: empty set (user defines)

    Requirements:
    - NO .unwrap() or .expect()
    - Use HashSet from std::collections
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportDescriptor enum with 4 variants
    - capabilities() method returns correct sets
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 3: Create TransportRegistry struct</n>
  <activeForm>Creating TransportRegistry struct</activeForm>
  <files>
    crates/transport/src/multiplexer.rs
  </files>
  <estimated_lines>~60</estimated_lines>
  <depends>Task 2</depends>
  <action>
    Add TransportRegistry struct to multiplexer.rs:

    1. TransportRegistry struct with fields:
       - transports: HashMap<TransportDescriptor, Arc<dyn TransportAdapter>>
       - default_transport: Option<TransportDescriptor>
    2. Methods:
       - new() -> Self
       - register(&mut self, descriptor, transport) -> Result<()>
       - deregister(&mut self, descriptor) -> Option<Arc<dyn TransportAdapter>>
       - get(&self, descriptor) -> Option<Arc<dyn TransportAdapter>>
       - set_default(&mut self, descriptor) -> Result<()>
       - get_default(&self) -> Option<Arc<dyn TransportAdapter>>
       - find_by_capability(&self, cap: TransportCapability) -> Vec<(TransportDescriptor, Arc<dyn TransportAdapter>)>
       - all_transports(&self) -> Vec<(TransportDescriptor, Arc<dyn TransportAdapter>)>

    3. Error handling:
       - Return TransportError::InvalidConfig for registration failures
       - Return TransportError::InvalidConfig if setting default for non-registered transport

    Requirements:
    - NO .unwrap() or .expect()
    - Use crate::error::{TransportError, TransportResult}
    - Use crate::TransportAdapter trait
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportRegistry with all methods
    - Proper error handling
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 4: Create TransportMultiplexer struct</n>
  <activeForm>Creating TransportMultiplexer struct</activeForm>
  <files>
    crates/transport/src/multiplexer.rs
  </files>
  <estimated_lines>~50</estimated_lines>
  <depends>Task 3</depends>
  <action>
    Add TransportMultiplexer struct to multiplexer.rs:

    1. TransportMultiplexer struct with fields:
       - registry: RwLock<TransportRegistry>
       - local_peer_id: PeerId
    2. Constructor:
       - new(local_peer_id: PeerId) -> Self
       - with_registry(local_peer_id: PeerId, registry: TransportRegistry) -> Self
    3. Registry delegation methods (async-safe):
       - register_transport(&self, descriptor, transport) -> TransportResult<()>
       - deregister_transport(&self, descriptor) -> Option<Arc<dyn TransportAdapter>>
       - set_default_transport(&self, descriptor) -> TransportResult<()>
    4. Query methods:
       - local_peer_id(&self) -> PeerId
       - available_transports(&self) -> Vec<TransportDescriptor>

    Requirements:
    - Use tokio::sync::RwLock for async-safe registry access
    - NO .unwrap() or .expect()
    - Keep routing logic for Task 5
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportMultiplexer struct with registry
    - Async-safe access via RwLock
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 5: Implement routing logic</n>
  <activeForm>Implementing routing logic</activeForm>
  <files>
    crates/transport/src/multiplexer.rs
  </files>
  <estimated_lines>~70</estimated_lines>
  <depends>Task 4</depends>
  <action>
    Add routing methods to TransportMultiplexer:

    1. TransportRequest struct:
       - required_capabilities: HashSet<TransportCapability>
       - preferred_descriptor: Option<TransportDescriptor>
       - exclude_descriptors: HashSet<TransportDescriptor>

    2. TransportRequest builder pattern:
       - new() -> Self
       - require(cap: TransportCapability) -> Self
       - prefer(descriptor: TransportDescriptor) -> Self
       - exclude(descriptor: TransportDescriptor) -> Self

    3. Routing methods on TransportMultiplexer:
       - select_transport(&self, request: &TransportRequest) -> TransportResult<Arc<dyn TransportAdapter>>
         Logic:
         a. If preferred_descriptor set and registered and has all capabilities → return it
         b. Find all transports with required capabilities (excluding excluded)
         c. If multiple, prefer default transport if it qualifies
         d. If none found, return TransportError::InvalidConfig("No transport matches request")

       - select_transport_for_stream(&self, stream_type: GossipStreamType) -> TransportResult<Arc<dyn TransportAdapter>>
         Convenience method:
         - Membership → require LowLatencyControl
         - PubSub → require LowLatencyControl
         - Bulk → require BulkTransfer

    Requirements:
    - NO .unwrap() or .expect()
    - Use ? operator for error propagation
    - Log routing decisions at debug level
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportRequest with builder pattern
    - select_transport() with fallback logic
    - select_transport_for_stream() convenience method
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 6: Add re-exports to lib.rs</n>
  <activeForm>Adding re-exports to lib.rs</activeForm>
  <files>
    crates/transport/src/lib.rs
  </files>
  <estimated_lines>~20</estimated_lines>
  <depends>Task 5</depends>
  <action>
    Update crates/transport/src/lib.rs:

    1. Add module declaration:
       mod multiplexer;

    2. Add public re-exports:
       pub use multiplexer::{
           TransportCapability,
           TransportDescriptor,
           TransportRegistry,
           TransportMultiplexer,
           TransportRequest,
       };

    3. Update module documentation to mention multiplexer

    Requirements:
    - Place module declaration near other mod declarations
    - Place pub use near other pub use statements
    - Follow existing file organization
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - Module declared and exported
    - All new types publicly accessible
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 7: Unit tests for types and routing</n>
  <activeForm>Adding unit tests for multiplexer</activeForm>
  <files>
    crates/transport/src/multiplexer.rs
  </files>
  <estimated_lines>~100</estimated_lines>
  <depends>Task 6</depends>
  <action>
    Add #[cfg(test)] mod tests to multiplexer.rs:

    1. TransportCapability tests:
       - test_capability_display
       - test_capability_equality

    2. TransportDescriptor tests:
       - test_descriptor_display
       - test_descriptor_capabilities (verify each variant returns correct set)
       - test_descriptor_custom_empty_capabilities

    3. TransportRegistry tests:
       - test_registry_register_and_get
       - test_registry_deregister
       - test_registry_set_default
       - test_registry_find_by_capability
       - test_registry_all_transports

    4. TransportMultiplexer tests:
       - test_multiplexer_creation
       - test_multiplexer_register_transport
       - test_select_transport_preferred
       - test_select_transport_fallback
       - test_select_transport_no_match
       - test_select_transport_for_stream

    5. TransportRequest tests:
       - test_request_builder
       - test_request_require_multiple

    Use a MockTransportAdapter for testing:
    - Implement TransportAdapter with minimal functionality
    - Return fixed values for capabilities()
    - Other methods can return Ok(()) or placeholder values

    Requirements:
    - #[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)] on test module
    - Use assert_eq!, assert!, matches! macros
    - Test both success and error cases
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - 15+ tests covering all new types
    - MockTransportAdapter for testing
    - All tests pass
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 8: Verify all tests pass</n>
  <activeForm>Running full test suite verification</activeForm>
  <files>
  </files>
  <estimated_lines>~0</estimated_lines>
  <depends>Task 7</depends>
  <action>
    Run full verification:

    1. cargo fmt --all -- --check
    2. cargo clippy --all-features --all-targets -- -D warnings
    3. cargo test --all-features

    Fix any issues that arise.
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy --all-features --all-targets -- -D warnings
    cargo test --all-features
  </verify>
  <done>
    - Zero formatting issues
    - Zero clippy warnings
    - All tests pass (270+ tests)
  </done>
</task>

## Exit Criteria
- [ ] All 8 tasks complete
- [ ] All tests passing
- [ ] Zero clippy warnings
- [ ] TransportMultiplexer can route by capability
- [ ] Code reviewed via /review
