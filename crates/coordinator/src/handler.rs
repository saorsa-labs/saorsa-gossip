//! Coordinator advertisement handler
//!
//! Manages coordinator discovery and FOAF query routing

use crate::{CoordinatorAdvert, FindCoordinatorQuery, FindCoordinatorResponse, GossipCacheAdapter};
use saorsa_gossip_types::PeerId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// Handler for coordinator advertisements and FOAF queries
pub struct CoordinatorHandler {
    /// Local peer ID
    peer_id: PeerId,
    /// Cache for coordinator adverts (shared with gossip layer)
    cache: Option<GossipCacheAdapter>,
    /// Recently seen query IDs with timestamps (for deduplication and age-based pruning)
    seen_queries: Arc<Mutex<HashMap<[u8; 32], Instant>>>,
}

impl CoordinatorHandler {
    /// Create a new coordinator handler without a cache
    ///
    /// Use `with_cache()` to attach a GossipCacheAdapter for full functionality.
    pub fn new(peer_id: PeerId) -> Self {
        Self {
            peer_id,
            cache: None,
            seen_queries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a coordinator handler with a gossip cache adapter
    ///
    /// This allows the handler to store and query coordinator adverts.
    pub fn with_cache(peer_id: PeerId, cache: GossipCacheAdapter) -> Self {
        Self {
            peer_id,
            cache: Some(cache),
            seen_queries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach a gossip cache adapter to this handler
    pub fn set_cache(&mut self, cache: GossipCacheAdapter) {
        self.cache = Some(cache);
    }

    fn seen_queries_guard(&self) -> Option<MutexGuard<'_, HashMap<[u8; 32], Instant>>> {
        match self.seen_queries.lock() {
            Ok(guard) => Some(guard),
            Err(e) => {
                tracing::warn!("Seen queries Mutex poisoned: {e}");
                None
            }
        }
    }

    /// Get the local peer ID
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Get a reference to the gossip cache adapter
    pub fn cache(&self) -> Option<&GossipCacheAdapter> {
        self.cache.as_ref()
    }

    /// Handle receiving a coordinator advert
    ///
    /// Validates that the public key corresponds to the claimed peer ID,
    /// verifies the signature, and adds to cache if valid.
    ///
    /// # Security
    ///
    /// This method performs two critical checks:
    /// 1. **PeerId binding**: Verifies that `PeerId::from_pubkey(public_key) == advert.peer`
    /// 2. **Signature verification**: Verifies the ML-DSA signature using the public key
    ///
    /// Both checks must pass before the advert is trusted.
    pub async fn handle_advert(
        &self,
        advert: CoordinatorAdvert,
        public_key: &saorsa_pqc::MlDsaPublicKey,
    ) -> anyhow::Result<bool> {
        // SECURITY: Verify public key corresponds to claimed peer ID
        // This prevents an attacker from signing an advert claiming to be any peer
        let expected_peer_id = PeerId::from_pubkey(public_key.as_bytes());
        if expected_peer_id != advert.peer {
            tracing::warn!(
                claimed_peer = ?advert.peer,
                actual_peer = ?expected_peer_id,
                "Rejecting advert: public key does not match claimed peer ID"
            );
            return Ok(false);
        }

        // Verify signature
        let valid = advert.verify(public_key)?;
        if !valid {
            tracing::warn!(peer = ?advert.peer, "Rejecting advert: invalid signature");
            return Ok(false);
        }

        // Add to cache if available
        if let Some(cache) = &self.cache {
            Ok(cache.insert_advert(advert).await)
        } else {
            // No cache, but signature was valid
            Ok(true)
        }
    }

    /// Handle a FIND_COORDINATOR query
    ///
    /// Returns a response with known coordinators if query is valid.
    /// Returns None if query should not be answered (duplicate, expired, TTL=0, no cache).
    pub fn handle_find_query(
        &self,
        mut query: FindCoordinatorQuery,
    ) -> Option<FindCoordinatorResponse> {
        // Check if we've seen this query before
        {
            let mut seen = self.seen_queries_guard()?;
            if seen.contains_key(&query.query_id) {
                return None; // Duplicate query
            }
            seen.insert(query.query_id, Instant::now());
        }

        // Check if query is expired
        if query.is_expired() {
            return None;
        }

        // Decrement TTL
        if !query.decrement_ttl() {
            return None; // TTL exhausted
        }

        // Get coordinator adverts from cache (roles are hints only).
        let mut coordinators = self
            .cache
            .as_ref()
            .map(|c| c.get_all_adverts())
            .unwrap_or_default();

        coordinators.sort_by(|a, b| {
            let a_hint = a.roles.coordinator as u8
                + a.roles.relay as u8
                + a.roles.rendezvous as u8
                + a.roles.reflector as u8;
            let b_hint = b.roles.coordinator as u8
                + b.roles.relay as u8
                + b.roles.rendezvous as u8
                + b.roles.reflector as u8;

            b_hint.cmp(&a_hint).then_with(|| b.score.cmp(&a.score))
        });

        // Return response with known coordinators
        Some(FindCoordinatorResponse::new(
            query.query_id,
            self.peer_id,
            coordinators,
        ))
    }

    /// Prune expired adverts and old query IDs
    ///
    /// Returns the number of expired adverts pruned.
    /// Query IDs older than 30 seconds are removed (matching FOAF query expiry per SPEC2 §7.3).
    pub fn prune(&self) -> usize {
        let pruned = self
            .cache
            .as_ref()
            .map(|c| c.prune_expired_adverts())
            .unwrap_or(0);

        // Remove only query IDs older than 30 seconds (SPEC2 §7.3 query expiry)
        if let Some(mut seen) = self.seen_queries_guard() {
            let now = Instant::now();
            let max_age = std::time::Duration::from_secs(30);

            let to_remove: Vec<_> = seen
                .iter()
                .filter(|(_, timestamp)| now.duration_since(**timestamp) > max_age)
                .map(|(query_id, _)| *query_id)
                .collect();

            let queries_pruned = to_remove.len();
            for query_id in to_remove {
                seen.remove(&query_id);
            }

            if queries_pruned > 0 {
                tracing::debug!(pruned = queries_pruned, "Pruned expired query IDs");
            }
        }

        pruned
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{AddrHint, CoordinatorRoles, NatClass, PeerRoles};
    use ant_quic::bootstrap_cache::{BootstrapCache, BootstrapCacheConfig};
    use saorsa_pqc::{MlDsa65, MlDsaOperations};
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
    fn test_handler_creation() {
        let peer_id = PeerId::new([1u8; 32]);
        let handler = CoordinatorHandler::new(peer_id);

        assert_eq!(handler.peer_id(), peer_id);
        assert!(handler.cache().is_none());
    }

    #[tokio::test]
    async fn test_handler_with_cache() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        assert!(handler.cache().is_some());
    }

    #[tokio::test]
    async fn test_handle_valid_advert() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        // Create and sign an advert
        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");

        // SECURITY: PeerId must be derived from public key
        let coord_peer = PeerId::from_pubkey(pk.as_bytes());
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().expect("valid");
        let mut advert = CoordinatorAdvert::new(
            coord_peer,
            CoordinatorRoles::default(),
            vec![AddrHint::new(addr)],
            NatClass::Eim,
            10_000,
        );
        advert.sign(&sk).expect("signing");

        // Handle the advert
        let result = handler
            .handle_advert(advert, &pk)
            .await
            .expect("handle advert");
        assert!(result, "Valid advert should be accepted");
        assert_eq!(handler.cache().unwrap().advert_count(), 1);
    }

    #[tokio::test]
    async fn test_handle_mismatched_peer_id() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        // SECURITY TEST: Advert with PeerId that doesn't match public key must be rejected
        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");

        // Use a fake peer ID instead of deriving from public key
        let fake_peer = PeerId::new([99u8; 32]);
        let mut advert = CoordinatorAdvert::new(
            fake_peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            10_000,
        );
        advert.sign(&sk).expect("signing");

        // Should be rejected because PeerId doesn't match public key
        let result = handler
            .handle_advert(advert, &pk)
            .await
            .expect("handle advert");
        assert!(!result, "Advert with mismatched PeerId should be rejected");
        assert_eq!(handler.cache().unwrap().advert_count(), 0);
    }

    #[tokio::test]
    async fn test_handle_invalid_signature() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        // Create advert with PeerId derived from pk2 but signed with sk1
        let signer = MlDsa65::new();
        let (_, sk1) = signer.generate_keypair().expect("keypair 1");
        let (pk2, _) = signer.generate_keypair().expect("keypair 2");

        // Derive PeerId from pk2 to pass PeerId binding check
        let coord_peer = PeerId::from_pubkey(pk2.as_bytes());
        let mut advert = CoordinatorAdvert::new(
            coord_peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            10_000,
        );
        // Sign with wrong key (sk1 instead of sk2)
        advert.sign(&sk1).expect("signing");

        // Verify with pk2 - should fail signature check
        let result = handler
            .handle_advert(advert, &pk2)
            .await
            .expect("handle advert");
        assert!(!result, "Invalid signature should be rejected");
        assert_eq!(handler.cache().unwrap().advert_count(), 0);
    }

