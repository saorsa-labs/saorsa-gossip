# Gossip Protocol Benchmarks

This document describes how to run and interpret benchmarks for Saorsa Gossip.

## Overview

The primary throughput benchmark (`examples/throughput_test.rs`) measures:

- Message delivery throughput
- CRDT synchronization performance
- Gossip protocol efficiency

## Running Benchmarks

### Prerequisites

Build in release mode for accurate measurements:

```bash
cargo build --release --examples
```

### Throughput Test

The throughput test measures gossip message delivery rates:

```bash
cargo run --example throughput_test --release
```

### Criterion Benchmarks

For more detailed microbenchmarks using the criterion framework:

```bash
cargo bench
```

This runs benchmarks for:
- CRDT operations (OR-Set, LWW-Register, RGA)
- Message serialization/deserialization
- Peer ID operations

## Transport Layer

As of v0.3.0, the transport layer uses `UdpTransportAdapter` which wraps the ant-quic P2P endpoint. Multi-transport routing (UDP, BLE, LoRa) is handled natively by ant-quic's `TransportRegistry` and `ConnectionRouter`.

For transport-level benchmarks, see the [ant-quic documentation](https://github.com/maidsafe/ant-quic).

## Performance Expectations

On localhost with release builds:

| Message Size | Expected Throughput |
|--------------|---------------------|
| 1 KB | 1-10 Mbps |
| 10 KB | 10-100 Mbps |
| 100 KB | 100-500 Mbps |
| 1 MB | 200-800 Mbps |
| 10+ MB | 300-1000 Mbps |

**Factors affecting performance:**
- System load
- Available memory
- Network conditions (for remote peers)
- Debug vs release build

## Troubleshooting

### Low Throughput

- Use `--release` flag for accurate measurements
- Check system load with `top` or `htop`
- For remote peers, check network latency with `ping`

### Connection Issues

- Ensure peers are reachable on the specified addresses
- Check firewall rules for UDP traffic
- Verify bootstrap peers are running
