//! LRU cache for coordinator advertisements

use crate::CoordinatorAdvert;
use lru::LruCache;
use saorsa_gossip_types::PeerId;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};

const DEFAULT_CAPACITY: usize = 100;

/// Compile-time validated non-zero default capacity
const DEFAULT_CAPACITY_NONZERO: NonZeroUsize = match NonZeroUsize::new(DEFAULT_CAPACITY) {
    Some(v) => v,
    None => panic!("DEFAULT_CAPACITY must be non-zero"),
};

fn non_zero_capacity(capacity: usize) -> NonZeroUsize {
    NonZeroUsize::new(capacity).unwrap_or(DEFAULT_CAPACITY_NONZERO)
}

/// LRU cache for coordinator advertisements
///
/// Stores recently seen coordinator adverts with automatic expiry.
/// Thread-safe and supports concurrent access.
pub struct AdvertCache {
    cache: Arc<Mutex<LruCache<PeerId, CoordinatorAdvert>>>,
}

impl AdvertCache {
    /// Create a new advert cache with given capacity
    ///
    /// # Arguments
    /// * `capacity` - Maximum number of adverts to cache
    pub fn new(capacity: usize) -> Self {
        let effective_capacity = if capacity == 0 {
            DEFAULT_CAPACITY
        } else {
            capacity
        };
        let cap = non_zero_capacity(effective_capacity);
        Self {
            cache: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }

    fn lock_cache(&self) -> Option<MutexGuard<'_, LruCache<PeerId, CoordinatorAdvert>>> {
        self.cache.lock().ok()
    }

    /// Insert an advert (if valid and not expired)
    ///
    /// Returns `true` if the advert was inserted, `false` if invalid/expired.
    pub fn insert(&self, advert: CoordinatorAdvert) -> bool {
        if !advert.is_valid() {
            return false;
        }

        match self.lock_cache() {
            Some(mut cache) => {
                cache.put(advert.peer, advert);
                true
            }
            None => false,
        }
    }

    /// Get an advert by peer ID
    ///
    /// Returns `None` if the advert is not in the cache or has expired.
    pub fn get(&self, peer: &PeerId) -> Option<CoordinatorAdvert> {
        let mut cache = self.lock_cache()?;
        cache.get(peer).filter(|advert| advert.is_valid()).cloned()
    }

    /// Get all cached adverts, sorted by score (highest first)
    ///
    /// Only returns valid (non-expired) adverts.
    pub fn get_all_sorted(&self) -> Vec<CoordinatorAdvert> {
        let cache = match self.lock_cache() {
            Some(cache) => cache,
            None => return Vec::new(),
        };
        let mut adverts: Vec<_> = cache
            .iter()
            .filter(|(_, advert)| advert.is_valid())
            .map(|(_, advert)| advert.clone())
            .collect();

        // Sort by score (descending)
        adverts.sort_by(|a, b| b.score.cmp(&a.score));
        adverts
    }

    /// Get all adverts with a specific role enabled
    pub fn get_by_role(
        &self,
        role_filter: impl Fn(&CoordinatorAdvert) -> bool,
    ) -> Vec<CoordinatorAdvert> {
        let cache = match self.lock_cache() {
            Some(cache) => cache,
            None => return Vec::new(),
        };
        cache
            .iter()
            .filter(|(_, advert)| advert.is_valid() && role_filter(advert))
            .map(|(_, advert)| advert.clone())
            .collect()
    }

    /// Prune expired adverts from the cache
    ///
    /// Returns the number of adverts removed.
    pub fn prune_expired(&self) -> usize {
        let mut cache = match self.lock_cache() {
            Some(cache) => cache,
            None => return 0,
        };
        let to_remove: Vec<_> = cache
            .iter()
            .filter(|(_, advert)| !advert.is_valid())
            .map(|(peer, _)| *peer)
            .collect();

        let count = to_remove.len();
        for peer in to_remove {
            cache.pop(&peer);
        }
        count
    }

    /// Get the number of valid adverts in the cache
    pub fn len(&self) -> usize {
        self.lock_cache()
            .map(|cache| cache.iter().filter(|(_, advert)| advert.is_valid()).count())
            .unwrap_or(0)
    }

