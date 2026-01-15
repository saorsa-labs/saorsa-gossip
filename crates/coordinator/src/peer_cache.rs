//! Peer cache for bootstrap coordination
//!
//! Implements SPEC2 §7.2 peer cache for storing coordinator connection info.
//!
//! ## Deprecation Notice
//!
//! `PeerCache` and `PeerCacheEntry` are deprecated in favor of [`GossipCacheAdapter`](crate::GossipCacheAdapter)
//! which wraps `ant-quic`'s `BootstrapCache` with gossip-specific functionality.
//!
//! **Migration**: Use [`GossipCacheAdapter`](crate::GossipCacheAdapter) with
//! [`crate::bootstrap_cache::BootstrapCache`] for new code.

use crate::NatClass;
use saorsa_gossip_types::{unix_millis, PeerId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

/// Peer cache entry per SPEC2 §7.2
///
/// Stores different address types for NAT traversal per SPEC2 §7.4:
/// - Direct: Public addresses (best)
/// - Reflexive: Hole-punched addresses (moderate)
/// - Relay: Relay peer reference (last resort)
///
/// # Deprecated
///
/// Use [`GossipCacheAdapter`](crate::GossipCacheAdapter) with [`crate::bootstrap_cache::CachedPeer`]
/// and [`CoordinatorAdvert`](crate::CoordinatorAdvert) instead.
#[deprecated(
    since = "0.2.0",
    note = "Use GossipCacheAdapter with bootstrap_cache::CachedPeer and CoordinatorAdvert instead"
)]
#[allow(deprecated)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerCacheEntry {
    /// Peer identifier
    pub peer_id: PeerId,
    /// Public addresses for direct connection (preferred)
    pub public_addrs: Vec<SocketAddr>,
    /// Reflexive addresses from hole punching
    pub reflexive_addrs: Vec<SocketAddr>,
    /// Relay peer ID (if this peer needs relay)
    pub relay_peer: Option<PeerId>,
    /// Last successful connection timestamp (unix ms)
    pub last_success: u64,
    /// NAT classification
    pub nat_class: NatClass,
    /// Roles this peer provides
    pub roles: PeerRoles,
}

/// Roles a peer can provide (per SPEC2 §8)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRoles {
    /// Acts as bootstrap coordinator
    pub coordinator: bool,
    /// Provides address reflection
    pub reflector: bool,
    /// Facilitates rendezvous for hole punching
    pub rendezvous: bool,
    /// Provides relay services
    pub relay: bool,
}

impl Default for PeerRoles {
    fn default() -> Self {
        Self {
            coordinator: true,
            reflector: true,
            rendezvous: false,
            relay: false,
        }
    }
}

#[allow(deprecated)]
impl PeerCacheEntry {
    /// Create a new cache entry with public addresses
    ///
    /// For backward compatibility, assumes all addresses are public (direct connection).
    /// Use `with_reflexive_addrs()` or `with_relay_peer()` to add other traversal options.
    pub fn new(
        peer_id: PeerId,
        public_addrs: Vec<SocketAddr>,
        nat_class: NatClass,
        roles: PeerRoles,
    ) -> Self {
        let now = unix_millis();

        Self {
            peer_id,
            public_addrs,
            reflexive_addrs: Vec::new(),
            relay_peer: None,
            last_success: now,
            nat_class,
            roles,
        }
    }

    /// Add reflexive (hole-punched) addresses
    pub fn with_reflexive_addrs(mut self, addrs: Vec<SocketAddr>) -> Self {
        self.reflexive_addrs = addrs;
        self
    }

    /// Add relay peer ID
    pub fn with_relay_peer(mut self, relay: PeerId) -> Self {
        self.relay_peer = Some(relay);
        self
    }

    /// Update last success timestamp
    pub fn mark_success(&mut self) {
        self.last_success = unix_millis();
    }

    /// Check if entry is recent (within last 24 hours)
    pub fn is_recent(&self) -> bool {
        let now = unix_millis();

        now - self.last_success < 24 * 3600 * 1000 // 24 hours in ms
    }
}

