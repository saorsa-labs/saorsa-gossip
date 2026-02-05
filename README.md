# Saorsa Gossip Overlay

[![CI](https://github.com/dirvine/saorsa-gossip/workflows/CI/badge.svg)](https://github.com/dirvine/saorsa-gossip/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)

## 📦 Published Crates

| Crate | Version | Docs | Downloads |
|-------|---------|------|-----------|
| [saorsa-gossip-types] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-types.svg)][types-crate] | [![Docs](https://docs.rs/saorsa-gossip-types/badge.svg)][types-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-types.svg)][types-crate] |
| [saorsa-gossip-identity] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-identity.svg)][identity-crate] | [![Docs](https://docs.rs/saorsa-gossip-identity/badge.svg)][identity-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-identity.svg)][identity-crate] |
| [saorsa-gossip-transport] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-transport.svg)][transport-crate] | [![Docs](https://docs.rs/saorsa-gossip-transport/badge.svg)][transport-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-transport.svg)][transport-crate] |
| [saorsa-gossip-membership] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-membership.svg)][membership-crate] | [![Docs](https://docs.rs/saorsa-gossip-membership/badge.svg)][membership-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-membership.svg)][membership-crate] |
| [saorsa-gossip-pubsub] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-pubsub.svg)][pubsub-crate] | [![Docs](https://docs.rs/saorsa-gossip-pubsub/badge.svg)][pubsub-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-pubsub.svg)][pubsub-crate] |
| [saorsa-gossip-coordinator] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-coordinator.svg)][coordinator-crate] | [![Docs](https://docs.rs/saorsa-gossip-coordinator/badge.svg)][coordinator-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-coordinator.svg)][coordinator-crate] |
| [saorsa-gossip-rendezvous] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-rendezvous.svg)][rendezvous-crate] | [![Docs](https://docs.rs/saorsa-gossip-rendezvous/badge.svg)][rendezvous-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-rendezvous.svg)][rendezvous-crate] |
| [saorsa-gossip-groups] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-groups.svg)][groups-crate] | [![Docs](https://docs.rs/saorsa-gossip-groups/badge.svg)][groups-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-groups.svg)][groups-crate] |
| [saorsa-gossip-presence] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-presence.svg)][presence-crate] | [![Docs](https://docs.rs/saorsa-gossip-presence/badge.svg)][presence-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-presence.svg)][presence-crate] |
| [saorsa-gossip-crdt-sync] | [![Crates.io](https://img.shields.io/crates/v/saorsa-gossip-crdt-sync.svg)][crdt-crate] | [![Docs](https://docs.rs/saorsa-gossip-crdt-sync/badge.svg)][crdt-docs] | [![Downloads](https://img.shields.io/crates/d/saorsa-gossip-crdt-sync.svg)][crdt-crate] |

[saorsa-gossip-types]: #core-crates
[saorsa-gossip-identity]: #core-crates
[saorsa-gossip-transport]: #core-crates
[saorsa-gossip-membership]: #core-crates
[saorsa-gossip-pubsub]: #core-crates
[saorsa-gossip-coordinator]: #core-crates
[saorsa-gossip-rendezvous]: #core-crates
[saorsa-gossip-groups]: #core-crates
[saorsa-gossip-presence]: #core-crates
[saorsa-gossip-crdt-sync]: #core-crates

[types-crate]: https://crates.io/crates/saorsa-gossip-types
[types-docs]: https://docs.rs/saorsa-gossip-types
[identity-crate]: https://crates.io/crates/saorsa-gossip-identity
[identity-docs]: https://docs.rs/saorsa-gossip-identity
[transport-crate]: https://crates.io/crates/saorsa-gossip-transport
[transport-docs]: https://docs.rs/saorsa-gossip-transport
[membership-crate]: https://crates.io/crates/saorsa-gossip-membership
[membership-docs]: https://docs.rs/saorsa-gossip-membership
[pubsub-crate]: https://crates.io/crates/saorsa-gossip-pubsub
[pubsub-docs]: https://docs.rs/saorsa-gossip-pubsub
[coordinator-crate]: https://crates.io/crates/saorsa-gossip-coordinator
[coordinator-docs]: https://docs.rs/saorsa-gossip-coordinator
[rendezvous-crate]: https://crates.io/crates/saorsa-gossip-rendezvous
[rendezvous-docs]: https://docs.rs/saorsa-gossip-rendezvous
[groups-crate]: https://crates.io/crates/saorsa-gossip-groups
[groups-docs]: https://docs.rs/saorsa-gossip-groups
[presence-crate]: https://crates.io/crates/saorsa-gossip-presence
[presence-docs]: https://docs.rs/saorsa-gossip-presence
[crdt-crate]: https://crates.io/crates/saorsa-gossip-crdt-sync
[crdt-docs]: https://docs.rs/saorsa-gossip-crdt-sync

A **post-quantum secure gossip overlay network** for decentralized peer-to-peer communication. Designed to replace DHT-based discovery with a contact-graph-aware gossip protocol, providing low-latency broadcast, partition tolerance, and quantum-resistant cryptography.

## 🎯 Overview

Saorsa Gossip implements a complete gossip overlay with:

- **Post-Quantum Cryptography**: ML-KEM-768 + ML-DSA-65 + ChaCha20-Poly1305 (FIPS 203/204)
- **QUIC Transport**: Low-latency, NAT-traversal with connection migration
- **MLS Group Security**: RFC 9420 compliant end-to-end encryption with ChaCha20-Poly1305
- **Gossip Protocols**: HyParView, SWIM, Plumtree for robust dissemination
- **Local-First CRDTs**: Delta-CRDTs with anti-entropy synchronization
- **No DHT**: Contact-graph-based discovery, no global directory

**Status**: ⚠️ **Alpha (workspace v0.2.1)** – libraries compile and ship with >260 automated tests, but the CLI/coordinator binaries are still experimental and several protocols (e.g. presence MLS export) are not finalized. See [DESIGN.md](DESIGN.md) for current limitations.

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   Saorsa Gossip                         │
│                                                         │
│  ┌─────────┐  ┌──────────┐  ┌─────────┐  ┌─────────┐ │
│  │Presence │  │  PubSub  │  │  CRDT   │  │ Groups  │ │
│  │(Beacons)│  │(Plumtree)│  │  Sync   │  │  (MLS)  │ │
│  └────┬────┘  └─────┬────┘  └────┬────┘  └────┬────┘ │
│       │             │            │            │       │
│  ┌────┴─────────────┴────────────┴────────────┴────┐ │
│  │            Membership Layer                      │ │
│  │         (HyParView + SWIM)                       │ │
│  └──────────────────┬───────────────────────────────┘ │
│                     │                                  │
│  ┌──────────────────┴───────────────────────────────┐ │
│  │          Transport Layer (ant-quic)               │ │
│  │   QUIC + PQC TLS 1.3 + NAT Traversal            │ │
│  └──────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

### Core Crates

This repository tracks the Saorsa Gossip workspace at **v0.2.1**. Some crates are published on [crates.io](https://crates.io) already, but others are only available from source while the alpha stabilises.

| Crate | Purpose | Why It's Important |
|-------|---------|-------------------|
| [**types**](https://crates.io/crates/saorsa-gossip-types) | Core types (TopicId, PeerId, MessageHeader, wire formats) | **Foundation** - Defines all fundamental data structures and message formats used across the entire network. Includes BLAKE3-based message ID generation and CBOR wire serialization. |
| [**identity**](https://crates.io/crates/saorsa-gossip-identity) | ML-DSA-65 key generation, signing, and verification | **Security Core** - Provides quantum-resistant digital signatures for all messages. Every peer identity is backed by ML-DSA-65 keypairs, ensuring authenticity in a post-quantum world. |
| [**transport**](https://crates.io/crates/saorsa-gossip-transport) | QUIC transport with ant-quic, NAT traversal | **Network Layer** - Handles all peer-to-peer communication with low-latency QUIC streams. Includes hole-punching for NAT traversal and connection migration for mobile nodes. |
| [**membership**](https://crates.io/crates/saorsa-gossip-membership) | HyParView partial views + SWIM failure detection | **Peer Discovery** - Maintains partial views of the network (8-12 active peers, 64-128 passive). SWIM detects failures in <5s, HyParView heals partitions through periodic shuffles. Critical for network connectivity. |
| [**pubsub**](https://crates.io/crates/saorsa-gossip-pubsub) | Plumtree epidemic broadcast with EAGER/IHAVE/IWANT | **Message Dissemination** - Efficiently broadcasts messages to all topic subscribers. Uses spanning tree (EAGER) for low latency and lazy links (IHAVE) for redundancy. Achieves <500ms P50 broadcast latency. |
| [**coordinator**](https://crates.io/crates/saorsa-gossip-coordinator) | Bootstrap node discovery, address reflection, relay | **Network Bootstrap** - Publishes Coordinator Adverts (ML-DSA signed) as *hints*. Peers validate reachability and success rates before selecting coordinators/relays ("measure, don't trust"). |
| [**rendezvous**](https://crates.io/crates/saorsa-gossip-rendezvous) | k=16 rendezvous sharding for global findability | **Global Discovery** - Implements 65,536 content-addressed shards (BLAKE3-based) for finding peers without DHTs. Providers publish signed summaries to deterministic shards, enabling discovery through capability queries. |
| [**groups**](https://crates.io/crates/saorsa-gossip-groups) | MLS group key derivation with BLAKE3 KDF | **Group Security** - Wraps MLS (RFC 9420) for end-to-end encrypted group messaging. Derives presence beaconing secrets from MLS exporter contexts using BLAKE3 keyed hashing. Essential for private group communication. |
| [**presence**](https://crates.io/crates/saorsa-gossip-presence) | MLS-derived beacon broadcasting, FOAF queries | **Online Detection** - Broadcasts encrypted presence beacons (10-15 min TTL) derived from group secrets. Enables "who's online" queries within groups and FOAF discovery (3-4 hop TTL). Privacy-preserving through MLS encryption. |
| [**crdt-sync**](https://crates.io/crates/saorsa-gossip-crdt-sync) | Delta-CRDTs (OR-Set, LWW-Register) with anti-entropy | **Local-First Data** - Provides conflict-free replicated data types for distributed state. OR-Set tracks membership, LWW-Register for scalar values. Delta-based sync minimizes bandwidth. Anti-entropy every 30s ensures eventual consistency. |

**Why these crates matter together**: They form a complete decentralized gossip network stack - from quantum-resistant identities and QUIC transport, through membership and broadcast protocols, to group encryption and local-first data sync. No DHT, no central servers, pure peer-to-peer with post-quantum security.

## 🎮 Running a Test Network

Saorsa Gossip provides two production-ready binaries for testing and deployment:

### 📦 Deployable Binaries

| Binary | Crate | Purpose |
|--------|-------|---------|
| `saorsa-gossip-coordinator` | [saorsa-coordinator](https://crates.io/crates/saorsa-coordinator) | Bootstrap/coordinator node for network discovery (alpha – adverts generated but not broadcast on the wire yet) |
| `saorsa-gossip` | [saorsa-gossip](https://crates.io/crates/saorsa-gossip) | CLI tool for testing network features (alpha – commands are gradually being implemented) |

> These binaries are still under heavy development. Use them for experimentation, not production deployments, until the remaining TODOs tracked in this README/DESIGN are resolved.

### Installation

Install both binaries from crates.io:

```bash
# Install coordinator binary (provides saorsa-gossip-coordinator command)
cargo install saorsa-coordinator

# Install CLI tool (provides saorsa-gossip command)
cargo install saorsa-gossip
```

Or build from source:

```bash
# Clone repository
git clone https://github.com/dirvine/saorsa-gossip.git
cd saorsa-gossip

# Build both binaries
cargo build --release -p saorsa-coordinator -p saorsa-gossip

# Binaries available at:
# - target/release/saorsa-gossip-coordinator
# - target/release/saorsa-gossip
```

### 🚀 Starting a Coordinator Node

Coordinators provide bootstrap discovery for new peers joining the network:

```bash
# Start a coordinator on port 7000 with verbose logging
saorsa-gossip-coordinator \
  --verbose \
  --bind 0.0.0.0:7000 \
  --roles coordinator,reflector,relay \
  --publish-interval 60
```

**Options:**
- `--bind <ADDR>` - Address to bind to (default: `0.0.0.0:7000`)
- `--roles <ROLES>` - Comma-separated roles: `coordinator`, `reflector`, `relay`, `rendezvous`
- `--publish-interval <SECS>` - Advert publish interval in seconds (default: 300)
- `--identity-path <PATH>` - Path to ML-DSA identity file (default: `~/.saorsa-gossip/coordinator.identity`)
- `--verbose` - Enable verbose DEBUG logging

**Roles Explained:**
- **coordinator**: Publishes signed coordinator adverts for bootstrap discovery
- **reflector**: Provides address reflection for NAT traversal (observes peers' public IPs)
- **relay**: Relays messages for NAT-restricted peers (optional, bandwidth-intensive)
- **rendezvous**: Provides rendezvous sharding for global peer discovery (future)

**What the coordinator does:**
1. Generates or loads an ML-DSA-65 identity (32-byte PeerId)
2. Publishes signed coordinator adverts every N seconds (~3.5KB CBOR messages)
3. Provides address reflection for peers behind NAT
4. Logs all activity with timestamps (INFO + DEBUG levels)

**Example output:**
```
INFO Starting Saorsa Gossip Coordinator
INFO Bind address: 0.0.0.0:7000
INFO Roles: coordinator,reflector,relay
INFO Loaded identity: c6333dcf4207a805989f9743e8b42d8e38ea35b085b2d54e80103f2c9725d41f
INFO Coordinator advert publisher started (interval: 60s)
DEBUG Published coordinator advert (3551 bytes)
```

### 🧪 Using the CLI Tool

The `saorsa-gossip` CLI exercises all library features:

#### Identity Management

```bash
# Create a new ML-DSA identity
saorsa-gossip identity create --alias Alice

# List all identities in keystore
saorsa-gossip identity list

# Show identity details
saorsa-gossip identity show Alice

# Delete an identity
saorsa-gossip identity delete Alice
```

**Output example:**
```
✓ Created identity: Alice
  PeerId: e4338043f8a848e62110892ca8321f25fad745a615f9dd30f7515aba93988d7a
  Saved to: /Users/you/.saorsa-gossip/keystore
```

#### Network Operations

```bash
# Join the gossip network via coordinator
saorsa-gossip network join \
  --coordinator 127.0.0.1:7000 \
  --identity Alice \
  --bind 0.0.0.0:0

# Show network status
saorsa-gossip network status

# List known peers
saorsa-gossip network peers
```

#### PubSub Messaging

```bash
# Subscribe to a topic
saorsa-gossip pubsub subscribe --topic news

# Publish a message
saorsa-gossip pubsub publish --topic news --message "Hello, gossip!"

# List subscriptions
saorsa-gossip pubsub list
```

#### Presence Beacons

```bash
# Start broadcasting presence
saorsa-gossip presence start --topic general

# Check who's online
saorsa-gossip presence online --topic general

# Stop broadcasting
saorsa-gossip presence stop --topic general
```

### 🌐 Local Test Network Setup

Run a multi-node test network on your local machine:

**Terminal 1 - Start Coordinator:**
```bash
saorsa-coordinator --verbose --bind 127.0.0.1:7000 --roles coordinator,reflector --publish-interval 10
```

**Terminal 2 - Start Second Coordinator:**
```bash
saorsa-coordinator --verbose --bind 127.0.0.1:7001 --roles coordinator,relay --publish-interval 15 \
  --identity-path ~/.saorsa-gossip/coordinator2.identity
```

**Terminal 3 - Create Test Identities:**
```bash
# Create 3 test node identities
saorsa-gossip identity create --alias Node1
saorsa-gossip identity create --alias Node2
saorsa-gossip identity create --alias Node3

# Verify they were created
saorsa-gossip identity list
```

**What you'll see:**
- **Coordinator 1 (port 7000)**: Publishing 3551-byte adverts every 10s with unique PeerId
- **Coordinator 2 (port 7001)**: Publishing 3552-byte adverts every 15s with different PeerId
- **CLI Tool**: Successfully creating ML-DSA identities and saving to keystore
- **Persistence**: Coordinators remember their identities across restarts

**Test Results from Local Validation:**
- ✅ 2 coordinators ran simultaneously without conflicts
- ✅ Identity persistence verified (same PeerId after restart)
- ✅ Precise timing: 10s and 15s intervals maintained perfectly
- ✅ Verbose logging showing all operations (INFO + DEBUG)
- ✅ Zero compilation warnings, zero runtime errors

### 📊 Logging and Monitoring

All binaries use structured logging with the `tracing` crate:

**Log Levels:**
- `INFO` - Operational events (startup, identity loading, service status)
- `DEBUG` - Detailed activity (advert publications, message counts)

**Enable verbose logging:**
```bash
# For coordinator
saorsa-coordinator --verbose ...

# For CLI tool
saorsa-gossip --verbose identity create --alias Test
```

**Log format:**
```
2025-10-05T13:34:34.486139Z  INFO Starting Saorsa Gossip Coordinator
2025-10-05T13:34:34.486960Z  INFO Loaded identity: c6333dcf...725d41f
2025-10-05T13:34:34.488876Z DEBUG Published coordinator advert (3551 bytes)
```

### 🧪 Testing Checklist

Before deploying to production, verify:

- [ ] Coordinator generates unique ML-DSA identity
- [ ] Coordinator publishes adverts at configured interval
- [ ] Identity persists across coordinator restarts (same PeerId)
- [ ] Multiple coordinators can run on different ports
- [ ] CLI can create and list identities
- [ ] All logging shows timestamps and correct levels
- [ ] No warnings or errors in logs

### 🔍 Troubleshooting

**Issue: "Address already in use"**
- Another process is using the port
- Solution: Use `--bind 127.0.0.1:PORT` with a different PORT

**Issue: "Failed to read keystore file"**
- Identity file doesn't exist yet (expected on first run)
- Solution: Let the binary create it automatically

**Issue: Coordinator not publishing adverts**
- Check logs for ERROR messages
- Verify `--roles` includes `coordinator`
- Ensure `--publish-interval` is reasonable (>5s)

## 🚀 Quick Start (Library Usage)

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
saorsa-gossip-types = "0.2.1"
saorsa-gossip-identity = "0.2.1"
saorsa-gossip-transport = "0.2.1"
saorsa-gossip-membership = "0.2.1"
saorsa-gossip-pubsub = "0.2.1"
saorsa-gossip-coordinator = "0.2.1"
saorsa-gossip-rendezvous = "0.2.1"
saorsa-gossip-groups = "0.2.1"
saorsa-gossip-presence = "0.2.1"
saorsa-gossip-crdt-sync = "0.2.1"
```

> NOTE: A few crates are still stabilising; if `cargo add` cannot find a version yet, depend on the git repository for now:
> `saorsa-gossip-pubsub = { git = "https://github.com/dirvine/saorsa-gossip", tag = "v0.2.1" }`

### Basic Usage

```rust
use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_membership::{
    Membership, HyParViewMembership, DEFAULT_ACTIVE_DEGREE, DEFAULT_PASSIVE_DEGREE,
};
use saorsa_gossip_pubsub::{PubSub, PlumtreePubSub};
use saorsa_gossip_transport::UdpTransportAdapter;
use saorsa_gossip_types::{PeerId, TopicId};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let topic = TopicId::from_entity("demo-room");
    let peer_id = PeerId::new([7u8; 32]);
    let bind_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let transport = Arc::new(UdpTransportAdapter::new(bind_addr, vec![]).await?);
    let signing_key = MlDsaKeyPair::generate()?;

    // Membership can run without seeds for local experimentation.
    let membership = HyParViewMembership::new(
        peer_id,
        DEFAULT_ACTIVE_DEGREE,
        DEFAULT_PASSIVE_DEGREE,
        transport.clone(),
    );
    membership.join(vec![]).await?;

    // PubSub requires a signing key and the shared transport.
    let pubsub = PlumtreePubSub::new(peer_id, transport.clone(), signing_key);
    let mut rx = pubsub.subscribe(topic);
    pubsub
        .initialize_topic_peers(topic, membership.active_view())
        .await;

    // Publish and observe the loopback delivery.
    pubsub.publish(topic, Bytes::from("Hello, gossip!")).await?;
    if let Some((from, data)) = rx.recv().await {
        println!("Received from {}: {:?}", from, data);
    }

    Ok(())
}
```

## 📚 Protocol Specifications

### Membership (HyParView + SWIM)

- **HyParView**: Partial views for connectivity
  - Active view: 8-12 peers (routing)
  - Passive view: 64-128 peers (healing)
  - Periodic shuffle: every 30s

- **SWIM**: Failure detection (complete)
  - K-random-peer probing: selects K=3 random alive peers each interval
  - Direct probes: Ping/Ack with 500ms timeout
  - Indirect probes: PingReq/AckResponse when direct probe times out (K=3 intermediaries)
  - State machine: Alive -> Suspect (probe timeout) -> Dead (suspect timeout, 3s)
  - Probe interval: 1s
  - Configurable fanout and timeouts

### Dissemination (Plumtree)

- **EAGER** push along spanning tree
- **IHAVE** digests to non-tree links (batch ≤ 1024)
- **IWANT** pull on demand
- **Anti-entropy**: every 30s with message-ID sketches
- **Peer scoring**: mesh gating for quality

### Presence & Discovery

- **Beacons**: MLS exporter-derived tags, ML-DSA signed _(alpha builds still use deterministic placeholders until full MLS exporter integration lands; treat them as non-private)_
  - TTL: 10-15 minutes
  - Encrypted to group with ChaCha20-Poly1305

- **FOAF Queries**: Friends-of-friends discovery
  - Fanout: 3
  - TTL: 3-4 hops
  - No DHT, no global directory

### CRDTs

- **OR-Set**: For membership tracking
- **LWW-Register**: For scalar values
- **Delta-CRDTs**: Bandwidth-efficient synchronization
- **IBLT**: Reconciliation for large sets

## 🔐 Security

### Post-Quantum Cryptography

- **ML-KEM-768**: Key encapsulation (FIPS 203)
- **ML-DSA-65**: Digital signatures (FIPS 204) - default
- **SLH-DSA**: Hash-based signatures (FIPS 205 / SPHINCS+) - available for long-term security
  - 12 parameter sets: SHA2/SHAKE variants at 128/192/256-bit security
  - Trade-offs: fast (larger sigs) vs small (smaller sigs)
- **ChaCha20-Poly1305**: AEAD symmetric encryption (quantum-resistant)
- **MLS**: Group messaging (RFC 9420)

Provided by:
- [`saorsa-pqc`](https://crates.io/crates/saorsa-pqc) v0.3.14+ - PQC primitives including ML-KEM, ML-DSA, SLH-DSA, and ChaCha20-Poly1305
- [`saorsa-mls`](https://crates.io/crates/saorsa-mls) - MLS protocol

### Threat Model

| Attack | Mitigation |
|--------|-----------|
| Spam/Sybil | Invited joins, capability checks, scoring |
| Eclipse | HyParView shuffles, passive diversity |
| Replay | Per-topic nonces, signature checks, expiry |
| Partition | Plumtree lazy links, anti-entropy |

## 🧪 Testing

> **Real hardware only.** As of January 16, 2026 we removed the deterministic simulator, mock transports, and synthetic load crates. Every test now speaks to a real ant-quic endpoint so local verification exercises the exact networking stack that ships to production.

### Automated Coverage

| Category | Description |
|----------|-------------|
| **Unit tests** | `cargo test --all` covers membership, pubsub, presence, rendezvous, transport, and coordinator logic using in-process ant-quic nodes. |
| **Doctests** | API snippets in this repository are compiled and executed with `cargo test --doc`. |
| **Transport benches** | `examples/throughput_test.rs` pushes real QUIC traffic between two processes to capture throughput/latency numbers. |

### Running Tests

```bash
# Run every unit test (real sockets will bind on 127.0.0.1)
cargo test --all

# Verify public API snippets
cargo test --doc
```

### Real-Network Drills

Use the shipping examples to exercise the QUIC stack end-to-end:

```bash
# Terminal 1 – receive large payloads over QUIC
cargo run --example throughput_test --release -- receiver --bind 127.0.0.1:8000

# Terminal 2 – stream payloads to the receiver
cargo run --example throughput_test --release -- sender --coordinator 127.0.0.1:8000 --bind 127.0.0.1:9000
```

These programs do not stub anything—they start real ant-quic nodes, perform ML-KEM+ML-DSA handshakes, and transfer actual data across the membership/pubsub/bulk streams. That matches the runtime used in production and in the `communitas` consumer.

### Manual System Testing

Spin up the coordinator and CLI binaries to validate bootstrap + gossip flows over real sockets:

```bash
cargo run --bin coordinator -- --bind 0.0.0.0:9090
cargo run --bin cli -- --coordinator 127.0.0.1:9090 --bind 127.0.0.1:9100
```

With two CLI instances joining the same coordinator you'll observe membership churn, FOAF lookups, and pub/sub fan-out exactly as they will behave on the public network.

## 🛠️ Development

### Building

```bash
# Build all crates
cargo build --release

# Run tests
cargo test --all

# Run with all features
cargo build --all-features
```

### Testing

```bash
# Unit tests
cargo test --all

# Integration tests
cargo test --test integration_tests

# Performance benchmarks
cargo bench --bench performance

# Code coverage report
cargo tarpaulin --out Html
```

### Code Quality

```bash
# Format code
cargo fmt --all

# Lint with Clippy (zero warnings enforced)
cargo clippy --all-features --all-targets -- -D warnings

# Generate documentation
cargo doc --all-features --no-deps --open
```

## 📖 Documentation

- [**DESIGN.md**](DESIGN.md) - Complete architecture and protocol specification
- [**Architecture Decisions**](docs/adr/) - Design decisions and rationale (ADRs)
- [**API Docs**](https://docs.rs/saorsa-gossip) - Rust API documentation

### Architecture Decision Records (ADRs)

We document significant architectural decisions in ADRs. These explain *why* we made specific choices:

| ADR | Title | Summary |
|-----|-------|---------|
| [ADR-001](docs/adr/ADR-001-protocol-layering.md) | Protocol Layering | HyParView + SWIM + Plumtree: three-layer gossip architecture |
| [ADR-002](docs/adr/ADR-002-post-quantum-cryptography.md) | Post-Quantum Cryptography | Pure PQC with ML-DSA-65, ML-KEM-768, ChaCha20-Poly1305 |
| [ADR-003](docs/adr/ADR-003-delta-crdt-synchronization.md) | Delta-CRDT Synchronization | OR-Set, LWW-Register with IBLT anti-entropy |
| [ADR-004](docs/adr/ADR-004-seedless-bootstrap.md) | Seedless Bootstrap | Coordinator Adverts for infrastructure-free discovery |
| [ADR-005](docs/adr/ADR-005-rendezvous-shards.md) | Rendezvous Shards | 65,536 content-addressed shards as DHT replacement |
| [ADR-006](docs/adr/ADR-006-mls-group-encryption.md) | MLS Group Encryption | RFC 9420 for efficient group key management |
| [ADR-007](docs/adr/ADR-007-foaf-discovery.md) | FOAF Discovery | Privacy-preserving bounded social graph walks |
| [ADR-008](docs/adr/ADR-008-stream-multiplexing.md) | Stream Multiplexing | 3-stream QUIC design for protocol isolation |
| [ADR-009](docs/adr/ADR-009-peer-scoring.md) | Peer Scoring | Multi-metric quality tracking for routing |
| [ADR-010](docs/adr/ADR-010-deterministic-simulator.md) | Deterministic Simulator *(retired)* | Historical record of the removed simulator effort |

See [docs/adr/README.md](docs/adr/README.md) for the complete index and ADR template.

### Crate Documentation

- [saorsa-gossip-types](https://docs.rs/saorsa-gossip-types) - Core types and wire format
- [saorsa-gossip-transport](https://docs.rs/saorsa-gossip-transport) - QUIC transport
- [saorsa-gossip-membership](https://docs.rs/saorsa-gossip-membership) - HyParView + SWIM
- [saorsa-gossip-pubsub](https://docs.rs/saorsa-gossip-pubsub) - Plumtree broadcast
- [saorsa-gossip-presence](https://docs.rs/saorsa-gossip-presence) - Presence beacons
- [saorsa-gossip-crdt-sync](https://docs.rs/saorsa-gossip-crdt-sync) - CRDT synchronization
- [saorsa-gossip-groups](https://docs.rs/saorsa-gossip-groups) - MLS groups
- [saorsa-gossip-identity](https://docs.rs/saorsa-gossip-identity) - Identity management

## 🗺️ Roadmap

### ✅ Phase 1: Foundation (Complete - v0.1.0)
- [x] Core types and traits
- [x] CRDT implementations (OR-Set, LWW)
- [x] MLS group wrapper
- [x] PQC identity management

### ✅ Phase 2: Protocols (Complete - v0.1.2)
- [x] HyParView trait definitions
- [x] SWIM trait definitions
- [x] Plumtree trait definitions
- [x] Membership wired to transport
- [x] Broadcast dissemination wired to transport
- [x] Delta-CRDT anti-entropy

### ✅ Phase 3: Transport (Complete - v0.1.2)
- [x] ant-quic 0.10.1 QUIC integration
- [x] NAT traversal with hole punching
- [x] Ed25519 keypair generation
- [x] Stream multiplexing (mship, pubsub, bulk)
- [x] Message send/receive with routing

### ✅ Phase 4: Production Crypto (Complete - v0.1.3)
- [x] Real ML-DSA-65 message signing/verification
- [x] BLAKE3 KDF for MLS exporter secrets
- [x] Coordinator binary with full CLI
- [x] Rendezvous shard implementation
- [x] Zero compilation warnings
- [x] 192 tests passing across all crates
- [x] Published to crates.io

### 📋 Phase 5: Advanced Features (In Progress)
- [x] Presence beacon broadcasting (basic)
- [x] FOAF query framework
- [ ] Complete IBLT reconciliation
- [ ] Peer scoring and mesh gating
- [ ] Saorsa Sites (website publishing)
- [ ] Complete anti-entropy with message sketches

### ✅ Phase 6: Production Hardening (Complete - v0.2.1)
- [x] **Real-network benchmarks** - `throughput_test` running against ant-quic
- [x] **Runtime crate + operator tooling** - deployable coordinator + CLI using the shared transport
- [x] **Bootstrap cache integration** - persistent peer cache via ant-quic's `BootstrapCache`
- [x] **PQC-only handshake verification** - ML-KEM-768 + ML-DSA-65 enforcement across transport/membership/pubsub
- [ ] 100-node test harness
- [ ] Security audit
- [ ] Production deployment guide

## 📊 Performance Benchmarks

### Transport Layer Performance (Real-World Results)

Comprehensive benchmarks on localhost (ant-quic 0.10.3 with direct stream acceptance):

| Message Size | Throughput (Mbps) | Throughput (MB/s) | Latency | Notes |
|--------------|-------------------|-------------------|---------|-------|
| 1 KB | 281 | 33.5 | <1ms | ✅ Low-latency messaging |
| 10 KB | 2,759 | 328.9 | <1ms | ✅ Optimal for small payloads |
| 100 KB | 22,230 | 2,650 | <10ms | 🚀 Excellent throughput |
| 1 MB | 79,875 | 9,522 | ~10ms | 🚀🚀 Outstanding performance |
| 10 MB | 1,471 | 175.4 | ~57ms | ✅ Sustained bulk transfer |
| 50 MB | 1,392 | 166.0 | ~300ms | ✅ Large file transfer |
| 100 MB | 1,400+ | 167+ | ~600ms | ✅ Consistent large transfers |

**Test Environment:**
- Platform: macOS Darwin 25.0.0
- Network: Localhost (127.0.0.1)
- Transport: QUIC with post-quantum TLS 1.3 (ML-DSA-65 signatures)
- NAT Traversal: Enabled with hole-punching capability
- Stream Type: Unidirectional (open_uni)
- Encryption: ChaCha20-Poly1305 AEAD
- Connection Time: 4ms (excellent)

**Key Achievements:**
- ✅ **Bidirectional data transfer working** - Direct stream acceptance implementation
- ✅ **Zero timeout issues** - Continuous stream acceptance without 100ms limits
- ✅ **Large message support** - Successfully transfers 100MB+ messages
- ✅ **Consistent performance** - Stable throughput across message sizes
- ✅ **Low latency** - Sub-millisecond for small messages, <1s for 100MB

**Technical Implementation:**
- Direct connection access via `nat_endpoint.list_connections()`
- Per-peer dedicated stream handlers (unidirectional + bidirectional)
- 100MB read limit per stream (configurable)
- Proper async/await handling with tokio runtime
- Dynamic peer discovery without polling delays

### Network Performance Targets

| Metric | Target | Status |
|--------|--------|--------|
| Broadcast P50 latency | < 500ms | 🔄 Testing |
| Broadcast P95 latency | < 2s | 🔄 Testing |
| Failure detection | < 5s | ✅ **Achieved** (SWIM: 500ms ack timeout + 3s suspect timeout) |
| Memory per node | < 50MB | 🔄 Testing |
| Messages/sec/node | > 100 | ✅ **Achieved** (>2000 small msgs/sec) |
| Transport latency | < 10ms | ✅ **Achieved** (4ms connection, <1ms for 1KB) |

## 🤝 Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

### Development Priorities

**High Priority** (blocking):
1. Complete IBLT reconciliation
2. Finalize peer scoring and mesh gating
3. 100-node test harness

**Medium Priority** (important):
4. Complete anti-entropy with message sketches
5. Saorsa Sites (website publishing)
6. Production deployment guide

**Low Priority** (enhancement):
7. Performance optimization
8. Security audit
9. Extended example applications

## 📜 License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## 🙏 Acknowledgments

Built on top of:
- [`ant-quic`](https://crates.io/crates/ant-quic) - QUIC transport with NAT traversal
- [`saorsa-pqc`](https://crates.io/crates/saorsa-pqc) - Post-quantum cryptography
- [`saorsa-mls`](https://crates.io/crates/saorsa-mls) - MLS group messaging

Inspired by:
- **Plumtree** - Efficient epidemic broadcast
- **HyParView** - Partial view membership protocol
- **SWIM** - Scalable failure detection
- **GossipSub** - Libp2p's gossip protocol

## 📞 Contact

- **Project**: [github.com/dirvine/saorsa-gossip](https://github.com/dirvine/saorsa-gossip)
- **Issues**: [github.com/dirvine/saorsa-gossip/issues](https://github.com/dirvine/saorsa-gossip/issues)
- **Author**: David Irvine ([@dirvine](https://github.com/dirvine))

---

**✅ Status (Jan 24 2026)**: v0.3.0 ships a production-ready QUIC + PQC gossip stack with deployable coordinator/CLI binaries and no simulators or mock transports. All tests operate over real sockets and ML-KEM/ML-DSA handshakes. The transport layer has been simplified to use ant-quic's native infrastructure directly, removing ~4,000 lines of redundant multiplexer code.

**Next Steps**: tighten ops tooling (metrics + alerting around real transports), finalize IBLT reconciliation + peer scoring, and extend the runtime glue used by Communitas Sites.

See [DESIGN.md](DESIGN.md) for the complete technical specification and implementation roadmap.
