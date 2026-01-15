//! Bootstrap flow for cold-start coordinator discovery
//!
//! Implements SPEC2 §7.4 bootstrap flow: cache → FOAF → connect

use crate::{CoordinatorHandler, FindCoordinatorQuery, GossipCacheAdapter};
use saorsa_gossip_types::PeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// Traversal method preference order per SPEC2 §7.4
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TraversalMethod {
    /// Direct connection (best, lowest cost)
    Direct = 0,
    /// Reflexive/punched path (moderate cost)
    Reflexive = 1,
    /// Relay (last resort, highest cost)
    Relay = 2,
}

/// Result of a successful bootstrap (found coordinator)
#[derive(Debug, Clone)]
pub struct BootstrapResult {
    /// Selected coordinator peer
    pub peer_id: PeerId,
    /// Address to connect to
    pub addr: SocketAddr,
    /// Traversal method to use
    pub method: TraversalMethod,
}

/// Action required after bootstrap attempt per SPEC2 §7.4
#[derive(Debug, Clone)]
pub enum BootstrapAction {
    /// Found coordinator in cache - can connect immediately
    Connect(BootstrapResult),
    /// Cache is cold - need to issue FOAF FIND_COORDINATOR query
    SendQuery(FindCoordinatorQuery),
    /// No action possible (no cache, no peers to query)
    NoAction,
}

/// Bootstrap coordinator for network entry
pub struct Bootstrap {
    /// Local peer ID
    peer_id: PeerId,
    /// Gossip cache adapter (wraps ant-quic BootstrapCache)
    cache: GossipCacheAdapter,
    /// Coordinator handler (for FOAF queries)
    handler: CoordinatorHandler,
    /// Pending FOAF queries (query_id → timestamp)
    pending_queries: Arc<Mutex<HashMap<[u8; 32], Instant>>>,
}