    /// Check if the cache is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all adverts from the cache
    pub fn clear(&self) {
        if let Some(mut cache) = self.lock_cache() {
            cache.clear();
        }
    }
}

impl Clone for AdvertCache {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
        }
    }
}

impl Default for AdvertCache {
    fn default() -> Self {
        Self::new(100)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{CoordinatorRoles, NatClass};

    #[test]
    fn test_cache_insert_and_get() {
        let cache = AdvertCache::new(10);
        let peer = PeerId::new([1u8; 32]);

        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            10_000,
        );

        assert!(cache.insert(advert.clone()), "Should insert valid advert");
        assert_eq!(cache.len(), 1);

        let retrieved = cache.get(&peer).expect("Should retrieve advert");
        assert_eq!(retrieved.peer, peer);
    }

    #[test]
    fn test_cache_rejects_expired() {
        let cache = AdvertCache::new(10);
        let peer = PeerId::new([2u8; 32]);

        let advert = CoordinatorAdvert::new(
            peer,
            CoordinatorRoles::default(),
            vec![],
            NatClass::Eim,
            1, // 1ms validity
        );

        // Sleep to let it expire
        std::thread::sleep(std::time::Duration::from_millis(5));

        assert!(!cache.insert(advert), "Should reject expired advert");
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_sorting_by_score() {
        let cache = AdvertCache::new(10);

        for i in 0..5 {
            let peer = PeerId::new([i; 32]);
            let mut advert = CoordinatorAdvert::new(
                peer,
                CoordinatorRoles::default(),
                vec![],
                NatClass::Eim,
                10_000,
            );
            advert.score = i as i32;
            cache.insert(advert);
        }

        let sorted = cache.get_all_sorted();
        assert_eq!(sorted.len(), 5);

        // Should be sorted descending by score
        for i in 0..4 {
            assert!(sorted[i].score >= sorted[i + 1].score);
        }
    }

    #[test]
    fn test_cache_prune_expired() {
        let cache = AdvertCache::new(10);

        // Insert some adverts with different expiry times
        // Use 50ms for the first one to avoid race condition during insert
        for i in 0..3 {
            let peer = PeerId::new([i; 32]);
            let advert = CoordinatorAdvert::new(
                peer,
                CoordinatorRoles::default(),
                vec![],
                NatClass::Eim,
                if i == 0 { 50 } else { 10_000 }, // First one expires after 50ms
            );
            cache.insert(advert);
        }

        assert_eq!(cache.len(), 3);

        // Sleep to let first one expire (50ms TTL + margin)
        std::thread::sleep(std::time::Duration::from_millis(100));

        let pruned = cache.prune_expired();
        assert_eq!(pruned, 1, "Should prune one expired advert");
        assert_eq!(cache.len(), 2, "Should have 2 valid adverts left");
    }

    #[test]
    fn test_cache_get_by_role() {
        let cache = AdvertCache::new(10);

        // Insert coordinator
        let peer1 = PeerId::new([1u8; 32]);
        let advert1 = CoordinatorAdvert::new(
            peer1,
            CoordinatorRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
            vec![],
            NatClass::Eim,
            10_000,
        );
        cache.insert(advert1);

        // Insert relay
        let peer2 = PeerId::new([2u8; 32]);
        let advert2 = CoordinatorAdvert::new(
            peer2,
            CoordinatorRoles {
                coordinator: false,
                reflector: false,
                rendezvous: false,
                relay: true,
            },
            vec![],
            NatClass::Eim,
            10_000,
        );
        cache.insert(advert2);

        // Get only coordinators
        let coordinators = cache.get_by_role(|advert| advert.roles.coordinator);
        assert_eq!(coordinators.len(), 1);
        assert_eq!(coordinators[0].peer, peer1);

        // Get only relays
        let relays = cache.get_by_role(|advert| advert.roles.relay);
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].peer, peer2);
    }

    #[test]
    fn test_cache_clear() {
        let cache = AdvertCache::new(10);

        for i in 0..5 {
            let peer = PeerId::new([i; 32]);
            let advert = CoordinatorAdvert::new(
                peer,
                CoordinatorRoles::default(),
                vec![],
                NatClass::Eim,
                10_000,
            );
            cache.insert(advert);
        }

        assert_eq!(cache.len(), 5);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }
}
