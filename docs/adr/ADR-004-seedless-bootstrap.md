# ADR-004: Seedless Bootstrap via Coordinator Adverts

## Status

Accepted (2025-12-24)

## Context

Joining a P2P network requires knowing at least one reachable peer. Traditional approaches create centralization risks:

| Approach | Problem |
|----------|---------|
| Hardcoded IPs | Stale, single point of failure, censorship target |
| DNS seeds | Requires domain ownership, DNS can be blocked |
| Bootstrap servers | Infrastructure cost, centralization |
| Trusted introducers | Social engineering attack vector |

We needed a bootstrap mechanism that:
- Requires no central infrastructure
- Survives node failures gracefully
- Resists censorship and attacks
- Works from cold start (new installation)

## Decision

Implement **Coordinator Adverts**: self-elected, cryptographically signed, gossip-disseminated peer announcements.

**Important:** Roles in adverts are *hints*, not trust anchors. Peers validate
reachability and observed behavior before relying on a node for coordination or relay.

### Coordinator Advert Structure

```rust
#[derive(Serialize, Deserialize)]
pub struct CoordinatorAdvert {
    /// Protocol version
    pub version: u8,
    /// Peer identifier (derived from public key)
    pub peer_id: PeerId,
    /// Roles this coordinator provides
    pub roles: CoordinatorRoles,
    /// Reachable addresses (IPv4/IPv6)
    pub addr_hints: Vec<SocketAddr>,
    /// NAT classification
    pub nat_class: NatClass,
    /// Validity start (unix milliseconds)
    pub not_before: u64,
    /// Validity end (unix milliseconds, ~24h from creation)
    pub not_after: u64,
    /// Local quality score (not transmitted)
    #[serde(skip)]
    pub score: i32,
    /// ML-DSA signature over serialized advert
    pub signature: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct CoordinatorRoles {
    /// Can help with bootstrap discovery
    pub coordinator: bool,
    /// Can observe reflexive addresses
    pub reflector: bool,
    /// Can assist with rendezvous
    pub rendezvous: bool,
    /// Can relay traffic for NAT traversal
    pub relay: bool,
}
```

### Self-Election

Any peer can become a coordinator by:

1. **Publishing an advert**: Sign and broadcast to well-known topic
2. **Maintaining reachability**: Keep advertised addresses valid
3. **Serving requests**: Respond to bootstrap queries

```rust
impl CoordinatorAdvert {
    pub fn new(keypair: &MlDsaKeyPair, roles: CoordinatorRoles, addrs: Vec<SocketAddr>) -> Self {
        let now = unix_time_millis();
        let mut advert = Self {
            version: 1,
            peer_id: PeerId::from_pubkey(keypair.public_key()),
            roles,
            addr_hints: addrs,
            nat_class: NatClass::Unknown,
            not_before: now,
            not_after: now + 24 * 3600 * 1000, // 24 hours
            score: 0,
            signature: vec![],
        };
        advert.signature = keypair.sign(&advert.serialize_unsigned());
        advert
    }

    pub fn verify(&self) -> bool {
        let pubkey = /* derive from peer_id or embed */;
        pubkey.verify(&self.serialize_unsigned(), &self.signature)
    }
}
```

### Advert Dissemination

Adverts are gossiped on a well-known topic:

```rust
const COORDINATOR_TOPIC: TopicId = TopicId::from_static("saorsa:coordinator-adverts");

// Publishing
pubsub.publish(COORDINATOR_TOPIC, advert.serialize()).await?;

// Receiving
let mut stream = pubsub.subscribe(COORDINATOR_TOPIC).await?;
while let Some(msg) = stream.next().await {
    if let Ok(advert) = CoordinatorAdvert::deserialize(&msg.payload) {
        if advert.verify() && advert.is_valid_time() {
            advert_cache.insert(advert);
        }
    }
}
```

### Bootstrap Flow

```
New Peer                    Network
   |                           |
   | 1. Load peer cache        |
   |    (from previous run)    |
   |                           |
   | 2. Try cached coordinators|
   |-------------------------->|
   |                           |
   | 3. Subscribe to advert    |
   |    topic                  |
   |<--------------------------|
   |                           |
   | 4. Receive coordinator    |
   |    adverts via gossip     |
   |<--------------------------|
   |                           |
   | 5. Connect to best-scored |
   |    coordinators           |
   |-------------------------->|
   |                           |
   | 6. Join membership        |
   |    (HyParView)            |
   |<------------------------->|
```

### Cold Start Solution

For truly new installations with no peer cache:

