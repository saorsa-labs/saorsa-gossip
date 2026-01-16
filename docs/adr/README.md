# Architecture Decision Records

This directory contains Architecture Decision Records (ADRs) for the Saorsa Gossip project.

## What are ADRs?

ADRs document significant architectural decisions made in the project. Each record captures the context, decision, and consequences to help future maintainers understand why things are the way they are.

## Project Overview

Saorsa Gossip is a **post-quantum secure, fully decentralized gossip overlay network** designed as a complete replacement for DHT-based P2P systems. It combines proven gossip protocols (HyParView, SWIM, Plumtree) with quantum-resistant cryptography (ML-DSA-65, ML-KEM-768, ChaCha20-Poly1305) and RFC 9420 MLS group encryption.

### Core Design Principles

1. **Zero Central Infrastructure** - No bootstrap servers, no coordinators required
2. **Quantum-Safe from Day 1** - FIPS-approved post-quantum cryptography only
3. **Partition Tolerance** - Local-first CRDTs enable offline operation
4. **Privacy Preservation** - FOAF discovery limits social graph exposure
5. **Production Ready** - Real-network testing only; no simulators or mocks

## Index

| ADR | Title | Status | Date |
|-----|-------|--------|------|
| [ADR-001](ADR-001-protocol-layering.md) | Protocol Layering (HyParView + SWIM + Plumtree) | Accepted | 2025-12-24 |
| [ADR-002](ADR-002-post-quantum-cryptography.md) | Post-Quantum Cryptography First | Accepted | 2025-12-24 |
| [ADR-003](ADR-003-delta-crdt-synchronization.md) | Delta-CRDT Synchronization | Accepted | 2025-12-24 |
| [ADR-004](ADR-004-seedless-bootstrap.md) | Seedless Bootstrap via Coordinator Adverts | Accepted | 2025-12-24 |
| [ADR-005](ADR-005-rendezvous-shards.md) | Rendezvous Shards (DHT Replacement) | Accepted | 2025-12-24 |
| [ADR-006](ADR-006-mls-group-encryption.md) | MLS Group Encryption (RFC 9420) | Accepted | 2025-12-24 |
| [ADR-007](ADR-007-foaf-discovery.md) | FOAF Privacy-Preserving Discovery | Accepted | 2025-12-24 |
| [ADR-008](ADR-008-stream-multiplexing.md) | QUIC Stream Multiplexing Design | Accepted | 2025-12-24 |
| [ADR-009](ADR-009-peer-scoring.md) | Peer Scoring Architecture | Accepted | 2025-12-24 |
| [ADR-010](ADR-010-deterministic-simulator.md) | Deterministic Network Simulator | **Retired** | 2026-01-16 |

## Architecture Layers

```
+-------------------------------------------------------------+
|                     Application Layer                        |
|  (Communitas, Saorsa Sites Browser, Custom Apps)            |
+---------------------------+---------------------------------+
                            |
+---------------------------v---------------------------------+
|                    Saorsa Gossip API                         |
|  +----------+----------+----------+--------------------+    |
|  | Identity |  PubSub  | Presence |  CRDT Sync         |    |
|  | Groups   |  Topics  |  Beacons |  Sites             |    |
|  +----------+----------+----------+--------------------+    |
+---------------------------+---------------------------------+
                            |
+---------------------------v---------------------------------+
|           Membership & Dissemination                         |
|  +--------------+--------------+------------------------+   |
|  |  HyParView   |   Plumtree   |   Peer Scoring         |   |
|  |  SWIM        |   Anti-Ent   |   Mesh Gating          |   |
|  +--------------+--------------+------------------------+   |
+---------------------------+---------------------------------+
                            |
+---------------------------v---------------------------------+
|                  Transport (ant-quic)                        |
|  +--------------+--------------+------------------------+   |
|  |  QUIC P2P    | NAT Traverse |  Address Discovery     |   |
|  |  3 Streams   | Hole Punch   |  Path Migration        |   |
|  +--------------+--------------+------------------------+   |
+---------------------------+---------------------------------+
                            |
+---------------------------v---------------------------------+
|                   Cryptography (saorsa-pqc)                  |
|   ML-KEM-768  |  ML-DSA-65  |  ChaCha20-Poly1305           |
+-------------------------------------------------------------+
```

## Crate Structure

| Crate | Purpose | Key ADRs |
|-------|---------|----------|
| `types` | Fundamental data types (TopicId, PeerId, MessageHeader) | - |
| `identity` | ML-DSA-65 keypairs, PeerId derivation | ADR-002 |
| `transport` | QUIC transport, NAT traversal, stream multiplexing | ADR-008 |
| `membership` | HyParView + SWIM overlay | ADR-001 |
| `pubsub` | Plumtree epidemic broadcast | ADR-001 |
| `coordinator` | Bootstrap via signed adverts | ADR-004 |
| `rendezvous` | Content-addressed sharding | ADR-005 |
| `groups` | MLS group key derivation | ADR-006 |
| `presence` | Encrypted presence beacons, FOAF | ADR-007 |
| `crdt-sync` | Delta-CRDTs + anti-entropy | ADR-003 |
| `runtime` | Glue orchestration for coordinators/sites | ADR-001, ADR-004 |
| *(retired)* `simulator` | Deterministic chaos testing (removed Jan 2026) | ADR-010 |
| *(retired)* `load-test` | Synthetic load harness (removed Jan 2026) | - |

## ADR Template

New ADRs should follow this structure:

```markdown
# ADR-N: Title

## Status
Proposed | Accepted | Deprecated | Superseded

## Context
Why this decision was necessary.

## Decision
What was chosen and why.

## Consequences
Benefits and trade-offs.

## Alternatives Considered
Other options and why rejected.

## References
Relevant commits, RFCs, code paths.
```

## Related Documentation

- [Project README](../../README.md)
- [ant-quic ADRs](https://github.com/dirvine/ant-quic/tree/main/docs/adr) - Transport layer decisions
- [RFC 9420 - MLS](https://www.rfc-editor.org/rfc/rfc9420) - Group encryption protocol
- [FIPS 203](https://csrc.nist.gov/pubs/fips/203/final) - ML-KEM specification
- [FIPS 204](https://csrc.nist.gov/pubs/fips/204/final) - ML-DSA specification
