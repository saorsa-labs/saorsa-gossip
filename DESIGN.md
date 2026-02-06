# Saorsa Gossip: Design and Architecture

**Version:** 0.2.1 (workspace)
**Status:** Alpha / Feature Incomplete
**Purpose:** Post-quantum secure, peer-to-peer gossip overlay for friend-of-a-friend networks, decentralized website hosting, and privacy-preserving social applications

---

## Table of Contents

1. [Vision and Philosophy](#vision-and-philosophy)
2. [Core Capabilities](#core-capabilities)
3. [Architecture Overview](#architecture-overview)
4. [Identity and Cryptography](#identity-and-cryptography)
5. [Transport Layer](#transport-layer)
6. [Membership and Topology](#membership-and-topology)
7. [Message Dissemination](#message-dissemination)
8. [Discovery Mechanisms](#discovery-mechanisms)
9. [Saorsa Sites: Decentralized Website Hosting](#saorsa-sites-decentralized-website-hosting)
10. [Presence and User Discovery](#presence-and-user-discovery)
11. [Data Synchronization](#data-synchronization)
12. [Communitas Integration](#communitas-integration)
13. [Network Topologies and Use Cases](#network-topologies-and-use-cases)

---

## Vision and Philosophy

Saorsa Gossip is a **post-quantum secure**, **DNS-free**, **DHT-free** peer-to-peer gossip overlay designed to enable:

- **Friend-of-a-Friend (FOAF) networking**: Organic network growth based on trust relationships
- **Decentralized website publishing**: Host and discover websites without central servers
- **Privacy-preserving presence**: Find people without revealing your social graph
- **Local-first data**: Own your data, sync peer-to-peer
- **NAT traversal**: Work anywhere, including behind restrictive firewalls
- **Post-quantum security**: Future-proof against quantum attacks

### Core Principles

1. **No Central Infrastructure**: No DNS, no DHT, no central discovery servers
2. **Pure P2P**: All communication happens directly between peers or through volunteer relays
3. **PQC-Only**: ML-KEM for key exchange, ML-DSA for signatures, ChaCha20-Poly1305 for encryption
4. **Local-First**: Data lives on your device, syncs via CRDTs when online
5. **Privacy-Preserving**: Bounded queries, capability tokens, peer scoring
6. **Partition-Tolerant**: Works in isolated networks, reconnects seamlessly
7. **Measure, Don’t Trust**: Capability claims are hints; peers validate reachability and behavior before relying on them

---

## Core Capabilities

### What Saorsa Gossip Provides

#### 1. **Secure Transport**
- QUIC-based peer-to-peer connections via `ant-quic`
- Post-quantum handshakes (ML-KEM-768)
- NAT traversal with hole punching
- Automatic connection establishment to bootstrap coordinators
- Path migration for mobile/roaming scenarios

#### 2. **Overlay Membership**
- HyParView partial views for scalable connectivity
- SWIM failure detection with indirect probes
- Self-healing topology via active/passive views
- Peer scoring to maintain mesh quality

#### 3. **Efficient Broadcast**
- Plumtree eager-push tree with lazy digests
- O(N) message propagation instead of O(N²)
- Anti-entropy reconciliation with IBLT
- Backpressure and rate limiting

#### 4. **Discovery Without DNS/DHT**
- **Coordinator Adverts**: Public nodes advertise themselves via gossip
- **Rendezvous Shards**: 65,536 content-addressed shards for global findability
- **FOAF Queries**: Bounded random-walk over friend graph (TTL=3)
- **Peer Cache**: Persistent storage of known peers with NAT hints
  - Coordinator roles are treated as *hints*; peers test and score nodes before selecting them

#### 5. **Decentralized Website Hosting (Saorsa Sites)**
- Content-addressed sites with BLAKE3 merkle trees
- ML-DSA signed manifests
- Efficient block-level sync via IBLT
- Optional private sites with MLS group encryption
- No hosting provider needed

#### 6. **Presence System**
- Encrypted presence beacons scoped to MLS groups
- TTL-based expiration (10-15 minutes)
- Privacy-preserving "find user" queries
- IBLT summaries for efficient membership tests

#### 7. **CRDT Synchronization**
- Delta-CRDTs (OR-Set, LWW-Register, RGA)
- IBLT reconciliation for large sets
- Anti-entropy repairs on reconnection
- Local-first with eventual consistency

#### 8. **Fallback Connectivity**
- Bluetooth Mesh bridging for offline scenarios
- Presence beacons and summaries over BLE
- Short messages (≤120 bytes) with FEC
- Gateway nodes translate Mesh ↔ QUIC

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                     Application Layer                        │
│  (Communitas, Saorsa Sites Browser, Custom Apps)            │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────▼─────────────────────────────┐
│                    Saorsa Gossip API                       │
│  ┌──────────┬──────────┬──────────┬────────────────────┐ │
│  │ Identity │  PubSub  │ Presence │  CRDT Sync         │ │
│  │ Groups   │  Topics  │  Beacons │  Sites             │ │
│  └──────────┴──────────┴──────────┴────────────────────┘ │
└──────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────▼─────────────────────────────┐
│              Membership & Dissemination                    │
│  ┌──────────────┬──────────────┬────────────────────────┐│
│  │  HyParView   │   Plumtree   │   Peer Scoring         ││
│  │  SWIM        │   Anti-Ent   │   Mesh Gating          ││
│  └──────────────┴──────────────┴────────────────────────┘│
└──────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────▼─────────────────────────────┐
│                  Transport (ant-quic)                      │
│  ┌──────────────┬──────────────┬────────────────────────┐│
│  │  QUIC P2P    │ NAT Traverse │  Address Discovery     ││
│  │  3 Streams   │ Hole Punch   │  Path Migration        ││
│  │  (mship/     │ Relay        │  PQC Handshake         ││
│  │   pubsub/    │              │                        ││
│  │   bulk)      │              │                        ││
│  └──────────────┴──────────────┴────────────────────────┘│
└──────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────▼─────────────────────────────┐
│                   Cryptography (saorsa-pqc)                │
│   ML-KEM-768  │  ML-DSA-65  │  ChaCha20-Poly1305          │
│   (or -1024)  │  (or SLH)   │  (AEAD)                     │
└──────────────────────────────────────────────────────────┘
```

### Stream Types

- **`mship`**: HyParView shuffles, SWIM probes, membership deltas
- **`pubsub`**: Plumtree EAGER/IHAVE/IWANT control messages
- **`bulk`**: Actual payloads, CRDT deltas, SITE_SYNC blocks

---

## Identity and Cryptography

### Peer Identity

Every peer has a long-term **ML-DSA keypair**:
- **Public Key**: Used for signatures, advertised in the network
- **PeerId**: `BLAKE3(ml_dsa_pubkey)[0..32]` - 32-byte unique identifier
- **Alias**: Optional human-readable name (app layer, not in protocol)

### Post-Quantum Cryptography

**All cryptography uses `saorsa-pqc` v0.3.14+:**

1. **Key Exchange**: ML-KEM-768 (or ML-KEM-1024 for higher security)
   - Quantum-resistant key encapsulation
   - Used in QUIC handshake via `ant-quic`

2. **Signatures**: ML-DSA-65 (default) or SLH-DSA (optional)
   - ML-DSA: Fast, FIPS 204 standard
   - SLH-DSA: Hash-based, long-term security, 12 parameter sets available

3. **Symmetric Encryption**: ChaCha20-Poly1305 AEAD
   - 256-bit keys derived via BLAKE3 KDF
   - All group messages, CRDT state, private sites

**No classical crypto**: No Ed25519, no X25519, no AES-GCM

### Group Security (MLS)

- **saorsa-mls**: Provides group keys for encrypted topics
- Derives per-epoch secrets for presence beacons and CRDT encryption
- Forward secrecy and post-compromise security

---

## Transport Layer

### Ant-QUIC Integration

**`ant-quic`** provides the P2P QUIC transport with:

#### Features
- **QUIC Multiplexing**: Multiple streams over single connection
- **0-RTT Resumption**: Fast reconnection to known peers
- **Path Migration**: Seamless transition between networks (WiFi ↔ cellular)
- **Connection Migration**: Survives IP address changes

#### NAT Traversal

**Coordinator-Assisted Hole Punching:**
1. Client connects to bootstrap coordinator (public node)
2. Coordinator observes client's reflexive address (via `OBSERVED_ADDRESS`)
3. Coordinator facilitates candidate exchange between clients behind NAT
4. Clients attempt simultaneous outbound packets (hole punching)
5. QUIC connection migrates to direct path on success

**Relay Fallback:**
- If hole punching fails, coordinator may act as relay
- Rate-limited to prevent abuse
- Only used as last resort

#### Endpoint Roles

- **Bootstrap**: Public coordinator, accepts incoming connections
- **Client**: Normal peer, connects to bootstraps, may accept after NAT punch
- **Relay**: Optional volunteer relay for clients with symmetric NAT

**Current Implementation (v0.1.6):**
- ✅ Automatic connection establishment to coordinators
- ✅ NAT traversal capability negotiation
- ✅ QUIC connection multiplexing
- ✅ Address observation
- 🚧 Full hole punching (in progress in `ant-quic`)
- 🚧 Relay services (scaffolded in coordinator binary)

---

## Membership and Topology

### HyParView: Scalable Partial Views

**Purpose**: Maintain a robust, self-healing overlay without requiring all-to-all connectivity.

#### Two Views

1. **Active View** (degree 8-12)
   - Direct QUIC connections
   - Used for message routing
   - Probed frequently via SWIM

2. **Passive View** (degree 64-128)
   - Candidate peers (not connected)
   - Used for recovery when active peer fails
   - Populated via shuffle protocol

#### Operations

**Shuffle** (every 30 seconds):
- Exchange random subsets of passive view
- Discover new peers transitively
- Prevent network partitioning

**Promotion**:
- When active peer fails, promote from passive
- Establish new QUIC connection
- Update routing state

**Neighbor Selection**:
- Prioritize low-latency, high-reliability peers
- Use peer scoring metrics
- Maintain diversity (different ASes, geos)

### SWIM: Failure Detection (Complete)

**Purpose**: Quickly detect and disseminate peer failures.

**Status**: Fully implemented with configurable fanout, direct and indirect probes, and suspect/dead state transitions.

#### Protocol

1. **K-Random-Peer Probing** (every 1 second)
   - Each interval selects `SWIM_PROBE_FANOUT` (K=3) random alive peers
   - Sends `SwimMessage::Ping` to each selected peer
   - Expects `SwimMessage::Ack` within `SWIM_ACK_TIMEOUT_MS` (500ms)

2. **Indirect Probes** (on direct probe timeout)
   - Selects `SWIM_INDIRECT_PROBE_FANOUT` (K=3) random alive peers as intermediaries
   - Sends `SwimMessage::PingReq { target, requester }` to each intermediary
   - Intermediaries probe the target and forward `SwimMessage::AckResponse { target, requester }` back
   - If any intermediary receives an Ack, the suspect is cleared

3. **State Transitions**
   - **Alive** -> **Suspect** (failed direct + indirect probes within ack timeout)
   - **Suspect** -> **Dead** (suspect timeout expires, default 3s)
   - **Dead** -> removed from active view, promoted from passive
   - **Any state** + Ack/AckResponse -> **Alive** (clears suspicion immediately)

4. **Background Timeout Detection**
   - A background task checks pending probes every 500ms
   - Probes that exceed the ack timeout trigger indirect probing or suspect transitions

5. **Message Dispatch**
   - Unified `MembershipProtocolMessage` envelope routes both HyParView and SWIM messages
   - All SWIM messages are serialized via CBOR and sent over the membership stream

#### Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `SWIM_PROBE_FANOUT` | 3 | Peers probed per interval |
| `SWIM_INDIRECT_PROBE_FANOUT` | 3 | Intermediaries for indirect probes |
| `SWIM_ACK_TIMEOUT_MS` | 500 | Milliseconds before marking probe as failed |
| `SWIM_PROBE_INTERVAL_SECS` | 1 | Seconds between probe rounds |
| `SWIM_SUSPECT_TIMEOUT_SECS` | 3 | Seconds before suspect becomes dead |

#### Message Types

- `SwimMessage::Ping` - Direct probe
- `SwimMessage::Ack` - Response to Ping
- `SwimMessage::PingReq { target, requester }` - Indirect probe request
- `SwimMessage::AckResponse { target, requester }` - Forwarded ack from indirect probe

---

## Message Dissemination

### Plumtree: Epidemic Broadcast Trees

**Goal**: Broadcast messages to all subscribed peers with O(N) overhead instead of flooding's O(N²).

#### How It Works

1. **Eager Push** (Tree Links)
   - Maintain spanning tree per topic
   - Forward messages eagerly along tree links
   - Guarantees delivery as long as tree is connected

2. **Lazy Pull** (Non-Tree Links)
   - Send IHAVE digest to non-tree neighbors
   - If neighbor hasn't seen message, they send IWANT
   - Sender replies with full payload

3. **Tree Repair**
   - If IHAVE reveals a gap, send IWANT
   - Temporarily graft sender into eager tree
   - Prune poor-quality links from tree

4. **Anti-Entropy** (every 30 seconds)
   - Exchange IBLT/Bloom filter of recent message IDs
   - Identify and fetch missing messages
   - Repairs partitions and late joins

#### Peer Scoring

**Metrics:**
- **Delivery Latency**: Time from publish to receive
- **IWANT Responsiveness**: How quickly peer sends requested messages
- **Invalid Messages**: Malformed or badly signed messages
- **Duplicate Floods**: Excessive retransmissions

**Actions:**
- **Prune** poor performers from eager tree
- **Graft** high-quality peers into tree
- **Blacklist** peers with persistent misbehavior

---

## Discovery Mechanisms

### No DNS, No DHT

Traditional P2P systems rely on:
- **DNS**: For initial coordinator discovery → ❌ *Single point of failure, censorship*
- **DHT**: For global key-value lookups → ❌ *Sybil attacks, poor locality*

**Saorsa Gossip uses a multi-layered approach:**

### Layer 1: Peer Cache

**Persistent local storage** of known peers:
```rust
struct CachedPeer {
    peer_id: PeerId,
    addr_hints: Vec<SocketAddr>,  // IPv4/IPv6 addresses
    last_success: u64,             // Unix timestamp
    nat_class: NatClass,           // EIM, EDM, Symmetric, Unknown
    roles: CoordinatorRoles,       // Coordinator, Reflector, Relay, Rendezvous
    score: i32,                    // Local quality metric
}
```

**On startup:**
1. Read peer cache from disk (~/.saorsa-gossip/peers.db)
2. Sort by score and last_success
3. Attempt connection to top 3-5 peers
4. If any connect, join overlay via membership protocol

### Layer 2: Coordinator Adverts

**Self-Elected Coordinators:**

Any public node can become a coordinator by:
1. Generating a signed Coordinator Advert
2. Gossiping it on the well-known **Coordinator Topic**
3. Other peers cache it with TTL and score

**Advert Format (CBOR):**
```json
{
  "v": 1,
  "peer": "< PeerId (32 bytes) >",
  "roles": {
    "coordinator": true,     // Accepts bootstrap connections
    "reflector": true,       // Provides address observation
    "rendezvous": false,     // Coordinates rendezvous shards
    "relay": false           // Relays for symmetric NAT peers
  },
  "addr_hints": [
    "203.0.113.42:7000",     // IPv4
    "[2001:db8::1]:7000"     // IPv6
  ],
  "nat_class": "eim",        // Endpoint-Independent Mapping
  "not_before": 1735000000000,  // Unix ms
  "not_after": 1735086400000,   // Unix ms (TTL ~24h)
  "score": 100,              // Local scoring, not signed
  "sig": "< ML-DSA signature >"
}
```

**Caching Strategy:**
- LRU cache with max size (e.g., 1000 adverts)
- Evict expired (not_after < now) or low-score entries
- Re-score based on connectivity success rate

**Benefits:**
- No hardcoded bootstrap IPs
- Censorship-resistant (any node can be coordinator)
- Automatic failover (many coordinators available)

### Layer 3: FOAF Discovery

**Friend-of-a-Friend Queries:**

If peer cache is empty (cold start), ask friends:

**Protocol:**
1. Send `FIND_COORDINATOR` query to all known peers
2. Each peer forwards to 3 random friends (fanout=3)
3. TTL=3 hops max (exponential decay: 9 → 27 → 81 peers)
4. Replies carry Coordinator Adverts
5. Cache received adverts and connect

**Privacy:**
- Query is signed by requester (rate limit by PeerId)
- No full graph disclosure (bounded fanout & TTL)
- Capability tokens prevent spam

### Layer 4: Rendezvous Shards

**Global Findability Without Directories:**

**Problem**: How do you find content (website, user) without a global index?

**Solution**: Content-addressed sharding.

#### Shard Space

- **K = 16** bits → 65,536 shards
- Shard ID for target T: `shard(T) = BLAKE3("saorsa-rendezvous" || T) & 0xFFFF`

#### Provider Summaries

Publishers of content (e.g., a Saorsa Site) gossip **Provider Summaries** to their target's shard:

```json
{
  "v": 1,
  "target": "< SiteId or PeerId (32 bytes) >",
  "provider": "< PeerId of publisher >",
  "cap": ["SITE", "IDENTITY"],        // What this provider serves
  "have_root": true,                  // Has full content vs partial
  "manifest_ver": 42,                 // Version number (for sites)
  "summary": {
    "bloom": "< Bloom filter of block CIDs >",
    "iblt": "< IBLT for efficient reconciliation >"
  },
  "exp": 1735100000000,               // Expiration timestamp
  "sig": "< ML-DSA signature >"
}
```

#### Seeking Content

1. **Subscribe** to `SITE_ADVERT:<shard(SID)>` topic
2. Receive Provider Summaries from peers hosting the site
3. Pick top providers by **score** (latency, bandwidth, reputation)
4. **Fetch** manifest and blocks directly via QUIC `bulk` stream

**Scaling:**
- Subscribers only join relevant shards (not all 65k)
- Each shard is a separate Plumtree instance
- Sharding distributes load across overlay

---

## Saorsa Sites: Decentralized Website Hosting

### Vision

Host and discover websites **without DNS, without web hosts**. Content is:
- **Content-Addressed**: BLAKE3 hashes ensure integrity
- **Signed**: ML-DSA proves authorship
- **Versioned**: Incremental updates with merkle proofs
- **Syncable**: IBLT-based block reconciliation
- **Private**: Optional MLS-encrypted blocks for group-only sites

### Site Identity

```
SiteId (SID) = BLAKE3(site_signing_pubkey)[0..32]
```

- Derived from site owner's ML-DSA public key
- Immutable, self-certifying identifier
- No registration, no centralized namespace

### Manifest Structure

**Manifest** is the root document, ML-DSA signed:

```json
{
  "v": 1,
  "sid": "< SiteId (32 bytes) >",
  "pub": "< Site ML-DSA public key >",
  "version": 42,                     // Increment on updates
  "chunk_size": 262144,              // 256 KB blocks
  "root": "< BLAKE3 merkle root of all CIDs >",
  "routes": [
    {
      "path": "/index.html",
      "cid": "< BLAKE3(content) >",
      "mime": "text/html"
    },
    {
      "path": "/style.css",
      "cid": "< BLAKE3(css) >",
      "mime": "text/css"
    }
  ],
  "assets": [
    { "cid": "< CID >", "len": 12345 },
    { "cid": "< CID >", "len": 67890 }
  ],
  "private": null                    // For public sites
}
```

**For Private Sites:**
```json
{
  ...
  "private": {
    "mls_group": "< MLS group ID (32 bytes) >",
    "encrypted_routes": "< ChaCha20-Poly1305(routes) >",
    "key_epoch": 5                   // MLS epoch for decryption key
  }
}
```

### Content Blocks

- **Fixed-size chunks** (e.g., 256 KB)
- **Content-Addressed**: `CID = BLAKE3(chunk_data)[0..32]`
- **No per-block signatures**: Integrity via CID + manifest signature
- **Deduplication**: Identical blocks across sites share CID

### Publishing Flow

**Publisher Node:**

1. **Prepare** site directory (HTML, CSS, JS, images)
2. **Chunk** large files into 256 KB blocks
3. **Hash** each block → CID
4. **Build** manifest with routes and asset list
5. **Sign** manifest with site ML-DSA key
6. **Gossip** Provider Summary to `SITE_ADVERT:<shard(SID)>`
7. **Serve** manifest and blocks on demand via `SITE_SYNC:<SID>` stream

**Updates:**
- Increment `version` number
- Generate new manifest (same SID, new merkle root)
- Gossip updated Provider Summary
- Clients detect version change and re-sync

### Fetching Flow

**Browser/Client:**

1. **Resolve** SID (from bookmark, link, or search)
2. **Subscribe** to `SITE_ADVERT:<shard(SID)>`
3. **Receive** Provider Summaries from multiple publishers
4. **Score** providers (latency, bandwidth, reputation)
5. **Connect** to top provider via QUIC
6. **Fetch** manifest via `GET_MANIFEST` on `SITE_SYNC` stream
7. **Verify** ML-DSA signature on manifest
8. **Reconcile** block set:
   - Compare local CIDs (if cached) with manifest
   - Generate IBLT of missing blocks
   - Send IBLT to provider
   - Provider responds with delta
9. **Fetch** missing blocks via `GET_BLOCKS [cid1, cid2, ...]`
10. **Verify** each block: `CID == BLAKE3(block_data)`
11. **Render** site locally

**Caching:**
- Locally cache blocks by CID
- Cache manifest with TTL based on `version`
- Opportunistically serve blocks to other clients (become provider)

### Private Sites

**Use Case**: Company intranet, family photo album, private forum

**Encryption:**
- **MLS Group**: Site owner creates MLS group, adds authorized members
- **Exporter Secret**: Derive encryption key from MLS exporter (`site_aead_key = KDF(exporter, "site-encryption")`)
- **Block Encryption**: Encrypt each block with ChaCha20-Poly1305
- **Manifest**: Encrypt sensitive fields (routes, metadata) or leave public (CIDs are opaque)

**Access Control:**
- Only MLS group members can derive decryption key
- Publisher verifies membership before serving blocks
- Capability tokens can gate SITE_SYNC requests

---

## Presence and User Discovery

### Presence Beacons

**Problem**: How do you know if a friend is online without centralized status servers?

**Solution**: Periodic presence beacons scoped to MLS groups, cryptographically signed for authenticity.

#### Beacon Format

For each MLS group the user is in:

```json
{
  "presence_tag": "< KDF(mls_exporter, user_id || time_slice) >",
  "addr_hints": ["192.0.2.1:9000"],  // Current reflexive addresses
  "since": 1735000000000,             // When user came online
  "expires": 1735000900000,           // TTL (15 minutes)
  "seq": 42,                          // Sequence number for ordering
  "four_words": "ocean-forest-moon-star",  // Optional four-word identity
  "signature": "< ML-DSA-65 signature >",   // Post-quantum signature
  "signer_pubkey": "< ML-DSA-65 public key >"  // Signer's public key
}
```

**Cryptographic Security:**
- **Signatures**: All beacons signed with ML-DSA-65 (FIPS 204 post-quantum)
- **Verification**: Incoming beacons verified on receipt; invalid signatures rejected
- **Canonical Serialization**: `signable_bytes()` ensures deterministic signing
- **Optional Strict Mode**: Set `COMMUNITAS_PRESENCE_REQUIRE_SIGNED=1` to reject unsigned beacons

**Encryption:**
- Encrypted to MLS group with ChaCha20-Poly1305
- Only group members can decrypt
- Gossiped on the group's topic

**Timing:**
- Beacon every 5-10 minutes while online
- TTL 10-15 minutes (expires if user goes offline)
- `presence_tag` changes every epoch (forward secrecy)

**Privacy:**
- Only shared groups see presence
- `presence_tag` is pseudonymous (not raw user_id)
- Address hints reveal IP but only to group members

**Rate Limiting:**
- Per-peer token bucket limiting (default: burst of 3, refill 0.2/sec = 12/min)
- Applied to incoming beacons and FOAF queries
- Prevents spam and denial-of-service attacks
- Stale peer entries automatically cleaned up

### Finding Users

**Scenario 1: Shared Group**

If you share an MLS group with the target user:
1. Wait for their presence beacon on that group's topic
2. Extract `addr_hints` and connect directly

**Scenario 2: No Shared Group (FOAF Query)**

If you don't share a group but are socially connected:

1. **Send** `FIND_USER(target_peer_id)` to all active peers
2. **Propagate** via FOAF:
   - Each peer checks if they share group with target
   - If yes, reply with encrypted `addr_hints`
   - If no, forward to 3 random friends (fanout=3)
   - TTL=3 hops max
3. **Receive** replies encrypted to requester
4. **Connect** using received `addr_hints`

**Abuse Prevention:**
- **Per-Peer Rate Limiting**: Token bucket limiting (burst of 3, refill 0.2/sec)
  - Applied to all incoming queries and beacons
  - Prevents spam and DoS attacks
  - Automatic cleanup of stale peer entries
- **Capability Tokens**: Require signed capability from mutual friend
- **Proximity Check**: Only propagate within 2-hop social distance
- **Scoring Penalty**: Peers who spam queries get low score

**Scenario 3: Public Findability (Rendezvous)**

If user opts into global findability:
1. User gossips **Provider Summary** to `rendezvous_shard(peer_id)`
2. Seekers subscribe to that shard
3. Receive provider info and connect

**Privacy Trade-off:**
- Rendezvous reveals that user exists and is online
- Does not reveal social graph
- User can disable at any time

---

## Data Synchronization

### Local-First Architecture

**Philosophy**: Your data lives on your device, not in the cloud.

- **Offline-First**: Full functionality without network
- **Eventual Consistency**: Changes propagate when online
- **Peer-to-Peer Sync**: No central database
- **Conflict-Free**: CRDTs ensure deterministic merge

### CRDT Primitives

**OR-Set** (Observed-Remove Set):
- **Use Case**: Group membership, contact lists
- **Operations**: `add(element)`, `remove(element)`
- **Merge**: Union of adds, remove wins only if add was observed
- **Example**: Alice and Bob both add Charlie to a group offline, sync later → Charlie is in group (no conflict)

**LWW-Register** (Last-Write-Wins):
- **Use Case**: Profile fields (name, avatar, bio)
- **Operations**: `write(value, timestamp)`
- **Merge**: Keep value with highest timestamp
- **Example**: Alice updates her bio at T=100, Bob's stale update at T=50 → Alice's wins

**RGA** (Replicated Growable Array):
- **Use Case**: Text documents, chat messages
- **Operations**: `insert(index, char)`, `delete(index)`
- **Merge**: Convergent ordering based on causal history
- **Example**: Collaborative text editing without lock-step sync

### Delta-CRDTs

**Efficiency Improvement:**
- Instead of sending full state, send deltas (changes since last sync)
- Reduces bandwidth from O(state_size) to O(changes)
- Causal stability tracking via version vectors

**Example**: Alice adds 10 new contacts. Instead of sending all 500 contacts, send:
```json
{
  "delta": {
    "adds": [
      {"id": "peer1", "name": "Charlie", ...},
      {"id": "peer2", "name": "Diana", ...},
      ...
    ]
  },
  "version": {"alice": 42, "bob": 30}
}
```

### Reconciliation (IBLT)

**Problem**: After offline period, you have 1000s of updates, peer has different 1000s.

**Inefficient**: Send all IDs, compare, fetch diffs → O(N) messages

**Efficient**: Invertible Bloom Lookup Table (IBLT)

**How IBLT Works:**
1. Encode your set of update IDs into fixed-size IBLT (e.g., 512 bytes)
2. Send IBLT to peer
3. Peer subtracts their IBLT
4. Result is symmetric difference (what you have that they don't, vice versa)
5. Fetch missing updates

**Scaling:**
- IBLT size is O(d) where d = difference size
- If d is small (typical case), 512-byte IBLT suffices
- Fallback to full set exchange if IBLT decode fails (too many diffs)

### Anti-Entropy

**Periodic Background Sync:**
- Every 30-60 seconds, pick random active peer
- Exchange IBLT of recent updates
- Fetch missing deltas
- Repairs network partitions, late joins, dropped messages

---

## Communitas Integration

**Communitas** is a privacy-preserving social networking app built on Saorsa Gossip. Here's how it leverages each capability:

### Architecture Overview

```
┌───────────────────────────────────────────────────────────┐
│                   Communitas App (Tauri)                  │
│                                                           │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────┐  │
│  │  React UI   │  │   Chat View  │  │  Profile View  │  │
│  │  (TypeScript)│  │              │  │                │  │
│  └──────┬──────┘  └──────┬───────┘  └───────┬────────┘  │
│         │                 │                  │           │
│  ┌──────▼─────────────────▼──────────────────▼────────┐  │
│  │          Communitas Core (Rust Backend)             │  │
│  │  ┌────────────┐  ┌────────────┐  ┌──────────────┐ │  │
│  │  │  Groups    │  │  Contacts  │  │  Profile     │ │  │
│  │  │  Manager   │  │  CRDT      │  │  Store       │ │  │
│  │  └─────┬──────┘  └──────┬─────┘  └──────┬───────┘ │  │
│  └────────┼─────────────────┼────────────────┼─────────┘  │
└───────────┼─────────────────┼────────────────┼────────────┘
            │                 │                │
┌───────────▼─────────────────▼────────────────▼────────────┐
│                    Saorsa Gossip Library                   │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐ │
│  │ Identity │  │  PubSub  │  │ Presence │  │   Sync   │ │
│  │  (MLS)   │  │(Plumtree)│  │ (Beacons)│  │ (CRDTs)  │ │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘ │
└───────────────────────────────────────────────────────────┘
```

### Feature Mapping

#### 1. **Group Messaging**

**Technology Stack:**
- **MLS Groups**: End-to-end encrypted group membership
- **Plumtree**: Efficient message broadcast to all group members
- **CRDTs**: Message ordering (RGA), reactions (OR-Set)

**User Flow:**
1. **Create Group**: Alice creates "Family Chat"
   - Communitas generates MLS group with Alice as owner
   - Publishes group advert to `rendezvous_shard(group_id)`

2. **Invite Members**: Alice invites Bob and Charlie
   - Sends MLS Welcome messages via private topic
   - Bob/Charlie accept, join MLS group
   - All future messages encrypted with group key

3. **Send Message**: Alice types "Hello family!"
   - Communitas encrypts message with ChaCha20-Poly1305 (key from MLS)
   - Publishes to group's Plumtree topic
   - Bob and Charlie receive via eager-push
   - CRDT ensures causal ordering (RGA)

4. **Offline/Online**: Bob is offline when Alice sends message
   - Message propagates to Charlie
   - When Bob reconnects, anti-entropy fetches missed messages
   - CRDT merge resolves any conflicts

**Implementation Details:**
```rust
// Communitas Group Manager
struct GroupManager {
    gossip: Arc<GossipClient>,
    mls_groups: HashMap<GroupId, MlsGroup>,
    message_store: CrdtMessageLog,
}

impl GroupManager {
    async fn send_message(&self, group_id: &GroupId, text: &str) -> Result<()> {
        // 1. Get MLS group and derive epoch key
        let mls_group = self.mls_groups.get(group_id)?;
        let epoch_key = mls_group.exporter_secret("message-encryption")?;

        // 2. Create message CRDT op
        let msg_id = MessageId::new();
        let timestamp = SystemTime::now();
        let crdt_op = RgaOp::Insert {
            id: msg_id,
            pos: self.message_store.len(),
            content: text.to_string(),
            timestamp,
        };

        // 3. Encrypt with ChaCha20-Poly1305
        let plaintext = postcard::to_stdvec(&crdt_op)?;
        let ciphertext = chacha20poly1305::encrypt(&epoch_key, &plaintext)?;

        // 4. Sign with user's ML-DSA key
        let signature = self.gossip.identity().sign(&ciphertext)?;

        // 5. Publish to group topic via Plumtree
        let group_topic = TopicId::from_group(group_id);
        self.gossip.publish(group_topic, Bytes::from(ciphertext)).await?;

        // 6. Apply locally
        self.message_store.apply(crdt_op);

        Ok(())
    }
}
```

#### 2. **Friend Discovery**

**Scenario**: Alice wants to add Bob as a friend, but they've never connected before.

**Solution**: FOAF + Rendezvous

**User Flow:**
1. **Search**: Alice searches for "Bob Smith"
   - Communitas generates query: `FIND_USER(bob_peer_id)`
   - Sends to Alice's active peers (friends)

2. **FOAF Propagation**:
   - Alice's friend Charlie knows Bob (shares a group)
   - Charlie replies with Bob's addr_hints (encrypted to Alice)
   - If Charlie doesn't know Bob, forwards to his friends (TTL=3)

3. **Connection**:
   - Alice receives Bob's addr_hints
   - Communitas establishes QUIC connection
   - Alice sends friend request (MLS Add proposal)
   - Bob accepts, now in each other's contact CRDT

4. **Ongoing Presence**:
   - Both subscribe to a shared "Friends" MLS group
   - Exchange presence beacons (online/offline status)
   - Can message directly or in group

**Implementation:**
```rust
async fn find_user(&self, target_alias: &str) -> Result<Vec<PeerInfo>> {
    // 1. Query local contact CRDT
    if let Some(peer) = self.contacts.get_by_alias(target_alias) {
        return Ok(vec![peer]);
    }

    // 2. FOAF query
    let query = FindUserQuery {
        target: alias_to_peer_id(target_alias)?,
        requester: self.gossip.identity().peer_id(),
        capability: self.generate_capability_token()?,
        ttl: 3,
        fanout: 3,
    };

    // 3. Send to active peers
    let results = self.gossip.foaf_query(query).await?;

    // 4. If no results, try rendezvous
    if results.is_empty() {
        let shard_id = rendezvous_shard(&target);
        self.gossip.subscribe(TopicId::from_shard(shard_id)).await?;
        // Wait for Provider Summaries...
    }

    Ok(results)
}
```

#### 3. **Shared Albums (Saorsa Sites)**

**Scenario**: Alice's family wants to share vacation photos privately.

**User Flow:**
1. **Create Album**: Alice creates "Summer 2025" album
   - Communitas packages photos into Saorsa Site
   - Generates site manifest with image routes
   - Derives `SID = BLAKE3(alice_site_key)`

2. **Make Private**: Alice restricts to family MLS group
   - Encrypts each photo block with group key (from MLS exporter)
   - Publishes Provider Summary to `SITE_ADVERT:<shard(SID)>`
   - Includes `private.mls_group` field in manifest

3. **Share Link**: Alice sends site link to family group
   - `saorsa://site/<SID>` (custom URI scheme)
   - Group members click link

4. **View Album**: Bob (family member) clicks link
   - Communitas subscribes to `SITE_ADVERT:<shard(SID)>`
   - Receives Provider Summary from Alice's node
   - Fetches manifest via `SITE_SYNC` stream
   - Derives decryption key from shared MLS group
   - Decrypts and displays photos

5. **Sync Updates**: Alice adds new photos
   - Increments site `version`
   - Gossips updated Provider Summary
   - Bob's Communitas detects version change
   - Fetches only new blocks (IBLT reconciliation)

**Implementation:**
```rust
struct AlbumManager {
    gossip: Arc<GossipClient>,
    sites: SaorsaSiteClient,
    mls_groups: HashMap<GroupId, MlsGroup>,
}

impl AlbumManager {
    async fn create_private_album(
        &self,
        name: &str,
        photos: Vec<PathBuf>,
        group_id: &GroupId,
    ) -> Result<SiteId> {
        // 1. Get MLS group key
        let mls_group = self.mls_groups.get(group_id)?;
        let encryption_key = mls_group.exporter_secret("site-encryption")?;

        // 2. Build site
        let mut site_builder = SiteBuilder::new();
        site_builder.set_title(name);

        for photo_path in photos {
            let content = fs::read(&photo_path).await?;
            let encrypted = chacha20poly1305::encrypt(&encryption_key, &content)?;
            site_builder.add_file(
                photo_path.file_name().unwrap().to_str().unwrap(),
                encrypted,
                "image/jpeg",
            );
        }

        // 3. Add MLS group metadata
        site_builder.set_private_group(*group_id);

        // 4. Sign and publish
        let site = site_builder.build()?;
        let sid = site.id();
        self.sites.publish(site).await?;

        // 5. Gossip Provider Summary
        let summary = ProviderSummary {
            target: sid,
            provider: self.gossip.identity().peer_id(),
            capabilities: vec![Capability::Site],
            manifest_version: site.version(),
            // ...
        };
        self.gossip.publish_to_shard(rendezvous_shard(&sid), summary).await?;

        Ok(sid)
    }
}
```

#### 4. **Profile Pages**

**Public Profile:**
- Each user can publish a public Saorsa Site for their profile
- `SID = BLAKE3(user_ml_dsa_pubkey)` (tied to identity)
- Contains: avatar, bio, public posts, links
- Discoverable via rendezvous shard
- Updatable by incrementing version

**Private Details:**
- Contact details (phone, email) stored in encrypted CRDT
- Shared only with friends (MLS group)
- Synced via delta-CRDT anti-entropy

#### 5. **Status Updates / Social Feed**

**Local Timeline:**
- Each user maintains an RGA CRDT of their posts
- Posts encrypted to "All Friends" MLS group
- Gossiped via Plumtree to all friends

**Aggregated Feed:**
- Communitas subscribes to all friends' post topics
- CRDT merge creates unified timeline
- Local scoring/filtering (user preferences)

**Reactions:**
- OR-Set CRDT for likes/reactions
- Add/remove operations, conflict-free merge
- No central like counter

#### 6. **Offline-First Operation**

**Scenario**: Alice is on a plane (no network) for 6 hours.

**Experience:**
1. **Compose Messages**: Alice writes messages to family group
   - Stored locally in CRDT message log
   - UI shows "pending sync" indicator

2. **Edit Profile**: Alice updates her bio
   - LWW-Register applied locally
   - Timestamped for eventual conflict resolution

3. **View Cached Content**: Alice browses photos in shared album
   - All previously fetched blocks cached locally
   - Full functionality for cached sites

4. **Reconnect**: Plane lands, Alice gets WiFi
   - Communitas reconnects to Saorsa Gossip overlay
   - Anti-entropy kicks in:
     - Sends IBLT of local message IDs
     - Receives IBLT of missed messages
     - Fetches diffs, applies CRDT merges
   - All pending messages broadcast
   - UI updates with merged state

**No data loss, deterministic merges, seamless sync.**

---

## Network Topologies and Use Cases

### Topology 1: Fully Connected Friend Circle (< 50 people)

**Use Case**: Small community, family, close friends

**Topology:**
- All peers maintain active connections to all others
- No coordinators needed (everyone is reachable)
- Plumtree degenerates to simple broadcast (efficient for small N)

**Advantages:**
- Lowest latency (direct peer-to-peer)
- Maximum reliability (no single point of failure)
- Simple membership (OR-Set of all peers)

**Example**: Family of 12 sharing photos and messages

### Topology 2: FOAF Network (100-10,000 people)

**Use Case**: Extended social network, alumni group, local community

**Topology:**
- HyParView partial views (active=10, passive=100)
- Plumtree spanning trees per topic
- 1-2 public coordinators for cold starts
- Rendezvous shards for content discovery

**Advantages:**
- Scales to thousands without all-to-all connections
- Social clustering (friends of friends closer in topology)
- Efficient broadcast via tree (not flood)

**Example**: University alumni network with shared groups, event announcements, job board

### Topology 3: Federated Communities (10,000+ people)

**Use Case**: Large-scale social platform, city-wide mesh, global activism network

**Topology:**
- Multiple coordinator tiers (regional, global)
- Rendezvous sharding for all content
- SWIM + HyParView for scalable membership
- Bluetooth bridges for offline regions

**Advantages:**
- No single point of control
- Censorship-resistant (anyone can run coordinator)
- Partition-tolerant (communities can operate offline, resync later)

**Example**: Decentralized Twitter-like platform
- Users publish to their `<peer_id>` shard
- Followers subscribe to that shard
- Rendezvous ensures global findability
- No central server can ban users

### Topology 4: Hybrid Mesh (Corporate/Campus Network)

**Use Case**: Company intranet, university campus, conference venue

**Topology:**
- LAN gossip beacons (UDP multicast) for zero-config joins
- Local coordinator on LAN for fast bootstrap
- Optional gateway to public Saorsa network
- Bluetooth fallback for auditoriums/basements

**Advantages:**
- LAN-only mode (no internet required)
- High bandwidth for large file sharing (site sync)
- Private by default, opt-in to public network

**Example**: Conference app
- Attendees auto-discover on venue WiFi
- Shared schedule, speaker sites, Q&A topics
- Works in basement conference halls (Bluetooth)
- Optionally sync with remote attendees (QUIC)

---

## Conclusion

Saorsa Gossip provides a **complete stack** for building **decentralized, privacy-preserving, post-quantum secure** applications:

✅ **Transport**: QUIC with NAT traversal, PQC handshakes
✅ **Membership**: Scalable, self-healing topology (HyParView + SWIM)
✅ **Dissemination**: Efficient broadcast (Plumtree + anti-entropy)
✅ **Discovery**: No DNS/DHT (Coordinator adverts, Rendezvous, FOAF)
✅ **Content**: Decentralized websites (Saorsa Sites)
✅ **Presence**: Privacy-preserving user discovery
✅ **Sync**: Local-first CRDTs with IBLT reconciliation
✅ **Security**: ML-KEM, ML-DSA, MLS, ChaCha20-Poly1305

**Communitas** demonstrates the full potential by providing:
- End-to-end encrypted group messaging
- Friend-of-a-friend social networking
- Decentralized photo/file sharing
- Offline-first with seamless sync
- No central servers, no surveillance

**The future is peer-to-peer, privacy-preserving, and quantum-safe. Saorsa Gossip makes it real.**

---

## References

- **SPEC2.md**: Original protocol specification (now superseded by this design doc)
- **ant-quic**: https://github.com/maidsafe/ant-quic
- **saorsa-pqc**: https://github.com/saorsalabs/saorsa-pqc
- **MLS RFC 9420**: https://datatracker.ietf.org/doc/rfc9420/
- **Plumtree Paper**: https://asc.di.fct.unl.pt/~jleitao/pdf/srds07-leitao.pdf
- **HyParView Paper**: http://asc.di.fct.unl.pt/~jleitao/pdf/dsn07-leitao.pdf
- **SWIM Paper**: https://www.cs.cornell.edu/projects/Quicksilver/public_pdfs/SWIM.pdf
