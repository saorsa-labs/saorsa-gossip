# Phase 2.5: Transport Capability Requests

## Overview
Update the membership and pubsub modules to use TransportRequest for capability-based routing. This enables automatic transport selection based on message requirements (low-latency for control messages, bulk transfer for CRDT sync).

## Technical Decisions
- Breakdown approach: By module (GossipTransport API → Membership → PubSub → Fallback → Tests)
- Task size: Small (~50 lines per task)
- Testing strategy: Unit tests for capability routing, integration tests for module behavior
- Dependencies: TransportRequest from Phase 2.2, MultiplexedGossipTransport from Phase 2.3, GossipContext from Phase 2.4
- Pattern: Extend GossipTransport trait with capability-aware send methods

## Task Complexity Summary

| Task | Est. Lines | Files | Complexity | Model |
|------|-----------|-------|------------|-------|
| Task 1 | ~60 | 2 | standard | sonnet |
| Task 2 | ~40 | 1 | simple | haiku |
| Task 3 | ~40 | 1 | simple | haiku |
| Task 4 | ~60 | 1 | standard | sonnet |
| Task 5 | ~80 | 1 | standard | sonnet |
| Task 6 | ~50 | 1 | simple | haiku |

## Tasks

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 1: Add capability-aware send to GossipTransport</n>
  <activeForm>Adding capability-aware send to GossipTransport</activeForm>
  <files>
    crates/transport/src/gossip_transport.rs
    crates/transport/src/multiplexed_transport.rs
  </files>
  <estimated_lines>~60</estimated_lines>
  <depends></depends>
  <action>
    Extend GossipTransport trait with capability-aware send method:

    1. Add to GossipTransport trait in gossip_transport.rs:
       ```rust
       /// Sends data to a peer with specific transport requirements.
       ///
       /// This method allows specifying capability requirements for transport
       /// selection. If the requirements cannot be met, falls back to default transport.
       async fn send_with_request(
           &self,
           peer: PeerId,
           stream_type: GossipStreamType,
           data: Bytes,
           request: &TransportRequest,
       ) -> Result<()> {
           // Default implementation ignores request, uses stream_type routing
           self.send_to_peer(peer, stream_type, data).await
       }
       ```

    2. Implement in MultiplexedGossipTransport:
       - Use multiplexer.select_transport(request) instead of select_transport_for_stream
       - Fall back to default transport if no transport matches requirements

    3. Add re-export of TransportRequest in lib.rs if not already exported

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Default implementation maintains backward compatibility
    - Add doc comments with examples
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - GossipTransport trait has send_with_request method
    - MultiplexedGossipTransport uses TransportRequest for routing
    - Default implementation maintains backward compatibility
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 2: Add LowLatencyControl request helper</n>
  <activeForm>Adding LowLatencyControl request helper</activeForm>
  <files>
    crates/transport/src/multiplexer.rs
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends>Task 1</depends>
  <action>
    Add convenience constructors to TransportRequest:

    1. Add static methods to TransportRequest:
       ```rust
       /// Creates a request for low-latency control messages.
       ///
       /// Used by membership protocol for probes, joins, and heartbeats.
       #[must_use]
       pub fn low_latency_control() -> Self {
           Self::new().require(TransportCapability::LowLatencyControl)
       }

       /// Creates a request for bulk transfer.
       ///
       /// Used by pubsub for CRDT deltas and large messages.
       #[must_use]
       pub fn bulk_transfer() -> Self {
           Self::new().require(TransportCapability::BulkTransfer)
       }

       /// Creates a request for reliable delivery.
       ///
       /// Used when message delivery must be guaranteed.
       #[must_use]
       pub fn reliable() -> Self {
           Self::new().require(TransportCapability::ReliableDelivery)
       }
       ```

    2. Add tests for the convenience constructors

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Add doc comments
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport multiplexer
  </verify>
  <done>
    - TransportRequest has convenience constructors
    - Unit tests verify constructors
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 3: Update membership to use capability requests</n>
  <activeForm>Updating membership to use capability requests</activeForm>
  <files>
    crates/membership/src/lib.rs
  </files>
  <estimated_lines>~40</estimated_lines>
  <depends>Task 2</depends>
  <action>
    Update HyParViewMembership to use send_with_request for control messages:

    1. Add TransportRequest import:
       ```rust
       use saorsa_gossip_transport::{GossipStreamType, GossipTransport, TransportRequest};
       ```

    2. Update send_hyparview_message to use low-latency request:
       - Create TransportRequest::low_latency_control() for all HyParView messages
       - Use send_with_request instead of send_to_peer

    3. Update SwimDetector probe sends:
       - PING/ACK messages use low-latency request

    Note: If GossipTransport trait doesn't have send_with_request, the default
    implementation will fall back to send_to_peer, maintaining compatibility.

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Existing tests must pass
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-membership -- -D warnings
    cargo test -p saorsa-gossip-membership
  </verify>
  <done>
    - Membership uses TransportRequest for control messages
    - All membership tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 4: Update pubsub to use capability requests</n>
  <activeForm>Updating pubsub to use capability requests</activeForm>
  <files>
    crates/pubsub/src/lib.rs
  </files>
  <estimated_lines>~60</estimated_lines>
  <depends>Task 3</depends>
  <action>
    Update PlumtreePubSub to use capability-based routing:

    1. Add TransportRequest import

    2. Categorize messages by transport needs:
       - GOSSIP (tree maintenance): Use low_latency_control()
       - GRAFT/PRUNE (membership): Use low_latency_control()
       - Data messages (CRDT sync): Use bulk_transfer() for large messages

    3. Add message size threshold for bulk routing:
       - Messages > 64KB use bulk_transfer()
       - Smaller messages use default routing

    4. Update send methods to use send_with_request

    Requirements:
    - NO .unwrap() or .expect() in src/
    - Size threshold configurable (const for now)
    - Existing tests must pass
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-pubsub -- -D warnings
    cargo test -p saorsa-gossip-pubsub
  </verify>
  <done>
    - PubSub uses TransportRequest for routing
    - Large messages use bulk_transfer capability
    - All pubsub tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="standard" model="sonnet">
  <n>Task 5: Add fallback and capability tests</n>
  <activeForm>Adding fallback and capability tests</activeForm>
  <files>
    crates/transport/src/multiplexed_transport.rs
  </files>
  <estimated_lines>~80</estimated_lines>
  <depends>Task 4</depends>
  <action>
    Add comprehensive tests for capability-based routing:

    1. Test send_with_request routes correctly:
       - test_send_with_low_latency_request
       - test_send_with_bulk_request
       - test_send_with_reliable_request

    2. Test fallback behavior:
       - test_send_falls_back_when_capability_unavailable
       - test_send_uses_default_when_no_match

    3. Test backward compatibility:
       - test_send_to_peer_still_works
       - test_default_implementation_ignores_request

    Requirements:
    - Tests can use .unwrap()/.expect()
    - Cover all routing scenarios
    - Verify fallback behavior
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy -p saorsa-gossip-transport -- -D warnings
    cargo test -p saorsa-gossip-transport
  </verify>
  <done>
    - 7+ new tests for capability routing
    - Fallback behavior tested
    - All tests pass
  </done>
