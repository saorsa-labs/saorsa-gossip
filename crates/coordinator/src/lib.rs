#![warn(missing_docs)]

//! Coordinator advertisements for seedless bootstrap
//!
//! Implements SPEC2 §8 Coordinator Adverts for NAT traversal and seedless discovery.
//!
//! ## Overview
//!
//! Coordinators are self-elected public nodes that provide:
//! - **Bootstrap**: Seedless network entry
//! - **Reflection**: Address observation for NAT detection
//! - **Rendezvous**: Connection coordination for hole punching
//! - **Relay**: Last-resort message forwarding
//!
//! ## Cache Architecture
//!
//! This crate uses a two-layer cache architecture:
//!
//! 1. **ant-quic's `BootstrapCache`**: Handles peer quality scoring, epsilon-greedy selection,
//!    persistent storage, and connection statistics.
//!
//! 2. **`GossipCacheAdapter`**: Extends the bootstrap cache with gossip-specific data
//!    (coordinator adverts with ML-DSA signatures, expiry times, role information).
//!
//! ### Ownership Pattern
//!
//! The application owns the `Arc<BootstrapCache>` and shares it with the gossip layer:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │  Application (saorsa-node / communitas)             │
//! │  ┌───────────────────────────────────┐              │
//! │  │ Arc<BootstrapCache>               │ ← App owns   │
//! │  └───────────────────────────────────┘              │
//! │              │                                       │
//! │              ▼                                       │
//! │  ┌───────────────────────────────────┐              │
//! │  │ GossipCacheAdapter                │ ← Wraps      │
//! │  │ - cache: Arc<BootstrapCache>      │              │
//! │  │ - adverts: HashMap<PeerId, Advert>│              │
//! │  └───────────────────────────────────┘              │
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! ## Quick Start
//!
//! ### Creating a Cache
//!
//! ```rust,ignore
//! use saorsa_gossip_coordinator::{GossipCacheAdapter, bootstrap_cache::*};
//! use std::sync::Arc;
//!
//! // Application creates and owns the bootstrap cache
//! let config = BootstrapCacheConfig::builder()
//!     .cache_dir("/path/to/cache".into())
//!     .build();
//! let cache = Arc::new(BootstrapCache::open(config).await?);
//!
//! // Gossip layer wraps it with adapter
//! let adapter = GossipCacheAdapter::new(cache);
//!
//! // Use for coordinator discovery
//! let coordinators = adapter.select_coordinators(3).await;
//! ```
//!
//! ### Bootstrap Flow
//!
//! ```rust,ignore
//! use saorsa_gossip_coordinator::{Bootstrap, GossipCacheAdapter, CoordinatorHandler, BootstrapAction};
//!
//! // Create handler and bootstrap with shared cache
//! let handler = CoordinatorHandler::with_cache(peer_id, adapter.clone());
//! let bootstrap = Bootstrap::new(peer_id, adapter, handler);
//!
//! // Find a coordinator
//! match bootstrap.find_coordinator().await {
//!     BootstrapAction::Connect(result) => {
//!         // Connect to result.selected_peer at result.addrs
//!     }
//!     BootstrapAction::Query(query) => {
//!         // Send FOAF query to discover coordinators
//!     }
//!     BootstrapAction::Wait => {
//!         // Wait and retry later
//!     }
//! }
//! ```
//!
//! ### Creating Adverts
//!
//! ```rust
//! use saorsa_gossip_coordinator::{CoordinatorAdvert, CoordinatorRoles, NatClass};
//! use saorsa_gossip_types::PeerId;
//!
//! # fn main() -> anyhow::Result<()> {
//! // Create a coordinator advert
//! let peer = PeerId::new([1u8; 32]);
//! let roles = CoordinatorRoles {
//!     coordinator: true,
//!     reflector: true,
//!     rendezvous: false,
//!     relay: false,
//! };
//!
//! let advert = CoordinatorAdvert::new(
//!     peer,
//!     roles,
//!     vec![],
//!     NatClass::Eim,
//!     3600_000, // 1 hour validity
//! );
//!
//! assert!(advert.is_valid());
//! # Ok(())
//! # }
//! ```
//!
//! ## Migration from Deprecated Types
//!
//! The following types are deprecated and will be removed in a future version:
//!
//! | Deprecated | Replacement |
//! |------------|-------------|
//! | `PeerCache` | [`GossipCacheAdapter`] with [`bootstrap_cache::BootstrapCache`] |
//! | `PeerCacheEntry` | [`bootstrap_cache::CachedPeer`] + [`CoordinatorAdvert`] |
//! | `AdvertCache` | [`GossipCacheAdapter`] |
//!
//! See the documentation on each deprecated type for migration examples.

