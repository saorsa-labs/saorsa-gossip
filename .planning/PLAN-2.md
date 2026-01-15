# Phase 2: FOAF Discovery Completion

## Overview
Complete the Friend-of-a-Friend (FOAF) presence discovery implementation in the presence crate.
This enables users to find each other via random-walk queries through the mesh.

## Target Crate
`crates/presence/src/lib.rs`

## Tasks

### Task 2.1: Implement handle_foaf_query()
**Type**: auto
**Location**: `crates/presence/src/lib.rs:470`

Implement the FOAF query handler in `handle_presence_message()` for `PresenceMessage::Query`.

**Requirements**:
- Check if we have presence records for the requested topic
- If yes, respond with known records via `PresenceMessage::QueryResponse`
- If TTL > 0, forward query to random peers (fanout = 3)
- Decrement TTL before forwarding
- Track seen query IDs to prevent loops (deduplication)

**Pattern**: Follow coordinator FOAF pattern from `crates/coordinator/src/handler.rs`

### Task 2.2: Implement query forwarding
**Type**: auto
**Location**: `crates/presence/src/lib.rs`

Add methods to forward FOAF queries:
- `forward_foaf_query(&self, query: PresenceMessage, exclude: &PeerId)`
- Select random peers from `broadcast_peers` (fanout = 3)
- Send via transport using `GossipStreamType::Membership` or new presence stream

**Requirements**:
- Random peer selection (use rand crate)
- Exclude the peer we received query from
- Respect TTL limits (max 3-4 hops)

### Task 2.3: Implement response aggregation
**Type**: auto
**Location**: `crates/presence/src/lib.rs`

Track pending queries and aggregate responses:
- Add `pending_queries: HashMap<QueryId, PendingQuery>` field
- Store originator, timeout, received responses
- Deduplicate records by peer ID
- Return aggregated results to original caller

**Requirements**:
- Query timeout (30 seconds, per SPEC2)
- Deduplication of presence records
- Clean up expired queries

### Task 2.4: Add integration tests
**Type**: auto
**Location**: `crates/presence/src/lib.rs` (tests module)

Add tests for multi-hop FOAF discovery:
- Test query creation and forwarding
- Test response aggregation
- Test TTL exhaustion
- Test deduplication

## Verification Commands
```bash
cargo fmt --all -- --check
cargo clippy --all-features -p saorsa-gossip-presence -- -D clippy::panic -D clippy::unwrap_used -D clippy::expect_used
cargo test --all-features -p saorsa-gossip-presence
```

## Dependencies
- Phase 1 complete (Bootstrap Cache Integration) ✓
