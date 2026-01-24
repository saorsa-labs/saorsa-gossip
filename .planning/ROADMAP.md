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

## Milestone 1: Production Bug Fixes & Improvements ✅

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

## Milestone 2: Transport Multi-Plexing Refactor ✅

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
### Phase 2.2: Implement TransportMultiplexer ✅
### Phase 2.3: Plumb Multiplexer into GossipTransport ✅
### Phase 2.4: Configuration & GossipContext ✅
### Phase 2.5: Transport Capability Requests ✅
### Phase 2.6: Transport Hints in Peer Adverts ✅
### Phase 2.7: Benchmarking & BLE Stub ✅

---

## Milestone 3: Transport Layer Simplification ✅

### Overview
Simplify saorsa-gossip's transport layer by leveraging ant-quic's native TransportRegistry, ConnectionRouter, and shared-socket capabilities. The custom TransportMultiplexer built in Milestone 2 is now redundant - ant-quic 0.20+ handles multi-transport routing internally.

### Problem Statement (from review)
- `Cargo.toml:41` pins ant-quic = "0.19", missing new multi-transport release
- `crates/transport/src/lib.rs` implements custom TransportMultiplexer that duplicates ant-quic
- `crates/transport/src/udp_transport_adapter.rs` doesn't expose transport_registry plumbing
- `crates/runtime/src/runtime.rs` wraps every transport in redundant MultiplexedGossipTransport
- Tests exercise custom multiplexer instead of real ant-quic paths

### Success Criteria
- ant-quic upgraded to version with TransportRegistry/ConnectionRouter
- Custom TransportMultiplexer retired (flatten onto ant-quic's P2pEndpoint)
- Runtime no longer wraps transports in redundant multiplexer
- Tests exercise real ant-quic flow, not custom multiplexer
- Documentation reflects that multi-transport routing lives in ant-quic
- Estimated code reduction: ~3000 lines (60% of transport crate)
- Zero compilation errors, zero warnings, all tests pass

### Technical Decisions
| Topic | Decision |
|-------|----------|
| Error Handling | Keep existing TransportError, adapt as needed |
| Async Model | Match existing pattern |
| Testing | Unit + Integration + Property-based tests |
| Documentation | Full public API docs, update README, DESIGN, ADRs |
| Migration | Staged deprecation for backward compatibility |

### Phase 3.1: Upgrade ant-quic Dependency ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 1.1 | Research latest ant-quic API | ✅ | Confirmed TransportRegistry, ConnectionRouter in ant-quic 0.20 |
| 1.2 | Bump ant-quic version | ✅ | Updated Cargo.toml to ant-quic = "0.20" |
| 1.3 | Run cargo update -p ant-quic | ✅ | Lock file updated |
| 1.4 | Fix compilation errors | ✅ | TransportAddr API change, clippy fix |
| 1.5 | Verify CI builds | ✅ | 363 tests pass, zero warnings |
| 1.6 | Re-export new types | ✅ | AntTransportRegistry, TransportAddr, TransportType, etc. |

### Phase 3.2: Deprecate Custom Multiplexer ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 2.1 | Deprecate TransportMultiplexer | ✅ | Added #[deprecated] with migration docs |
| 2.2 | Deprecate TransportRegistry | ✅ | Points to AntTransportRegistry |
| 2.3 | Deprecate TransportRequest | ✅ | Points to TransportType |
| 2.4 | Deprecate MultiplexedGossipTransport | ✅ | Points to ant-quic SharedTransport |
| 2.5 | Deprecate BLE stub | ✅ | Points to ant_quic::transport::ble |
| 2.6 | Update lib.rs exports | ✅ | Added migration documentation |
| 2.7 | Add #![allow(deprecated)] | ✅ | Internal consumers allowed during migration |

### Phase 3.3: Wire Runtime Directly to ant-quic ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 3.1 | Remove multiplexer wrapping | ✅ | GossipRuntime uses UdpTransportAdapter directly |
| 3.2 | Remove transport descriptors | ✅ | Deleted with multiplexer |
| 3.3 | Update GossipContext | ✅ | Simplified transport config |
| 3.4 | Backward compatibility shim | ✅ | Skipped - no consumers, deleted directly |

### Phase 3.4: Clean Up Tests ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 4.1 | Remove multiplexer tests | ✅ | Deleted with multiplexer.rs |
| 4.2 | Remove BLE stub tests | ✅ | Deleted with ble_transport_adapter.rs |
| 4.3 | Remove transport_benchmark | ✅ | Deleted example |
| 4.4 | Verify remaining tests pass | ✅ | 319 tests pass |

### Phase 3.5: Documentation Updates ✅
| # | Task | Status | Details |
|---|------|--------|---------|
| 5.1 | Update README.md | ✅ | Updated UdpTransportAdapter, removed benchmark refs |
| 5.2 | Update crate docs | ✅ | transport/src/lib.rs simplified |
| 5.3 | Create ADR | ✅ | ADR-011-transport-layer-simplification.md |
| 5.4 | Update benchmarks.md | ✅ | Removed transport_benchmark references |

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

### Transport Layer (Post-Milestone 3)
```
┌─────────────────────────────────────────────────────┐
│  GossipRuntime                                       │
│  ┌───────────────────────────────────┐              │
│  │ AntQuicTransportAdapter           │ ← Direct     │
│  │ - endpoint: P2pEndpoint           │              │
│  │ - registry: TransportRegistry     │ ← ant-quic   │
│  └───────────────────────────────────┘              │
│              │                                       │
│              ▼                                       │
│  ┌───────────────────────────────────┐              │
│  │ ant-quic (native routing)         │              │
│  │ - ConnectionRouter                │              │
│  │ - TransportRegistry               │              │
│  │ - SharedSocket                    │              │
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

### Milestone 1 ✅
**Completed**: 2025-01-15
- Bootstrap cache integration
- FOAF discovery completion
- Shuffle protocol completion
- Test coverage & polish

### Milestone 2 ✅
**Completed**: 2026-01-23
- Transport multi-plexing refactor
- Custom TransportMultiplexer with capability-based routing
- BLE stub for testing
- Property-based tests for routing
- 320+ tests passing

### Milestone 3 ✅
**Completed**: 2026-01-24
- Transport layer simplification
- Upgraded ant-quic 0.19 → 0.20
- Removed custom multiplexer in favor of ant-quic native capabilities
- **Actual LOC reduction: ~4,130 lines**
- Deleted: multiplexer.rs, multiplexed_transport.rs, ble_transport_adapter.rs, transport_benchmark.rs
- 319 tests pass, zero warnings