impl Bootstrap {
    /// Create a new bootstrap instance
    pub fn new(peer_id: PeerId, cache: GossipCacheAdapter, handler: CoordinatorHandler) -> Self {
        Self {
            peer_id,
            cache,
            handler,
            pending_queries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn pending_queries_guard(&self) -> Option<MutexGuard<'_, HashMap<[u8; 32], Instant>>> {
        match self.pending_queries.lock() {
            Ok(guard) => Some(guard),
            Err(e) => {
                tracing::warn!("Pending queries Mutex poisoned: {e}");
                None
            }
        }
    }

    /// Get a reference to the gossip cache adapter
    pub fn cache(&self) -> &GossipCacheAdapter {
        &self.cache
    }

    /// Get a reference to the coordinator handler
    ///
    /// The handler is used for processing FOAF queries and validating adverts.
    pub fn handler(&self) -> &CoordinatorHandler {
        &self.handler
    }

    /// Attempt to find a coordinator to bootstrap from
    ///
    /// Strategy per SPEC2 §7:
    /// 1. Check cache for coordinators (using epsilon-greedy selection)
    /// 2. If cache is cold, issue FOAF FIND_COORDINATOR
    /// 3. Select best coordinator by traversal preference
    ///
    /// Returns an action to take (Connect, SendQuery, or NoAction)
    pub async fn find_coordinator(&self) -> BootstrapAction {
        // Step 1: Try cache first using ant-quic's quality-based selection
        let coordinators = self.cache.select_coordinators(3).await;

        if !coordinators.is_empty() {
            if let Some(result) = self.select_best_coordinator(&coordinators) {
                return BootstrapAction::Connect(result);
            }
        }

        // Step 2: Cache is cold, issue FOAF query per SPEC2 §7.4
        let query = FindCoordinatorQuery::new(self.peer_id);

        // Track this query
        if let Some(mut pending) = self.pending_queries_guard() {
            pending.insert(query.query_id, Instant::now());
        }

        BootstrapAction::SendQuery(query)
    }

    /// Select the best coordinator based on traversal preference
    ///
    /// Preference order: Direct → Reflexive → Relay
    fn select_best_coordinator(
        &self,
        coordinators: &[(
            ant_quic::bootstrap_cache::CachedPeer,
            Option<crate::CoordinatorAdvert>,
        )],
    ) -> Option<BootstrapResult> {
        if coordinators.is_empty() {
            return None;
        }

        // Try each traversal method in preference order
        for method in [
            TraversalMethod::Direct,
            TraversalMethod::Reflexive,
            TraversalMethod::Relay,
        ] {
            for (cached_peer, advert_opt) in coordinators {
                if let Some(addr) =
                    self.get_addr_for_method(cached_peer, advert_opt.as_ref(), method)
                {
                    let peer_id = PeerId::new(cached_peer.peer_id.0);
                    return Some(BootstrapResult {
                        peer_id,
                        addr,
                        method,
                    });
                }
            }
        }

        None
    }

    /// Get an address for a specific traversal method per SPEC2 §7.4
    ///
    /// Traversal preference order:
    /// 1. Direct: Use addresses from cached peer (best performance, lowest cost)
    /// 2. Reflexive: Use addr_hints from advert (moderate cost)
    /// 3. Relay: Not yet implemented (requires relay peer lookup)
    fn get_addr_for_method(
        &self,
        cached_peer: &ant_quic::bootstrap_cache::CachedPeer,
        advert: Option<&crate::CoordinatorAdvert>,
        method: TraversalMethod,
    ) -> Option<SocketAddr> {
        match method {
            TraversalMethod::Direct => {
                // Direct connection via cached peer's addresses
                cached_peer.addresses.first().copied()
            }
            TraversalMethod::Reflexive => {
                // Reflexive connection via advert hints
                advert
                    .and_then(|a| a.addr_hints.first())
                    .map(|hint| hint.addr)
            }
            TraversalMethod::Relay => {
                // Relay connection: would need relay peer lookup
                // For now, try advert hints as fallback
                advert
                    .and_then(|a| a.addr_hints.get(1))
                    .map(|hint| hint.addr)
            }
        }
    }

    /// Handle a FOAF FIND_COORDINATOR response
    ///
    /// Processes coordinator adverts from response, updates cache, and returns connect action.
    /// Per SPEC2 §7.3, responses contain coordinator adverts that should be added to cache.
    ///
    /// # Security Note
    ///
    /// FOAF responses currently do not include public keys, so adverts cannot be
    /// cryptographically verified. Only adverts with non-empty signatures are accepted,
    /// providing basic structural validation. A future protocol enhancement should
    /// include public keys alongside adverts for full verification.
    pub async fn handle_find_response(
        &self,
        response: crate::FindCoordinatorResponse,
    ) -> Option<BootstrapAction> {
        // Remove from pending queries
        if let Some(mut pending) = self.pending_queries_guard() {
            pending.remove(&response.query_id);
        }

        // Add coordinator adverts to adapter cache with security logging
        // TODO(security): FOAF protocol should include public keys for signature verification
        let advert_count = response.adverts.len();
        let mut accepted = 0usize;
        let mut rejected = 0usize;

        for advert in response.adverts {
            // Basic structural validation: check for non-empty signature
            // This is NOT cryptographic verification (requires public key)
            if advert.sig.is_empty() {
                tracing::warn!(
                    peer = ?advert.peer,
                    "Rejecting FOAF advert: missing signature"
                );
                rejected += 1;
                continue;
            }

            // Check if advert is valid (not expired)
            if !advert.is_valid() {
                tracing::debug!(
                    peer = ?advert.peer,
                    "Rejecting FOAF advert: expired or invalid"
                );
                rejected += 1;
                continue;
            }

            // Insert with result logging (don't discard)
            if self.cache.insert_advert(advert.clone()).await {
                accepted += 1;
            } else {
                tracing::debug!(
                    peer = ?advert.peer,
                    "FOAF advert insertion returned false (duplicate or invalid)"
                );
            }
        }

        if advert_count > 0 {
            tracing::debug!(
                total = advert_count,
                accepted = accepted,
                rejected = rejected,
                "Processed FOAF response adverts (note: signature verification requires protocol enhancement)"
            );
        }

        // Also add to handler cache for FOAF queries
        let coordinators = self.cache.get_adverts_by_role(|a| a.roles.coordinator);

        self.select_best_from_adverts(&coordinators)
            .map(BootstrapAction::Connect)
    }

    /// Select best coordinator from coordinator adverts
    ///
    /// When selecting from FOAF response adverts, we only have addr_hints available
    /// (reflexive addresses). Direct addresses require CachedPeer data which isn't
    /// available in this context.
    fn select_best_from_adverts(
        &self,
        adverts: &[crate::CoordinatorAdvert],
    ) -> Option<BootstrapResult> {
        // From FOAF responses, we only have addr_hints (reflexive addresses)
        // Find first advert with a usable address hint
        adverts.iter().find_map(|advert| {
            advert.addr_hints.first().map(|addr_hint| BootstrapResult {
                peer_id: advert.peer,
                addr: addr_hint.addr,
                // addr_hints are reflexive addresses per SPEC2
                method: TraversalMethod::Reflexive,
            })
        })
    }

    /// Clean up expired pending queries
    ///
    /// Per SPEC2 §7.3, queries expire after 30 seconds.
    /// Returns the number of expired queries removed.
    pub fn prune_expired_queries(&self) -> usize {
        let Some(mut pending) = self.pending_queries_guard() else {
            return 0;
        };
        let now = Instant::now();

        let expired: Vec<_> = pending
            .iter()
            .filter(|(_, timestamp)| now.duration_since(**timestamp).as_secs() > 30)
            .map(|(query_id, _)| *query_id)
            .collect();

        let count = expired.len();
        for query_id in expired {
            pending.remove(&query_id);
        }

        count
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{AddrHint, CoordinatorAdvert, NatClass, PeerRoles};
    use ant_quic::bootstrap_cache::{BootstrapCache, BootstrapCacheConfig};
    use std::sync::Arc;

    async fn create_test_cache() -> GossipCacheAdapter {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config = BootstrapCacheConfig::builder()
            .cache_dir(temp_dir.path().to_path_buf())
            .build();
        let cache = Arc::new(BootstrapCache::open(config).await.expect("create cache"));
        GossipCacheAdapter::new(cache)
    }

    #[test]
    fn test_traversal_method_ordering() {
        assert!(TraversalMethod::Direct < TraversalMethod::Reflexive);
        assert!(TraversalMethod::Reflexive < TraversalMethod::Relay);
        assert!(TraversalMethod::Direct < TraversalMethod::Relay);
    }

    #[tokio::test]
    async fn test_bootstrap_creation() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);

        let bootstrap = Bootstrap::new(peer_id, cache, handler);
        assert_eq!(bootstrap.peer_id, peer_id);
    }

