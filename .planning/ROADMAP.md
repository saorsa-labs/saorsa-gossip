# GSD-Hybrid Roadmap

## Project: saorsa-gossip
Bug fixes and improvements: Bootstrap cache integration, FOAF discovery, shuffle protocol

---

## Codebase Foundation

### What Exists
- 12 library crates + 2 binaries
- 242 passing tests
- HyParView membership, Plumtree pubsub, SWIM failure detection
- Delta-CRDT sync (OR-Set, LWW-Register, Rga)
- Presence beacons and coordinator discovery
- Custom PeerCache and AdvertCache (to be replaced)

### What Needs Building
1. **GossipCacheAdapter** - Wraps ant-quic BootstrapCache with advert storage
2. **FOAF query handling** - Complete presence discovery (line 470)
3. **Shuffle protocol** - Complete HyParView shuffle (line 731)
4. **Test coverage** - Verify async behavior for beacons/shutdown

---

## Milestone 1: Production Bug Fixes & Improvements

### Phase 1: Bootstrap Cache Integration (HIGH PRIORITY) ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 1.1 | Create `GossipCacheAdapter` struct | ✅ | Wrap `Arc<BootstrapCache>` + `RwLock<HashMap<PeerId, CoordinatorAdvert>>` |
| 1.2 | Implement adapter methods | ✅ | `get_coordinator()`, `insert_advert()`, `select_coordinators()`, `prune_expired_adverts()` |
| 1.3 | Re-export ant-quic types | ✅ | `BootstrapCache`, `BootstrapCacheConfig`, `CachedPeer` from coordinator crate |
| 1.4 | Update Bootstrap struct | ✅ | Use GossipCacheAdapter instead of PeerCache |
| 1.5 | Update CoordinatorHandler | ✅ | Added PeerId-to-public-key binding security check |
| 1.6 | Deprecate old caches | ✅ | Deprecated `AdvertCache` and `PeerCache` with migration docs |
| 1.7 | Update tests | ✅ | 80 coordinator tests passing |
| 1.8 | Security fixes | ✅ | PeerId binding, FOAF validation, lock poisoning logging |

### Phase 2: FOAF Discovery Completion ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 2.1 | Implement `handle_foaf_query()` | ✅ | presence/src/lib.rs - Query handler with deduplication |
| 2.2 | Implement query forwarding | ✅ | TTL decrement, fanout=3, origin exclusion |
| 2.3 | Implement response aggregation | ✅ | PendingQuery struct, initiate_foaf_query(), find() |
| 2.4 | Add integration tests | ✅ | 9 new tests for multi-hop discovery (27 total presence tests) |

### Phase 3: Shuffle Protocol Completion ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 3.1 | Implement `spawn_shuffle_task()` | ✅ | Fixed to actually send SHUFFLE messages |
| 3.2 | Implement `handle_shuffle()` | ✅ | TTL forwarding + terminal node reply |
| 3.3 | Implement `handle_shuffle_reply()` | ✅ | Integrates peers into passive view |
| 3.4 | Add shuffle tests | ✅ | 23 membership tests (15 new shuffle tests) |
| 3.5 | Review fixes | ✅ | Silent failure logging, lock warnings |

### Phase 4: Test Coverage & Polish ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 4.1 | Verify beacon broadcast | ✅ | Updated test_start_beacons_broadcasts_periodically |
| 4.2 | Verify shutdown halts | ✅ | Updated test_stop_beacons_halts_broadcasting |
| 4.3 | Verify FOAF query params | ✅ | Updated test_find_foaf_random_walk, added pending_query_count() |
| 4.4 | Run full test suite | ✅ | 331 tests passing (32 presence tests) |
| 4.5 | Run clippy with -D warnings | ✅ | Zero warnings |

---

## Architecture Notes

### Cache Ownership Pattern
```
┌─────────────────────────────────────────────────────┐
│  Application (saorsa-node / communitas)             │
│  ┌───────────────────────────────────┐              │
│  │ Arc<BootstrapCache>               │ ← App owns   │
│  └───────────────────────────────────┘              │
│              │                                       │
│              ▼                                       │
│  ┌───────────────────────────────────┐              │
│  │ GossipCacheAdapter                │ ← Wraps      │
│  │ - cache: Arc<BootstrapCache>      │              │
│  │ - adverts: HashMap<PeerId, Advert>│              │
│  └───────────────────────────────────┘              │
│              │                                       │
│              ▼                                       │
│  ┌───────────────────────────────────┐              │
│  │ CoordinatorHandler / Bootstrap    │ ← Uses       │
│  └───────────────────────────────────┘              │
└─────────────────────────────────────────────────────┘
```

### Type Mappings
| ant-quic | saorsa-gossip | Notes |
|----------|---------------|-------|
| `PeerCapabilities.supports_coordination` | `PeerRoles.coordinator` | Direct map |
| `PeerCapabilities.supports_relay` | `PeerRoles.relay` | Direct map |
| `CachedPeer.quality_score` | `CoordinatorAdvert.score` | May need conversion |
| `NatType` | `NatClass` | May need enum mapping |

---

## Completion Log

### Phase 1: Bootstrap Cache Integration ✅
**Completed**: 2025-01-15
- Created `GossipCacheAdapter` wrapping `Arc<BootstrapCache>` with gossip-specific advert storage
- Added security fix: PeerId-to-public-key binding verification in `handle_advert()`
- Added security fix: FOAF response structural validation
- Fixed `select_best_from_adverts()` logic bug
- Added proper lock poisoning logging throughout
- Added age-based query ID pruning (30s expiry per SPEC2 §7.3)
- 80 coordinator tests passing

### Phase 2: FOAF Discovery Completion ✅
**Completed**: 2025-01-15
- Implemented `handle_foaf_query()` with deduplication via `seen_queries`
- Implemented query forwarding with TTL decrement and fanout of 3
- Added `PendingQuery` struct for response aggregation
- Implemented `initiate_foaf_query()` for initiating FOAF queries
- Updated `find()` method to use FOAF queries across all groups
- Added `cleanup_pending_queries()` for expired query cleanup
- Added 9 integration tests for multi-hop discovery
- 27 presence tests passing

### Phase 3: Shuffle Protocol Completion ✅
**Completed**: 2025-01-15
- Fixed `spawn_shuffle_task()` to actually send SHUFFLE messages (was TODO)
- `handle_shuffle()` implements TTL forwarding and terminal node reply
- `handle_shuffle_reply()` integrates received peers into passive view
- Added 15 new shuffle tests (23 total membership tests)
- Review fixes applied:
  - Silent serialization failures now logged with `warn!`
  - Double silent failure in probe task fixed
  - Lock contention warnings added to view accessors
  - TTL > 0 forwarding path tested
- 331 tests passing, clippy clean

### Phase 4: Test Coverage & Polish ✅
**Completed**: 2025-01-15
- Updated `test_start_beacons_broadcasts_periodically` to verify beacon self-storage
- Updated `test_stop_beacons_halts_broadcasting` to verify restart capability
- Updated `test_find_foaf_random_walk` with proper setup and verification
- Added `pending_query_count()` method for monitoring/testing
- 331 tests passing (32 presence tests, up from 27)
- Clippy clean with strict settings