/// Persistent peer cache for bootstrap coordination
///
/// # Deprecated
///
/// Use [`GossipCacheAdapter`](crate::GossipCacheAdapter) with [`crate::bootstrap_cache::BootstrapCache`]
/// instead. The new adapter provides epsilon-greedy peer selection, quality tracking, and
/// persistent storage through `ant-quic`'s bootstrap cache.
///
/// ## Migration Example
///
/// ```ignore
/// // Old code:
/// let peer_cache = PeerCache::new();
/// peer_cache.insert(entry);
///
/// // New code:
/// use saorsa_gossip_coordinator::{GossipCacheAdapter, bootstrap_cache::*};
/// let config = BootstrapCacheConfig::builder().cache_dir(path).build();
/// let cache = Arc::new(BootstrapCache::open(config).await?);
/// let adapter = GossipCacheAdapter::new(cache);
/// adapter.insert_advert(advert).await;
/// ```
#[deprecated(
    since = "0.2.0",
    note = "Use GossipCacheAdapter with bootstrap_cache::BootstrapCache instead"
)]
#[allow(deprecated)]
pub struct PeerCache {
    entries: Arc<Mutex<HashMap<PeerId, PeerCacheEntry>>>,
}

#[allow(deprecated)]
impl PeerCache {
    /// Create a new empty peer cache
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn lock_entries(&self) -> Option<MutexGuard<'_, HashMap<PeerId, PeerCacheEntry>>> {
        match self.entries.lock() {
            Ok(guard) => Some(guard),
            Err(e) => {
                tracing::warn!("PeerCache Mutex poisoned: {e}");
                None
            }
        }
    }

    /// Insert or update a peer cache entry
    pub fn insert(&self, entry: PeerCacheEntry) {
        if let Some(mut entries) = self.lock_entries() {
            entries.insert(entry.peer_id, entry);
        }
    }

    /// Get a peer cache entry by ID
    pub fn get(&self, peer_id: &PeerId) -> Option<PeerCacheEntry> {
        self.lock_entries()
            .and_then(|entries| entries.get(peer_id).cloned())
    }

    /// Get all coordinators, sorted by recency
    pub fn get_coordinators(&self) -> Vec<PeerCacheEntry> {
        let mut coordinators = self
            .lock_entries()
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| entry.roles.coordinator)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Sort by last_success descending (most recent first)
        coordinators.sort_by(|a, b| b.last_success.cmp(&a.last_success));
        coordinators
    }

    /// Get all entries with a specific role
    pub fn get_by_role(
        &self,
        role_filter: impl Fn(&PeerCacheEntry) -> bool,
    ) -> Vec<PeerCacheEntry> {
        self.lock_entries()
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| role_filter(entry))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Prune old entries (older than 7 days)
    pub fn prune_old(&self) -> usize {
        let mut entries = match self.lock_entries() {
            Some(guard) => guard,
            None => return 0,
        };
        let now = unix_millis();

        let cutoff = now - (7 * 24 * 3600 * 1000); // 7 days in ms

        let to_remove: Vec<_> = entries
            .iter()
            .filter(|(_, entry)| entry.last_success < cutoff)
            .map(|(peer_id, _)| *peer_id)
            .collect();

        let count = to_remove.len();
        for peer_id in to_remove {
            entries.remove(&peer_id);
        }
        count
    }

    /// Get the number of entries in the cache
    pub fn len(&self) -> usize {
        self.lock_entries()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    /// Check if the cache is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all entries
    pub fn clear(&self) {
        if let Some(mut entries) = self.lock_entries() {
            entries.clear();
        }
    }

    /// Save the peer cache to disk in CBOR format
    ///
    /// Uses file locking to ensure safe concurrent access.
    /// Creates parent directories if they don't exist.
    ///
    /// # Arguments
    /// * `path` - Path to save the cache file
    ///
    /// # Example
    /// ```no_run
    /// use saorsa_gossip_coordinator::PeerCache;
    /// use std::path::Path;
    ///
    /// let cache = PeerCache::new();
    /// cache.save(Path::new("/tmp/peer_cache.cbor")).expect("save failed");
    /// ```
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open file for writing
        let file = std::fs::File::create(path)?;

        // Acquire exclusive lock (use fully qualified syntax to avoid unstable_name_collisions)
        fs2::FileExt::lock_exclusive(&file)?;

        // Serialize entries to CBOR
        let entries_vec: Vec<PeerCacheEntry> = self
            .lock_entries()
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default();

        let mut writer = std::io::BufWriter::new(file);
        ciborium::into_writer(&entries_vec, &mut writer)?;
        writer.flush()?;

        // Lock is released when file is dropped
        Ok(())
    }

    /// Load a peer cache from disk
    ///
    /// If the file doesn't exist, returns an empty cache.
    /// If the file is corrupted, returns an error.
    ///
    /// # Arguments
    /// * `path` - Path to load the cache file from
    ///
    /// # Example
    /// ```no_run
    /// use saorsa_gossip_coordinator::PeerCache;
    /// use std::path::Path;
    ///
    /// let cache = PeerCache::load(Path::new("/tmp/peer_cache.cbor")).expect("load failed");
    /// ```
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        // If file doesn't exist, return empty cache
        if !path.exists() {
            return Ok(Self::new());
        }

        // Open file for reading
        let file = std::fs::File::open(path)?;

        // Acquire shared lock (use fully qualified syntax to avoid unstable_name_collisions)
        fs2::FileExt::lock_shared(&file)?;

        // Deserialize from CBOR
        let reader = std::io::BufReader::new(file);
        let entries_vec: Vec<PeerCacheEntry> = ciborium::from_reader(reader)?;

        // Build cache from entries
        let cache = Self::new();
        for entry in entries_vec {
            cache.insert(entry);
        }

        // Lock is released when file is dropped
        Ok(cache)
    }
}