    #[tokio::test]
    async fn test_find_coordinator_empty_cache() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);

        let bootstrap = Bootstrap::new(peer_id, cache, handler);
        let action = bootstrap.find_coordinator().await;

        // Empty cache should trigger FOAF query per SPEC2 §7.4
        match action {
            BootstrapAction::SendQuery(query) => {
                assert_eq!(query.origin, peer_id, "Query origin should be local peer");
                assert_eq!(query.ttl, 3, "TTL should be 3 per SPEC2 §7.3");
            }
            _ => panic!("Expected SendQuery action for empty cache"),
        }
    }

    #[tokio::test]
    async fn test_find_coordinator_from_cache() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);

        // Add a coordinator to cache
        let coord_peer = PeerId::new([2u8; 32]);
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("valid address");
        let advert = CoordinatorAdvert::new(
            coord_peer,
            PeerRoles {
                coordinator: true,
                reflector: true,
                rendezvous: false,
                relay: false,
            },
            vec![AddrHint::new(addr)],
            NatClass::Eim,
            60_000,
        );
        cache.insert_advert(advert).await;

        let bootstrap = Bootstrap::new(peer_id, cache, handler);
        let action = bootstrap.find_coordinator().await;

        // Warm cache should return Connect action
        match action {
            BootstrapAction::Connect(result) => {
                assert_eq!(result.peer_id, coord_peer);
                assert_eq!(result.addr, addr);
                assert_eq!(result.method, TraversalMethod::Direct);
            }
            _ => panic!("Expected Connect action for warm cache"),
        }
    }

    #[tokio::test]
    async fn test_traversal_preference_direct_first() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);

        let coord = PeerId::new([2u8; 32]);
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("valid");

        let advert = CoordinatorAdvert::new(
            coord,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
            vec![AddrHint::new(addr)],
            NatClass::Eim,
            60_000,
        );
        cache.insert_advert(advert).await;

        let bootstrap = Bootstrap::new(peer_id, cache, handler);
        let action = bootstrap.find_coordinator().await;

        match action {
            BootstrapAction::Connect(result) => {
                assert_eq!(
                    result.method,
                    TraversalMethod::Direct,
                    "Should prefer direct connection"
                );
            }
            _ => panic!("Expected Connect action"),
        }
    }

    #[test]
    fn test_bootstrap_result_creation() {
        let peer_id = PeerId::new([1u8; 32]);
        let addr: SocketAddr = "192.168.1.1:9000".parse().expect("valid");

        let result = BootstrapResult {
            peer_id,
            addr,
            method: TraversalMethod::Reflexive,
        };

        assert_eq!(result.peer_id, peer_id);
        assert_eq!(result.addr, addr);
        assert_eq!(result.method, TraversalMethod::Reflexive);
    }

    /// Test FOAF query is tracked in pending queries
    #[tokio::test]
    async fn test_foaf_query_is_tracked() {
        let peer_id = PeerId::new([10u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);

        let bootstrap = Bootstrap::new(peer_id, cache, handler);

        // Empty cache triggers FOAF query
        let action = bootstrap.find_coordinator().await;

        match action {
            BootstrapAction::SendQuery(query) => {
                // Query should be tracked
                let pending = bootstrap.pending_queries.lock().expect("lock");
                assert!(
                    pending.contains_key(&query.query_id),
                    "Query should be tracked"
                );
            }
            _ => panic!("Expected SendQuery action"),
        }
    }

    /// Test handling FOAF query response
    #[tokio::test]
    async fn test_handle_foaf_response() {
        use crate::{CoordinatorRoles, FindCoordinatorResponse};
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let peer_id = PeerId::new([11u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);
        let bootstrap = Bootstrap::new(peer_id, cache, handler);

        // Issue query first
        let action = bootstrap.find_coordinator().await;
        let query_id = match action {
            BootstrapAction::SendQuery(query) => query.query_id,
            _ => panic!("Expected SendQuery"),
        };

        // Create a response with a coordinator advert
        let coord_peer = PeerId::new([12u8; 32]);
        let addr: SocketAddr = "10.0.0.1:8080".parse().expect("valid addr");

        let mut advert = CoordinatorAdvert::new(
            coord_peer,
            CoordinatorRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
            vec![AddrHint::new(addr)],
            NatClass::Eim,
            60_000,
        );

        // Sign the advert
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");
        advert.sign(&sk).expect("signing");

        let response = FindCoordinatorResponse::new(query_id, peer_id, vec![advert]);

        // Handle the response
        let result_action = bootstrap
            .handle_find_response(response)
            .await
            .expect("should return action");

        // Should return Connect action with coordinator
        match result_action {
            BootstrapAction::Connect(result) => {
                assert_eq!(result.peer_id, coord_peer);
                assert_eq!(result.addr, addr);
            }
            _ => panic!("Expected Connect action after response"),
        }

        // Query should be removed from pending
        let pending = bootstrap.pending_queries.lock().expect("lock");
        assert!(
            !pending.contains_key(&query_id),
            "Query should be removed after response"
        );
    }

    /// Test query timeout pruning
    #[tokio::test]
    async fn test_prune_expired_queries() {
        use std::time::Duration;

        let peer_id = PeerId::new([13u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);
        let bootstrap = Bootstrap::new(peer_id, cache, handler);

        // Create a query
        let _ = bootstrap.find_coordinator().await;

        // Manually expire it by manipulating timestamp
        {
            let mut pending = bootstrap.pending_queries.lock().expect("lock");
            if let Some((query_id, _)) = pending.iter().next() {
                let old_query_id = *query_id;
                pending.insert(old_query_id, Instant::now() - Duration::from_secs(35));
            }
        }

        // Prune should remove expired query
        let pruned = bootstrap.prune_expired_queries();
        assert_eq!(pruned, 1, "Should prune 1 expired query");

        let pending = bootstrap.pending_queries.lock().expect("lock");
        assert_eq!(pending.len(), 0, "No queries should remain");
    }

    /// Test BootstrapAction enum variants
    #[test]
    fn test_bootstrap_action_variants() {
        let peer_id = PeerId::new([14u8; 32]);
        let addr: SocketAddr = "1.2.3.4:5678".parse().expect("valid");

        // Test Connect variant
        let connect_action = BootstrapAction::Connect(BootstrapResult {
            peer_id,
            addr,
            method: TraversalMethod::Direct,
        });
        assert!(matches!(connect_action, BootstrapAction::Connect(_)));

        // Test SendQuery variant
        let query_action = BootstrapAction::SendQuery(FindCoordinatorQuery::new(peer_id));
        assert!(matches!(query_action, BootstrapAction::SendQuery(_)));

        // Test NoAction variant
        let no_action = BootstrapAction::NoAction;
        assert!(matches!(no_action, BootstrapAction::NoAction));
    }

    /// Test response with multiple coordinators selects best
    #[tokio::test]
    async fn test_response_with_multiple_coordinators() {
        use crate::{CoordinatorRoles, FindCoordinatorResponse};
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let peer_id = PeerId::new([15u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::new(peer_id);
        let bootstrap = Bootstrap::new(peer_id, cache, handler);

        // Issue query
        let action = bootstrap.find_coordinator().await;
        let query_id = match action {
            BootstrapAction::SendQuery(query) => query.query_id,
            _ => panic!("Expected SendQuery"),
        };

        // Create response with 3 coordinators
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");

        let mut adverts = vec![];
        for i in 0..3 {
            let coord_peer = PeerId::new([16 + i; 32]);
            let addr: SocketAddr = format!("10.0.0.{}:8080", i + 1)
                .parse()
                .expect("valid addr");

            let mut advert = CoordinatorAdvert::new(
                coord_peer,
                CoordinatorRoles {
                    coordinator: true,
                    reflector: false,
                    rendezvous: false,
                    relay: false,
                },
                vec![AddrHint::new(addr)],
                NatClass::Eim,
                60_000,
            );
            advert.sign(&sk).expect("signing");
            adverts.push(advert);
        }

        let response = FindCoordinatorResponse::new(query_id, peer_id, adverts);

        // Should select the first coordinator (simplest traversal logic)
        let result_action = bootstrap
            .handle_find_response(response)
            .await
            .expect("should return action");

        match result_action {
            BootstrapAction::Connect(result) => {
                // Just verify we got a coordinator back
                assert!(result.addr.port() >= 8080);
            }
            _ => panic!("Expected Connect action"),
        }
    }
}
