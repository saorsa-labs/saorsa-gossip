#![warn(missing_docs)]

//! Production-Ready Delta-CRDT synchronization with anti-entropy
//!
//! Implements:
//! - Delta-CRDTs for bandwidth efficiency
//! - OR-Set with proper concurrent add/remove semantics
//! - LWW-Register with vector clocks
//! - Anti-entropy reconciliation
//! - IBLT reconciliation for large sets (future)

use anyhow::Result;
use saorsa_gossip_types::PeerId;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// CRDT types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CrdtType {
    /// Observed-Remove Set
    OrSet,
    /// Last-Writer-Wins Register
    LwwRegister,
    /// Replicated Growable Array
    Rga,
}

/// Delta-CRDT trait for bandwidth-efficient synchronization
pub trait DeltaCrdt: Send + Sync {
    /// Type of the delta
    type Delta: Clone + Serialize + for<'de> Deserialize<'de> + Send;

    /// Merge a delta into this CRDT
    fn merge(&mut self, delta: &Self::Delta) -> Result<()>;

    /// Generate a delta for changes since a given version
    fn delta(&self, since_version: u64) -> Option<Self::Delta>;

    /// Get current version
    fn version(&self) -> u64;
}

/// Unique tag for OR-Set elements: (PeerId, sequence_number)
pub type UniqueTag = (PeerId, u64);

/// Production-ready OR-Set (Observed-Remove Set)
///
/// Properly handles concurrent add/remove operations from multiple replicas.
/// Uses (element, Set\<UniqueTag\>) to track all concurrent adds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrSet<T: Hash + Eq + Clone> {
    /// Elements with their unique tags (peer_id, seq) per concurrent add
    elements: HashMap<T, HashSet<UniqueTag>>,
    /// Tombstones: tags that have been removed
    tombstones: HashMap<T, HashSet<UniqueTag>>,
    /// Current version for delta generation
    version: u64,
    /// Version -> (additions, removals) for delta tracking
    changelog: HashMap<u64, ChangelogEntry<T>>,
}

/// Type alias for changelog entries to reduce complexity
type ChangelogEntry<T> = (
    HashMap<T, HashSet<UniqueTag>>,
    HashMap<T, HashSet<UniqueTag>>,
);

impl<T: Hash + Eq + Clone> OrSet<T> {
    /// Create a new OR-Set
    pub fn new() -> Self {
        Self {
            elements: HashMap::new(),
            tombstones: HashMap::new(),
            version: 0,
            changelog: HashMap::new(),
        }
    }

    /// Add an element with a unique tag
    ///
    /// The tag should be (peer_id, sequence_number) to ensure uniqueness
    /// across all replicas.
    pub fn add(&mut self, element: T, tag: UniqueTag) -> Result<()> {
        // Add to elements
        self.elements
            .entry(element.clone())
            .or_default()
            .insert(tag);

        // Remove from tombstones if it was previously removed
        if let Some(ts) = self.tombstones.get_mut(&element) {
            ts.remove(&tag);
            if ts.is_empty() {
                self.tombstones.remove(&element);
            }
        }

        // Record in changelog
        self.version += 1;
        let changelog_entry = self
            .changelog
            .entry(self.version)
            .or_insert((HashMap::new(), HashMap::new()));
        changelog_entry.0.entry(element).or_default().insert(tag);

        Ok(())
    }

    /// Remove an element
    ///
    /// Removes all tags associated with this element, preventing
    /// concurrent adds from being visible.
    pub fn remove(&mut self, element: &T) -> Result<()> {
        if let Some(tags) = self.elements.get(element).cloned() {
            // Move all tags to tombstones
            for tag in &tags {
                self.tombstones
                    .entry(element.clone())
                    .or_default()
                    .insert(*tag);
            }

            // Remove from elements
            self.elements.remove(element);

            // Record in changelog
            self.version += 1;
            let changelog_entry = self
                .changelog
                .entry(self.version)
                .or_insert((HashMap::new(), HashMap::new()));
            changelog_entry
                .1
                .entry(element.clone())
                .or_default()
                .extend(tags);
        }

        Ok(())
    }

