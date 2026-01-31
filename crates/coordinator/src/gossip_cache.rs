//! Gossip cache adapter for ant-quic BootstrapCache integration
//!
//! This module provides [`GossipCacheAdapter`], which wraps ant-quic's `BootstrapCache`
//! and extends it with gossip-specific data (coordinator adverts with signatures, scores, etc.).
//!
//! ## Ownership Pattern
//!
//! The recommended pattern is app-owned, shared:
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
//! ## Example
//!
//! ```rust,ignore
//! use ant_quic::bootstrap_cache::{BootstrapCache, BootstrapCacheConfig};
//! use saorsa_gossip_coordinator::GossipCacheAdapter;
//! use std::sync::Arc;
//!
//! // App creates and owns the bootstrap cache
//! let config = BootstrapCacheConfig::default();
//! let cache = Arc::new(BootstrapCache::open(config).await?);
//!
//! // Gossip layer wraps it with adapter
//! let gossip_cache = GossipCacheAdapter::new(cache);
//!
//! // Use for coordinator discovery
//! let coordinators = gossip_cache.select_coordinators(3).await;
//! ```

use crate::{CoordinatorAdvert, NatClass, PeerRoles};
use ant_quic::bootstrap_cache::{BootstrapCache, CachedPeer, NatType, PeerCapabilities};
use saorsa_gossip_types::PeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::debug;

/// Adapter that wraps ant-quic's `BootstrapCache` with gossip-specific extensions.
///
/// This adapter provides:
/// - Access to ant-quic's peer selection and quality scoring
/// - Storage for gossip-specific `CoordinatorAdvert` data (signatures, expiry, roles)
/// - Unified API for coordinator lookup combining both data sources
///
/// ## Thread Safety
///
/// The adapter is thread-safe and can be shared across async tasks via `Arc`.
/// Internal state is protected by `RwLock` for concurrent read access.
#[derive(Clone)]
pub struct GossipCacheAdapter {
    /// Underlying ant-quic bootstrap cache (app-owned)
    cache: Arc<BootstrapCache>,
    /// Gossip-specific coordinator adverts
    adverts: Arc<RwLock<HashMap<PeerId, CoordinatorAdvert>>>,
}

impl GossipCacheAdapter {
    /// Create a new gossip cache adapter wrapping an existing bootstrap cache.
    ///
    /// # Arguments
    /// * `cache` - Arc-wrapped ant-quic BootstrapCache owned by the application
    ///
    /// # Example
    /// ```rust,ignore
    /// let cache = Arc::new(BootstrapCache::open(config).await?);
    /// let adapter = GossipCacheAdapter::new(cache);
    /// ```
    pub fn new(cache: Arc<BootstrapCache>) -> Self {
        Self {
            cache,
            adverts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn adverts_read(&self) -> Option<RwLockReadGuard<'_, HashMap<PeerId, CoordinatorAdvert>>> {
        match self.adverts.read() {
            Ok(guard) => Some(guard),
            Err(e) => {
                tracing::warn!("Adverts RwLock poisoned (read): {e}");
                None
            }
        }
    }

    fn adverts_write(&self) -> Option<RwLockWriteGuard<'_, HashMap<PeerId, CoordinatorAdvert>>> {
        match self.adverts.write() {
            Ok(guard) => Some(guard),
            Err(e) => {
                tracing::warn!("Adverts RwLock poisoned (write): {e}");
                None
            }
        }
    }

    /// Get the underlying ant-quic bootstrap cache.
    ///
    /// Use this for direct access to ant-quic's peer selection and maintenance APIs.
    pub fn bootstrap_cache(&self) -> &Arc<BootstrapCache> {
        &self.cache
    }