1. **Bundled seeds**: Ship with known coordinator addresses (optional)
2. **QR code sharing**: Scan friend's peer info
3. **Manual entry**: User provides coordinator address
4. **Out-of-band**: Share via website, social media

Once connected to any single peer, coordinator adverts propagate automatically.

### Advert Caching and Scoring

```rust
pub struct AdvertCache {
    adverts: LruCache<PeerId, CoordinatorAdvert>,
    max_size: usize, // 1000 adverts
}

impl AdvertCache {
    pub fn insert(&mut self, mut advert: CoordinatorAdvert) {
        // Score based on connectivity success
        advert.score = self.calculate_score(&advert);
        self.adverts.put(advert.peer_id, advert);
    }

    fn calculate_score(&self, advert: &CoordinatorAdvert) -> i32 {
        let mut score = 0;

        // Prefer coordinators with more roles
        if advert.roles.reflector { score += 10; }
        if advert.roles.relay { score += 20; }
        if advert.roles.rendezvous { score += 10; }

        // Prefer better NAT types
        match advert.nat_class {
            NatClass::Eim => score += 30,
            NatClass::Edm => score += 20,
            NatClass::Symmetric => score += 0,
            NatClass::Unknown => score += 10,
        }

        // Prefer fresher adverts
        let age_hours = advert.age_hours();
        score -= (age_hours * 2) as i32;

        // Historical success rate
        score += self.success_rate(&advert.peer_id) * 50;

        score
    }

    pub fn best_coordinators(&self, count: usize) -> Vec<&CoordinatorAdvert> {
        let mut sorted: Vec<_> = self.adverts.iter().collect();
        sorted.sort_by(|a, b| b.1.score.cmp(&a.1.score));
        sorted.into_iter().take(count).map(|(_, a)| a).collect()
    }
}
```

### Persistence

Advert cache is persisted across restarts:

```rust
impl AdvertCache {
    pub fn save(&self, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        ciborium::into_writer(&self.adverts, &mut BufWriter::new(file))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let file = File::open(path)?;
        fs2::FileExt::lock_shared(&file)?;
        let adverts = ciborium::from_reader(BufReader::new(file))?;
        Ok(Self { adverts, ..Default::default() })
    }
}
```

## Consequences

### Benefits

1. **No central infrastructure**: Any peer can become coordinator
2. **Censorship resistant**: No single domain/IP to block
3. **Self-healing**: Failed coordinators replaced by new ones
4. **Automatic discovery**: New coordinators propagate via gossip
5. **Quality-based selection**: Score-weighted selection improves over time

### Trade-offs

1. **Cold start dependency**: First boot needs some seed mechanism
2. **Advert spam potential**: Rate limiting needed
3. **TTL management**: Stale adverts waste connection attempts
4. **Trust bootstrapping**: First connection has weak verification

### Rate Limiting

Prevent coordinator spam:

```rust
impl AdvertValidator {
    fn validate(&self, advert: &CoordinatorAdvert) -> bool {
        // Check signature
        if !advert.verify() { return false; }

        // Check TTL (max 24 hours)
        if advert.not_after - advert.not_before > 24 * 3600 * 1000 {
            return false;
        }

        // Rate limit per peer
        if self.recent_adverts.contains(&advert.peer_id) {
            return false;
        }

        // Check advert size
        if advert.addr_hints.len() > 10 { return false; }

        true
    }
}
```

## Alternatives Considered

### 1. DNS-Based Bootstrap

Use DNS TXT records or SRV records for peer discovery.

**Rejected because**:
- Requires domain registration
- DNS can be blocked/poisoned
- Centralized control over records

### 2. Blockchain-Based Registry

Store coordinator addresses in smart contract.

**Rejected because**:
- Requires blockchain access
- Transaction fees for updates
- Slow update propagation
- Additional infrastructure dependency

### 3. DHT-Based Bootstrap

Use existing DHT network for peer discovery.

**Rejected because**:
- Chicken-and-egg: need peers to query DHT
- DHT nodes can be attacked
- Adds DHT dependency we're trying to avoid

### 4. Static Seed List

Hardcode known peer addresses.

**Rejected because**:
- Addresses become stale
- Requires software updates to change
- Single point of failure

### 5. mDNS/Local Discovery

Use multicast DNS for local network discovery.

**Rejected because**:
- Only works on local network
- Doesn't help global bootstrap
- May be disabled on networks

## References

- **Implementation**: `crates/coordinator/src/`
- **Advert format**: `crates/coordinator/src/advert.rs`
- **Peer cache**: `crates/coordinator/src/peer_cache.rs`
- **ant-quic ADR-002**: Epsilon-Greedy Bootstrap Cache (related transport-level caching)
