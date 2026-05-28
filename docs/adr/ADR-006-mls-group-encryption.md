# ADR-006: MLS Group Encryption (RFC 9420)

## Status

Accepted (2025-12-24)

## Context

Group communication in P2P systems requires encryption that:

1. **Scales efficiently**: Adding/removing members shouldn't require N^2 key exchanges
2. **Provides forward secrecy**: Compromised keys don't reveal past messages
3. **Provides post-compromise security**: Recovery after key compromise
4. **Works asynchronously**: Members can be offline during key updates
5. **Is quantum-resistant**: Future-proof against quantum attacks

Traditional approaches have limitations:

| Approach | Problem |
|----------|---------|
| Pairwise encryption | O(N^2) keys, doesn't scale |
| Single group key | No forward secrecy, member removal broken |
| Sender keys | No post-compromise security |
| Custom protocol | Security analysis expensive |

## Decision

Adopt **MLS (Message Layer Security)** per RFC 9420, with post-quantum ciphersuites:

### MLS Integration Architecture

```rust
use saorsa_mls::{MlsGroup, MlsConfig, Proposal, Commit};

/// Wraps MLS for Saorsa Gossip group management
pub struct GroupManager {
    /// Per-group MLS state
    groups: HashMap<GroupId, MlsGroup>,
    /// Our MLS credentials
    credential: MlsCredential,
    /// Key derivation for message encryption
    kdf: Blake3Kdf,
}

impl GroupManager {
    /// Create a new group (we become the first member)
    pub fn create_group(&mut self, group_id: GroupId) -> Result<GroupId> {
        let group = MlsGroup::new(
            MlsConfig::default()
                .with_ciphersuite(MLS_CIPHERSUITE_PQ)
                .with_credential(self.credential.clone()),
        )?;
        self.groups.insert(group_id, group);
        Ok(group_id)
    }

    /// Add member to group
    pub fn add_member(&mut self, group_id: &GroupId, key_package: KeyPackage) -> Result<Commit> {
        let group = self.groups.get_mut(group_id)?;
        let proposal = Proposal::Add { key_package };
        let commit = group.commit(&[proposal])?;
        Ok(commit)
    }

    /// Remove member from group
    pub fn remove_member(&mut self, group_id: &GroupId, member: PeerId) -> Result<Commit> {
        let group = self.groups.get_mut(group_id)?;
        let proposal = Proposal::Remove { member };
        let commit = group.commit(&[proposal])?;
        Ok(commit)
    }

    /// Process incoming commit (updates our view of group)
    pub fn process_commit(&mut self, group_id: &GroupId, commit: Commit) -> Result<()> {
        let group = self.groups.get_mut(group_id)?;
        group.process_commit(commit)?;
        Ok(())
    }
}
```

### Key Derivation for Message Encryption

MLS provides an "exporter secret" per epoch. We derive per-message keys:

```rust
impl GroupManager {
    /// Derive encryption key for current epoch
    pub fn derive_message_key(&self, group_id: &GroupId, context: &[u8]) -> Result<[u8; 32]> {
        let group = self.groups.get(group_id)?;

        // Get MLS exporter secret for current epoch
        let exporter_secret = group.export_secret(
            b"saorsa-gossip-message-key",
            context,
            32,
        )?;

        Ok(exporter_secret.try_into().unwrap())
    }

    /// Encrypt message for group
    pub fn encrypt_message(&self, group_id: &GroupId, plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = self.derive_message_key(group_id, b"message")?;
        let nonce = generate_nonce();

        // ChaCha20-Poly1305 AEAD
        let ciphertext = chacha20poly1305::encrypt(&key, &nonce, plaintext)?;

        Ok(encode_ciphertext(&nonce, &ciphertext))
    }

    /// Decrypt message from group
    pub fn decrypt_message(&self, group_id: &GroupId, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let key = self.derive_message_key(group_id, b"message")?;
        let (nonce, encrypted) = decode_ciphertext(ciphertext)?;

        let plaintext = chacha20poly1305::decrypt(&key, &nonce, &encrypted)?;
        Ok(plaintext)
    }
}
```

### Group Epoch Model

MLS uses epochs to track key evolution:

```
Epoch 0: Initial group creation
         ↓ Add(Bob)
Epoch 1: Alice, Bob
         ↓ Add(Charlie)
Epoch 2: Alice, Bob, Charlie
         ↓ Remove(Bob)
Epoch 3: Alice, Charlie (new keys, Bob can't decrypt)
         ↓ Update(Alice) [key rotation]
Epoch 4: Alice, Charlie (Alice's compromise healed)
```

Each epoch has distinct encryption keys. Messages from earlier epochs are unreadable after removal.

### Post-Quantum Ciphersuite

```rust
/// Post-quantum MLS ciphersuite
const MLS_CIPHERSUITE_PQ: CipherSuite = CipherSuite {
    /// ML-KEM-768 for key encapsulation
    kem: MlKem768,
    /// ML-DSA-65 for signatures
    signature: MlDsa65,
    /// BLAKE3 for hashing
    hash: Blake3,
    /// ChaCha20-Poly1305 for symmetric encryption
    aead: ChaCha20Poly1305,
};
```