    /// Insert a coordinator advert.
    ///
    /// This stores the full advert (including signature) in the gossip layer,
    /// and also updates the underlying ant-quic cache with peer capabilities.
    ///
    /// # Arguments
    /// * `advert` - The coordinator advert to store
    ///
    /// # Returns
    /// `true` if the advert was inserted, `false` if invalid or expired
    pub async fn insert_advert(&self, advert: CoordinatorAdvert) -> bool {
        if !advert.is_valid() {
            debug!(peer = ?advert.peer, "Rejecting expired advert");
            return false;
        }

        let peer_id = advert.peer;

        // Store in gossip-specific map
        if let Some(mut adverts) = self.adverts_write() {
            adverts.insert(peer_id, advert.clone());
        } else {
            return false;
        }

        // Also update ant-quic cache with peer info
        let addresses: Vec<SocketAddr> = advert.addr_hints.iter().map(|h| h.addr).collect();

        let ant_peer_id = gossip_to_ant_peer_id(&peer_id);
        self.cache
            .add_from_connection(ant_peer_id, addresses, None)
            .await;

        debug!(peer = ?peer_id, "Inserted coordinator advert (hints only)");
        true
    }

    /// Get a coordinator advert by peer ID.
    ///
    /// Returns the full advert including signature and gossip-specific fields.
    /// Returns `None` if the peer is not found or the advert has expired.
    pub fn get_advert(&self, peer_id: &PeerId) -> Option<CoordinatorAdvert> {
        self.adverts_read()
            .and_then(|adverts| adverts.get(peer_id).cloned())
            .filter(|advert| advert.is_valid())
    }

    /// Get all valid coordinator adverts.
    ///
    /// Returns adverts that have not expired, sorted by score (highest first).
    pub fn get_all_adverts(&self) -> Vec<CoordinatorAdvert> {
        let adverts = match self.adverts_read() {
            Some(adverts) => adverts,
            None => return Vec::new(),
        };

        let mut valid: Vec<_> = adverts
            .values()
            .filter(|advert| advert.is_valid())
            .cloned()
            .collect();

        // Sort by score descending
        valid.sort_by(|a, b| b.score.cmp(&a.score));
        valid
    }

    /// Get adverts filtered by role.
    ///
    /// # Arguments
    /// * `role_filter` - Predicate to filter adverts by role
    pub fn get_adverts_by_role(
        &self,
        role_filter: impl Fn(&CoordinatorAdvert) -> bool,
    ) -> Vec<CoordinatorAdvert> {
        let adverts = match self.adverts_read() {
            Some(adverts) => adverts,
            None => return Vec::new(),
        };

        adverts
            .values()
            .filter(|advert| advert.is_valid() && role_filter(advert))
            .cloned()
            .collect()
    }

    /// Select coordinators using ant-quic's quality-based selection.
    ///
    /// This uses ant-quic's epsilon-greedy strategy for peer selection,
    /// then enriches results with gossip-specific advert data.
    ///
    /// # Arguments
    /// * `count` - Number of coordinators to select
    ///
    /// # Returns
    /// Vector of `(CachedPeer, Option<CoordinatorAdvert>)` tuples.
    /// The advert is `Some` if gossip-specific data is available.
    pub async fn select_coordinators(
        &self,
        count: usize,
    ) -> Vec<(CachedPeer, Option<CoordinatorAdvert>)> {
        let peers = self.cache.select_coordinators(count).await;

        peers
            .into_iter()
            .map(|peer| {
                let gossip_peer_id = ant_to_gossip_peer_id(&peer.peer_id);
                let advert = self.get_advert(&gossip_peer_id);
                (peer, advert)
            })
            .collect()
    }

    /// Select relay peers using ant-quic's relay selection.
    ///
    /// # Arguments
    /// * `count` - Number of relays to select
    pub async fn select_relays(
        &self,
        count: usize,
    ) -> Vec<(CachedPeer, Option<CoordinatorAdvert>)> {
        let peers = self.cache.select_relay_peers(count).await;

        peers
            .into_iter()
            .map(|peer| {
                let gossip_peer_id = ant_to_gossip_peer_id(&peer.peer_id);
                let advert = self.get_advert(&gossip_peer_id);
                (peer, advert)
            })
            .collect()
    }

    /// Select peers for bootstrap using ant-quic's epsilon-greedy strategy.
    ///
    /// # Arguments
    /// * `count` - Number of peers to select
    pub async fn select_peers(&self, count: usize) -> Vec<CachedPeer> {
        self.cache.select_peers(count).await
    }

    /// Record a successful connection to a peer.
    ///
    /// Updates the ant-quic cache statistics.
    pub async fn record_success(&self, peer_id: &PeerId, rtt_ms: u32) {
        let ant_peer_id = gossip_to_ant_peer_id(peer_id);
        self.cache.record_success(&ant_peer_id, rtt_ms).await;
    }

