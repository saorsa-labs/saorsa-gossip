# Phase 2.4: Configuration & GossipContext

## Overview
Create a GossipContext central configuration object that provides a clean, ergonomic API for configuring the gossip runtime with multiple transports. This will be the primary entry point for consumers like communitas-core.

## Technical Decisions
- Breakdown approach: By layer (GossipContext struct → Builder pattern → Public API → Documentation)
- Task size: Small (~50 lines per task)
- Testing strategy: Unit tests for builder pattern, integration tests for full configuration
- Dependencies: Uses TransportMultiplexer from Phase 2.2, MultiplexedGossipTransport from Phase 2.3
- Pattern: Follow existing GossipRuntimeBuilder and UdpTransportAdapterConfig patterns

## Task Complexity Summary

| Task | Est. Lines | Files | Complexity | Model |
|------|-----------|-------|------------|-------|
| Task 1 | ~50 | 1 | simple | haiku |
| Task 2 | ~80 | 1 | standard | sonnet |
| Task 3 | ~40 | 1 | simple | haiku |
| Task 4 | ~40 | 1 | simple | haiku |
| Task 5 | ~60 | 1 | standard | sonnet |
| Task 6 | ~80 | 1 | standard | sonnet |
| Task 7 | ~50 | 1 | simple | haiku |