#[allow(deprecated)]
impl Default for PeerCache {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(deprecated)]
impl Clone for PeerCache {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, deprecated)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_entry_creation() {
        let peer_id = PeerId::new([1u8; 32]);
        let addr = "127.0.0.1:8080".parse().expect("valid address");
        let roles = PeerRoles {
            coordinator: true,
            reflector: true,
            rendezvous: false,
            relay: false,
        };

        let entry = PeerCacheEntry::new(peer_id, vec![addr], NatClass::Eim, roles);

        assert_eq!(entry.peer_id, peer_id);
        assert_eq!(entry.public_addrs.len(), 1);
        assert_eq!(entry.public_addrs[0], addr);
        assert!(entry.is_recent());
    }

    #[test]
    fn test_entry_mark_success() {
        let peer_id = PeerId::new([1u8; 32]);
        let mut entry = PeerCacheEntry::new(
            peer_id,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        );

        let old_time = entry.last_success;
        std::thread::sleep(std::time::Duration::from_millis(10));
        entry.mark_success();

        assert!(entry.last_success > old_time, "Timestamp should be updated");
    }

    #[test]
    fn test_entry_is_recent() {
        let peer_id = PeerId::new([1u8; 32]);
        let mut entry = PeerCacheEntry::new(
            peer_id,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        );

        assert!(entry.is_recent(), "Fresh entry should be recent");

        // Simulate old entry
        entry.last_success = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64
            - (25 * 3600 * 1000); // 25 hours ago

        assert!(!entry.is_recent(), "Old entry should not be recent");
    }

    #[test]
    fn test_peer_cache_insert_and_get() {
        let cache = PeerCache::new();
        let peer_id = PeerId::new([1u8; 32]);

        let entry = PeerCacheEntry::new(
            peer_id,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        );

        cache.insert(entry.clone());
        assert_eq!(cache.len(), 1);

        let retrieved = cache.get(&peer_id).expect("should find entry");
        assert_eq!(retrieved.peer_id, peer_id);
    }

    #[test]
    fn test_get_coordinators_sorted() {
        let cache = PeerCache::new();

        // Insert coordinators with different timestamps
        for i in 0..3 {
            let peer_id = PeerId::new([i; 32]);
            let mut entry = PeerCacheEntry::new(
                peer_id,
                vec![],
                NatClass::Eim,
                PeerRoles {
                    coordinator: true,
                    reflector: false,
                    rendezvous: false,
                    relay: false,
                },
            );

            // Make timestamps different
            entry.last_success -= (i as u64) * 1000;
            cache.insert(entry);

            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let coordinators = cache.get_coordinators();
        assert_eq!(coordinators.len(), 3);

        // Should be sorted by recency (most recent first)
        for i in 0..2 {
            assert!(
                coordinators[i].last_success >= coordinators[i + 1].last_success,
                "Should be sorted by recency"
            );
        }
    }

    #[test]
    fn test_get_by_role() {
        let cache = PeerCache::new();

        // Insert coordinator
        let peer1 = PeerId::new([1u8; 32]);
        cache.insert(PeerCacheEntry::new(
            peer1,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        ));

        // Insert relay
        let peer2 = PeerId::new([2u8; 32]);
        cache.insert(PeerCacheEntry::new(
            peer2,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: false,
                reflector: false,
                rendezvous: false,
                relay: true,
            },
        ));

        let coordinators = cache.get_by_role(|entry| entry.roles.coordinator);
        assert_eq!(coordinators.len(), 1);
        assert_eq!(coordinators[0].peer_id, peer1);

        let relays = cache.get_by_role(|entry| entry.roles.relay);
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].peer_id, peer2);
    }

    #[test]
    fn test_prune_old_entries() {
        let cache = PeerCache::new();

        // Insert recent entry
        let peer1 = PeerId::new([1u8; 32]);
        cache.insert(PeerCacheEntry::new(
            peer1,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        ));

        // Insert old entry
        let peer2 = PeerId::new([2u8; 32]);
        let mut old_entry = PeerCacheEntry::new(
            peer2,
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        );
        old_entry.last_success = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64
            - (8 * 24 * 3600 * 1000); // 8 days ago

        cache.insert(old_entry);
        assert_eq!(cache.len(), 2);

        let pruned = cache.prune_old();
        assert_eq!(pruned, 1, "Should prune 1 old entry");
        assert_eq!(cache.len(), 1, "Should have 1 entry left");

        assert!(cache.get(&peer1).is_some(), "Recent entry should remain");
        assert!(cache.get(&peer2).is_none(), "Old entry should be removed");
    }

    #[test]
    fn test_cache_clear() {
        let cache = PeerCache::new();

        for i in 0..5 {
            let peer_id = PeerId::new([i; 32]);
            cache.insert(PeerCacheEntry::new(
                peer_id,
                vec![],
                NatClass::Eim,
                PeerRoles {
                    coordinator: true,
                    reflector: false,
                    rendezvous: false,
                    relay: false,
                },
            ));
        }

        assert_eq!(cache.len(), 5);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    /// Test saving cache to disk
    #[test]
    fn test_save_cache_to_disk() {
        use tempfile::tempdir;

        let temp_dir = tempdir().expect("create temp dir");
        let cache_path = temp_dir.path().join("peer_cache.cbor");

        let cache = PeerCache::new();

        // Add some entries
        for i in 0..3 {
            let peer_id = PeerId::new([i; 32]);
            let addr = format!("127.0.0.{}:8080", i + 1)
                .parse()
                .expect("valid addr");
            cache.insert(PeerCacheEntry::new(
                peer_id,
                vec![addr],
                NatClass::Eim,
                PeerRoles {
                    coordinator: true,
                    reflector: false,
                    rendezvous: false,
                    relay: false,
                },
            ));
        }

        // Save to disk
        cache.save(&cache_path).expect("save should succeed");

        // File should exist
        assert!(cache_path.exists(), "Cache file should exist");

        // File should not be empty
        let metadata = std::fs::metadata(&cache_path).expect("read metadata");
        assert!(metadata.len() > 0, "Cache file should not be empty");
    }

    /// Test loading cache from disk
    #[test]
    fn test_load_cache_from_disk() {
        use tempfile::tempdir;

        let temp_dir = tempdir().expect("create temp dir");
        let cache_path = temp_dir.path().join("peer_cache.cbor");

        // Create and save a cache
        let original_cache = PeerCache::new();
        let peer1 = PeerId::new([1u8; 32]);
        let addr1 = "10.0.0.1:8080".parse().expect("valid");
        original_cache.insert(PeerCacheEntry::new(
            peer1,
            vec![addr1],
            NatClass::Edm,
            PeerRoles {
                coordinator: true,
                reflector: true,
                rendezvous: false,
                relay: false,
            },
        ));

        original_cache.save(&cache_path).expect("save");

        // Load from disk
        let loaded_cache = PeerCache::load(&cache_path).expect("load should succeed");

        // Should have same entries
        assert_eq!(loaded_cache.len(), 1);
        let loaded_entry = loaded_cache.get(&peer1).expect("entry should exist");
        assert_eq!(loaded_entry.peer_id, peer1);
        assert_eq!(loaded_entry.public_addrs[0], addr1);
        assert_eq!(loaded_entry.nat_class, NatClass::Edm);
    }

    /// Test round-trip persistence
    #[test]
    fn test_cache_round_trip() {
        use tempfile::tempdir;

        let temp_dir = tempdir().expect("create temp dir");
        let cache_path = temp_dir.path().join("peer_cache.cbor");

        let original_cache = PeerCache::new();

        // Add diverse entries
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        original_cache.insert(
            PeerCacheEntry::new(
                peer1,
                vec!["192.168.1.1:9000".parse().expect("valid")],
                NatClass::Eim,
                PeerRoles {
                    coordinator: true,
                    reflector: false,
                    rendezvous: false,
                    relay: false,
                },
            )
            .with_reflexive_addrs(vec!["10.0.0.1:9001".parse().expect("valid")])
            .with_relay_peer(peer2),
        );

        original_cache.insert(PeerCacheEntry::new(
            peer2,
            vec!["203.0.113.1:8080".parse().expect("valid")],
            NatClass::Symmetric,
            PeerRoles {
                coordinator: false,
                reflector: false,
                rendezvous: false,
                relay: true,
            },
        ));

        // Save and load
        original_cache.save(&cache_path).expect("save");
        let loaded_cache = PeerCache::load(&cache_path).expect("load");

        // Verify all entries preserved
        assert_eq!(loaded_cache.len(), 2);

        let loaded1 = loaded_cache.get(&peer1).expect("peer1");
        assert_eq!(loaded1.public_addrs.len(), 1);
        assert_eq!(loaded1.reflexive_addrs.len(), 1);
        assert_eq!(loaded1.relay_peer, Some(peer2));

        let loaded2 = loaded_cache.get(&peer2).expect("peer2");
        assert!(loaded2.roles.relay);
    }

    /// Test loading from missing file returns empty cache
    #[test]
    fn test_load_missing_file() {
        use tempfile::tempdir;

        let temp_dir = tempdir().expect("create temp dir");
        let missing_path = temp_dir.path().join("nonexistent.cbor");

        let cache = PeerCache::load(&missing_path).expect("should handle missing file gracefully");

        assert_eq!(cache.len(), 0, "Missing file should return empty cache");
    }

    /// Test loading corrupted file returns error
    #[test]
    fn test_load_corrupted_file() {
        use std::io::Write;
        use tempfile::tempdir;

        let temp_dir = tempdir().expect("create temp dir");
        let corrupt_path = temp_dir.path().join("corrupt.cbor");

        // Write garbage data
        let mut file = std::fs::File::create(&corrupt_path).expect("create file");
        file.write_all(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect("write garbage");
        drop(file);

        let result = PeerCache::load(&corrupt_path);
        assert!(result.is_err(), "Corrupted file should return error");
    }

    /// Test concurrent access safety (file locking)
    #[test]
    fn test_concurrent_save_safety() {
        use std::sync::Arc;
        use std::thread;
        use tempfile::tempdir;

        let temp_dir = tempdir().expect("create temp dir");
        let cache_path = Arc::new(temp_dir.path().join("concurrent.cbor"));

        let cache1 = Arc::new(PeerCache::new());
        let cache2 = Arc::new(PeerCache::new());

        cache1.insert(PeerCacheEntry::new(
            PeerId::new([1u8; 32]),
            vec![],
            NatClass::Eim,
            PeerRoles {
                coordinator: true,
                reflector: false,
                rendezvous: false,
                relay: false,
            },
        ));

        cache2.insert(PeerCacheEntry::new(
            PeerId::new([2u8; 32]),
            vec![],
            NatClass::Edm,
            PeerRoles {
                coordinator: false,
                reflector: true,
                rendezvous: false,
                relay: false,
            },
        ));

        let path1 = Arc::clone(&cache_path);
        let c1 = Arc::clone(&cache1);
        let handle1 = thread::spawn(move || {
            c1.save(&path1).expect("save from thread 1");
        });

        let path2 = Arc::clone(&cache_path);
        let c2 = Arc::clone(&cache2);
        let handle2 = thread::spawn(move || {
            c2.save(&path2).expect("save from thread 2");
        });

        handle1.join().expect("thread 1");
        handle2.join().expect("thread 2");

        // Should complete without deadlock or corruption
        let loaded = PeerCache::load(&cache_path).expect("load after concurrent saves");
        assert!(!loaded.is_empty(), "Should have at least one entry");
    }
}