    /// Record a failed connection attempt.
    pub async fn record_failure(&self, peer_id: &PeerId) {
        let ant_peer_id = gossip_to_ant_peer_id(peer_id);
        self.cache.record_failure(&ant_peer_id).await;
    }

    /// Get a cached peer from ant-quic by ID.
    pub async fn get_peer(&self, peer_id: &PeerId) -> Option<CachedPeer> {
        let ant_peer_id = gossip_to_ant_peer_id(peer_id);
        self.cache.get_peer(&ant_peer_id).await
    }

    /// Check if a peer exists in the cache.
    pub async fn contains(&self, peer_id: &PeerId) -> bool {
        let ant_peer_id = gossip_to_ant_peer_id(peer_id);
        self.cache.contains(&ant_peer_id).await
    }

    /// Get the number of peers in the ant-quic cache.
    pub async fn peer_count(&self) -> usize {
        self.cache.peer_count().await
    }

    /// Get the number of adverts stored.
    pub fn advert_count(&self) -> usize {
        self.adverts_read().map(|a| a.len()).unwrap_or(0)
    }

    /// Prune expired adverts from the gossip layer.
    ///
    /// Returns the number of adverts removed.
    pub fn prune_expired_adverts(&self) -> usize {
        let mut adverts = match self.adverts_write() {
            Some(adverts) => adverts,
            None => return 0,
        };

        let to_remove: Vec<_> = adverts
            .iter()
            .filter(|(_, advert)| !advert.is_valid())
            .map(|(peer_id, _)| *peer_id)
            .collect();

        let count = to_remove.len();
        for peer_id in to_remove {
            adverts.remove(&peer_id);
        }

        if count > 0 {
            debug!(pruned = count, "Pruned expired adverts");
        }

        count
    }

    /// Remove a peer from both caches.
    pub async fn remove(&self, peer_id: &PeerId) {
        // Remove from ant-quic cache
        let ant_peer_id = gossip_to_ant_peer_id(peer_id);
        let _ = self.cache.remove(&ant_peer_id).await;

        // Remove from adverts
        if let Some(mut adverts) = self.adverts_write() {
            adverts.remove(peer_id);
        }
    }

    /// Clear all adverts (but not the underlying ant-quic cache).
    pub fn clear_adverts(&self) {
        if let Some(mut adverts) = self.adverts_write() {
            adverts.clear();
        }
    }
}

/// Convert gossip PeerId to ant-quic PeerId.
fn gossip_to_ant_peer_id(peer_id: &PeerId) -> ant_quic::PeerId {
    ant_quic::PeerId(*peer_id.as_bytes())
}

/// Convert ant-quic PeerId to gossip PeerId.
fn ant_to_gossip_peer_id(peer_id: &ant_quic::PeerId) -> PeerId {
    PeerId::new(peer_id.0)
}

/// Convert gossip PeerRoles to ant-quic PeerCapabilities.
///
/// Note: With the "hints only" approach (v0.4.2+), capabilities are no longer
/// set from adverts. This function is retained for tests and potential future use.
#[cfg(test)]
fn roles_to_capabilities(roles: &PeerRoles, nat_class: &NatClass) -> PeerCapabilities {
    PeerCapabilities {
        supports_relay: roles.relay,
        supports_coordination: roles.coordinator || roles.rendezvous,
        protocols: Default::default(),
        nat_type: Some(nat_class_to_nat_type(nat_class)),
        external_addresses: Vec::new(),
    }
}

/// Convert gossip NatClass to ant-quic NatType.
///
/// Note: With the "hints only" approach (v0.4.2+), this is primarily used for tests.
#[cfg(test)]
fn nat_class_to_nat_type(nat_class: &NatClass) -> NatType {
    match nat_class {
        NatClass::Eim => NatType::FullCone, // EIM maps to FullCone (easiest)
        NatClass::Edm => NatType::AddressRestrictedCone, // EDM maps to Address Restricted
        NatClass::Symmetric => NatType::Symmetric,
        NatClass::Unknown => NatType::Unknown,
    }
}

