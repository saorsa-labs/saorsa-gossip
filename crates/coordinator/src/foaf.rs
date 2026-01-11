//! Friend-of-a-Friend (FOAF) coordinator discovery
//!
//! Implements SPEC2 §7.3 FOAF queries for finding coordinators when the cache is cold.

use crate::CoordinatorAdvert;
use rand::RngCore;
use saorsa_gossip_types::{unix_millis, PeerId};
use serde::{Deserialize, Serialize};

/// Maximum TTL for FIND_COORDINATOR queries (SPEC2 §7.3)
pub const MAX_FIND_COORDINATOR_TTL: u8 = 3;

/// Default fanout for FIND_COORDINATOR queries (SPEC2 §7.3)
pub const DEFAULT_FIND_COORDINATOR_FANOUT: usize = 3;

/// FOAF query for finding coordinators
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindCoordinatorQuery {
    /// Query ID for deduplication
    pub query_id: [u8; 32],
    /// Originating peer
    pub origin: PeerId,
    /// Current TTL (decrements each hop)
    pub ttl: u8,
    /// Timestamp when query was created (unix ms)
    pub created_at: u64,
}

impl FindCoordinatorQuery {
    /// Create a new FIND_COORDINATOR query
    pub fn new(origin: PeerId) -> Self {
        let query_id = blake3::hash(&[&origin.to_bytes()[..], &rand_bytes()].concat());

        let now = unix_millis();

        Self {
            query_id: *query_id.as_bytes(),
            origin,
            ttl: MAX_FIND_COORDINATOR_TTL,
            created_at: now,
        }
    }

    /// Decrement TTL and check if query should continue
    pub fn decrement_ttl(&mut self) -> bool {
        if self.ttl > 0 {
            self.ttl -= 1;
            true
        } else {
            false
        }
    }

    /// Check if query has expired (older than 30 seconds)
    pub fn is_expired(&self) -> bool {
        let now = unix_millis();

        now > self.created_at + 30_000
    }
}

/// Response to a FIND_COORDINATOR query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindCoordinatorResponse {
    /// Original query ID
    pub query_id: [u8; 32],
    /// Peer responding
    pub responder: PeerId,
    /// Coordinator adverts known to this peer
    pub adverts: Vec<CoordinatorAdvert>,
}

impl FindCoordinatorResponse {
    /// Create a new response
    pub fn new(query_id: [u8; 32], responder: PeerId, adverts: Vec<CoordinatorAdvert>) -> Self {
        Self {
            query_id,
            responder,
            adverts,
        }
    }
}

/// Generate cryptographically random bytes for query IDs
fn rand_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_query_creation() {
        let origin = PeerId::new([1u8; 32]);
        let query = FindCoordinatorQuery::new(origin);

        assert_eq!(query.origin, origin);
        assert_eq!(query.ttl, MAX_FIND_COORDINATOR_TTL);
        assert!(!query.is_expired());
    }

    #[test]
    fn test_query_ttl_decrement() {
        let origin = PeerId::new([1u8; 32]);
        let mut query = FindCoordinatorQuery::new(origin);

        // Should continue for TTL=3
        assert!(query.decrement_ttl());
        assert_eq!(query.ttl, 2);

        // TTL=2
        assert!(query.decrement_ttl());
        assert_eq!(query.ttl, 1);

        // TTL=1
        assert!(query.decrement_ttl());
        assert_eq!(query.ttl, 0);

        // TTL=0, should stop
        assert!(!query.decrement_ttl());
        assert_eq!(query.ttl, 0);
    }

    #[test]
    fn test_query_expiry() {
        let origin = PeerId::new([1u8; 32]);
        let mut query = FindCoordinatorQuery::new(origin);

        assert!(!query.is_expired(), "Fresh query should not be expired");

        // Simulate old query
        query.created_at = unix_millis() - 40_000; // 40 seconds ago

        assert!(query.is_expired(), "Old query should be expired");
    }

    #[test]
    fn test_query_unique_ids() {
        let origin = PeerId::new([1u8; 32]);
        let query1 = FindCoordinatorQuery::new(origin);
        let query2 = FindCoordinatorQuery::new(origin);

        assert_ne!(
            query1.query_id, query2.query_id,
            "Query IDs should be unique"
        );
    }

    #[test]
    fn test_response_creation() {
        let query_id = [42u8; 32];
        let responder = PeerId::new([2u8; 32]);
        let adverts = vec![];

        let response = FindCoordinatorResponse::new(query_id, responder, adverts);

        assert_eq!(response.query_id, query_id);
        assert_eq!(response.responder, responder);
        assert!(response.adverts.is_empty());
    }
}