## Tasks

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 1: Create GossipContext struct</n>
  <activeForm>Creating GossipContext struct</activeForm>
  <files>
    crates/runtime/src/context.rs (NEW)
  </files>
  <estimated_lines>~50</estimated_lines>
  <depends></depends>
  <action>
    Create a new file crates/runtime/src/context.rs with:

    1. Module documentation explaining GossipContext as central configuration
    2. GossipContext struct with fields:
       - bind_addr: SocketAddr
       - known_peers: Vec<SocketAddr>
       - identity: Option<MlDsaKeyPair>
       - transport_configs: Vec<(TransportDescriptor, Box<dyn Any + Send + Sync>)>
       - default_transport: Option<TransportDescriptor>
       - channel_capacity: usize (default: 10_000)
       - max_peers: usize (default: 1_000)
    3. Constructor `new(bind_addr: SocketAddr) -> Self` with sensible defaults
    4. Implement Default for GossipContext using 0.0.0.0:0

    Follow patterns from UdpTransportAdapterConfig for struct organization.

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Add doc comments for all public items
    - Use TransportDescriptor from transport crate
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo check -p saorsa-gossip-runtime
  </verify>
  <done>
    - GossipContext struct exists with proper fields
    - Constructor with defaults available
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 2: Implement GossipContextBuilder</n>
  <activeForm>Implementing GossipContextBuilder</activeForm>
  <files>
    crates/runtime/src/context.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 1</depends>
  <action>
    Add builder methods to GossipContext in crates/runtime/src/context.rs:

    1. Builder-style methods (return Self):
       - `with_bind_addr(self, addr: SocketAddr) -> Self`
       - `with_known_peers(self, peers: Vec<SocketAddr>) -> Self`
       - `with_identity(self, keypair: MlDsaKeyPair) -> Self`
       - `with_channel_capacity(self, capacity: usize) -> Self`
       - `with_max_peers(self, max: usize) -> Self`

    2. Transport configuration methods:
       - `with_udp_transport(self, config: UdpTransportAdapterConfig) -> Self`
         Adds UDP transport to transport_configs list
       - `with_default_transport(self, descriptor: TransportDescriptor) -> Self`
         Sets which transport to use by default

    3. Convenience method:
       - `with_defaults() -> Self` - Creates context with UDP transport using default config

    Requirements:
    - NO .unwrap() or .expect()
    - Methods are chainable
    - Add doc comments with examples
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo check -p saorsa-gossip-runtime
  </verify>
  <done>
    - All builder methods implemented
    - Methods are chainable
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 3: Add build() method to create GossipRuntime</n>
  <activeForm>Adding build() method to GossipContext</activeForm>
  <files>
    crates/runtime/src/context.rs
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends>Task 2</depends>
  <action>
    Add the build() method to GossipContext in crates/runtime/src/context.rs:

    1. `pub async fn build(self) -> Result<GossipRuntime>`:
       - If no transport_configs, create default UDP transport
       - Create TransportMultiplexer
       - Register all configured transports
       - Set default transport
       - Create GossipRuntimeBuilder and configure it
       - Call builder.build() and return result

    2. Helper method `create_udp_transport(&self, config: &UdpTransportAdapterConfig) -> Result<Arc<UdpTransportAdapter>>`

    This method bridges GossipContext to the existing GossipRuntimeBuilder.

    Requirements:
    - NO .unwrap() or .expect()
    - Proper error propagation with ?
    - Log transport registration at debug level
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo check -p saorsa-gossip-runtime
  </verify>
  <done>
    - build() method creates GossipRuntime from context
    - Default UDP transport created if none configured
    - No compilation errors or warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 4: Add module to lib.rs and re-exports</n>
  <activeForm>Adding module and re-exports to lib.rs</activeForm>
  <files>
    crates/runtime/src/lib.rs
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends>Task 3</depends>
  <action>
    Update crates/runtime/src/lib.rs to:

    1. Add module declaration: `mod context;`
    2. Add public re-export: `pub use context::GossipContext;`
    3. Add transport re-exports for convenience:
       - `pub use saorsa_gossip_transport::{TransportDescriptor, TransportCapability, UdpTransportAdapterConfig};`
    4. Update module documentation to mention GossipContext as entry point

    This provides a clean public API where consumers can import everything from runtime crate.

    Requirements:
    - Keep existing exports intact
    - Follow existing re-export pattern
    - Update module-level docs
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo test -p saorsa-gossip-runtime
  </verify>
  <done>
    - GossipContext publicly exported
    - Transport types re-exported for convenience
    - All existing tests still pass
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 5: Unit tests for GossipContext</n>
  <activeForm>Adding unit tests for GossipContext</activeForm>
  <files>
    crates/runtime/src/context.rs
  </files>
  <estimated_lines>~60</estimated_lines>
  <depends>Task 4</depends>
  <action>
    Add unit tests to crates/runtime/src/context.rs:

    1. test_context_default - verify default values
    2. test_context_builder_chain - verify fluent API
    3. test_context_with_udp_transport - verify transport configuration stored
    4. test_context_with_multiple_configs - verify multiple settings
    5. test_context_with_defaults_convenience - verify convenience method

    Note: build() tests are async integration tests covered in Task 6.

    Requirements:
    - Tests can use .unwrap()/.expect()
    - Cover all builder methods
    - Verify field values after configuration
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo test -p saorsa-gossip-runtime context
  </verify>
  <done>
    - 5+ unit tests for GossipContext
    - Builder pattern fully covered
    - All tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 6: Integration tests for build()</n>
  <activeForm>Adding integration tests for build() method</activeForm>
  <files>
    crates/runtime/src/context.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 5</depends>
  <action>
    Add async integration tests for GossipContext::build():

    1. test_build_creates_runtime_with_defaults - default UDP transport works
    2. test_build_with_custom_bind_addr - verifies bind address used
    3. test_build_with_identity - verifies identity is propagated
    4. test_build_with_udp_config - custom UDP config applied
    5. test_build_registers_transport - multiplexer has transport registered

    Use #[tokio::test] for async tests.
    May need to bind to different ports to avoid conflicts.

    Requirements:
    - Tests can use .unwrap()/.expect()
    - Use random/dynamic ports to avoid conflicts
    - Verify runtime is functional
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo test -p saorsa-gossip-runtime context
  </verify>
  <done>
    - 5+ integration tests for build()
    - Runtime creation verified
    - Transport registration verified
    - All tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 7: API documentation with examples</n>
  <activeForm>Adding API documentation with examples</activeForm>
  <files>
    crates/runtime/src/context.rs
    crates/runtime/src/lib.rs
  </files>
  <estimated_lines>~50</estimated_lines>
  <depends>Task 6</depends>
  <action>
    Add comprehensive documentation:

    1. Module-level docs in context.rs:
       - Purpose of GossipContext
       - Quick start example showing basic usage
       - Example with custom UDP configuration
       - Example with multiple transports (BLE stub)

    2. Type-level docs on GossipContext struct:
       - Each field documented
       - Configuration defaults explained

    3. Method-level docs:
       - Each builder method with example
       - build() method with full example

    4. Update lib.rs module docs:
       - Add GossipContext to feature list
       - Show simple entry point example

    Requirements:
    - Examples should use `ignore` if they require network
    - Follow existing documentation patterns
    - Clear, concise explanations
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-runtime -- -D warnings
    cargo doc -p saorsa-gossip-runtime --no-deps
    cargo test -p saorsa-gossip-runtime
  </verify>
  <done>
    - Module-level docs complete
    - All public items documented
    - Examples compile (or use ignore)
    - No doc warnings
  </done>
</task>

## Exit Criteria
- [ ] All 7 tasks complete
- [ ] GossipContext struct with builder pattern
- [ ] build() method creates GossipRuntime
- [ ] Transport types re-exported from runtime crate
- [ ] Unit and integration tests passing
- [ ] Documentation with examples
- [ ] Zero clippy warnings
- [ ] Code reviewed via /review