use saorsa_gossip_types::{unix_millis, PeerId};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

mod bootstrap;
mod cache;
mod foaf;
mod gossip_cache;
mod handler;
mod peer_cache;
mod publisher;
mod topic;

pub use bootstrap::{Bootstrap, BootstrapAction, BootstrapResult, TraversalMethod};
#[allow(deprecated)]
pub use cache::AdvertCache;
pub use foaf::{
    FindCoordinatorQuery, FindCoordinatorResponse, DEFAULT_FIND_COORDINATOR_FANOUT,
    MAX_FIND_COORDINATOR_TTL,
};
pub use gossip_cache::{capabilities_to_roles, nat_type_to_nat_class, GossipCacheAdapter};
pub use handler::CoordinatorHandler;
pub use peer_cache::PeerRoles;
#[allow(deprecated)]
pub use peer_cache::{PeerCache, PeerCacheEntry};
pub use publisher::{CoordinatorPublisher, PeriodicPublisher};
pub use topic::coordinator_topic;

// Re-export ant-quic bootstrap cache types for consumer convenience
pub mod bootstrap_cache {
    //! Re-exports from ant-quic bootstrap cache.
    //!
    //! These types are provided for consumers who need direct access to
    //! ant-quic's bootstrap cache functionality.
    pub use ant_quic::bootstrap_cache::{
        BootstrapCache, BootstrapCacheConfig, BootstrapCacheConfigBuilder, CacheEvent, CacheStats,
        CachedPeer, ConnectionOutcome, ConnectionStats, NatType, PeerCapabilities, PeerSource,
        QualityWeights, SelectionStrategy,
    };
}

/// Coordinator roles per SPEC2 §8
///
/// Type alias for backward compatibility - use `PeerRoles` for new code.
pub type CoordinatorRoles = PeerRoles;

/// NAT class detection per SPEC2 §8
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NatClass {
    /// Endpoint-Independent Mapping (best for hole punching)
    Eim,
    /// Endpoint-Dependent Mapping (moderate difficulty)
    Edm,
    /// Symmetric NAT (hardest to traverse)
    Symmetric,
    /// Unknown NAT type
    #[default]
    Unknown,
}

/// Address hint for NAT traversal
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrHint {
    /// Socket address (IP:port)
    pub addr: SocketAddr,
    /// When this address was observed (unix timestamp ms)
    pub observed_at: u64,
}

impl AddrHint {
    /// Create a new address hint with current timestamp
    pub fn new(addr: SocketAddr) -> Self {
        let now = unix_millis();

        Self {
            addr,
            observed_at: now,
        }
    }
}

/// Coordinator Advertisement per SPEC2 §8
///
/// Wire format (CBOR):
/// ```json
/// {
///   "v": 1,
///   "peer": [u8; 32],
///   "roles": {"coordinator": true, "reflector": true, ...},
///   "addr_hints": [{"addr": "ip:port", "observed_at": u64}, ...],
///   "nat_class": "eim|edm|symmetric|unknown",
///   "not_before": u64,
///   "not_after": u64,
///   "score": i32,
///   "sig": [u8]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorAdvert {
    /// Protocol version (currently 1)
    pub v: u8,
    /// Peer identifier
    pub peer: PeerId,
    /// Coordinator roles
    pub roles: CoordinatorRoles,
    /// Address hints for connection
    pub addr_hints: Vec<AddrHint>,
    /// NAT classification
    pub nat_class: NatClass,
    /// Valid not before (unix ms)
    pub not_before: u64,
    /// Valid not after (unix ms)
    pub not_after: u64,
    /// Local-only advisory score (higher is better)
    pub score: i32,
    /// ML-DSA signature over all fields except sig
    #[serde(with = "serde_bytes")]
    pub sig: Vec<u8>,
}