### Use Cases

#### 1. Private Topics

Group encryption for topic subscriptions:

```rust
// Create private topic
let group_id = groups.create_group(TopicId::new_random())?;

// Add authorized subscribers
for member in authorized_peers {
    let kp = fetch_key_package(&member).await?;
    let commit = groups.add_member(&group_id, kp)?;
    gossip_commit(&group_id, commit).await?;
}

// Publish encrypted message
let ciphertext = groups.encrypt_message(&group_id, message)?;
pubsub.publish(group_id.as_topic(), ciphertext).await?;
```

#### 2. Presence Beacons

Encrypted presence for privacy:

```rust
// Beacon encrypted to presence group
let beacon = PresenceBeacon {
    peer_id: my_peer_id,
    status: "online",
    timestamp: now(),
};
let encrypted = groups.encrypt_message(&presence_group, &beacon.serialize())?;
pubsub.publish(PRESENCE_TOPIC, encrypted).await?;

// Only group members can decrypt
```

#### 3. Private Site Blocks

Site content encrypted to subscribers:

```rust
// Encrypt site block for site subscribers
let block = SiteBlock { content, metadata };
let encrypted = groups.encrypt_message(&site_group, &block.serialize())?;
crdt_sync.broadcast_delta(site_group.as_topic(), encrypted).await?;
```

### Key Package Distribution

New members provide KeyPackages (prekeys) for adding:

```rust
#[derive(Serialize, Deserialize)]
pub struct KeyPackage {
    /// ML-KEM-768 public key for key encapsulation
    pub init_key: Vec<u8>,
    /// ML-DSA-65 credential
    pub credential: Credential,
    /// Signed by credential
    pub signature: Vec<u8>,
    /// Extensions (lifetime, capabilities)
    pub extensions: Vec<Extension>,
}

impl KeyPackage {
    /// Generate new KeyPackage
    pub fn generate(keypair: &MlDsaKeyPair) -> Result<Self> {
        let kem_keypair = MlKem768::generate();
        let credential = Credential::new(keypair.public_key());

        let mut kp = Self {
            init_key: kem_keypair.public_key().to_vec(),
            credential,
            signature: vec![],
            extensions: vec![
                Extension::Lifetime { not_after: now() + days(30) },
            ],
        };
        kp.signature = keypair.sign(&kp.serialize_unsigned())?;
        Ok(kp)
    }
}
```

KeyPackages are distributed via rendezvous shards or direct request.

## Consequences

### Benefits

1. **Efficient scaling**: O(log N) for add/remove operations
2. **Forward secrecy**: Past messages unreadable after key rotation
3. **Post-compromise security**: Key updates heal compromises
4. **Standard protocol**: RFC 9420 with security analysis
5. **Quantum-resistant**: ML-KEM-768 + ML-DSA-65 ciphersuites

### Trade-offs

1. **State management**: Must track epoch and member state
2. **Commit coordination**: Need consensus on commit ordering
3. **KeyPackage distribution**: Members need accessible prekeys
4. **Complexity**: MLS is a complex protocol

### State Synchronization

MLS state must be consistent across members:

```rust
/// Handle commit ordering conflicts
fn handle_commit_conflict(&mut self, commit: Commit) -> Result<()> {
    // If commit's parent epoch doesn't match our state,
    // we may have missed previous commits

    if commit.parent_epoch() != self.current_epoch() {
        // Request missing commits via anti-entropy
        self.request_commits_since(commit.parent_epoch()).await?;
    }

    self.process_commit(commit)?;
    Ok(())
}
```

## Alternatives Considered

### 1. Sender Keys (Signal's Approach)

Each member has asymmetric sender key, recipients decrypt.

**Rejected because**:
- No post-compromise security
- Key distribution complexity
- Not standardized

### 2. Pairwise Encryption

Encrypt to each recipient individually.

**Rejected because**:
- O(N) encryption per message
- Doesn't scale to large groups
- Bandwidth explosion

### 3. Single Group Symmetric Key

All members share one AES key.

**Rejected because**:
- No forward secrecy
- Remove = re-key entire group
- Compromise exposes all messages

### 4. Custom Tree-Based Protocol

Design custom ratcheting tree scheme.

**Rejected because**:
- Expensive security analysis
- Reinventing wheel
- MLS already solves this

### 5. OpenMLS or mlspp

Use existing MLS implementations directly.

**Considered but**:
- We wrap saorsa-mls for PQ ciphersuites
- Need integration with our crypto stack
- May migrate to these if they add PQ support

## References

- **RFC 9420**: The Messaging Layer Security (MLS) Protocol
  - https://www.rfc-editor.org/rfc/rfc9420
- **MLS Architecture**: RFC 9420 Section 2
- **Implementation**: `crates/groups/src/`
- **Dependency**: `saorsa-mls` (post-quantum MLS implementation)
- **Key derivation**: `crates/groups/src/kdf.rs`
- **IETF Draft**: `draft-ietf-mls-pq-ciphersuites-04` — ML-KEM and Hybrid Cipher Suites for MLS (March 2026)
  - https://datatracker.ietf.org/doc/draft-ietf-mls-pq-ciphersuites/
