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

---

## Milestone 2: Transport Multi-Plexing Refactor

### Overview
Refactor the transport layer to support multiple transports (UDP/QUIC, BLE, LoRa) through a unified multiplexer architecture. This enables gossip protocols to work across constrained and broadband links while maintaining the existing GossipTransport trait API.

### Success Criteria
- All planned transports work correctly (UDP, BLE stubs, LoRa stubs)
- TransportMultiplexer routes messages based on capability requirements
- Peer adverts include transport-type hints for BLE bridge discovery
- Dual-transport benchmarks compare constrained vs broadband links
- Zero compilation warnings, zero test failures

### Technical Decisions
| Topic | Decision |
|-------|----------|
| Error Handling | Dedicated error types with thiserror |
| Async Model | Match existing pattern (async methods, tokio::spawn) |
| Testing | Unit tests + Integration tests + Property-based tests |
| Documentation | Full public API docs with examples |
| Priority | Technical foundation first (registry/multiplexer) |

### Phase 2.1: Error Handling & Adapter Trait ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 1.1 | Create `TransportAdapter` trait | ✅ | Common transport operations |
| 1.2 | Create `TransportError` enum | ✅ | Dedicated error types with thiserror |
| 1.3 | Rename `AntQuicTransport` to `UdpTransportAdapter` | ✅ | File and struct rename |
| 1.4 | Implement `TransportAdapter` for `UdpTransportAdapter` | ✅ | Wrap existing functionality |
| 1.5 | Update all imports and references | ✅ | crates/runtime, examples, tests |

### Phase 2.2: Implement TransportMultiplexer ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 2.1 | Create `TransportMultiplexer` struct | ✅ | Registry + routing logic |
| 2.2 | Create `TransportCapability` enum | ✅ | LowLatencyControl, BulkTransfer, Broadcast, OfflineReady |
| 2.3 | Create `TransportDescriptor` enum | ✅ | Udp, Ble, Lora, Custom variants |
| 2.4 | Implement transport registration | ✅ | `register_transport()` API |
| 2.5 | Implement capability-based routing | ✅ | Route based on request requirements |
| 2.6 | Unit tests for multiplexer | ✅ | Routing logic tests (268 tests) |

### Phase 2.3: Plumb Multiplexer into GossipTransport ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 3.1 | Create `MultiplexedGossipTransport` | ✅ | Implements GossipTransport |
| 3.2 | Wire multiplexer to runtime | ✅ | GossipRuntimeBuilder integration |
| 3.3 | Backward compatibility | ✅ | Single-transport mode works |
| 3.4 | Integration tests | ✅ | 14 unit + 6 integration tests |
| 3.5 | Update examples | ✅ | --multiplexed flag in benchmark |

### Phase 2.4: Configuration & GossipContext ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 4.1 | Create `GossipContext` struct | ✅ | Central configuration object |
| 4.2 | Create `GossipContextBuilder` | ✅ | Builder pattern for config |
| 4.3 | Public API for communitas-core | ✅ | Clean interface for consumers |
| 4.4 | Integration tests | ✅ | GossipContext::build() tests |

### Phase 2.5: Transport Capability Requests ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 5.1 | Create `TransportRequest` type | ✅ | Builder with require/exclude/prefer |
| 5.2 | Add `send_with_request()` to GossipTransport | ✅ | Capability-aware send |
| 5.3 | Update membership module | ✅ | Request "LowLatencyControl" |
| 5.4 | Update pubsub module | ✅ | Request "BulkTransfer" for large |
| 5.5 | Tests for capability requests | ✅ | 320+ tests passing |
| 5.6 | Documentation | ✅ | Module docs with examples |

### Phase 2.6: Transport Hints in Peer Adverts ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 6.1 | Create `TransportHint` struct | ✅ | transport_type, endpoint, available |
| 6.2 | Extend `CoordinatorAdvert` | ✅ | Add transport_hints field |
| 6.3 | Update serialization | ✅ | CBOR with #[serde(default)] |
| 6.4 | Helper methods | ✅ | has_transport(), available_transports() |
| 6.5 | SignableFields update | ✅ | Include hints in signature |
| 6.6 | Tests | ✅ | 12 transport hint tests |

### Phase 2.7: Benchmarking & BLE Stub ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 7.1 | Create `BleTransportAdapter` | ✅ | Simulated BLE (512B MTU, 50-150ms latency) |
| 7.2 | Add `MtuExceeded` error | ✅ | TransportError variant |
| 7.3 | Add `--ble` flag to benchmark | ✅ | BLE transport simulation |
| 7.4 | Property-based tests | ✅ | 9 proptest tests for routing |
| 7.5 | Benchmark documentation | ✅ | docs/benchmarks.md |
| 7.6 | Per-transport statistics | ✅ | BLE stats display in benchmark |

---

## Files to be Modified (Milestone 2)

| File | Changes |
|------|---------|
| `crates/transport/src/lib.rs` | Add TransportAdapter trait, TransportCapability, re-exports |
| `crates/transport/src/ant_quic_transport.rs` | Rename to udp_transport_adapter.rs |
| `crates/transport/src/multiplexer.rs` | NEW: TransportMultiplexer implementation |
| `crates/transport/src/error.rs` | NEW: TransportError enum |
| `crates/runtime/src/runtime.rs` | Add GossipContext, update runtime |
| `crates/coordinator/src/lib.rs` | CoordinatorAdvert transport hints |
| `crates/membership/src/lib.rs` | Transport capability requests |
| `crates/pubsub/src/lib.rs` | Transport capability requests |
| `examples/transport_benchmark.rs` | Dual-transport benchmarks |