impl CoordinatorAdvert {
    /// Create a new coordinator advert (unsigned)
    ///
    /// # Arguments
    /// * `peer` - Peer identifier
    /// * `roles` - Coordinator roles
    /// * `addr_hints` - Known addresses for this coordinator
    /// * `nat_class` - NAT classification
    /// * `validity_duration_ms` - How long this advert is valid (milliseconds)
    pub fn new(
        peer: PeerId,
        roles: CoordinatorRoles,
        addr_hints: Vec<AddrHint>,
        nat_class: NatClass,
        validity_duration_ms: u64,
    ) -> Self {
        let now = unix_millis();

        Self {
            v: 1,
            peer,
            roles,
            addr_hints,
            nat_class,
            not_before: now,
            not_after: now + validity_duration_ms,
            score: 0,
            sig: Vec::new(), // Will be filled by sign()
        }
    }

    /// Sign the advert with ML-DSA
    ///
    /// Signs all fields except `sig` using ML-DSA-65 from saorsa-pqc.
    /// Uses CBOR serialization for deterministic signing.
    pub fn sign(&mut self, signing_key: &saorsa_pqc::MlDsaSecretKey) -> anyhow::Result<()> {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        // Serialize all fields except signature using CBOR for determinism
        let mut to_sign = Vec::new();
        ciborium::into_writer(
            &SignableFields {
                v: self.v,
                peer: &self.peer,
                roles: &self.roles,
                addr_hints: &self.addr_hints,
                nat_class: &self.nat_class,
                not_before: self.not_before,
                not_after: self.not_after,
                score: self.score,
            },
            &mut to_sign,
        )?;

        // Sign with ML-DSA-65
        let signer = MlDsa65::new();
        let signature = signer.sign(signing_key, &to_sign)?;

        self.sig = signature.as_bytes().to_vec();
        Ok(())
    }

    /// Verify the advert signature
    ///
    /// Verifies the ML-DSA-65 signature over all fields except `sig`.
    /// Uses CBOR serialization for deterministic verification.
    pub fn verify(&self, public_key: &saorsa_pqc::MlDsaPublicKey) -> anyhow::Result<bool> {
        use saorsa_pqc::{MlDsa65, MlDsaOperations, MlDsaSignature};

        // Reconstruct signed data using CBOR
        let mut to_verify = Vec::new();
        ciborium::into_writer(
            &SignableFields {
                v: self.v,
                peer: &self.peer,
                roles: &self.roles,
                addr_hints: &self.addr_hints,
                nat_class: &self.nat_class,
                not_before: self.not_before,
                not_after: self.not_after,
                score: self.score,
            },
            &mut to_verify,
        )?;

        // Verify signature
        let verifier = MlDsa65::new();
        let sig = MlDsaSignature::from_bytes(&self.sig)?;

        Ok(verifier.verify(public_key, &to_verify, &sig)?)
    }

    /// Check if advert is currently valid (not expired)
    pub fn is_valid(&self) -> bool {
        let now = unix_millis();

        now >= self.not_before && now <= self.not_after
    }

    /// Serialize to CBOR wire format (RFC 8949)
    pub fn to_cbor(&self) -> anyhow::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        ciborium::into_writer(self, &mut buffer)?;
        Ok(buffer)
    }

    /// Deserialize from CBOR wire format (RFC 8949)
    pub fn from_cbor(data: &[u8]) -> anyhow::Result<Self> {
        let advert = ciborium::from_reader(data)?;
        Ok(advert)
    }

    /// Convert to bytes for transport
    pub fn to_bytes(&self) -> anyhow::Result<bytes::Bytes> {
        Ok(bytes::Bytes::from(self.to_cbor()?))
    }

    /// Parse from bytes received over transport
    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        Self::from_cbor(data)
    }
}

