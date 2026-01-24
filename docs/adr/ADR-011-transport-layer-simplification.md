# ADR-011: Transport Layer Simplification

## Status

Accepted

## Date

2026-01-24

## Context

In Milestone 2 (Transport Multi-Plexing), we implemented a custom `TransportMultiplexer` and `MultiplexedGossipTransport` to support multiple transport types (UDP/QUIC, BLE, LoRa) with capability-based routing. This added approximately 3,200 lines of code including:

- `multiplexer.rs` - Custom transport registry and request routing (~1,550 lines)
- `multiplexed_transport.rs` - Wrapper implementing GossipTransport trait (~1,198 lines)
- `ble_transport_adapter.rs` - BLE transport stub for testing (~458 lines)
- `transport_benchmark.rs` - Example demonstrating multi-transport (~646 lines)

Shortly after completing Milestone 2, ant-quic 0.20 was released with native multi-transport capabilities:

- `TransportRegistry` - Native transport type management
- `ConnectionRouter` - Built-in capability-based routing
- `SharedSocket` - Efficient socket sharing across transports
- Native BLE and LoRa transport modules

Our custom implementation now duplicated functionality that ant-quic handles natively.

## Decision

Remove the custom transport multiplexer infrastructure and rely on ant-quic's native multi-transport capabilities:

1. **Delete custom multiplexer code**: Remove `multiplexer.rs`, `multiplexed_transport.rs`, `ble_transport_adapter.rs`
2. **Simplify GossipTransport trait**: Remove `send_with_request()` method, use `send_to_peer()` for all cases
3. **Update runtime**: Use `UdpTransportAdapter` directly instead of wrapping in `MultiplexedGossipTransport`
4. **Re-export ant-quic types**: Provide `AntTransportRegistry`, `TransportType`, etc. from transport crate
5. **Update documentation**: Reflect simplified architecture

## Consequences

### Positive

- **~4,130 lines removed** - Significant reduction in maintenance burden
- **Simpler architecture** - One less abstraction layer between gossip and network
- **Better performance** - ant-quic's native routing is optimized for their connection model
- **Future compatibility** - Automatic access to new ant-quic transport features
- **Reduced testing surface** - No need to maintain custom multiplexer tests

### Negative

- **Less control** - Capability routing decisions now made by ant-quic
- **API coupling** - More tightly coupled to ant-quic's API design
- **Migration effort** - Consumers using deprecated types need to update

### Neutral

- **Testing approach** - Multi-transport testing now uses ant-quic's test utilities
- **Constrained transport stubs** - BLE/LoRa stubs available from ant-quic::transport

## Implementation

### Files Deleted (Milestone 3)

| File | Lines | Purpose |
|------|-------|---------|
| `multiplexer.rs` | 1,550 | Custom TransportMultiplexer |
| `multiplexed_transport.rs` | 1,198 | GossipTransport wrapper |
| `ble_transport_adapter.rs` | 458 | BLE stub |
| `transport_benchmark.rs` | 646 | Multi-transport example |
| Various test code | ~278 | Multiplexer tests |
| **Total** | **~4,130** | |

### Architecture After Simplification

```
┌─────────────────────────────────────────────────────┐
│  GossipRuntime                                       │
│  ┌───────────────────────────────────┐              │
│  │ UdpTransportAdapter               │ ← Direct     │
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

## Related

- [Milestone 2: Transport Multi-Plexing](../.planning/ROADMAP.md) - Original multiplexer implementation
- [Milestone 3: Transport Simplification](../.planning/ROADMAP.md) - This refactoring
- [ant-quic 0.20 release](https://github.com/maidsafe/ant-quic) - Native multi-transport