    #[tokio::test]
    async fn test_handle_find_query_with_no_coordinators() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        let origin = PeerId::new([2u8; 32]);
        let query = FindCoordinatorQuery::new(origin);

        let response = handler.handle_find_query(query).expect("should respond");

        assert_eq!(response.responder, peer_id);
        assert!(response.adverts.is_empty(), "No coordinators known yet");
    }

    #[tokio::test]
    async fn test_handle_find_query_with_coordinators() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        // Add a coordinator to cache
        let coord_peer = PeerId::new([2u8; 32]);
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().expect("valid");
        let advert = CoordinatorAdvert::new(
            coord_peer,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
            vec![AddrHint::new(addr)],
            NatClass::Eim,
            10_000,
        );
        handler.cache().unwrap().insert_advert(advert).await;

        // Query for coordinators
        let origin = PeerId::new([3u8; 32]);
        let query = FindCoordinatorQuery::new(origin);

        let response = handler.handle_find_query(query).expect("should respond");

        assert_eq!(response.responder, peer_id);
        assert_eq!(response.adverts.len(), 1, "Should return the coordinator");
        assert_eq!(response.adverts[0].peer, coord_peer);
    }

    #[tokio::test]
    async fn test_handle_duplicate_query() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        let origin = PeerId::new([2u8; 32]);
        let query = FindCoordinatorQuery::new(origin);
        let query_id = query.query_id;

        // First query should succeed
        let response1 = handler.handle_find_query(query.clone());
        assert!(response1.is_some(), "First query should get response");

        // Duplicate query should be ignored
        let response2 = handler.handle_find_query(query.clone());
        assert!(response2.is_none(), "Duplicate query should be ignored");

        // Same query_id should be ignored
        let mut duplicate = FindCoordinatorQuery::new(origin);
        duplicate.query_id = query_id;
        let response3 = handler.handle_find_query(duplicate);
        assert!(response3.is_none(), "Same query_id should be ignored");
    }

    #[test]
    fn test_handle_expired_query() {
        let peer_id = PeerId::new([1u8; 32]);
        let handler = CoordinatorHandler::new(peer_id);

        let origin = PeerId::new([2u8; 32]);
        let mut query = FindCoordinatorQuery::new(origin);

        // Make query expired
        query.created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64
            - 40_000; // 40 seconds ago

        let response = handler.handle_find_query(query);
        assert!(response.is_none(), "Expired query should be ignored");
    }

    #[test]
    fn test_handle_query_ttl_exhausted() {
        let peer_id = PeerId::new([1u8; 32]);
        let handler = CoordinatorHandler::new(peer_id);

        let origin = PeerId::new([2u8; 32]);
        let mut query = FindCoordinatorQuery::new(origin);

        // Exhaust TTL
        query.ttl = 0;

        let response = handler.handle_find_query(query);
        assert!(response.is_none(), "Query with TTL=0 should be ignored");
    }

    #[tokio::test]
    async fn test_prune() {
        let peer_id = PeerId::new([1u8; 32]);
        let cache = create_test_cache().await;
        let handler = CoordinatorHandler::with_cache(peer_id, cache);

        // Add short-lived advert
        let coord_peer = PeerId::new([2u8; 32]);
        let advert = CoordinatorAdvert::new(
            coord_peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            50, // 50ms validity
        );
        handler.cache().unwrap().insert_advert(advert).await;

        assert_eq!(handler.cache().unwrap().advert_count(), 1);

        // Wait for expiry
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Prune should remove expired advert
        let pruned = handler.prune();
        assert_eq!(pruned, 1, "Should have pruned 1 expired advert");
        assert_eq!(
            handler.cache().unwrap().advert_count(),
            0,
            "Cache should be empty after prune"
        );
    }

    #[test]
    fn test_handler_without_cache() {
        let peer_id = PeerId::new([1u8; 32]);
        let handler = CoordinatorHandler::new(peer_id);

        // Query should still work, just return empty response
        let origin = PeerId::new([2u8; 32]);
        let query = FindCoordinatorQuery::new(origin);

        let response = handler.handle_find_query(query).expect("should respond");

        assert_eq!(response.responder, peer_id);
        assert!(response.adverts.is_empty());
    }
}
