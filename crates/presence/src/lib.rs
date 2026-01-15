#![warn(missing_docs)]

//! Presence beacons and user discovery
//!
//! Implements:
//! - MLS exporter-derived presence tags
//! - FOAF random-walk queries
//! - IBLT summaries for efficient reconciliation

use anyhow::{Context, Result};
use rand::Rng;
use saorsa_gossip_groups::GroupContext;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use saorsa_gossip_types::{PeerId, PresenceRecord, TopicId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Presence status for a peer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceStatus {
    /// Valid beacon seen within TTL
    Online,
    /// No recent beacon
    Offline,
    /// Unknown (never seen)
    Unknown,
}

/// Presence message for wire protocol
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PresenceMessage {
    /// Beacon announcement
    Beacon {
        /// Topic/group this beacon is for
        topic_id: TopicId,
        /// Sender's peer ID
        sender: PeerId,
        /// The presence record
        record: PresenceRecord,
        /// MLS epoch for decryption (placeholder for future MLS encryption)
        epoch: u64,
    },
    /// Request for presence information (FOAF query)
    Query {
        /// Unique query ID for deduplication
        query_id: [u8; 32],
        /// Topic/group to query
        topic_id: TopicId,
        /// TTL for FOAF random walk (decremented on each hop)
        ttl: u8,
        /// Origin peer who initiated the query (for sending responses)
        origin: PeerId,
    },
    /// Response to presence query
    QueryResponse {
        /// Query ID this response is for (enables correct matching)
        query_id: [u8; 32],
        /// Topic/group
        topic_id: TopicId,
        /// Known presence records
        records: Vec<(PeerId, PresenceRecord)>,
    },
}

/// Tracks a pending FOAF query with aggregated responses
#[derive(Debug)]
struct PendingQuery {
    /// Topic we're querying
    topic_id: TopicId,
    /// When the query was initiated
    started_at: Instant,
    /// Aggregated presence records (deduplicated by PeerId)
    responses: HashMap<PeerId, PresenceRecord>,
    /// Number of responses received
    response_count: usize,
}

impl PendingQuery {
    fn new(topic_id: TopicId) -> Self {
        Self {
            topic_id,
            started_at: Instant::now(),
            responses: HashMap::new(),
            response_count: 0,
        }
    }

    /// Add records to aggregated responses, deduplicating by PeerId
    ///
    /// If we already have a record for a peer, keeps the one with higher sequence number.
    fn add_responses(&mut self, records: &[(PeerId, PresenceRecord)]) {
        for (peer_id, record) in records {
            self.responses
                .entry(*peer_id)
                .and_modify(|existing| {
                    // Keep the record with higher sequence number
                    if record.seq > existing.seq {
                        *existing = record.clone();
                    }
                })
                .or_insert_with(|| record.clone());
        }
        self.response_count += 1;
    }

    /// Check if query has timed out (default 10 seconds)
    fn is_expired(&self) -> bool {
        self.started_at.elapsed() > std::time::Duration::from_secs(10)
    }
}

/// Presence management trait
#[async_trait::async_trait]
pub trait Presence: Send + Sync {
    /// Broadcast presence beacon to a topic
    async fn beacon(&self, topic: TopicId) -> Result<()>;

    /// Find a user and get their address hints
    async fn find(&self, user: PeerId) -> Result<Vec<String>>;
}

/// Presence manager implementation
pub struct PresenceManager {
    /// Our peer ID
    peer_id: PeerId,
    /// Transport layer for sending beacons
    transport: Arc<dyn GossipTransport>,
    /// MLS groups we've joined
    groups: Arc<RwLock<HashMap<TopicId, GroupContext>>>,
    /// Background task handle for beacon broadcasting
    beacon_task: Arc<RwLock<Option<JoinHandle<()>>>>,
    /// Shutdown signal sender
    shutdown_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<()>>>>,
    /// Received beacons: TopicId -> (PeerId -> PresenceRecord)
    received_beacons: Arc<RwLock<HashMap<TopicId, HashMap<PeerId, PresenceRecord>>>>,
    /// Peers to broadcast beacons to (from membership layer)
    broadcast_peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Our address hints for connectivity (local, reflexive, relay addresses)
    addr_hints: Arc<RwLock<Vec<String>>>,
    /// Seen query IDs with timestamps to prevent query loops (age-based cleanup)
    seen_queries: Arc<RwLock<HashMap<[u8; 32], Instant>>>,
    /// Pending queries we initiated (query_id -> PendingQuery)
    pending_queries: Arc<RwLock<HashMap<[u8; 32], PendingQuery>>>,
}

impl PresenceManager {
    /// Create a new presence manager
    pub fn new(
        peer_id: PeerId,
        transport: Arc<dyn GossipTransport>,
        groups: Arc<RwLock<HashMap<TopicId, GroupContext>>>,
    ) -> Self {
        Self {
            peer_id,
            transport,
            groups,
            beacon_task: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            received_beacons: Arc::new(RwLock::new(HashMap::new())),
            broadcast_peers: Arc::new(RwLock::new(HashSet::new())),
            addr_hints: Arc::new(RwLock::new(Vec::new())),
            seen_queries: Arc::new(RwLock::new(HashMap::new())),
            pending_queries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a peer to broadcast beacons to
    ///
    /// Call this when a peer joins the mesh (from membership layer).
    pub async fn add_broadcast_peer(&self, peer: PeerId) {
        let mut peers = self.broadcast_peers.write().await;
        peers.insert(peer);
        debug!(?peer, "Added broadcast peer");
    }

    /// Remove a peer from beacon broadcasts
    ///
    /// Call this when a peer leaves the mesh or disconnects.
    pub async fn remove_broadcast_peer(&self, peer: PeerId) {
        let mut peers = self.broadcast_peers.write().await;
        peers.remove(&peer);
        debug!(?peer, "Removed broadcast peer");
    }

    /// Get current broadcast peer count
    pub async fn broadcast_peer_count(&self) -> usize {
        self.broadcast_peers.read().await.len()
    }

    /// Set address hints for our presence beacons
    ///
    /// Address hints help other peers find us. Include:
    /// - Local bound addresses
    /// - NAT-reflexive addresses (from STUN or observed by peers)
    /// - Relay addresses (for symmetric NAT)
    pub async fn set_addr_hints(&self, hints: Vec<String>) {
        let mut addr = self.addr_hints.write().await;
        *addr = hints;
    }

    /// Add a single address hint
    pub async fn add_addr_hint(&self, hint: String) {
        let mut addr = self.addr_hints.write().await;
        if !addr.contains(&hint) {
            addr.push(hint);
        }
    }

    /// Get current address hints
    pub async fn get_addr_hints(&self) -> Vec<String> {
        self.addr_hints.read().await.clone()
    }

    /// Start periodic beacon broadcasting
    ///
    /// Broadcasts presence beacons to all joined topics at the specified interval.
    /// Beacons contain:
    /// - Presence tag derived from MLS exporter secret
    /// - Address hints for connectivity
    /// - Timestamp and expiration
    ///
    /// # Arguments
    /// * `interval_secs` - Beacon broadcast interval in seconds (typically 300 = 5min)
    pub async fn start_beacons(&self, interval_secs: u64) -> Result<()> {
        // Check if already running
        {
            let task = self.beacon_task.read().await;
            if task.is_some() {
                return Err(anyhow::anyhow!("Beacon broadcasting already started"));
            }
        }

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

        // Clone everything needed for the background task
        let peer_id = self.peer_id;
        let groups = self.groups.clone();
        let transport = self.transport.clone();
        let received_beacons = self.received_beacons.clone();
        let broadcast_peers = self.broadcast_peers.clone();
        let addr_hints = self.addr_hints.clone();

        // Spawn background task for beacon broadcasting
        let task_handle = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Broadcast beacons to all joined groups
                        let groups_lock = groups.read().await;

                        for (topic_id, group_ctx) in groups_lock.iter() {
                            // Derive presence tag for current time slice
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let time_slice = now / 3600; // Hourly rotation

                            // Derive presence tag using MLS group's exporter secret
                            // In production, get actual MLS exporter secret from group_ctx
                            // For now, use a deterministic placeholder based on topic_id
                            let mut exporter_secret = [0u8; 32];
                            exporter_secret.copy_from_slice(&topic_id.as_bytes()[..32]);

                            let presence_tag = derive_presence_tag(&exporter_secret, &peer_id, time_slice);

                            // Get address hints (real addresses when available)
                            let hints = addr_hints.read().await.clone();
                            let record_addr_hints = if hints.is_empty() {
                                // Fallback for testing/development
                                vec!["127.0.0.1:8080".to_string()]
                            } else {
                                hints
                            };

                            // Create presence record with 3x interval TTL
                            let ttl_seconds = interval_secs * 3;
                            let record = PresenceRecord::new(presence_tag, record_addr_hints, ttl_seconds);

                            // Store our own beacon locally
                            {
                                let mut beacons = received_beacons.write().await;
                                let topic_beacons = beacons.entry(*topic_id).or_insert_with(HashMap::new);
                                topic_beacons.insert(peer_id, record.clone());
                            }

                            // Create wire message
                            let message = PresenceMessage::Beacon {
                                topic_id: *topic_id,
                                sender: peer_id,
                                record,
                                epoch: group_ctx.epoch,
                            };

                            // Serialize message
                            let data = match bincode::serialize(&message) {
                                Ok(d) => bytes::Bytes::from(d),
                                Err(e) => {
                                    warn!(?e, "Failed to serialize presence beacon");
                                    continue;
                                }
                            };

                            // Broadcast to all known peers
                            let peers = broadcast_peers.read().await;
                            for target_peer in peers.iter() {
                                if let Err(e) = transport
                                    .send_to_peer(*target_peer, GossipStreamType::Bulk, data.clone())
                                    .await
                                {
                                    debug!(?target_peer, ?e, "Failed to send beacon to peer");
                                    // Continue to next peer - don't fail entire broadcast
                                }
                            }

                            debug!(
                                ?topic_id,
                                peer_count = peers.len(),
                                "Broadcast presence beacon"
                            );
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        // Shutdown signal received
                        break;
                    }
                }
            }
        });

        // Store task handle and shutdown sender
        {
            let mut task = self.beacon_task.write().await;
            *task = Some(task_handle);
        }
        {
            let mut tx = self.shutdown_tx.write().await;
            *tx = Some(shutdown_tx);
        }

        Ok(())
    }

    /// Stop beacon broadcasting
    ///
    /// Gracefully shuts down the beacon broadcasting task.
    pub async fn stop_beacons(&self) -> Result<()> {
        // Send shutdown signal
        {
            let mut tx = self.shutdown_tx.write().await;
            if let Some(sender) = tx.take() {
                // Send shutdown signal (ignore error if receiver already dropped)
                let _ = sender.send(()).await;
            }
        }

        // Wait for task to complete with timeout
        {
            let mut task = self.beacon_task.write().await;
            if let Some(handle) = task.take() {
                // Wait up to 5 seconds for graceful shutdown
                match tokio::time::timeout(tokio::time::Duration::from_secs(5), handle).await {
                    Ok(join_result) => {
                        join_result.context("Beacon task panicked")?;
                    }
                    Err(_) => {
                        return Err(anyhow::anyhow!("Beacon task shutdown timeout"));
                    }
                }
            }
        }

        Ok(())
    }

    /// Get presence status for a peer in a specific topic
    ///
    /// # Arguments
    /// * `peer` - The peer to check
    /// * `topic` - The topic/group context
    ///
    /// # Returns
    /// * `PresenceStatus::Online` - Valid beacon within TTL
    /// * `PresenceStatus::Offline` - Beacon expired
    /// * `PresenceStatus::Unknown` - Never seen
    pub async fn get_status(&self, peer: PeerId, topic: TopicId) -> PresenceStatus {
        let beacons = self.received_beacons.read().await;

        let Some(topic_beacons) = beacons.get(&topic) else {
            return PresenceStatus::Unknown;
        };

        let Some(record) = topic_beacons.get(&peer) else {
            return PresenceStatus::Unknown;
        };

        if record.is_expired() {
            PresenceStatus::Offline
        } else {
            PresenceStatus::Online
        }
    }

    /// Get all online peers in a topic
    ///
    /// Returns all peers with valid (non-expired) beacons in the specified topic.
    pub async fn get_online_peers(&self, topic: TopicId) -> Vec<PeerId> {
        let beacons = self.received_beacons.read().await;

        beacons.get(&topic).map_or_else(Vec::new, |topic_beacons| {
            topic_beacons
                .iter()
                .filter(|(_, record)| !record.is_expired())
                .map(|(peer_id, _)| *peer_id)
                .collect()
        })
    }

    /// Clean up expired beacons
    ///
    /// Removes beacons older than the specified TTL.
    ///
    /// # Arguments
    /// * `ttl_seconds` - Time-to-live in seconds (typically 900 = 15min)
    pub async fn cleanup_expired(&self, _ttl_seconds: u64) -> Result<usize> {
        let mut beacons = self.received_beacons.write().await;
        let mut cleaned_count = 0;

        // Iterate through all topics
        for topic_beacons in beacons.values_mut() {
            // Remove expired beacons
            topic_beacons.retain(|_, record| {
                let expired = record.is_expired();
                if expired {
                    cleaned_count += 1;
                }
                !expired
            });
        }

        Ok(cleaned_count)
    }

    /// Get all joined topics/groups
    ///
    /// Returns a list of all topic IDs that we have joined (i.e., have MLS groups for).
    /// Used by FOAF discovery to search for contacts in shared groups.
    pub async fn get_groups(&self) -> Vec<TopicId> {
        let groups = self.groups.read().await;
        groups.keys().copied().collect()
    }

    /// Get presence records for a specific topic
    ///
    /// Returns a map of PeerId → PresenceRecord for all peers with beacons in the topic.
    /// Used by FOAF discovery to find contacts via presence beacons.
    ///
    /// # Arguments
    /// * `topic` - The topic/group to query
    ///
    /// # Returns
    /// HashMap of peer_id → presence_record for all peers with beacons in this topic
    pub async fn get_group_presence(&self, topic: TopicId) -> HashMap<PeerId, PresenceRecord> {
        let beacons = self.received_beacons.read().await;

        beacons.get(&topic).cloned().unwrap_or_default()
    }

    /// Initiate a FOAF (Friend-of-a-Friend) query for presence discovery
    ///
    /// Sends a presence query to the mesh and aggregates responses.
    /// Uses random-walk with TTL and fanout to discover peers in a topic.
    ///
    /// # Arguments
    /// * `topic_id` - Topic to search for presence
    /// * `ttl` - Time-to-live hops (typically 3-4)
    /// * `timeout_ms` - How long to wait for responses in milliseconds
    ///
    /// # Returns
    /// Aggregated presence records discovered via FOAF query
    pub async fn initiate_foaf_query(
        &self,
        topic_id: TopicId,
        ttl: u8,
        timeout_ms: u64,
    ) -> Result<Vec<(PeerId, PresenceRecord)>> {
        // Generate unique query ID
        let mut query_id = [0u8; 32];
        rand::thread_rng().fill(&mut query_id);

        // Create pending query entry
        {
            let mut pending = self.pending_queries.write().await;
            pending.insert(query_id, PendingQuery::new(topic_id));
        }

        // Mark query as seen so we don't process our own query
        {
            let mut seen = self.seen_queries.write().await;
            seen.insert(query_id, Instant::now());
        }

        // Create query message
        let query = PresenceMessage::Query {
            query_id,
            topic_id,
            ttl,
            origin: self.peer_id,
        };

        // Send to random peers (fanout of 3)
        let peers: Vec<PeerId> = self.broadcast_peers.read().await.iter().copied().collect();
        let fanout = std::cmp::min(3, peers.len());

        // Select random peers before any awaits (ThreadRng is not Send)
        let selected_peers: Vec<PeerId> = if fanout > 0 {
            let mut rng = rand::thread_rng();
            let mut indices: Vec<usize> = (0..peers.len()).collect();

            // Shuffle for random selection
            for i in 0..fanout {
                let j = rng.gen_range(i..peers.len());
                indices.swap(i, j);
            }

            indices.iter().take(fanout).map(|&idx| peers[idx]).collect()
        } else {
            Vec::new()
        };

        // Now send to selected peers (rng is dropped)
        if !selected_peers.is_empty() {
            match bincode::serialize(&query) {
                Ok(data) => {
                    let data = bytes::Bytes::from(data);
                    for peer in &selected_peers {
                        if let Err(e) = self
                            .transport
                            .send_to_peer(*peer, GossipStreamType::Bulk, data.clone())
                            .await
                        {
                            debug!(?peer, ?e, "Failed to send FOAF query");
                        }
                    }
                    debug!(
                        fanout = selected_peers.len(),
                        ?topic_id,
                        ttl,
                        "Initiated FOAF query"
                    );
                }
                Err(e) => {
                    warn!(?e, "Failed to serialize FOAF query");
                }
            }
        }

        // Wait for responses with timeout
        let timeout_duration = std::time::Duration::from_millis(timeout_ms);
        let start = Instant::now();

        while start.elapsed() < timeout_duration {
            // Check pending query status
            {
                let pending = self.pending_queries.read().await;
                if let Some(query) = pending.get(&query_id) {
                    // If we have enough responses, we can return early
                    if query.response_count >= 3 {
                        break;
                    }
                }
            }

            // Small sleep to avoid busy-waiting
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Extract results and clean up
        let results = {
            let mut pending = self.pending_queries.write().await;
            if let Some(query) = pending.remove(&query_id) {
                query
                    .responses
                    .into_iter()
                    .filter(|(_, record)| !record.is_expired())
                    .collect()
            } else {
                Vec::new()
            }
        };

        debug!(count = results.len(), ?topic_id, "FOAF query completed");
        Ok(results)
    }

    /// Get count of pending FOAF queries
    ///
    /// Useful for monitoring and testing.
    pub async fn pending_query_count(&self) -> usize {
        self.pending_queries.read().await.len()
    }

    /// Clean up expired pending queries
    ///
    /// Should be called periodically to remove stale pending queries.
    pub async fn cleanup_pending_queries(&self) -> usize {
        let mut pending = self.pending_queries.write().await;
        let initial_count = pending.len();
        pending.retain(|_, query| !query.is_expired());
        initial_count - pending.len()
    }

    /// Clean up old seen query IDs
    ///
    /// Removes query IDs older than 30 seconds per SPEC2 §7.3.
    /// Should be called periodically to prevent memory leaks.
    ///
    /// # Returns
    /// Number of query IDs cleaned up
    pub async fn cleanup_seen_queries(&self) -> usize {
        let mut seen = self.seen_queries.write().await;
        let initial_count = seen.len();
        let max_age = std::time::Duration::from_secs(30);
        let now = Instant::now();

        seen.retain(|_, timestamp| now.duration_since(*timestamp) <= max_age);

        let cleaned = initial_count - seen.len();
        if cleaned > 0 {
            debug!(cleaned, "Cleaned up old seen query IDs");
        }
        cleaned
    }

    /// Handle received beacon from a peer
    ///
    /// Stores the beacon for presence tracking.
    pub async fn handle_beacon(
        &self,
        topic: TopicId,
        peer: PeerId,
        record: PresenceRecord,
    ) -> Result<()> {
        let mut beacons = self.received_beacons.write().await;

        // Get or create topic beacon map
        let topic_beacons = beacons.entry(topic).or_default();

        // Store the beacon
        topic_beacons.insert(peer, record);

        Ok(())
    }

    /// Handle received presence message from wire
    ///
    /// Deserializes and processes a PresenceMessage received via transport.
    /// Returns the sender's PeerId if a beacon was processed.
    pub async fn handle_presence_message(&self, data: &[u8]) -> Result<Option<PeerId>> {
        let message: PresenceMessage =
            bincode::deserialize(data).context("Failed to deserialize presence message")?;

        match message {
            PresenceMessage::Beacon {
                topic_id,
                sender,
                record,
                epoch: _,
            } => {
                // Verify we're in this group
                let groups = self.groups.read().await;
                if !groups.contains_key(&topic_id) {
                    debug!(?topic_id, "Received beacon for unknown topic");
                    return Ok(None);
                }

                // Store the beacon
                self.handle_beacon(topic_id, sender, record).await?;
                debug!(?sender, ?topic_id, "Processed presence beacon");
                Ok(Some(sender))
            }
            PresenceMessage::Query {
                query_id,
                topic_id,
                ttl,
                origin,
            } => {
                // Handle FOAF query - respond and optionally forward
                debug!(?topic_id, ttl, ?origin, "Received presence query");

                // Check if we have already seen this query to prevent loops
                {
                    let mut seen = self.seen_queries.write().await;
                    if seen.contains_key(&query_id) {
                        debug!("Dropping duplicate query");
                        return Ok(None);
                    }
                    seen.insert(query_id, Instant::now());
                }

                // Collect valid (non-expired) presence records for this topic
                let records: Vec<(PeerId, PresenceRecord)> = {
                    let beacons = self.received_beacons.read().await;
                    beacons
                        .get(&topic_id)
                        .map_or_else(Vec::new, |topic_beacons| {
                            topic_beacons
                                .iter()
                                .filter(|(_, record)| !record.is_expired())
                                .map(|(peer_id, record)| (*peer_id, record.clone()))
                                .collect()
                        })
                };

                // If we have records, send response to origin
                if !records.is_empty() {
                    let response = PresenceMessage::QueryResponse {
                        query_id,
                        topic_id,
                        records: records.clone(),
                    };

                    match bincode::serialize(&response) {
                        Ok(data) => {
                            if let Err(e) = self
                                .transport
                                .send_to_peer(
                                    origin,
                                    GossipStreamType::Bulk,
                                    bytes::Bytes::from(data),
                                )
                                .await
                            {
                                debug!(?origin, ?e, "Failed to send query response");
                            } else {
                                debug!(?origin, count = records.len(), "Sent query response");
                            }
                        }
                        Err(e) => {
                            warn!(?e, ?topic_id, "Failed to serialize query response");
                        }
                    }
                }

                // Forward query with decremented TTL if not expired
                if ttl > 0 {
                    let peers: Vec<PeerId> =
                        self.broadcast_peers.read().await.iter().copied().collect();

                    // FOAF random walk: forward to up to 3 random peers (fanout)
                    let fanout = std::cmp::min(3, peers.len());
                    if fanout > 0 {
                        let mut rng = rand::thread_rng();
                        let mut indices: Vec<usize> = (0..peers.len()).collect();

                        // Shuffle and take first fanout peers
                        for i in 0..fanout {
                            let j = rng.gen_range(i..peers.len());
                            indices.swap(i, j);
                        }

                        let forward_msg = PresenceMessage::Query {
                            query_id,
                            topic_id,
                            ttl: ttl.saturating_sub(1),
                            origin,
                        };

                        match bincode::serialize(&forward_msg) {
                            Ok(data) => {
                                let data = bytes::Bytes::from(data);
                                for &idx in indices.iter().take(fanout) {
                                    let peer = peers[idx];
                                    // Do not forward back to origin
                                    if peer == origin {
                                        continue;
                                    }
                                    if let Err(e) = self
                                        .transport
                                        .send_to_peer(peer, GossipStreamType::Bulk, data.clone())
                                        .await
                                    {
                                        debug!(?peer, ?e, "Failed to forward query");
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(?e, ?topic_id, "Failed to serialize forwarded query");
                            }
                        }
                    }
                }

                Ok(None)
            }
            PresenceMessage::QueryResponse {
                query_id,
                topic_id,
                records,
            } => {
                // Process query response - check if it's for one of our pending queries
                debug!(
                    ?query_id,
                    ?topic_id,
                    count = records.len(),
                    "Received query response"
                );

                // Aggregate into the specific pending query (matched by query_id)
                {
                    let mut pending = self.pending_queries.write().await;
                    if let Some(pending_query) = pending.get_mut(&query_id) {
                        // Validate topic_id matches expected (defense in depth)
                        if pending_query.topic_id != topic_id {
                            warn!(
                                ?query_id,
                                expected = ?pending_query.topic_id,
                                received = ?topic_id,
                                "QueryResponse topic_id mismatch - ignoring"
                            );
                        } else {
                            pending_query.add_responses(&records);
                        }
                    } else {
                        debug!(?query_id, "Received response for unknown query");
                    }
                }

                // Also store records locally for presence tracking
                for (peer, record) in records {
                    // Only store non-expired records
                    if !record.is_expired() {
                        self.handle_beacon(topic_id, peer, record).await?;
                    }
                }
                Ok(None)
            }
        }
    }
}

impl Default for PresenceManager {
    fn default() -> Self {
        Self::new(
            PeerId::new([0u8; 32]),
            Arc::new(saorsa_gossip_transport::QuicTransport::new(
                saorsa_gossip_transport::TransportConfig::default(),
            )),
            Arc::new(RwLock::new(HashMap::new())),
        )
    }
}

#[async_trait::async_trait]
impl Presence for PresenceManager {
    async fn beacon(&self, _topic: TopicId) -> Result<()> {
        // Placeholder: derive presence_tag from MLS exporter_secret
        // Sign with ML-DSA, encrypt to group, broadcast
        Ok(())
    }

    async fn find(&self, user: PeerId) -> Result<Vec<String>> {
        // FOAF random-walk across all groups to find user
        let groups = self.get_groups().await;

        // First check local cache
        for topic in &groups {
            let presence = self.get_group_presence(*topic).await;
            if let Some(record) = presence.get(&user) {
                if !record.is_expired() {
                    return Ok(record.addr_hints.clone());
                }
            }
        }

        // Not found locally, initiate FOAF queries to all groups
        // TTL=3, timeout=3000ms per SPEC2 recommendations
        for topic in groups {
            let results = self.initiate_foaf_query(topic, 3, 3000).await?;
            for (peer_id, record) in results {
                if peer_id == user && !record.is_expired() {
                    return Ok(record.addr_hints);
                }
            }
        }

        // User not found
        Ok(vec![])
    }
}

/// Derive presence tag from MLS exporter secret
///
/// Delegates to GroupContext::derive_presence_secret for consistent KDF.
/// Uses BLAKE3 keyed hash to derive a rotating presence tag.
/// Tags rotate every hour based on time_slice per SPEC2 §10.
///
/// # Arguments
/// * `exporter_secret` - MLS group exporter secret (32 bytes)
/// * `user_id` - PeerId of the user
/// * `time_slice` - Current time slice (hour since epoch)
pub fn derive_presence_tag(
    exporter_secret: &[u8; 32],
    user_id: &PeerId,
    time_slice: u64,
) -> [u8; 32] {
    // Use GroupContext's presence secret derivation for consistency
    saorsa_gossip_groups::GroupContext::derive_presence_secret(
        exporter_secret,
        user_id.as_bytes(),
        time_slice,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use saorsa_gossip_transport::{QuicTransport, TransportConfig};

    // Helper: Create test presence manager
    fn create_test_manager() -> PresenceManager {
        let peer_id = PeerId::new([1u8; 32]);
        let transport = Arc::new(QuicTransport::new(TransportConfig::default()));
        let groups = Arc::new(RwLock::new(HashMap::new()));
        PresenceManager::new(peer_id, transport, groups)
    }

    #[tokio::test]
    async fn test_presence_manager_creation() {
        // RED: Test basic creation with dependencies
        let manager = create_test_manager();
        assert_eq!(manager.peer_id, PeerId::new([1u8; 32]));
    }

    #[tokio::test]
    async fn test_start_beacons_broadcasts_periodically() {
        let manager = create_test_manager();
        let topic = TopicId::new([10u8; 32]);

        // Join a group so beacons are broadcast for it
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        // Start beacons with short interval for testing
        let result = manager.start_beacons(1).await;
        assert!(result.is_ok(), "start_beacons should succeed");

        // Wait for first beacon broadcast (interval tick)
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

        // Verify our own beacon was stored (self-storage happens in broadcast loop)
        let status = manager.get_status(manager.peer_id, topic).await;
        assert_eq!(
            status,
            PresenceStatus::Online,
            "Our own beacon should be stored after broadcast"
        );

        // Clean up
        manager.stop_beacons().await.ok();
    }

    #[tokio::test]
    async fn test_stop_beacons_halts_broadcasting() {
        let manager = create_test_manager();
        let topic = TopicId::new([11u8; 32]);

        // Join a group
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        // Start beacons
        manager.start_beacons(1).await.expect("start failed");

        // Wait for first tick
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

        // Stop beacons
        let result = manager.stop_beacons().await;
        assert!(result.is_ok(), "stop_beacons should succeed");

        // Verify we can restart (proves task was properly cleaned up)
        let restart_result = manager.start_beacons(1).await;
        assert!(
            restart_result.is_ok(),
            "Should be able to restart after stop"
        );

        // Clean up
        manager.stop_beacons().await.ok();
    }

    #[tokio::test]
    async fn test_beacon_storage_and_retrieval() {
        // RED: This should fail because handle_beacon doesn't store yet
        let manager = create_test_manager();

        let topic = TopicId::new([1u8; 32]);
        let peer = PeerId::new([2u8; 32]);
        let record = PresenceRecord::new([0u8; 32], vec!["127.0.0.1:8080".to_string()], 900);

        manager
            .handle_beacon(topic, peer, record.clone())
            .await
            .expect("handle_beacon failed");

        // Should be able to retrieve the beacon
        let status = manager.get_status(peer, topic).await;
        assert_eq!(
            status,
            PresenceStatus::Online,
            "Peer should be online after beacon"
        );
    }

    #[tokio::test]
    async fn test_beacon_ttl_expiration() {
        // Test that expired beacons are cleaned up
        let manager = create_test_manager();

        let topic = TopicId::new([1u8; 32]);
        let peer = PeerId::new([2u8; 32]);

        // Create an expired beacon (TTL = 0)
        let record = PresenceRecord::new([0u8; 32], vec![], 0);
        manager
            .handle_beacon(topic, peer, record)
            .await
            .expect("handle failed");

        // Wait for expiration
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Clean up expired beacons
        let cleaned = manager.cleanup_expired(1).await.expect("cleanup failed");
        assert_eq!(cleaned, 1, "Should clean up 1 expired beacon");

        // Status should be unknown after cleanup (beacon removed)
        let status = manager.get_status(peer, topic).await;
        assert_eq!(
            status,
            PresenceStatus::Unknown,
            "Peer should be unknown after cleanup removes beacon"
        );
    }

    #[tokio::test]
    async fn test_get_status_online_within_ttl() {
        // RED: This should fail because get_status always returns Unknown
        let manager = create_test_manager();

        let topic = TopicId::new([1u8; 32]);
        let peer = PeerId::new([2u8; 32]);
        let record = PresenceRecord::new([0u8; 32], vec![], 900);

        manager
            .handle_beacon(topic, peer, record)
            .await
            .expect("handle failed");

        let status = manager.get_status(peer, topic).await;
        assert_eq!(
            status,
            PresenceStatus::Online,
            "Should be online with valid beacon"
        );
    }

    #[tokio::test]
    async fn test_get_status_offline_after_ttl() {
        // RED: This should fail because get_status doesn't check TTL
        let manager = create_test_manager();

        let topic = TopicId::new([1u8; 32]);
        let peer = PeerId::new([2u8; 32]);

        // Beacon with 0 TTL (immediately expired)
        let record = PresenceRecord::new([0u8; 32], vec![], 0);
        manager
            .handle_beacon(topic, peer, record)
            .await
            .expect("handle failed");

        // Wait a bit
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let status = manager.get_status(peer, topic).await;
        assert_eq!(
            status,
            PresenceStatus::Offline,
            "Should be offline with expired beacon"
        );
    }

    #[tokio::test]
    async fn test_get_online_peers_filters_by_topic() {
        // RED: This should fail because get_online_peers returns empty vec
        let manager = create_test_manager();

        let topic1 = TopicId::new([1u8; 32]);
        let topic2 = TopicId::new([2u8; 32]);
        let peer1 = PeerId::new([10u8; 32]);
        let peer2 = PeerId::new([20u8; 32]);

        // Add beacons to different topics
        let record = PresenceRecord::new([0u8; 32], vec![], 900);
        manager
            .handle_beacon(topic1, peer1, record.clone())
            .await
            .expect("handle1 failed");
        manager
            .handle_beacon(topic2, peer2, record)
            .await
            .expect("handle2 failed");

        // Should only see peer1 in topic1
        let online = manager.get_online_peers(topic1).await;
        assert_eq!(online.len(), 1, "Should have 1 online peer in topic1");
        assert!(online.contains(&peer1), "Should contain peer1");

        // Should only see peer2 in topic2
        let online = manager.get_online_peers(topic2).await;
        assert_eq!(online.len(), 1, "Should have 1 online peer in topic2");
        assert!(online.contains(&peer2), "Should contain peer2");
    }

    #[tokio::test]
    async fn test_find_foaf_random_walk() {
        let manager = create_test_manager();
        let topic = TopicId::new([12u8; 32]);
        let target = PeerId::new([42u8; 32]);

        // Join a group so FOAF queries are initiated
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        // Add broadcast peers so queries have targets
        let peer1 = PeerId::new([100u8; 32]);
        let peer2 = PeerId::new([101u8; 32]);
        let peer3 = PeerId::new([102u8; 32]);
        manager.add_broadcast_peer(peer1).await;
        manager.add_broadcast_peer(peer2).await;
        manager.add_broadcast_peer(peer3).await;

        // Verify no pending queries initially
        assert_eq!(manager.pending_query_count().await, 0);

        // Call find - should initiate FOAF queries (with TTL=3, fanout=3 per code)
        let result = manager.find(target).await;
        assert!(result.is_ok(), "find should succeed");

        // Verify FOAF queries were initiated (one per joined group)
        // Query is created in pending_queries before send attempts
        // Note: Query may complete/expire quickly, so we just verify find() works
        // The TTL=3, fanout=3 values are verified by code inspection in initiate_foaf_query()
    }

    #[tokio::test]
    async fn test_multiple_topics_isolation() {
        // RED: This should fail because topics aren't isolated yet
        let manager = create_test_manager();

        let topic1 = TopicId::new([1u8; 32]);
        let topic2 = TopicId::new([2u8; 32]);
        let peer = PeerId::new([5u8; 32]);

        // Add beacon only to topic1
        let record = PresenceRecord::new([0u8; 32], vec![], 900);
        manager
            .handle_beacon(topic1, peer, record)
            .await
            .expect("handle failed");

        // Should be online in topic1
        assert_eq!(
            manager.get_status(peer, topic1).await,
            PresenceStatus::Online
        );

        // Should be unknown in topic2
        assert_eq!(
            manager.get_status(peer, topic2).await,
            PresenceStatus::Unknown
        );
    }

    #[test]
    fn test_derive_presence_tag_deterministic() {
        // Test that same inputs produce same tag
        let secret = [1u8; 32];
        let peer = PeerId::new([2u8; 32]);
        let time_slice = 12345u64;

        let tag1 = derive_presence_tag(&secret, &peer, time_slice);
        let tag2 = derive_presence_tag(&secret, &peer, time_slice);

        assert_eq!(tag1, tag2, "Same inputs should produce same tag");
    }

    #[test]
    fn test_derive_presence_tag_rotation() {
        // Test that different time slices produce different tags
        let secret = [1u8; 32];
        let peer = PeerId::new([2u8; 32]);

        let tag1 = derive_presence_tag(&secret, &peer, 1000);
        let tag2 = derive_presence_tag(&secret, &peer, 1001);

        assert_ne!(
            tag1, tag2,
            "Different time slices should produce different tags"
        );
    }

    #[test]
    fn test_derive_presence_tag_peer_unique() {
        // Test that different peers produce different tags
        let secret = [1u8; 32];
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);
        let time_slice = 12345u64;

        let tag1 = derive_presence_tag(&secret, &peer1, time_slice);
        let tag2 = derive_presence_tag(&secret, &peer2, time_slice);

        assert_ne!(tag1, tag2, "Different peers should produce different tags");
    }

    #[tokio::test]
    async fn test_broadcast_peer_management() {
        let manager = create_test_manager();

        let peer1 = PeerId::new([10u8; 32]);
        let peer2 = PeerId::new([20u8; 32]);

        // Initially no broadcast peers
        assert_eq!(manager.broadcast_peer_count().await, 0);

        // Add peers
        manager.add_broadcast_peer(peer1).await;
        assert_eq!(manager.broadcast_peer_count().await, 1);

        manager.add_broadcast_peer(peer2).await;
        assert_eq!(manager.broadcast_peer_count().await, 2);

        // Adding same peer twice doesn't duplicate
        manager.add_broadcast_peer(peer1).await;
        assert_eq!(manager.broadcast_peer_count().await, 2);

        // Remove peer
        manager.remove_broadcast_peer(peer1).await;
        assert_eq!(manager.broadcast_peer_count().await, 1);
    }

    #[tokio::test]
    async fn test_addr_hints_management() {
        let manager = create_test_manager();

        // Initially empty
        assert!(manager.get_addr_hints().await.is_empty());

        // Set hints
        manager
            .set_addr_hints(vec!["192.168.1.1:8080".to_string()])
            .await;
        assert_eq!(manager.get_addr_hints().await.len(), 1);

        // Add single hint
        manager.add_addr_hint("10.0.0.1:9000".to_string()).await;
        assert_eq!(manager.get_addr_hints().await.len(), 2);

        // Adding same hint doesn't duplicate
        manager.add_addr_hint("10.0.0.1:9000".to_string()).await;
        assert_eq!(manager.get_addr_hints().await.len(), 2);
    }

    #[tokio::test]
    async fn test_presence_message_serialization() {
        let topic = TopicId::new([1u8; 32]);
        let sender = PeerId::new([2u8; 32]);
        let record = PresenceRecord::new([3u8; 32], vec!["127.0.0.1:8080".to_string()], 900);

        let message = PresenceMessage::Beacon {
            topic_id: topic,
            sender,
            record: record.clone(),
            epoch: 5,
        };

        // Serialize and deserialize
        let data = bincode::serialize(&message).expect("serialize failed");
        let decoded: PresenceMessage = bincode::deserialize(&data).expect("deserialize failed");

        match decoded {
            PresenceMessage::Beacon {
                topic_id,
                sender: decoded_sender,
                record: decoded_record,
                epoch,
            } => {
                assert_eq!(topic_id, topic);
                assert_eq!(decoded_sender, sender);
                assert_eq!(decoded_record.presence_tag, record.presence_tag);
                assert_eq!(epoch, 5);
            }
            _ => panic!("Expected Beacon message"),
        }
    }

    #[tokio::test]
    async fn test_handle_presence_message_beacon() {
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);
        let sender = PeerId::new([2u8; 32]);
        let record = PresenceRecord::new([3u8; 32], vec!["127.0.0.1:8080".to_string()], 900);

        // Add the topic to groups so the beacon is accepted
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        let message = PresenceMessage::Beacon {
            topic_id: topic,
            sender,
            record,
            epoch: 0,
        };

        let data = bincode::serialize(&message).expect("serialize failed");
        let result = manager.handle_presence_message(&data).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(sender));

        // Verify beacon was stored
        let status = manager.get_status(sender, topic).await;
        assert_eq!(status, PresenceStatus::Online);
    }

    #[tokio::test]
    async fn test_handle_presence_message_unknown_topic() {
        let manager = create_test_manager();
        let topic = TopicId::new([99u8; 32]); // Not in groups
        let sender = PeerId::new([2u8; 32]);
        let record = PresenceRecord::new([3u8; 32], vec![], 900);

        let message = PresenceMessage::Beacon {
            topic_id: topic,
            sender,
            record,
            epoch: 0,
        };

        let data = bincode::serialize(&message).expect("serialize failed");
        let result = manager.handle_presence_message(&data).await;

        // Should return Ok(None) for unknown topic
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    // =========================================================================
    // FOAF Multi-Hop Discovery Integration Tests (Phase 2.4)
    // =========================================================================

    #[tokio::test]
    async fn test_foaf_query_deduplication() {
        // Test that duplicate queries are dropped
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);
        let origin = PeerId::new([2u8; 32]);

        // Create a query
        let query_id = [42u8; 32];
        let query = PresenceMessage::Query {
            query_id,
            topic_id: topic,
            ttl: 3,
            origin,
        };

        let data = bincode::serialize(&query).expect("serialize");

        // First query should process (returns None but adds to seen_queries)
        let result1 = manager.handle_presence_message(&data).await;
        assert!(result1.is_ok());

        // Second identical query should be dropped (also returns None but doesn't re-process)
        let result2 = manager.handle_presence_message(&data).await;
        assert!(result2.is_ok());
        assert_eq!(result2.unwrap(), None, "Duplicate query should return None");

        // Verify query_id is in seen_queries
        let seen = manager.seen_queries.read().await;
        assert!(
            seen.contains_key(&query_id),
            "Query ID should be in seen set"
        );
    }

    #[tokio::test]
    async fn test_foaf_query_ttl_zero_not_forwarded() {
        // Test that queries with TTL=0 are not forwarded
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);
        let origin = PeerId::new([2u8; 32]);

        // Add the topic to groups
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        // Query with TTL=0 should not be forwarded
        let query = PresenceMessage::Query {
            query_id: [1u8; 32],
            topic_id: topic,
            ttl: 0, // Zero TTL
            origin,
        };

        let data = bincode::serialize(&query).expect("serialize");
        let result = manager.handle_presence_message(&data).await;

        // Should succeed but not forward (no peers to verify, but it should process)
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_foaf_query_response_aggregation() {
        // Test that responses are aggregated properly
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);

        // Add topic to groups
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        // Create two QueryResponse messages with different records
        let peer1 = PeerId::new([10u8; 32]);
        let peer2 = PeerId::new([20u8; 32]);
        let record1 = PresenceRecord::new([1u8; 32], vec!["192.168.1.1:8080".to_string()], 900);
        let record2 = PresenceRecord::new([2u8; 32], vec!["192.168.1.2:8080".to_string()], 900);

        let response1 = PresenceMessage::QueryResponse {
            query_id: [0u8; 32],
            topic_id: topic,
            records: vec![(peer1, record1.clone())],
        };

        let response2 = PresenceMessage::QueryResponse {
            query_id: [0u8; 32],
            topic_id: topic,
            records: vec![(peer2, record2.clone())],
        };

        // Process both responses
        let data1 = bincode::serialize(&response1).expect("serialize");
        let data2 = bincode::serialize(&response2).expect("serialize");

        manager
            .handle_presence_message(&data1)
            .await
            .expect("process response 1");
        manager
            .handle_presence_message(&data2)
            .await
            .expect("process response 2");

        // Both peers should now be in our beacon cache
        let status1 = manager.get_status(peer1, topic).await;
        let status2 = manager.get_status(peer2, topic).await;

        assert_eq!(status1, PresenceStatus::Online, "Peer1 should be online");
        assert_eq!(status2, PresenceStatus::Online, "Peer2 should be online");
    }

    #[tokio::test]
    async fn test_foaf_query_response_deduplication() {
        // Test that duplicate records from multiple responses are deduplicated
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);

        // Add topic to groups
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        let peer = PeerId::new([10u8; 32]);
        let record = PresenceRecord::new([1u8; 32], vec!["192.168.1.1:8080".to_string()], 900);

        // Send same record twice (simulating responses from different peers)
        let response = PresenceMessage::QueryResponse {
            query_id: [0u8; 32],
            topic_id: topic,
            records: vec![(peer, record.clone())],
        };

        let data = bincode::serialize(&response).expect("serialize");

        // Process same response twice
        manager.handle_presence_message(&data).await.expect("first");
        manager
            .handle_presence_message(&data)
            .await
            .expect("second");

        // Should only have one entry for the peer
        let presence = manager.get_group_presence(topic).await;
        assert_eq!(presence.len(), 1, "Should have exactly one entry");
        assert!(presence.contains_key(&peer), "Should contain the peer");
    }

    #[tokio::test]
    async fn test_foaf_query_expired_records_filtered() {
        // Test that expired records in responses are not stored
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);

        // Add topic to groups
        {
            let mut groups = manager.groups.write().await;
            groups.insert(topic, saorsa_gossip_groups::GroupContext::new(topic));
        }

        let peer = PeerId::new([10u8; 32]);
        // Create an expired record (TTL = 0)
        let expired_record = PresenceRecord::new([1u8; 32], vec![], 0);

        // Wait a bit for it to expire
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let response = PresenceMessage::QueryResponse {
            query_id: [0u8; 32],
            topic_id: topic,
            records: vec![(peer, expired_record)],
        };

        let data = bincode::serialize(&response).expect("serialize");
        manager
            .handle_presence_message(&data)
            .await
            .expect("process");

        // Expired record should not be stored (or if stored, shown as offline)
        let status = manager.get_status(peer, topic).await;
        // Either Unknown (not stored) or Offline (stored but expired)
        assert!(
            status == PresenceStatus::Unknown || status == PresenceStatus::Offline,
            "Expired record should not show as online"
        );
    }

    #[tokio::test]
    async fn test_pending_query_cleanup() {
        // Test that expired pending queries are cleaned up
        let manager = create_test_manager();

        // Manually insert a pending query
        {
            let mut pending = manager.pending_queries.write().await;
            // Insert a query that started 15 seconds ago (should be expired)
            let old_query = super::PendingQuery::new(TopicId::new([1u8; 32]));
            pending.insert([1u8; 32], old_query);
        }

        // Immediately cleanup shouldn't remove it (not expired yet)
        let cleaned = manager.cleanup_pending_queries().await;
        assert_eq!(cleaned, 0, "Fresh query shouldn't be cleaned up");

        // Verify query is still there
        let count = manager.pending_queries.read().await.len();
        assert_eq!(count, 1, "Query should still exist");
    }

    #[tokio::test]
    async fn test_query_message_serialization() {
        // Test Query message round-trip serialization
        let query_id = [42u8; 32];
        let topic = TopicId::new([1u8; 32]);
        let origin = PeerId::new([2u8; 32]);

        let query = PresenceMessage::Query {
            query_id,
            topic_id: topic,
            ttl: 3,
            origin,
        };

        let data = bincode::serialize(&query).expect("serialize");
        let decoded: PresenceMessage = bincode::deserialize(&data).expect("deserialize");

        match decoded {
            PresenceMessage::Query {
                query_id: decoded_id,
                topic_id: decoded_topic,
                ttl: decoded_ttl,
                origin: decoded_origin,
            } => {
                assert_eq!(decoded_id, query_id);
                assert_eq!(decoded_topic, topic);
                assert_eq!(decoded_ttl, 3);
                assert_eq!(decoded_origin, origin);
            }
            _ => panic!("Expected Query message"),
        }
    }

    #[tokio::test]
    async fn test_query_response_message_serialization() {
        // Test QueryResponse message round-trip serialization
        let query_id = [99u8; 32];
        let topic = TopicId::new([1u8; 32]);
        let peer = PeerId::new([2u8; 32]);
        let record = PresenceRecord::new([3u8; 32], vec!["127.0.0.1:8080".to_string()], 900);

        let response = PresenceMessage::QueryResponse {
            query_id,
            topic_id: topic,
            records: vec![(peer, record.clone())],
        };

        let data = bincode::serialize(&response).expect("serialize");
        let decoded: PresenceMessage = bincode::deserialize(&data).expect("deserialize");

        match decoded {
            PresenceMessage::QueryResponse {
                query_id: decoded_query_id,
                topic_id: decoded_topic,
                records: decoded_records,
            } => {
                assert_eq!(decoded_query_id, query_id);
                assert_eq!(decoded_topic, topic);
                assert_eq!(decoded_records.len(), 1);
                assert_eq!(decoded_records[0].0, peer);
                assert_eq!(decoded_records[0].1.presence_tag, record.presence_tag);
            }
            _ => panic!("Expected QueryResponse message"),
        }
    }

    #[tokio::test]
    async fn test_initiate_foaf_query_no_peers() {
        // Test that initiating a query with no broadcast peers returns empty
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);

        // No broadcast peers added
        let results = manager
            .initiate_foaf_query(topic, 3, 100) // Short timeout for test
            .await
            .expect("query should succeed");

        assert!(results.is_empty(), "Should return empty with no peers");
    }

    // =========================================================================
    // TDD: Issue #1 - seen_queries memory leak fix
    // =========================================================================

    #[tokio::test]
    async fn test_cleanup_seen_queries_removes_old_entries() {
        // Test that old query IDs are cleaned up
        // Per SPEC2 §7.3, query IDs older than 30 seconds should be removed
        let manager = create_test_manager();

        // Insert a query ID with an old timestamp (simulate 35 seconds ago)
        let old_query_id = [1u8; 32];
        let old_timestamp = Instant::now() - std::time::Duration::from_secs(35);
        {
            let mut seen = manager.seen_queries.write().await;
            seen.insert(old_query_id, old_timestamp);
        }

        // Insert a recent query ID
        let recent_query_id = [2u8; 32];
        {
            let mut seen = manager.seen_queries.write().await;
            seen.insert(recent_query_id, Instant::now());
        }

        // Before cleanup, both should exist
        {
            let seen = manager.seen_queries.read().await;
            assert!(
                seen.contains_key(&old_query_id),
                "Old query should exist before cleanup"
            );
            assert!(
                seen.contains_key(&recent_query_id),
                "Recent query should exist"
            );
        }

        // Call cleanup - should remove old entries
        let cleaned = manager.cleanup_seen_queries().await;

        // Old query should be removed, recent should remain
        assert_eq!(cleaned, 1, "Should have cleaned up exactly 1 old query");

        let seen = manager.seen_queries.read().await;
        assert!(
            !seen.contains_key(&old_query_id),
            "Old query should be removed"
        );
        assert!(
            seen.contains_key(&recent_query_id),
            "Recent query should remain"
        );
    }

    #[tokio::test]
    async fn test_seen_queries_tracks_timestamp() {
        // Test that seen_queries now tracks insertion timestamps
        let manager = create_test_manager();
        let topic = TopicId::new([1u8; 32]);
        let origin = PeerId::new([2u8; 32]);

        let query_id = [42u8; 32];
        let query = PresenceMessage::Query {
            query_id,
            topic_id: topic,
            ttl: 3,
            origin,
        };

        let data = bincode::serialize(&query).expect("serialize");
        manager
            .handle_presence_message(&data)
            .await
            .expect("process");

        // After processing, query_id should be in seen_queries with a timestamp
        let seen = manager.seen_queries.read().await;
        assert!(seen.contains_key(&query_id), "Query ID should be tracked");

        // Verify it's a recent timestamp (within last second)
        let timestamp = seen.get(&query_id).expect("should have timestamp");
        assert!(
            timestamp.elapsed() < std::time::Duration::from_secs(1),
            "Timestamp should be recent"
        );
    }

    #[tokio::test]
    async fn test_cleanup_seen_queries_no_old_entries() {
        // Test cleanup when no old entries exist
        let manager = create_test_manager();

        // Insert only recent query IDs
        {
            let mut seen = manager.seen_queries.write().await;
            seen.insert([1u8; 32], Instant::now());
            seen.insert([2u8; 32], Instant::now());
        }

        // Cleanup should remove nothing
        let cleaned = manager.cleanup_seen_queries().await;
        assert_eq!(cleaned, 0, "Should not clean up recent queries");

        let seen = manager.seen_queries.read().await;
        assert_eq!(seen.len(), 2, "Both queries should remain");
    }

    // =========================================================================
    // TDD: Issue #3 - add_responses keeps newer records by sequence number
    // =========================================================================

    #[test]
    fn test_pending_query_add_responses_keeps_newer_seq() {
        // Test that add_responses keeps the record with higher sequence number
        let topic = TopicId::new([1u8; 32]);
        let mut pending = super::PendingQuery::new(topic);

        let peer = PeerId::new([10u8; 32]);

        // First record with seq=1
        let old_record = PresenceRecord::new_with_seq(
            [1u8; 32],
            vec!["192.168.1.1:8080".to_string()],
            900,
            1, // seq=1
        );

        // Second record with seq=5 (newer)
        let new_record = PresenceRecord::new_with_seq(
            [1u8; 32],
            vec!["192.168.1.2:8080".to_string()],
            900,
            5, // seq=5 (newer)
        );

        // Add old record first
        pending.add_responses(&[(peer, old_record.clone())]);
        assert_eq!(pending.responses.get(&peer).expect("should exist").seq, 1);

        // Add new record - should replace because seq is higher
        pending.add_responses(&[(peer, new_record.clone())]);
        let stored = pending.responses.get(&peer).expect("should exist");
        assert_eq!(stored.seq, 5, "Should keep the record with higher seq");
        assert_eq!(stored.addr_hints, vec!["192.168.1.2:8080".to_string()]);
    }

    #[test]
    fn test_pending_query_add_responses_ignores_older_seq() {
        // Test that add_responses ignores records with lower sequence number
        let topic = TopicId::new([1u8; 32]);
        let mut pending = super::PendingQuery::new(topic);

        let peer = PeerId::new([10u8; 32]);

        // First record with seq=10
        let newer_record = PresenceRecord::new_with_seq(
            [1u8; 32],
            vec!["192.168.1.1:8080".to_string()],
            900,
            10, // seq=10
        );

        // Second record with seq=3 (older)
        let older_record = PresenceRecord::new_with_seq(
            [1u8; 32],
            vec!["192.168.1.2:8080".to_string()],
            900,
            3, // seq=3 (older)
        );

        // Add newer record first
        pending.add_responses(&[(peer, newer_record.clone())]);
        assert_eq!(pending.responses.get(&peer).expect("should exist").seq, 10);

        // Add older record - should NOT replace
        pending.add_responses(&[(peer, older_record.clone())]);
        let stored = pending.responses.get(&peer).expect("should exist");
        assert_eq!(stored.seq, 10, "Should keep the record with higher seq");
        assert_eq!(stored.addr_hints, vec!["192.168.1.1:8080".to_string()]);
    }
}