/// Helper struct for serializing fields to sign
#[derive(Serialize)]
struct SignableFields<'a> {
    v: u8,
    peer: &'a PeerId,
    roles: &'a CoordinatorRoles,
    addr_hints: &'a Vec<AddrHint>,
    nat_class: &'a NatClass,
    not_before: u64,
    not_after: u64,
    score: i32,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_advert_creation() {
        let peer = PeerId::new([1u8; 32]);
        let roles = CoordinatorRoles {
            coordinator: true,
            reflector: true,
            rendezvous: false,
            relay: false,
        };
        let addr_hints = vec![];
        let nat_class = NatClass::Eim;

        let advert = CoordinatorAdvert::new(peer, roles, addr_hints, nat_class, 3_600_000);

        assert_eq!(advert.v, 1);
        assert_eq!(advert.peer, peer);
        assert!(advert.roles.coordinator);
        assert!(advert.roles.reflector);
        assert!(!advert.roles.rendezvous);
        assert!(advert.is_valid());
    }

    #[test]
    fn test_advert_serialization() {
        let peer = PeerId::new([1u8; 32]);
        let roles = CoordinatorRoles {
            coordinator: true,
            reflector: false,
            rendezvous: false,
            relay: false,
        };
        let advert = CoordinatorAdvert::new(peer, roles, vec![], NatClass::Unknown, 1000);

        let bytes = advert.to_cbor().expect("serialization should succeed");
        let decoded = CoordinatorAdvert::from_cbor(&bytes).expect("deserialization should succeed");

        assert_eq!(advert.peer, decoded.peer);
        assert_eq!(advert.v, decoded.v);
        assert_eq!(advert.roles, decoded.roles);
    }

    #[test]
    fn test_advert_with_addr_hints() {
        let peer = PeerId::new([2u8; 32]);
        let addr1 = "127.0.0.1:8080".parse().expect("valid address");
        let addr2 = "192.168.1.1:9000".parse().expect("valid address");

        let hints = vec![AddrHint::new(addr1), AddrHint::new(addr2)];

        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            hints,
            NatClass::Edm,
            5000,
        );

        assert_eq!(advert.addr_hints.len(), 2);
        assert_eq!(advert.addr_hints[0].addr, addr1);
        assert_eq!(advert.addr_hints[1].addr, addr2);
    }

    #[test]
    fn test_advert_expiry() {
        let peer = PeerId::new([3u8; 32]);

        // Create advert with very short validity
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            1, // 1ms validity
        );

        assert!(advert.is_valid(), "Should be valid immediately");

        // Sleep to let it expire
        std::thread::sleep(std::time::Duration::from_millis(5));

        assert!(!advert.is_valid(), "Should be expired after sleep");
    }

    #[test]
    fn test_sign_and_verify() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let peer = PeerId::new([4u8; 32]);
        let mut advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            10_000,
        );

        // Generate keypair
        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair generation");

        // Sign
        advert.sign(&sk).expect("signing should succeed");
        assert!(!advert.sig.is_empty(), "Signature should be populated");

        // Verify
        let valid = advert.verify(&pk).expect("verification should succeed");
        assert!(valid, "Signature should be valid");

        // Tamper with advert
        advert.score = 100;

        // Verify should fail
        let valid = advert.verify(&pk).expect("verification should succeed");
        assert!(!valid, "Tampered signature should be invalid");
    }

    /// Test that CBOR serialization produces RFC 8949 compliant output
    #[test]
    fn test_cbor_is_not_bincode() {
        let peer = PeerId::new([5u8; 32]);
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            5_000,
        );

        let cbor_bytes = advert.to_cbor().expect("CBOR serialization");
        let bincode_bytes = bincode::serialize(&advert).expect("bincode serialization");

        // CBOR and bincode MUST produce different wire formats
        assert_ne!(
            cbor_bytes, bincode_bytes,
            "CBOR must not be bincode - wire formats differ"
        );
    }

    /// Test CBOR round-trip serialization
    #[test]
    fn test_cbor_round_trip() {
        let peer = PeerId::new([6u8; 32]);
        let addr = "192.168.1.100:9000".parse().expect("valid addr");
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles {
                coordinator: true,
                reflector: true,
                rendezvous: true,
                relay: false,
            },
            vec![AddrHint::new(addr)],
            NatClass::Edm,
            7_000,
        );

        // Serialize to CBOR
        let cbor_bytes = advert.to_cbor().expect("CBOR serialization");

        // Deserialize from CBOR
        let decoded = CoordinatorAdvert::from_cbor(&cbor_bytes).expect("CBOR deserialization");

        // Verify all fields match
        assert_eq!(decoded.v, advert.v);
        assert_eq!(decoded.peer, advert.peer);
        assert_eq!(decoded.roles.coordinator, advert.roles.coordinator);
        assert_eq!(decoded.roles.reflector, advert.roles.reflector);
        assert_eq!(decoded.roles.rendezvous, advert.roles.rendezvous);
        assert_eq!(decoded.roles.relay, advert.roles.relay);
        assert_eq!(decoded.nat_class, advert.nat_class);
        assert_eq!(decoded.addr_hints.len(), advert.addr_hints.len());
        assert_eq!(decoded.not_before, advert.not_before);
        assert_eq!(decoded.not_after, advert.not_after);
        assert_eq!(decoded.score, advert.score);
    }

    /// Test CBOR handles malformed input gracefully
    #[test]
    fn test_cbor_malformed_input() {
        let malformed = vec![0xFF, 0xFF, 0xFF, 0xFF];
        let result = CoordinatorAdvert::from_cbor(&malformed);
        assert!(result.is_err(), "Malformed CBOR should fail to deserialize");
    }

    /// Test CBOR wire format compatibility with known bytes
    #[test]
    fn test_cbor_wire_format_structure() {
        let peer = PeerId::new([7u8; 32]);
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            1_000,
        );

        let cbor_bytes = advert.to_cbor().expect("CBOR serialization");

        // CBOR should start with a map indicator (major type 5)
        // First byte should be 0xA8 (map with 8 key-value pairs) or 0xBF (indefinite map)
        assert!(
            cbor_bytes[0] >= 0xA0 && cbor_bytes[0] <= 0xBF,
            "CBOR should start with map indicator, got: 0x{:02X}",
            cbor_bytes[0]
        );
    }

    /// Test signed advert CBOR round-trip preserves signature
    #[test]
    fn test_cbor_signed_advert_round_trip() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let peer = PeerId::new([8u8; 32]);
        let mut advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            10_000,
        );

        // Sign the advert
        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");
        advert.sign(&sk).expect("signing");

        let original_sig = advert.sig.clone();

        // Round-trip through CBOR
        let cbor_bytes = advert.to_cbor().expect("CBOR serialization");
        let decoded = CoordinatorAdvert::from_cbor(&cbor_bytes).expect("CBOR deserialization");

        // Signature should be preserved
        assert_eq!(decoded.sig, original_sig, "Signature must be preserved");

        // Signature should still verify
        let valid = decoded.verify(&pk).expect("verification");
        assert!(
            valid,
            "Signature should still be valid after CBOR round-trip"
        );
    }

    /// Test to_bytes and from_bytes use CBOR
    #[test]
    fn test_bytes_methods_use_cbor() {
        let peer = PeerId::new([9u8; 32]);
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Symmetric,
            3_000,
        );

        let bytes = advert.to_bytes().expect("to_bytes");
        let cbor_bytes = advert.to_cbor().expect("to_cbor");

        // to_bytes should produce same output as to_cbor
        assert_eq!(
            bytes.as_ref(),
            cbor_bytes.as_slice(),
            "to_bytes must use CBOR serialization"
        );

        // from_bytes should work with CBOR
        let decoded = CoordinatorAdvert::from_bytes(&bytes).expect("from_bytes");
        assert_eq!(decoded.peer, peer);
    }
}
