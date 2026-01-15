# GSD-Hybrid Issues Backlog

## P0: Blockers (Immediate)
_None_

## P1: Next Phase

### [ISSUE-001] Duplicate caching logic
- **Priority**: P1
- **Type**: tech-debt
- **Context**: saorsa-gossip has custom PeerCache/AdvertCache while ant-quic provides BootstrapCache
- **Proposed**: Create adapter, remove duplicates
- **Blocked By**: None - ready to implement

## P2: This Milestone

### [ISSUE-002] FOAF query handling not implemented
- **Priority**: P2
- **Type**: feature
- **Context**: presence/src/lib.rs:470 has TODO for FOAF query handling
- **Proposed**: Implement handle_foaf_query, forwarding, response aggregation
- **Blocked By**: None

### [ISSUE-003] Shuffle protocol incomplete
- **Priority**: P2
- **Type**: feature
- **Context**: membership/src/lib.rs:731 logs tick but doesn't send SHUFFLE
- **Proposed**: Implement send_shuffle, handle_shuffle, handle_shuffle_reply
- **Blocked By**: None

### [ISSUE-004] Test TODOs in presence
- **Priority**: P2
- **Type**: test
- **Context**: Lines 563, 578, 726 have verification TODOs
- **Proposed**: Add assertions for beacon broadcast, shutdown, FOAF params
- **Blocked By**: ISSUE-002 (FOAF implementation)

## P3: Future

### [ISSUE-005] Real transport integration tests
- **Priority**: P3
- **Type**: test
- **Context**: Tests use MockTransport, need real ant-quic integration
- **Proposed**: Add integration tests with actual QUIC connections
- **Blocked By**: Phase 1 completion

---

## Issue Template
```
### [ISSUE-XXX] Title
- **Priority**: P0/P1/P2/P3
- **Type**: bug/feature/tech-debt/question
- **Context**: Why this matters
- **Proposed**: Suggested approach
- **Blocked By**: Dependencies (if any)
```
