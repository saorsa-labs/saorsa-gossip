# saorsa-gossip-presence

Presence beacons and user discovery for the Saorsa Gossip overlay network.

## Overview

The `saorsa-gossip-presence` crate provides a secure presence system for distributed group communication. It implements:

- **ML-DSA Beacon Signing**: Cryptographic signing of presence beacons using ML-DSA-65 (post-quantum signature scheme)
- **FOAF (Friend-of-a-Friend) Discovery**: Random-walk queries to discover group members
- **Per-Peer Rate Limiting**: Token bucket rate limiting to prevent spam and DoS attacks
- **IBLT Summaries**: Efficient reconciliation for presence state synchronization (future)

## Features

### ML-DSA Beacon Signing

All presence beacons are cryptographically signed using ML-DSA-65 (FIPS 204 post-quantum signature standard):

- Each `PresenceRecord` includes a `signature` and `signer_pubkey` field
- Beacons are signed using `MlDsaKeyPair` from `saorsa-gossip-identity`
- Canonical serialization via `signable_bytes()` ensures deterministic signing
- Provides cryptographic proof of beacon authenticity

**Example:**
```rust
use saorsa_gossip_presence::PresenceService;
use saorsa_gossip_identity::MlDsaKeyPair;

// Create service with identity
let keypair = MlDsaKeyPair::generate();
let service = PresenceService::new_with_identity(
    transport,
    group_ctx,
    keypair,
);

// Beacons are automatically signed on broadcast
service.start_beaconing(topic_id, beacon_interval);
```

### Signature Verification on Receipt

Incoming beacons are automatically verified:

- `handle_presence_message()` verifies ML-DSA signatures on received beacons
- Invalid signatures are rejected and logged
- Unsigned beacons can be optionally required via environment variable

**Strict verification mode:**
```bash
# Require all beacons to be signed (reject unsigned)
export COMMUNITAS_PRESENCE_REQUIRE_SIGNED=1
```

Without this environment variable, unsigned beacons are accepted but logged as warnings (for backward compatibility during migration).

### Per-Peer Rate Limiting

Token bucket rate limiting protects against spam and denial-of-service:

- **Default limits**: Burst of 3 messages, refill at 0.2/sec (12 per minute)
- Applied to incoming beacons and queries
- Per-peer tracking with automatic cleanup of stale entries
- Configurable via `RateLimitConfig`

**Example:**
```rust
use saorsa_gossip_presence::{RateLimitConfig, RateLimiter};

// Custom rate limiting
let config = RateLimitConfig {
    max_tokens: 5.0,     // Allow burst of 5
    refill_rate: 0.5,    // 30 per minute
};

let mut limiter = RateLimiter::new(config);
if limiter.check_rate_limit(peer_id) {
    // Process message
} else {
    // Rate limited - drop message
}
```

Rate-limited messages are dropped and logged. Stale peer entries are automatically cleaned every 10 beacon cycles to prevent memory leaks.

### Identity Late Binding

Identity can be set after construction to support flexible initialization:

```rust
// Create service without identity
let service = PresenceService::new(transport, group_ctx);

// Set identity later
let keypair = MlDsaKeyPair::generate();
service.set_identity(keypair).await;
```

This is useful when identity is not available at service creation time (e.g., during async initialization or lazy loading).

### FOAF Discovery

Friend-of-a-Friend queries discover presence through the social graph:

- Random-walk queries with TTL-based hop limiting
- Aggregated responses deduplicated by PeerId
- Efficient for discovering new group members

**Example:**
```rust
// Query for group members
let members = service.query_presence(topic_id).await?;
for (peer_id, record) in members {
    println!("Found: {}", peer_id);
}
```

## Architecture

### Presence Messages

The wire protocol supports three message types:

1. **Beacon**: Periodic announcements of presence (signed with ML-DSA)
2. **Query**: FOAF discovery requests (rate limited)
3. **QueryResponse**: Aggregated responses to queries

### Security Model

- **Post-quantum signatures**: ML-DSA-65 for long-term security
- **Rate limiting**: Per-peer token buckets prevent abuse
- **Signature verification**: Cryptographic authenticity for all beacons
- **Canonical serialization**: Deterministic signing via `signable_bytes()`

### Performance

- **Automatic cleanup**: Stale rate limiter entries removed every 10 beacon cycles
- **Efficient verification**: Fast ML-DSA verification using hardware acceleration where available
- **Configurable limits**: Tune rate limiting for network characteristics

## Configuration

### Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `COMMUNITAS_PRESENCE_REQUIRE_SIGNED` | Reject unsigned beacons | Optional (warn only) |

### Rate Limit Defaults

- **Burst capacity**: 3 messages
- **Refill rate**: 0.2 tokens/second (12 per minute)
- **Cleanup interval**: Every 10 beacon cycles

## Testing

```bash
# Run all tests
cargo nextest run -p saorsa-gossip-presence

# Run with output
cargo nextest run -p saorsa-gossip-presence --no-capture

# Property-based tests
cargo test -p saorsa-gossip-presence -- --ignored
```

## Dependencies

- `saorsa-gossip-identity`: ML-DSA keypair and signature verification
- `saorsa-gossip-transport`: Gossip message transport
- `saorsa-gossip-groups`: Group context management
- `saorsa-gossip-types`: Core types (PeerId, TopicId, PresenceRecord)

## License

Dual-licensed under AGPL-3.0 or commercial license.
