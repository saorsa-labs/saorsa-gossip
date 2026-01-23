# Transport Benchmarking Guide

This document describes how to run and interpret transport layer benchmarks for Saorsa Gossip.

## Overview

The transport benchmark (`examples/transport_benchmark.rs`) measures:

- Connection establishment time
- Throughput (MB/s and Mbps)
- Message delivery success rate
- Per-transport statistics (when using multiplexed mode)

## Running Benchmarks

### Prerequisites

Build in release mode for accurate measurements:

```bash
cargo build --release --example transport_benchmark
```

### Basic Benchmark (Direct UDP)

**Terminal 1 - Start coordinator (receiver):**
```bash
cargo run --example transport_benchmark --release -- coordinator --bind 127.0.0.1:8000
```

**Terminal 2 - Run benchmark client:**
```bash
cargo run --example transport_benchmark --release -- benchmark \
    --coordinator 127.0.0.1:8000 \
    --bind 127.0.0.1:9000
```

### Multiplexed Transport Mode

Tests the `TransportMultiplexer` and `MultiplexedGossipTransport`:

```bash
cargo run --example transport_benchmark --release -- benchmark \
    --coordinator 127.0.0.1:8000 \
    --bind 127.0.0.1:9000 \
    --multiplexed
```

### With BLE Transport Stub

Tests multi-transport routing with simulated BLE characteristics:

```bash
cargo run --example transport_benchmark --release -- benchmark \
    --coordinator 127.0.0.1:8000 \
    --bind 127.0.0.1:9000 \
    --multiplexed \
    --ble
```

**Note:** The `--ble` flag requires `--multiplexed` mode.

## Command-Line Options

| Flag | Description |
|------|-------------|
| `--bind <ADDR>` | Local address to bind (e.g., `127.0.0.1:9000`) |
| `--coordinator <ADDR>` | Coordinator address to connect to |
| `--multiplexed` | Use `MultiplexedGossipTransport` instead of direct UDP |
| `--ble` | Add BLE transport stub (requires `--multiplexed`) |

## Benchmark Configuration

The benchmark tests several message sizes:

| Size | Description |
|------|-------------|
| 1 KB | Small control messages |
| 10 KB | Typical gossip messages |
| 100 KB | Medium CRDT updates |
| 1 MB | Large CRDT deltas |
| 10 MB | Bulk synchronization |
| 50 MB | Major state transfers |
| 100 MB | Stress test |

Each size is tested with 10 iterations to measure variance.

## Understanding Results

### Connection Statistics

```
Connection 1: 0.123s (PeerId: abc123...)
```

Shows time to establish QUIC connection with the coordinator.

### Per-Size Results

```
Test 1/7: 1024 bytes (0.001 MB)
  [1/10] Sending... ✓ 0.002s (4.10 Mbps, 0.51 MB/s)
  ...
  Summary:
    Success: 10/10
    Avg Duration: 0.002s
    Avg Throughput: 4.05 Mbps (0.50 MB/s)
```

### BLE Transport Statistics

When `--ble` is enabled:

```
📡 BLE Transport Statistics
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Messages sent: 0
  Bytes sent: 0 bytes
  MTU: 512 bytes
  Typical latency: 100ms
```

The BLE stub does not actually send messages (it's a simulation), but shows its configured characteristics.

## BLE Transport Stub

The `BleTransportAdapter` simulates constrained BLE link characteristics:

| Property | Value |
|----------|-------|
| MTU | 512 bytes |
| Latency | 50-150ms (random) |
| Reliability | Reliable (simulated ACKs) |
| Bulk Transfer | Not supported |

This enables testing fallback routing behavior when messages exceed BLE's MTU.

## Transport Capabilities

| Transport | Low Latency | Bulk Transfer | Broadcast | Offline Ready |
|-----------|-------------|---------------|-----------|---------------|
| UDP/QUIC  | Yes         | Yes           | No        | No            |
| BLE (stub)| Yes         | No            | No        | No            |
| LoRa      | No          | No            | Yes       | Yes           |

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
- Network conditions (for remote coordinators)
- Debug vs release build

## Troubleshooting

### Connection Refused

Ensure the coordinator is running and listening on the specified address.

### Low Throughput

- Use `--release` flag for accurate measurements
- Check system load with `top` or `htop`
- For remote coordinators, check network latency with `ping`

### BLE Messages Failing

Messages over 512 bytes will fail on the BLE transport due to MTU limits.
The multiplexer should fall back to UDP for large messages.