</task>

<task type="auto" priority="p1" complexity="simple" model="haiku">
  <n>Task 6: Full verification and documentation</n>
  <activeForm>Running full verification and documentation</activeForm>
  <files>
    crates/transport/src/lib.rs
    crates/membership/src/lib.rs
    crates/pubsub/src/lib.rs
  </files>
  <estimated_lines>~50</estimated_lines>
  <depends>Task 5</depends>
  <action>
    Final verification and documentation updates:

    1. Run full test suite:
       - cargo test --workspace
       - Verify 320+ tests pass

    2. Run clippy on all affected crates:
       - cargo clippy --workspace -- -D warnings

    3. Update module documentation:
       - Add capability routing examples to transport/src/lib.rs
       - Note capability usage in membership module docs
       - Note capability usage in pubsub module docs

    4. Verify re-exports:
       - TransportRequest exported from transport crate
       - TransportCapability exported from transport crate

    Requirements:
    - Zero warnings
    - All tests pass
    - Documentation complete
  </action>
  <verify>
    cargo fmt --all -- --check
    cargo clippy --workspace -- -D warnings
    cargo test --workspace
    cargo doc --workspace --no-deps
  </verify>
  <done>
    - 320+ tests passing
    - Zero warnings
    - Documentation updated
  </done>
</task>

## Exit Criteria
- [ ] All 6 tasks complete
- [ ] GossipTransport has send_with_request method
- [ ] Membership uses LowLatencyControl for probes
- [ ] PubSub uses BulkTransfer for large messages
- [ ] Fallback to default transport works
- [ ] Unit and integration tests passing
- [ ] Zero clippy warnings
- [ ] Code reviewed via /review