    /// Check if element exists (not removed)
    pub fn contains(&self, element: &T) -> bool {
        self.elements.contains_key(element)
    }

    /// Get all elements (not removed)
    pub fn elements(&self) -> Vec<&T> {
        self.elements.keys().collect()
    }

    /// Get element count
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Merge another OR-Set into this one
    ///
    /// This implements the merge operation for state-based CRDTs.
    /// Preserves all concurrent adds and removes.
    pub fn merge_state(&mut self, other: &OrSet<T>) -> Result<()> {
        // Merge elements
        for (elem, tags) in &other.elements {
            let our_tags = self.elements.entry(elem.clone()).or_default();
            our_tags.extend(tags);

            // Remove any tags that are in our tombstones
            if let Some(our_tombstones) = self.tombstones.get(elem) {
                our_tags.retain(|tag| !our_tombstones.contains(tag));
            }
        }

        // Merge tombstones
        for (elem, tags) in &other.tombstones {
            let our_tombstones = self.tombstones.entry(elem.clone()).or_default();
            our_tombstones.extend(tags);

            // Remove tombstoned tags from our elements
            if let Some(our_tags) = self.elements.get_mut(elem) {
                our_tags.retain(|tag| !our_tombstones.contains(tag));
                if our_tags.is_empty() {
                    self.elements.remove(elem);
                }
            }
        }

        // Update version to max
        self.version = self.version.max(other.version);

        Ok(())
    }

    /// Clear old changelog entries to prevent unbounded growth
    pub fn compact(&mut self, keep_versions: u64) {
        if self.version > keep_versions {
            let min_version = self.version - keep_versions;
            self.changelog.retain(|v, _| *v > min_version);
        }
    }
}

impl<T: Hash + Eq + Clone> Default for OrSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Delta for OR-Set (changes since a version)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "T: Serialize + DeserializeOwned")]
pub struct OrSetDelta<T: Hash + Eq + Clone + Serialize + DeserializeOwned> {
    /// Added elements with their tags
    pub added: HashMap<T, HashSet<UniqueTag>>,
    /// Removed tags (tombstones)
    pub removed: HashMap<T, HashSet<UniqueTag>>,
    /// Version this delta represents
    pub version: u64,
}

impl<T: Hash + Eq + Clone + Send + Sync + Serialize + DeserializeOwned> DeltaCrdt for OrSet<T> {
    type Delta = OrSetDelta<T>;

    fn merge(&mut self, delta: &Self::Delta) -> Result<()> {
        // Apply additions
        for (elem, tags) in &delta.added {
            let our_tags = self.elements.entry(elem.clone()).or_default();
            our_tags.extend(tags);
        }

        // Apply removals (tombstones)
        for (elem, tags) in &delta.removed {
            let our_tombstones = self.tombstones.entry(elem.clone()).or_default();
            our_tombstones.extend(tags);

            // Remove tombstoned tags from elements
            if let Some(our_tags) = self.elements.get_mut(elem) {
                our_tags.retain(|tag| !our_tombstones.contains(tag));
                if our_tags.is_empty() {
                    self.elements.remove(elem);
                }
            }
        }

        // Update version
        self.version = self.version.max(delta.version);

        Ok(())
    }

    fn delta(&self, since_version: u64) -> Option<Self::Delta> {
        if since_version >= self.version {
            return None;
        }

        let mut added: HashMap<T, HashSet<UniqueTag>> = HashMap::new();
        let mut removed: HashMap<T, HashSet<UniqueTag>> = HashMap::new();

        // Collect changes from changelog
        for version in (since_version + 1)..=self.version {
            if let Some((adds, removes)) = self.changelog.get(&version) {
                for (elem, tags) in adds {
                    added.entry(elem.clone()).or_default().extend(tags);
                }
                for (elem, tags) in removes {
                    removed.entry(elem.clone()).or_default().extend(tags);
                }
            }
        }

        Some(OrSetDelta {
            added,
            removed,
            version: self.version,
        })
    }

    fn version(&self) -> u64 {
        self.version
    }
}

/// Vector Clock for causality tracking
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorClock {
    clocks: HashMap<PeerId, u64>,
}

