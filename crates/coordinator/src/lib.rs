#![warn(missing_docs)]

//! Coordinator advertisements for seedless bootstrap
//!
//! Implements SPEC2 §8 Coordinator Adverts for NAT traversal and seedless discovery.
//!
//! ## Overview
//!
//! Coordinators are self-elected nodes that *attempt* to provide:
//! - **Bootstrap**: Seedless network entry
//! - **Reflection**: Address observation for NAT detection
//! - **Rendezvous**: Connection coordination for hole punching
//! - **Relay**: Last-resort message forwarding
//!
//! Roles are **hints**, not trusted claims. Peers are selected based on
//! observed reachability and success rates ("measure, don't trust").
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

/// Transport capability hint for multi-transport peer discovery.
///
/// This struct advertises which transports a peer supports, enabling
/// other peers to discover BLE bridges, LoRa gateways, etc.
///
/// # Example
///
/// ```
/// use saorsa_gossip_coordinator::TransportHint;
///
/// // Advertise UDP/QUIC transport
/// let udp_hint = TransportHint::new("udp");
///
/// // Advertise BLE bridge with endpoint
/// let ble_hint = TransportHint::with_endpoint("ble", "AA:BB:CC:DD:EE:FF");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportHint {
    /// Transport type identifier (e.g., "udp", "ble", "lora")
    pub transport_type: String,
    /// Optional endpoint or connection info for this transport
    pub endpoint: Option<String>,
    /// Whether this transport is currently available
    pub available: bool,
}

impl TransportHint {
    /// Create a new transport hint with the given type, marked as available.
    ///
    /// # Arguments
    ///
    /// * `transport_type` - Transport type identifier (e.g., "udp", "ble", "lora")
    #[must_use]
    pub fn new(transport_type: impl Into<String>) -> Self {
        Self {
            transport_type: transport_type.into(),
            endpoint: None,
            available: true,
        }
    }

