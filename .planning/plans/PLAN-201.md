# Phase 2.1: Rename and Create TransportAdapter Trait

## Overview
Create the foundation for multi-transport support by introducing the TransportAdapter trait abstraction,
dedicated TransportError types, and renaming AntQuicTransport to UdpTransportAdapter. This phase
establishes the architecture for future BLE/LoRa transports.

## Technical Decisions
- Breakdown approach: By layer (Error types → Trait → Rename → Update imports)
- Task size: Small (1 file, ~50 lines per task)
- Testing strategy: Unit tests for new types + all existing tests must pass
- Dependencies: ant-quic 0.19.0 (update required first)
- E2E test command: cargo test --all-features

## Task Complexity Summary

| Task | Est. Lines | Files | Complexity | Model |
|------|-----------|-------|------------|-------|
| Task 1 | ~10 | 1 | simple | haiku |
| Task 2 | ~80 | 1 | standard | sonnet |
| Task 3 | ~60 | 1 | standard | sonnet |
| Task 4 | ~5 | 2 | simple | haiku |
| Task 5 | ~50 | 1 | standard | sonnet |
| Task 6 | ~100 | 1 | standard | sonnet |
| Task 7 | ~80 | 5 | standard | sonnet |
| Task 8 | ~10 | 0 | simple | haiku |

## Tasks

