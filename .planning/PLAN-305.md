# Phase 3.5: Documentation Updates

## Overview
Update all documentation to reflect the simplified transport architecture where the custom multiplexer has been removed in favor of ant-quic's native transport infrastructure.

## Tasks

### Task 1: Update README.md transport section
- **Files**: `README.md`
- **Description**: Update the transport layer documentation to reflect that multi-transport routing now uses ant-quic's native TransportRegistry/ConnectionRouter. Remove references to the custom TransportMultiplexer, BLE stub, etc.
- **Tests**: N/A (documentation only)
- **Status**: pending

### Task 2: Update crate-level documentation
- **Files**: `crates/transport/src/lib.rs`
- **Description**: Update module-level documentation to accurately describe the simplified architecture. Remove deprecated migration tables and outdated examples.
- **Tests**: `cargo doc --no-deps` should succeed without warnings
- **Status**: pending

### Task 3: Update ROADMAP.md completion status
- **Files**: `.planning/ROADMAP.md`, `.planning/STATE.json`
- **Description**: Mark Phase 3.3-3.5 as complete in ROADMAP.md. Update STATE.json to reflect milestone completion. Note actual code reduction achieved (~4,000 lines).
- **Tests**: N/A
- **Status**: pending

### Task 4: Create ADR for transport simplification
- **Files**: `docs/adr/ADR-003-transport-layer-simplification.md`
- **Description**: Document the architectural decision to remove the custom TransportMultiplexer in favor of ant-quic's native capabilities. Include context, decision, consequences.
- **Tests**: N/A
- **Status**: pending

### Task 5: Update benchmarks.md
- **Files**: `docs/benchmarks.md`
- **Description**: Remove references to deleted transport_benchmark.rs example. Update benchmark documentation to reflect current examples (throughput_test.rs).
- **Tests**: N/A
- **Status**: pending