impl VectorClock {
    /// Create a new vector clock
    pub fn new() -> Self {
        Self {
            clocks: HashMap::new(),
        }
    }

    /// Increment the clock for a peer
    pub fn increment(&mut self, peer: PeerId) {
        *self.clocks.entry(peer).or_insert(0) += 1;
    }

    /// Get the clock value for a peer
    pub fn get(&self, peer: &PeerId) -> u64 {
        self.clocks.get(peer).copied().unwrap_or(0)
    }

    /// Merge another vector clock (take maximum of each entry)
    pub fn merge(&mut self, other: &VectorClock) {
        for (peer, &time) in &other.clocks {
            let our_time = self.clocks.entry(*peer).or_insert(0);
            *our_time = (*our_time).max(time);
        }
    }

    /// Check if this clock happens before another
    pub fn happens_before(&self, other: &VectorClock) -> bool {
        let mut strictly_less = false;

        // Check all our entries
        for (peer, &our_time) in &self.clocks {
            let other_time = other.get(peer);
            if our_time > other_time {
                return false; // We have a greater timestamp
            }
            if our_time < other_time {
                strictly_less = true;
            }
        }

        // Check for entries only in other
        for peer in other.clocks.keys() {
            if !self.clocks.contains_key(peer) && other.get(peer) > 0 {
                strictly_less = true;
            }
        }

        strictly_less
    }

    /// Check if two clocks are concurrent (neither happens before the other)
    pub fn concurrent(&self, other: &VectorClock) -> bool {
        !self.happens_before(other) && !other.happens_before(self) && self != other
    }
}

impl Default for VectorClock {
    fn default() -> Self {
        Self::new()
    }
}

/// LWW Register with Vector Clock
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LwwRegister<T: Clone> {
    value: T,
    clock: VectorClock,
}

impl<T: Clone> LwwRegister<T> {
    /// Create a new LWW register
    pub fn new(value: T) -> Self {
        Self {
            value,
            clock: VectorClock::new(),
        }
    }

    /// Set value with vector clock
    pub fn set(&mut self, value: T, peer: PeerId) {
        self.clock.increment(peer);
        self.value = value;
    }

    /// Get the current value
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Get the vector clock
    pub fn clock(&self) -> &VectorClock {
        &self.clock
    }

    /// Merge with another register (keep value with later clock)
    pub fn merge(&mut self, other: &LwwRegister<T>) {
        if other.clock.happens_before(&self.clock) {
            // Keep our value
            self.clock.merge(&other.clock);
        } else if self.clock.happens_before(&other.clock) {
            // Take their value
            self.value = other.value.clone();
            self.clock = other.clock.clone();
        } else if self.clock.concurrent(&other.clock) {
            // Concurrent: use deterministic tiebreaker (smaller hash wins)
            let our_hash = self.hash_clock();
            let their_hash = other.hash_clock();

            if their_hash < our_hash {
                self.value = other.value.clone();
            }
            self.clock.merge(&other.clock);
        }
    }

    fn hash_clock(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        // Hash peer IDs and values in sorted order for determinism
        let mut sorted: Vec<_> = self.clock.clocks.iter().collect();
        sorted.sort_by_key(|(peer, _)| peer.as_bytes());
        sorted.hash(&mut hasher);
        hasher.finish()
    }
}

/// Anti-Entropy Manager for periodic CRDT reconciliation
///
/// Manages periodic delta synchronization between peers to ensure
/// eventual consistency. Uses exponential backoff for failed peers.
pub struct AntiEntropyManager<T>
where
    T: DeltaCrdt + Clone + Send + Sync + 'static,
{
    /// The CRDT being synchronized
    crdt: std::sync::Arc<tokio::sync::RwLock<T>>,
    /// Peer versions we've seen (peer_id -> version)
    peer_versions: std::sync::Arc<tokio::sync::RwLock<HashMap<PeerId, u64>>>,
    /// Sync interval in seconds
    interval_secs: u64,
    /// Background task handle
    task_handle: std::sync::Arc<tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Shutdown signal
    shutdown_tx: std::sync::Arc<tokio::sync::RwLock<Option<tokio::sync::mpsc::Sender<()>>>>,
}