/// Convert ant-quic NatType to gossip NatClass.
pub fn nat_type_to_nat_class(nat_type: &NatType) -> NatClass {
    match nat_type {
        NatType::None | NatType::FullCone => NatClass::Eim,
        NatType::AddressRestrictedCone | NatType::PortRestrictedCone => NatClass::Edm,
        NatType::Symmetric => NatClass::Symmetric,
        NatType::Unknown => NatClass::Unknown,
    }
}

/// Convert ant-quic PeerCapabilities to gossip PeerRoles.
pub fn capabilities_to_roles(caps: &PeerCapabilities) -> PeerRoles {
    PeerRoles {
        coordinator: caps.supports_coordination,
        reflector: caps.supports_coordination, // Assume coordinators can reflect
        rendezvous: caps.supports_coordination,
        relay: caps.supports_relay,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::AddrHint;
    use ant_quic::bootstrap_cache::BootstrapCacheConfig;

    async fn create_test_cache() -> Arc<BootstrapCache> {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config = BootstrapCacheConfig::builder()
            .cache_dir(temp_dir.path().to_path_buf())
            .build();
        Arc::new(BootstrapCache::open(config).await.expect("create cache"))
    }

    #[tokio::test]
    async fn test_adapter_creation() {
        let cache = create_test_cache().await;
        let adapter = GossipCacheAdapter::new(cache.clone());

        assert_eq!(adapter.peer_count().await, 0);
        assert_eq!(adapter.advert_count(), 0);
    }

    #[tokio::test]
    async fn test_insert_and_get_advert() {
        let cache = create_test_cache().await;
        let adapter = GossipCacheAdapter::new(cache);

        let peer_id = PeerId::new([1u8; 32]);
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("valid");

        let advert = CoordinatorAdvert::new(
            peer_id,
            PeerRoles {
                coordinator: true,
                reflector: true,
                rendezvous: false,
                relay: false,
            },
            vec![AddrHint::new(addr)],
            NatClass::Eim,
            10_000, // 10 second validity
        );

        assert!(adapter.insert_advert(advert.clone()).await);
        assert_eq!(adapter.advert_count(), 1);

        let retrieved = adapter.get_advert(&peer_id).expect("should find advert");
        assert_eq!(retrieved.peer, peer_id);
        assert!(retrieved.roles.coordinator);
    }

    #[tokio::test]
    async fn test_reject_expired_advert() {
        let cache = create_test_cache().await;
        let adapter = GossipCacheAdapter::new(cache);

        let peer_id = PeerId::new([2u8; 32]);

        let advert = CoordinatorAdvert::new(
            peer_id,
            PeerRoles::default(),
            vec![],
            NatClass::Unknown,
            1, // 1ms validity - will expire immediately
        );

        // Sleep to ensure expiry
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        assert!(!adapter.insert_advert(advert).await);
        assert_eq!(adapter.advert_count(), 0);
    }

    #[tokio::test]
    async fn test_get_adverts_by_role() {
        let cache = create_test_cache().await;
        let adapter = GossipCacheAdapter::new(cache);

        // Insert coordinator
        let coord_peer = PeerId::new([3u8; 32]);
        let coord_advert = CoordinatorAdvert::new(
            coord_peer,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
            vec![],
            NatClass::Eim,
            10_000,
        );
        adapter.insert_advert(coord_advert).await;

        // Insert relay
        let relay_peer = PeerId::new([4u8; 32]);
        let relay_advert = CoordinatorAdvert::new(
            relay_peer,
            PeerRoles {
                coordinator: false,
                reflector: false,
                rendezvous: false,
                relay: true,
            },
            vec![],
            NatClass::Edm,
            10_000,
        );
        adapter.insert_advert(relay_advert).await;

        // Get coordinators
        let coordinators = adapter.get_adverts_by_role(|a| a.roles.coordinator);
        assert_eq!(coordinators.len(), 1);
        assert_eq!(coordinators[0].peer, coord_peer);

        // Get relays
        let relays = adapter.get_adverts_by_role(|a| a.roles.relay);
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].peer, relay_peer);
    }

    #[tokio::test]
    async fn test_prune_expired_adverts() {
        let cache = create_test_cache().await;
        let adapter = GossipCacheAdapter::new(cache);

        // Insert short-lived advert
        let peer1 = PeerId::new([5u8; 32]);
        let short_advert = CoordinatorAdvert::new(
            peer1,
            PeerRoles::default(),
            vec![],
            NatClass::Unknown,
            50, // 50ms validity
        );
        adapter.insert_advert(short_advert).await;

        // Insert long-lived advert
        let peer2 = PeerId::new([6u8; 32]);
        let long_advert = CoordinatorAdvert::new(
            peer2,
            PeerRoles::default(),
            vec![],
            NatClass::Unknown,
            60_000, // 60 second validity
        );
        adapter.insert_advert(long_advert).await;

        assert_eq!(adapter.advert_count(), 2);

        // Wait for short one to expire
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let pruned = adapter.prune_expired_adverts();
        assert_eq!(pruned, 1);
        assert_eq!(adapter.advert_count(), 1);

        // Long-lived one should still be there
        assert!(adapter.get_advert(&peer2).is_some());
        assert!(adapter.get_advert(&peer1).is_none());
    }

    #[tokio::test]
    async fn test_remove_peer() {
        let cache = create_test_cache().await;
        let adapter = GossipCacheAdapter::new(cache);

        let peer_id = PeerId::new([7u8; 32]);
        let advert =
            CoordinatorAdvert::new(peer_id, PeerRoles::default(), vec![], NatClass::Eim, 10_000);
        adapter.insert_advert(advert).await;

        assert!(adapter.get_advert(&peer_id).is_some());

        adapter.remove(&peer_id).await;

        assert!(adapter.get_advert(&peer_id).is_none());
    }

    #[test]
    fn test_nat_class_conversion() {
        assert!(matches!(
            nat_class_to_nat_type(&NatClass::Eim),
            NatType::FullCone
        ));
        assert!(matches!(
            nat_class_to_nat_type(&NatClass::Edm),
            NatType::AddressRestrictedCone
        ));
        assert!(matches!(
            nat_class_to_nat_type(&NatClass::Symmetric),
            NatType::Symmetric
        ));
        assert!(matches!(
            nat_class_to_nat_type(&NatClass::Unknown),
            NatType::Unknown
        ));
    }

    #[test]
    fn test_nat_type_to_class_conversion() {
        assert!(matches!(
            nat_type_to_nat_class(&NatType::None),
            NatClass::Eim
        ));
        assert!(matches!(
            nat_type_to_nat_class(&NatType::FullCone),
            NatClass::Eim
        ));
        assert!(matches!(
            nat_type_to_nat_class(&NatType::AddressRestrictedCone),
            NatClass::Edm
        ));
        assert!(matches!(
            nat_type_to_nat_class(&NatType::PortRestrictedCone),
            NatClass::Edm
        ));
        assert!(matches!(
            nat_type_to_nat_class(&NatType::Symmetric),
            NatClass::Symmetric
        ));
        assert!(matches!(
            nat_type_to_nat_class(&NatType::Unknown),
            NatClass::Unknown
        ));
    }

    #[test]
    fn test_roles_to_capabilities() {
        let roles = PeerRoles {
            coordinator: true,
            reflector: true,
            rendezvous: false,
            relay: true,
        };

        let caps = roles_to_capabilities(&roles, &NatClass::Eim);

        assert!(caps.supports_relay);
        assert!(caps.supports_coordination);
        assert!(matches!(caps.nat_type, Some(NatType::FullCone)));
    }

    #[test]
    fn test_capabilities_to_roles() {
        let caps = PeerCapabilities {
            supports_relay: true,
            supports_coordination: true,
            protocols: Default::default(),
            nat_type: Some(NatType::AddressRestrictedCone),
            external_addresses: Vec::new(),
        };

        let roles = capabilities_to_roles(&caps);

        assert!(roles.coordinator);
        assert!(roles.relay);
        assert!(roles.rendezvous);
    }

    #[test]
    fn test_peer_id_conversion_roundtrip() {
        let gossip_id = PeerId::new([42u8; 32]);
        let ant_id = gossip_to_ant_peer_id(&gossip_id);
        let back = ant_to_gossip_peer_id(&ant_id);

        assert_eq!(gossip_id, back);
    }
}