    /// Create a new transport hint with an endpoint.
    ///
    /// # Arguments
    ///
    /// * `transport_type` - Transport type identifier
    /// * `endpoint` - Connection info (e.g., BLE MAC address, LoRa device ID)
    #[must_use]
    pub fn with_endpoint(transport_type: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            transport_type: transport_type.into(),
            endpoint: Some(endpoint.into()),
            available: true,
        }
    }

    /// Mark this transport as unavailable.
    #[must_use]
    pub fn unavailable(mut self) -> Self {
        self.available = false;
        self
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
    /// Transport capability hints for multi-transport discovery.
    ///
    /// Advertises which transports this peer supports (UDP, BLE, LoRa, etc.)
    /// to enable transport-aware routing and BLE bridge discovery.
    #[serde(default)]
    pub transport_hints: Vec<TransportHint>,
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
            transport_hints: Vec::new(),
            sig: Vec::new(), // Will be filled by sign()
        }
    }

    /// Add transport hints to this advert (builder pattern).
    ///
    /// # Arguments
    ///
    /// * `hints` - Transport capability hints to include
    ///
    /// # Example
    ///
    /// ```
    /// use saorsa_gossip_coordinator::{CoordinatorAdvert, CoordinatorRoles, NatClass, TransportHint};
    /// use saorsa_gossip_types::PeerId;
    ///
    /// let advert = CoordinatorAdvert::new(
    ///     PeerId::new([1u8; 32]),
    ///     CoordinatorRoles::default(),
    ///     vec![],
    ///     NatClass::Unknown,
    ///     3600_000,
    /// )
    /// .with_transport_hints(vec![
    ///     TransportHint::new("udp"),
    ///     TransportHint::with_endpoint("ble", "AA:BB:CC:DD:EE:FF"),
    /// ]);
    /// ```
    #[must_use]
    pub fn with_transport_hints(mut self, hints: Vec<TransportHint>) -> Self {
        self.transport_hints = hints;
        self
    }

    /// Add a single transport hint to this advert.
    pub fn add_transport_hint(&mut self, hint: TransportHint) {
        self.transport_hints.push(hint);
    }

    /// Check if this peer supports a given transport type.
    ///
    /// # Arguments
    ///
    /// * `transport_type` - The transport type to check (e.g., "udp", "ble", "lora")
    ///
    /// # Returns
    ///
    /// `true` if a matching, available transport hint exists.
    #[must_use]
    pub fn has_transport(&self, transport_type: &str) -> bool {
        self.transport_hints
            .iter()
            .any(|h| h.transport_type == transport_type && h.available)
    }

    /// Get all available transports advertised by this peer.
    ///
    /// # Returns
    ///
    /// References to all transport hints marked as available.
    #[must_use]
    pub fn available_transports(&self) -> Vec<&TransportHint> {
        self.transport_hints
            .iter()
            .filter(|h| h.available)
            .collect()
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
                transport_hints: &self.transport_hints,
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
                transport_hints: &self.transport_hints,
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
    transport_hints: &'a Vec<TransportHint>,
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
        let postcard_bytes = postcard::to_stdvec(&advert).expect("postcard serialization");

        // CBOR and postcard MUST produce different wire formats
        assert_ne!(
            cbor_bytes, postcard_bytes,
            "CBOR must not be postcard - wire formats differ"
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

    // ========================================================================
    // TransportHint Tests
    // ========================================================================

    #[test]
    fn test_transport_hint_new() {
        let hint = TransportHint::new("udp");
        assert_eq!(hint.transport_type, "udp");
        assert!(hint.endpoint.is_none());
        assert!(hint.available);
    }

    #[test]
    fn test_transport_hint_with_endpoint() {
        let hint = TransportHint::with_endpoint("ble", "AA:BB:CC:DD:EE:FF");
        assert_eq!(hint.transport_type, "ble");
        assert_eq!(hint.endpoint, Some("AA:BB:CC:DD:EE:FF".to_string()));
        assert!(hint.available);
    }

    #[test]
    fn test_transport_hint_unavailable() {
        let hint = TransportHint::new("lora").unavailable();
        assert_eq!(hint.transport_type, "lora");
        assert!(!hint.available);
    }

    #[test]
    fn test_advert_with_transport_hints() {
        let peer = PeerId::new([10u8; 32]);
        let hints = vec![
            TransportHint::new("udp"),
            TransportHint::with_endpoint("ble", "AA:BB:CC:DD:EE:FF"),
        ];

        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            5_000,
        )
        .with_transport_hints(hints);

        assert_eq!(advert.transport_hints.len(), 2);
        assert_eq!(advert.transport_hints[0].transport_type, "udp");
        assert_eq!(advert.transport_hints[1].transport_type, "ble");
    }

    #[test]
    fn test_advert_add_transport_hint() {
        let peer = PeerId::new([11u8; 32]);
        let mut advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            5_000,
        );

        assert!(advert.transport_hints.is_empty());

        advert.add_transport_hint(TransportHint::new("udp"));
        assert_eq!(advert.transport_hints.len(), 1);

        advert.add_transport_hint(TransportHint::new("ble"));
        assert_eq!(advert.transport_hints.len(), 2);
    }

    #[test]
    fn test_advert_has_transport() {
        let peer = PeerId::new([12u8; 32]);
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            5_000,
        )
        .with_transport_hints(vec![
            TransportHint::new("udp"),
            TransportHint::new("ble"),
            TransportHint::new("lora").unavailable(),
        ]);

        assert!(advert.has_transport("udp"));
        assert!(advert.has_transport("ble"));
        assert!(!advert.has_transport("lora")); // unavailable
        assert!(!advert.has_transport("unknown"));
    }

    #[test]
    fn test_advert_available_transports() {
        let peer = PeerId::new([13u8; 32]);
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            5_000,
        )
        .with_transport_hints(vec![
            TransportHint::new("udp"),
            TransportHint::new("ble"),
            TransportHint::new("lora").unavailable(),
        ]);

        let available = advert.available_transports();
        assert_eq!(available.len(), 2);
        assert!(available.iter().any(|h| h.transport_type == "udp"));
        assert!(available.iter().any(|h| h.transport_type == "ble"));
        assert!(!available.iter().any(|h| h.transport_type == "lora"));
    }

    #[test]
    fn test_transport_hints_cbor_roundtrip() {
        let peer = PeerId::new([14u8; 32]);
        let hints = vec![
            TransportHint::new("udp"),
            TransportHint::with_endpoint("ble", "AA:BB:CC:DD:EE:FF"),
            TransportHint::new("lora").unavailable(),
        ];

        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Edm,
            5_000,
        )
        .with_transport_hints(hints);

        // Serialize and deserialize
        let cbor_bytes = advert.to_cbor().expect("CBOR serialization");
        let decoded = CoordinatorAdvert::from_cbor(&cbor_bytes).expect("CBOR deserialization");

        // Verify transport hints are preserved
        assert_eq!(decoded.transport_hints.len(), 3);
        assert_eq!(decoded.transport_hints[0], advert.transport_hints[0]);
        assert_eq!(decoded.transport_hints[1], advert.transport_hints[1]);
        assert_eq!(decoded.transport_hints[2], advert.transport_hints[2]);
    }

    #[test]
    fn test_transport_hints_backward_compat_empty() {
        // Test that adverts without transport_hints can still be deserialized
        // This simulates receiving an advert from an older peer
        let peer = PeerId::new([15u8; 32]);
        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Unknown,
            5_000,
        );

        // Advert has no transport hints
        assert!(advert.transport_hints.is_empty());

        // Serialize and deserialize
        let cbor_bytes = advert.to_cbor().expect("CBOR serialization");
        let decoded = CoordinatorAdvert::from_cbor(&cbor_bytes).expect("CBOR deserialization");

        // Empty transport hints should be preserved
        assert!(decoded.transport_hints.is_empty());
    }

    #[test]
    fn test_transport_hints_included_in_signature() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let peer = PeerId::new([16u8; 32]);
        let mut advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            10_000,
        )
        .with_transport_hints(vec![TransportHint::new("udp")]);

        // Sign the advert
        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");
        advert.sign(&sk).expect("signing");

        // Verify original
        let valid = advert.verify(&pk).expect("verification");
        assert!(valid, "Original should be valid");

        // Tamper with transport hints
        advert.transport_hints.push(TransportHint::new("ble"));

        // Verification should fail
        let valid = advert.verify(&pk).expect("verification");
        assert!(!valid, "Tampered advert should be invalid");
    }
}