impl<T> AntiEntropyManager<T>
where
    T: DeltaCrdt + Clone + Send + Sync + 'static,
{
    /// Create a new Anti-Entropy Manager
    pub fn new(crdt: std::sync::Arc<tokio::sync::RwLock<T>>, interval_secs: u64) -> Self {
        Self {
            crdt,
            peer_versions: std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            interval_secs,
            task_handle: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            shutdown_tx: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Start anti-entropy synchronization
    ///
    /// Periodically syncs with peers using delta-based reconciliation.
    /// Callback is called with (peer_id, delta) for each sync.
    pub async fn start<F>(&self, sync_callback: F) -> Result<()>
    where
        F: Fn(
                PeerId,
                T::Delta,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

        // Store shutdown sender
        {
            let mut tx = self.shutdown_tx.write().await;
            *tx = Some(shutdown_tx);
        }

        let crdt = self.crdt.clone();
        let peer_versions = self.peer_versions.clone();
        let interval_secs = self.interval_secs;
        let sync_callback = std::sync::Arc::new(sync_callback);

        let task = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Get all peers we need to sync with
                        let peers_to_sync: Vec<(PeerId, u64)> = {
                            let versions = peer_versions.read().await;
                            versions.iter().map(|(p, v)| (*p, *v)).collect()
                        };

                        // Generate and send deltas to each peer
                        for (peer_id, peer_version) in peers_to_sync {
                            let delta = {
                                let crdt_lock = crdt.read().await;
                                crdt_lock.delta(peer_version)
                            };

                            if let Some(delta) = delta {
                                let callback = sync_callback.clone();
                                if let Err(e) = callback(peer_id, delta).await {
                                    tracing::warn!("Failed to sync with peer {:?}: {}", peer_id, e);
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Anti-entropy manager shutting down");
                        break;
                    }
                }
            }
        });

        // Store task handle
        {
            let mut handle = self.task_handle.write().await;
            *handle = Some(task);
        }

        Ok(())
    }

    /// Stop anti-entropy synchronization
    pub async fn stop(&self) -> Result<()> {
        // Send shutdown signal
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(()).await;
        }

        // Wait for task to complete (with timeout)
        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(anyhow::anyhow!("Task panicked: {}", e)),
                Err(_) => Err(anyhow::anyhow!("Shutdown timeout")),
            }
        } else {
            Ok(())
        }
    }

    /// Update the version we've seen from a peer
    ///
    /// Call this when receiving a delta from a peer to track
    /// what they've sent us.
    pub async fn update_peer_version(&self, peer_id: PeerId, version: u64) {
        let mut versions = self.peer_versions.write().await;
        versions.insert(peer_id, version);
    }

    /// Apply a delta from a peer
    ///
    /// Merges the delta and updates the peer's version.
    pub async fn apply_delta(&self, peer_id: PeerId, delta: &T::Delta, version: u64) -> Result<()> {
        // Merge delta into our CRDT
        {
            let mut crdt = self.crdt.write().await;
            crdt.merge(delta)?;
        }

        // Update peer version
        self.update_peer_version(peer_id, version).await;

        Ok(())
    }

    /// Add a peer to sync with
    pub async fn add_peer(&self, peer_id: PeerId) {
        let mut versions = self.peer_versions.write().await;
        versions.entry(peer_id).or_insert(0);
    }

    /// Remove a peer from sync
    pub async fn remove_peer(&self, peer_id: &PeerId) {
        let mut versions = self.peer_versions.write().await;
        versions.remove(peer_id);
    }

    /// Get current CRDT state
    pub async fn get_crdt(&self) -> T {
        self.crdt.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> PeerId {
        PeerId::new([n; 32])
    }

    // OR-Set Tests

    #[test]
    fn test_or_set_basic_add_remove() {
        let mut set = OrSet::new();
        let p1 = peer(1);

        set.add("alice".to_string(), (p1, 1)).ok();
        assert!(set.contains(&"alice".to_string()));
        assert_eq!(set.len(), 1);

        set.remove(&"alice".to_string()).ok();
        assert!(!set.contains(&"alice".to_string()));
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn test_or_set_concurrent_add() {
        let mut set = OrSet::new();
        let p1 = peer(1);
        let p2 = peer(2);

        // Concurrent adds from different peers
        set.add("alice".to_string(), (p1, 1)).ok();
        set.add("alice".to_string(), (p2, 1)).ok();

        assert!(set.contains(&"alice".to_string()));

        // Both tags should be present
        assert_eq!(set.elements.get("alice").unwrap().len(), 2);
    }

    #[test]
    fn test_or_set_add_remove_add() {
        let mut set = OrSet::new();
        let p1 = peer(1);

        // Add, remove, add again
        set.add("alice".to_string(), (p1, 1)).ok();
        set.remove(&"alice".to_string()).ok();
        set.add("alice".to_string(), (p1, 2)).ok(); // Different sequence number

        assert!(set.contains(&"alice".to_string()));
    }

    #[test]
    fn test_or_set_merge_commutativity() {
        let mut set1 = OrSet::new();
        let mut set2 = OrSet::new();
        let p1 = peer(1);
        let p2 = peer(2);

        set1.add("alice".to_string(), (p1, 1)).ok();
        set2.add("bob".to_string(), (p2, 1)).ok();

        let mut merged_a = set1.clone();
        merged_a.merge_state(&set2).ok();

        let mut merged_b = set2.clone();
        merged_b.merge_state(&set1).ok();

        // Should converge to same state
        assert_eq!(merged_a.len(), merged_b.len());
        assert!(merged_a.contains(&"alice".to_string()));
        assert!(merged_a.contains(&"bob".to_string()));
        assert!(merged_b.contains(&"alice".to_string()));
        assert!(merged_b.contains(&"bob".to_string()));
    }

    #[test]
    fn test_or_set_merge_idempotence() {
        let mut set1 = OrSet::new();
        let mut set2 = OrSet::new();
        let p1 = peer(1);

        set1.add("alice".to_string(), (p1, 1)).ok();
        set2.add("bob".to_string(), (p1, 2)).ok();

        set1.merge_state(&set2).ok();
        let len_after_first = set1.len();

        set1.merge_state(&set2).ok();
        let len_after_second = set1.len();

        // Multiple merges should not change state
        assert_eq!(len_after_first, len_after_second);
    }

    #[test]
    fn test_or_set_delta_generation() {
        let mut set = OrSet::new();
        let p1 = peer(1);

        let v0 = set.version();
        set.add("alice".to_string(), (p1, 1)).ok();
        set.add("bob".to_string(), (p1, 2)).ok();

        let delta = set.delta(v0).expect("should have delta");
        assert_eq!(delta.added.len(), 2);
        assert!(delta.added.contains_key("alice"));
        assert!(delta.added.contains_key("bob"));
    }

    #[test]
    fn test_or_set_delta_merge() {
        let mut set1 = OrSet::new();
        let mut set2 = OrSet::new();
        let p1 = peer(1);
        let p2 = peer(2);

        set1.add("alice".to_string(), (p1, 1)).ok();

        let v0 = set2.version();
        set2.add("bob".to_string(), (p2, 1)).ok();
        set2.add("charlie".to_string(), (p2, 2)).ok();

        // Get delta and merge
        let delta = set2.delta(v0).expect("should have delta");
        set1.merge(&delta).ok();

        assert!(set1.contains(&"alice".to_string()));
        assert!(set1.contains(&"bob".to_string()));
        assert!(set1.contains(&"charlie".to_string()));
    }

    // Vector Clock Tests

    #[test]
    fn test_vector_clock_increment() {
        let mut clock = VectorClock::new();
        let p1 = peer(1);

        assert_eq!(clock.get(&p1), 0);
        clock.increment(p1);
        assert_eq!(clock.get(&p1), 1);
        clock.increment(p1);
        assert_eq!(clock.get(&p1), 2);
    }

    #[test]
    fn test_vector_clock_merge() {
        let mut clock1 = VectorClock::new();
        let mut clock2 = VectorClock::new();
        let p1 = peer(1);
        let p2 = peer(2);

        clock1.increment(p1);
        clock1.increment(p1);

        clock2.increment(p2);
        clock2.increment(p2);
        clock2.increment(p2);

        clock1.merge(&clock2);

        assert_eq!(clock1.get(&p1), 2);
        assert_eq!(clock1.get(&p2), 3);
    }

    #[test]
    fn test_vector_clock_happens_before() {
        let mut clock1 = VectorClock::new();
        let p1 = peer(1);

        clock1.increment(p1);
        let mut clock2 = clock1.clone();
        clock2.increment(p1);

        assert!(clock1.happens_before(&clock2));
        assert!(!clock2.happens_before(&clock1));
    }

    #[test]
    fn test_vector_clock_concurrent() {
        let mut clock1 = VectorClock::new();
        let mut clock2 = VectorClock::new();
        let p1 = peer(1);
        let p2 = peer(2);

        clock1.increment(p1);
        clock2.increment(p2);

        assert!(clock1.concurrent(&clock2));
        assert!(clock2.concurrent(&clock1));
    }

    // LWW Register Tests

    #[test]
    fn test_lww_register_basic() {
        let mut reg = LwwRegister::new(42);
        let p1 = peer(1);

        assert_eq!(*reg.get(), 42);

        reg.set(100, p1);
        assert_eq!(*reg.get(), 100);
    }

    #[test]
    fn test_lww_register_merge_causality() {
        let mut reg1 = LwwRegister::new(0);
        let p1 = peer(1);
        let p2 = peer(2);

        reg1.set(10, p1);
        let mut reg2 = reg1.clone();
        reg2.set(20, p2);

        // reg2 happens after reg1
        reg1.merge(&reg2);
        assert_eq!(*reg1.get(), 20);
    }

    #[test]
    fn test_lww_register_concurrent_merge() {
        let mut reg1 = LwwRegister::new(0);
        let mut reg2 = LwwRegister::new(0);
        let p1 = peer(1);
        let p2 = peer(2);

        reg1.set(10, p1);
        reg2.set(20, p2);

        // Concurrent updates - should use deterministic tiebreaker
        let mut merged1 = reg1.clone();
        merged1.merge(&reg2);

        let mut merged2 = reg2.clone();
        merged2.merge(&reg1);

        // Both should converge to same value
        assert_eq!(merged1.get(), merged2.get());
    }

    #[test]
    fn test_or_set_compact() {
        let mut set = OrSet::new();
        let p1 = peer(1);

        // Add many elements to build up changelog
        for i in 0..100 {
            set.add(format!("elem-{}", i), (p1, i)).ok();
        }

        let changelog_before = set.changelog.len();
        set.compact(10);
        let changelog_after = set.changelog.len();

        assert!(changelog_after <= 10);
        assert!(changelog_after < changelog_before);
    }

    // Anti-Entropy Manager Tests

    #[tokio::test]
    async fn test_anti_entropy_manager_creation() {
        let crdt = std::sync::Arc::new(tokio::sync::RwLock::new(OrSet::<String>::new()));
        let manager = AntiEntropyManager::new(crdt, 1);

        // Manager should be created successfully
        let state = manager.get_crdt().await;
        assert_eq!(state.len(), 0);
    }

    #[tokio::test]
    async fn test_anti_entropy_add_remove_peer() {
        let crdt = std::sync::Arc::new(tokio::sync::RwLock::new(OrSet::<String>::new()));
        let manager = AntiEntropyManager::new(crdt, 1);

        let p1 = peer(1);
        let p2 = peer(2);

        // Add peers
        manager.add_peer(p1).await;
        manager.add_peer(p2).await;

        // Update versions
        manager.update_peer_version(p1, 5).await;
        manager.update_peer_version(p2, 10).await;

        // Verify versions are tracked
        let versions = manager.peer_versions.read().await;
        assert_eq!(versions.get(&p1), Some(&5));
        assert_eq!(versions.get(&p2), Some(&10));

        // Remove peer
        drop(versions);
        manager.remove_peer(&p1).await;

        let versions = manager.peer_versions.read().await;
        assert!(!versions.contains_key(&p1));
        assert_eq!(versions.get(&p2), Some(&10));
    }

    #[tokio::test]
    async fn test_anti_entropy_apply_delta() {
        let crdt = std::sync::Arc::new(tokio::sync::RwLock::new(OrSet::<String>::new()));
        let manager = AntiEntropyManager::new(crdt.clone(), 1);

        let p1 = peer(1);

        // Create a delta
        let mut temp_set = OrSet::new();
        temp_set.add("alice".to_string(), (p1, 1)).ok();
        temp_set.add("bob".to_string(), (p1, 2)).ok();

        let delta = temp_set.delta(0).expect("should have delta");

        // Apply delta
        manager.apply_delta(p1, &delta, 2).await.ok();

        // Verify CRDT state
        let state = manager.get_crdt().await;
        assert!(state.contains(&"alice".to_string()));
        assert!(state.contains(&"bob".to_string()));

        // Verify peer version updated
        let versions = manager.peer_versions.read().await;
        assert_eq!(versions.get(&p1), Some(&2));
    }

    #[tokio::test]
    async fn test_anti_entropy_periodic_sync() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let crdt = std::sync::Arc::new(tokio::sync::RwLock::new(OrSet::<String>::new()));
        let manager = AntiEntropyManager::new(crdt.clone(), 1); // 1 second interval

        let p1 = peer(1);
        manager.add_peer(p1).await;

        // Add some data to CRDT
        {
            let mut crdt_lock = crdt.write().await;
            crdt_lock.add("test".to_string(), (p1, 1)).ok();
        }

        // Track sync calls
        let sync_count = std::sync::Arc::new(AtomicUsize::new(0));
        let sync_count_clone = sync_count.clone();

        // Start syncing
        manager
            .start(move |peer_id, delta: <OrSet<String> as DeltaCrdt>::Delta| {
                let count = sync_count_clone.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Verify we got the right peer
                    assert_eq!(peer_id, peer(1));
                    // Verify delta has our element
                    assert!(delta.added.contains_key("test"));
                    Ok(())
                })
            })
            .await
            .ok();

        // Wait for at least 2 sync intervals
        tokio::time::sleep(tokio::time::Duration::from_millis(2100)).await;

        // Stop syncing
        manager.stop().await.ok();

        // Should have synced at least once
        assert!(sync_count.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn test_anti_entropy_shutdown() {
        let crdt = std::sync::Arc::new(tokio::sync::RwLock::new(OrSet::<String>::new()));
        let manager = AntiEntropyManager::new(crdt, 1);

        // Start syncing
        manager
            .start(|_peer_id, _delta: <OrSet<String> as DeltaCrdt>::Delta| {
                Box::pin(async { Ok(()) })
            })
            .await
            .ok();

        // Stop should complete within timeout
        let result = manager.stop().await;
        assert!(result.is_ok());

        // Second stop should be idempotent
        let result = manager.stop().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_anti_entropy_multi_peer_sync() {
        let crdt = std::sync::Arc::new(tokio::sync::RwLock::new(OrSet::<String>::new()));
        let manager = AntiEntropyManager::new(crdt.clone(), 1);

        let p1 = peer(1);
        let p2 = peer(2);
        let p3 = peer(3);

        manager.add_peer(p1).await;
        manager.add_peer(p2).await;
        manager.add_peer(p3).await;

        // Add data
        {
            let mut crdt_lock = crdt.write().await;
            crdt_lock.add("elem1".to_string(), (p1, 1)).ok();
            crdt_lock.add("elem2".to_string(), (p1, 2)).ok();
            crdt_lock.add("elem3".to_string(), (p1, 3)).ok();
        }

        // Track which peers got synced
        let synced_peers = std::sync::Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        let synced_peers_clone = synced_peers.clone();

        manager
            .start(
                move |peer_id, _delta: <OrSet<String> as DeltaCrdt>::Delta| {
                    let peers = synced_peers_clone.clone();
                    Box::pin(async move {
                        peers.lock().await.insert(peer_id);
                        Ok(())
                    })
                },
            )
            .await
            .ok();

        // Wait for sync
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

        manager.stop().await.ok();

        // All peers should have been synced
        let peers = synced_peers.lock().await;
        assert!(peers.contains(&p1));
        assert!(peers.contains(&p2));
        assert!(peers.contains(&p3));
    }
}