<task type="auto" priority="p0" complexity="simple" model="haiku">
  <n>Task 1: Update ant-quic to 0.19.0</n>
  <activeForm>Updating ant-quic dependency to 0.19.0</activeForm>
  <files>
    Cargo.toml
  </files>
  <estimated_lines>~10</estimated_lines>
  <depends></depends>
  <action>
    Update the ant-quic dependency from 0.18 to 0.19.0 in the workspace Cargo.toml.

    1. Change `ant-quic = "0.18"` to `ant-quic = "0.19"`
    2. Run `cargo update -p ant-quic` to update Cargo.lock
    3. Check for any breaking API changes from 0.18 → 0.19
    4. Fix any compilation errors from API changes

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Zero compilation warnings
  </action>
  <verify>
    cargo build --all-features
    cargo test --all-features --no-run
  </verify>
  <done>
    - ant-quic dependency updated to 0.19.0
    - Cargo.lock updated
    - Build passes with zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 2: Create TransportError enum</n>
  <activeForm>Creating TransportError enum with thiserror</activeForm>
  <files>
    crates/transport/src/error.rs (NEW)
    crates/transport/src/lib.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 1</depends>
  <action>
    Create a dedicated error type for transport operations using thiserror.

    1. Create new file `crates/transport/src/error.rs`
    2. Define TransportError enum with variants:
       - ConnectionFailed { peer_id: Option<PeerId>, addr: SocketAddr, source: anyhow::Error }
       - SendFailed { peer_id: PeerId, source: anyhow::Error }
       - ReceiveFailed { source: anyhow::Error }
       - DialFailed { addr: SocketAddr, source: anyhow::Error }
       - InvalidPeerId { reason: String }
       - InvalidConfig { reason: String }
       - Closed
       - Timeout { operation: String }
       - Other { source: anyhow::Error }
    3. Implement std::error::Error via thiserror derive
    4. Add From impls for common error types (anyhow::Error, std::io::Error)
    5. Add Result type alias: `pub type TransportResult<T> = Result<T, TransportError>;`
    6. Update lib.rs to declare and re-export the error module

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Use thiserror for error derive
    - Follow existing error patterns in codebase
    - Full doc comments on all public items
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportError enum defined with all variants
    - thiserror derive working
    - From impls for common types
    - Re-exported from lib.rs
    - Unit tests for error creation/display
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 3: Create TransportAdapter trait</n>
  <activeForm>Creating TransportAdapter trait definition</activeForm>
  <files>
    crates/transport/src/lib.rs
  </files>
  <estimated_lines>~60</estimated_lines>
  <depends>Task 2</depends>
  <action>
    Create the TransportAdapter trait that abstracts underlying transport implementations.

    1. Define TransportAdapter trait in lib.rs with async methods:
       ```rust
       #[async_trait::async_trait]
       pub trait TransportAdapter: Send + Sync {
           /// Returns the local peer ID for this transport
           fn local_peer_id(&self) -> PeerId;

           /// Dials a peer at the given address
           async fn dial(&self, addr: SocketAddr) -> TransportResult<PeerId>;

           /// Sends data to a peer with the given stream type
           async fn send(&self, peer_id: PeerId, stream_type: GossipStreamType, data: Bytes) -> TransportResult<()>;

           /// Receives the next message from any connected peer
           async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)>;

           /// Closes the transport
           async fn close(&self) -> TransportResult<()>;

           /// Returns currently connected peers
           async fn connected_peers(&self) -> Vec<(PeerId, SocketAddr)>;

           /// Returns transport-specific capabilities
           fn capabilities(&self) -> TransportCapabilities;
       }
       ```
    2. Define TransportCapabilities struct:
       ```rust
       #[derive(Debug, Clone, Default)]
       pub struct TransportCapabilities {
           pub supports_broadcast: bool,
           pub max_message_size: usize,
           pub typical_latency_ms: u32,
           pub is_reliable: bool,
       }
       ```
    3. Add blanket Arc<T> implementation for TransportAdapter (like GossipTransport)
    4. Add documentation for trait and all methods

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Use async_trait macro
    - Send + Sync bounds for dyn-safety
    - Full doc comments
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportAdapter trait defined
    - TransportCapabilities struct defined
    - Arc<T> blanket impl added
    - All methods documented
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 4: Rename file ant_quic_transport.rs to udp_transport_adapter.rs</n>
  <activeForm>Renaming transport file</activeForm>
  <files>
    crates/transport/src/ant_quic_transport.rs → crates/transport/src/udp_transport_adapter.rs
    crates/transport/src/lib.rs
  </files>
  <estimated_lines>~5</estimated_lines>
  <depends>Task 3</depends>
  <action>
    Rename the file containing the QUIC-based transport implementation.

    1. Use git mv to rename the file:
       `git mv crates/transport/src/ant_quic_transport.rs crates/transport/src/udp_transport_adapter.rs`
    2. Update mod declaration in lib.rs:
       - Change `mod ant_quic_transport;` to `mod udp_transport_adapter;`
       - Update any use statements

    Requirements:
    - Use git mv for proper tracking
    - Update lib.rs module declaration
  </action>
  <verify>
    cargo build -p saorsa-gossip-transport
  </verify>
  <done>
    - File renamed via git mv
    - Module declaration updated in lib.rs
    - Build passes
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 5: Rename struct AntQuicTransport to UdpTransportAdapter</n>
  <activeForm>Renaming AntQuicTransport struct</activeForm>
  <files>
    crates/transport/src/udp_transport_adapter.rs
    crates/transport/src/lib.rs
  </files>
  <estimated_lines>~50</estimated_lines>
  <depends>Task 4</depends>
  <action>
    Rename the main struct from AntQuicTransport to UdpTransportAdapter.

    1. In udp_transport_adapter.rs:
       - Rename `pub struct AntQuicTransport` to `pub struct UdpTransportAdapter`
       - Rename `AntQuicTransportConfig` to `UdpTransportAdapterConfig`
       - Update all impl blocks: `impl AntQuicTransport` → `impl UdpTransportAdapter`
       - Update constructor names if needed
       - Update all internal references to the struct name
       - Update all doc comments mentioning the old name

    2. In lib.rs:
       - Update re-exports to use new names
       - Keep deprecated type aliases for backward compatibility:
         ```rust
         #[deprecated(since = "0.3.0", note = "Use UdpTransportAdapter instead")]
         pub type AntQuicTransport = UdpTransportAdapter;

         #[deprecated(since = "0.3.0", note = "Use UdpTransportAdapterConfig instead")]
         pub type AntQuicTransportConfig = UdpTransportAdapterConfig;
         ```

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Update all doc comments
    - Add deprecated aliases for backward compat
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - Struct renamed to UdpTransportAdapter
    - Config renamed to UdpTransportAdapterConfig
    - All internal references updated
    - Deprecated aliases added
    - All tests pass
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 6: Implement TransportAdapter for UdpTransportAdapter</n>
  <activeForm>Implementing TransportAdapter trait</activeForm>
  <files>
    crates/transport/src/udp_transport_adapter.rs
  </files>
  <estimated_lines>~100</estimated_lines>
  <depends>Task 3, Task 5</depends>
  <action>
    Implement the TransportAdapter trait for UdpTransportAdapter.

    1. Add `impl TransportAdapter for UdpTransportAdapter` block
    2. Implement all trait methods:
       - `local_peer_id()` → return self.gossip_peer_id
       - `dial()` → wrap existing dial logic, return TransportError
       - `send()` → wrap existing send_to_peer, return TransportError
       - `recv()` → wrap existing receive_message, return TransportError
       - `close()` → wrap existing close, return TransportError
       - `connected_peers()` → delegate to existing method
       - `capabilities()` → return UDP capabilities:
         ```rust
         TransportCapabilities {
             supports_broadcast: false,
             max_message_size: 100 * 1024 * 1024, // 100MB from config
             typical_latency_ms: 50,
             is_reliable: true, // QUIC is reliable
         }
         ```
    3. Update existing methods to use TransportError where appropriate
    4. Keep GossipTransport impl unchanged (uses anyhow::Result)
    5. Add conversion from TransportError to anyhow::Error where needed

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Keep GossipTransport impl working
    - Use TransportError for TransportAdapter methods
    - Full doc comments
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - TransportAdapter implemented for UdpTransportAdapter
    - All trait methods working
    - TransportError used for error handling
    - GossipTransport still works
    - Unit tests for TransportAdapter methods
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 7: Update imports and re-exports</n>
  <activeForm>Updating imports across codebase</activeForm>
  <files>
    crates/transport/src/lib.rs
    crates/runtime/src/runtime.rs
    bin/cli/src/main.rs
    bin/coordinator/src/main.rs
    examples/transport_benchmark.rs
    examples/throughput_test.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 5, Task 6</depends>
  <action>
    Update all imports and re-exports to use the new names.

    1. In crates/transport/src/lib.rs:
       - Re-export new types: UdpTransportAdapter, UdpTransportAdapterConfig
       - Re-export TransportAdapter trait
       - Re-export TransportError, TransportResult
       - Re-export TransportCapabilities
       - Keep deprecated aliases for old names

    2. In crates/runtime/src/runtime.rs:
       - Update import: `AntQuicTransport` → `UdpTransportAdapter`
       - Update import: `AntQuicTransportConfig` → `UdpTransportAdapterConfig`
       - Update struct field type if exposed publicly
       - Update any constructor calls

    3. In bin/cli/src/main.rs:
       - Update any direct imports of transport types

    4. In bin/coordinator/src/main.rs:
       - Update any direct imports of transport types

    5. In examples/transport_benchmark.rs:
       - Update import: `AntQuicTransport` → `UdpTransportAdapter`
       - Update any direct usage of the type

    6. In examples/throughput_test.rs:
       - Update import: `AntQuicTransport` → `UdpTransportAdapter`
       - Update any direct usage of the type

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Use new names, not deprecated aliases
    - Keep deprecated aliases available for external consumers
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy --all-features -- -D warnings
    cargo build --all-features
  </verify>
  <done>
    - All internal imports use new names
    - Re-exports configured correctly
    - Examples compile and work
    - Binaries compile and work
    - Zero warnings
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 8: Verify all tests pass</n>
  <activeForm>Running full test suite verification</activeForm>
  <files>
    (all crates)
  </files>
  <estimated_lines>~10</estimated_lines>
  <depends>Task 7</depends>
  <action>
    Run the full test suite to verify all changes work correctly.

    1. Run cargo fmt check
    2. Run cargo clippy with -D warnings on all features
    3. Run cargo test with all features
    4. Verify test count is still 331+
    5. Document any test changes needed

    Requirements:
    - All 331+ tests must pass
    - Zero clippy warnings
    - Zero compilation warnings
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy --all-features -- -D warnings
    cargo test --all-features
  </verify>
  <done>
    - All tests passing (331+)
    - Zero warnings
    - Phase 2.1 complete
  </done>
</task>

## Exit Criteria
- [ ] All 8 tasks complete
- [ ] ant-quic updated to 0.19.0
- [ ] TransportError enum with thiserror working
- [ ] TransportAdapter trait defined and documented
- [ ] UdpTransportAdapter renamed and implementing TransportAdapter
- [ ] All imports updated across codebase
- [ ] All 331+ tests passing
- [ ] Zero clippy warnings
- [ ] Code reviewed via /review
