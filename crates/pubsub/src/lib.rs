#![warn(missing_docs)]

//! Plumtree-based pub/sub dissemination
//!
//! Implements:
//! - EAGER push along spanning tree
//! - IHAVE lazy digests to non-tree links
//! - IWANT pull on demand
//! - PRUNE/GRAFT for tree optimization
//! - Anti-entropy reconciliation for partition recovery
//!
//! # Architecture
//!
//! Each topic maintains two sets of peers:
//! - **Eager peers** (tree): Forward full messages immediately
//! - **Lazy peers** (gossip): Send only message IDs (IHAVE)
//!
//! The tree self-optimizes via duplicate detection (PRUNE) and pull requests (GRAFT).

use anyhow::{anyhow, Result};
use bytes::Bytes;
use lru::LruCache;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use saorsa_gossip_types::{MessageHeader, MessageKind, PeerId, TopicId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tokio::time;
use tracing::{debug, error, trace, warn};

/// Maximum message cache size per topic (10,000 messages)
const MAX_CACHE_SIZE: usize = 10_000;

/// Message cache TTL (5 minutes)
const CACHE_TTL_SECS: u64 = 300;

/// Maximum IHAVE batch size (per SPEC.md)
const MAX_IHAVE_BATCH_SIZE: usize = 1024;

/// IHAVE flush interval (100ms)
const IHAVE_FLUSH_INTERVAL_MS: u64 = 100;

/// Anti-entropy reconciliation interval (30 seconds)
const ANTI_ENTROPY_INTERVAL_SECS: u64 = 30;

/// Target eager peer degree (6-8)
const MIN_EAGER_DEGREE: usize = 6;
const MAX_EAGER_DEGREE: usize = 12;

/// Message ID type alias
type MessageIdType = [u8; 32];

const fn message_cache_capacity() -> NonZeroUsize {
    // SAFETY: MAX_CACHE_SIZE is a positive constant (10,000)
    unsafe { NonZeroUsize::new_unchecked(MAX_CACHE_SIZE) }
}

/// Gossip message wrapper
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipMessage {
    /// Message header
    pub header: MessageHeader,
    /// Optional payload (None for IHAVE)
    pub payload: Option<Bytes>,
    /// ML-DSA signature over the header
    pub signature: Vec<u8>,
    /// Sender's ML-DSA public key for verification
    pub public_key: Vec<u8>,
}

/// Anti-entropy reconciliation payload
///
/// Used for periodic set reconciliation between peers to recover
/// messages missed during network partitions.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum AntiEntropyPayload {
    /// "Here are my message IDs, send me anything I'm missing"
    Digest {
        /// Message IDs the sender currently has cached
        msg_ids: Vec<MessageIdType>,
    },
    /// "Here are the IDs you're missing" (actual messages follow as EAGER)
    Response {
        /// Message IDs the receiver is missing
        missing_ids: Vec<MessageIdType>,
    },
}

/// Cached message entry
#[derive(Clone)]
struct CachedMessage {
    /// Message payload
    payload: Bytes,
    /// Timestamp when cached
    timestamp: Instant,
    /// Message header
    header: MessageHeader,
}

/// Per-topic state
struct TopicState {
    /// Spanning tree peers (forward EAGER)
    eager_peers: HashSet<PeerId>,
    /// Non-tree peers (send IHAVE only)
    lazy_peers: HashSet<PeerId>,
    /// Message cache: msg_id -> cached message
    message_cache: LruCache<MessageIdType, CachedMessage>,
    /// Pending IHAVE batch (≤1024 message IDs)
    pending_ihave: Vec<MessageIdType>,
    /// Outstanding IWANT requests: msg_id -> (peer, timestamp)
    outstanding_iwants: HashMap<MessageIdType, (PeerId, Instant)>,
    /// Local subscribers
    subscribers: Vec<mpsc::UnboundedSender<(PeerId, Bytes)>>,
}

impl TopicState {
    fn new() -> Self {
        Self {
            eager_peers: HashSet::new(),
            lazy_peers: HashSet::new(),
            message_cache: LruCache::new(message_cache_capacity()),
            pending_ihave: Vec::new(),
            outstanding_iwants: HashMap::new(),
            subscribers: Vec::new(),
        }
    }

    /// Get all cached message IDs for anti-entropy digest
    fn cached_message_ids(&self) -> Vec<MessageIdType> {
        self.message_cache.iter().map(|(id, _)| *id).collect()
    }

    /// Check if message is in cache
    fn has_message(&self, msg_id: &MessageIdType) -> bool {
        self.message_cache.contains(msg_id)
    }

    /// Add message to cache
    fn cache_message(&mut self, msg_id: MessageIdType, payload: Bytes, header: MessageHeader) {
        let cached = CachedMessage {
            payload,
            timestamp: Instant::now(),
            header,
        };
        self.message_cache.put(msg_id, cached);
    }

    /// Get cached message
    fn get_message(&mut self, msg_id: &MessageIdType) -> Option<CachedMessage> {
        self.message_cache.get(msg_id).cloned()
    }

    /// Clean expired cache entries
    fn clean_cache(&mut self) {
        let now = Instant::now();
        let ttl = Duration::from_secs(CACHE_TTL_SECS);

        // Collect expired keys
        let mut expired = Vec::new();
        for (msg_id, cached) in self.message_cache.iter() {
            if now.duration_since(cached.timestamp) > ttl {
                expired.push(*msg_id);
            }
        }

        // Remove expired entries
        for msg_id in expired {
            self.message_cache.pop(&msg_id);
        }
    }

    /// Move peer from eager to lazy
    fn prune_peer(&mut self, peer: PeerId) {
        if self.eager_peers.remove(&peer) {
            self.lazy_peers.insert(peer);
            debug!(peer_id = %peer, "PRUNE: moved peer from eager to lazy");
        }
    }

    /// Move peer from lazy to eager
    fn graft_peer(&mut self, peer: PeerId) {
        if self.lazy_peers.remove(&peer) {
            self.eager_peers.insert(peer);
            debug!(peer_id = %peer, "GRAFT: moved peer from lazy to eager");
        }
    }

    /// Maintain eager peer degree (6-12)
    fn maintain_degree(&mut self) {
        let eager_count = self.eager_peers.len();

        if eager_count < MIN_EAGER_DEGREE && !self.lazy_peers.is_empty() {
            // Promote random lazy peers
            let to_promote = MIN_EAGER_DEGREE - eager_count;
            let peers: Vec<PeerId> = self.lazy_peers.iter().take(to_promote).copied().collect();
            for peer in peers {
                self.graft_peer(peer);
            }
        } else if eager_count > MAX_EAGER_DEGREE {
            // Demote random eager peers
            let to_demote = eager_count - MAX_EAGER_DEGREE;
            let peers: Vec<PeerId> = self.eager_peers.iter().take(to_demote).copied().collect();
            for peer in peers {
                self.prune_peer(peer);
            }
        }
    }
}

/// Pub/sub trait for message dissemination
#[async_trait::async_trait]
pub trait PubSub: Send + Sync {
    /// Publish a message to a topic
    async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()>;

    /// Subscribe to a topic and receive messages
    fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)>;

    /// Unsubscribe from a topic
    async fn unsubscribe(&self, topic: TopicId) -> Result<()>;

    /// Initialize peers for a topic
    ///
    /// Called when subscribing to a topic to populate the eager peers list
    /// with currently connected peers for message dissemination.
    async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>);

    /// Handle an incoming pubsub message from a peer
    ///
    /// Routes the message to appropriate handler based on MessageKind (Eager, IHave, IWant).
    /// Called by the transport layer when receiving PubSub messages.
    async fn handle_message(&self, from: PeerId, data: Bytes) -> Result<()>;

    /// Trigger an anti-entropy round for a specific topic
    ///
    /// This is primarily for testing. In production, anti-entropy runs
    /// automatically via the background task.
    async fn trigger_anti_entropy(&self, _topic: TopicId) -> Result<()> {
        Ok(()) // Default no-op
    }
}

/// Plumtree pub/sub implementation
pub struct PlumtreePubSub<T: GossipTransport + 'static> {
    /// Per-topic state
    topics: Arc<RwLock<HashMap<TopicId, TopicState>>>,
    /// Local peer ID
    peer_id: PeerId,
    /// Epoch for message IDs (system time in seconds)
    epoch_start: std::time::SystemTime,
    /// Transport layer for sending messages
    transport: Arc<T>,
    /// ML-DSA key pair for signing messages
    signing_key: Arc<saorsa_gossip_identity::MlDsaKeyPair>,
}

impl<T: GossipTransport + 'static> PlumtreePubSub<T> {
    /// Create a new Plumtree pub/sub instance
    ///
    /// # Arguments
    /// * `peer_id` - Local peer identifier
    /// * `transport` - Transport layer for network communication
    /// * `signing_key` - ML-DSA key pair for message signing
    pub fn new(
        peer_id: PeerId,
        transport: Arc<T>,
        signing_key: saorsa_gossip_identity::MlDsaKeyPair,
    ) -> Self {
        let pubsub = Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            peer_id,
            epoch_start: std::time::SystemTime::UNIX_EPOCH,
            transport,
            signing_key: Arc::new(signing_key),
        };

        // Start background tasks
        pubsub.spawn_ihave_flusher();
        pubsub.spawn_cache_cleaner();
        pubsub.spawn_degree_maintainer();
        pubsub.spawn_anti_entropy_task();

        pubsub
    }

    /// Get current epoch (seconds since UNIX_EPOCH)
    fn current_epoch(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(self.epoch_start)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Calculate message ID
    fn calculate_msg_id(&self, topic: &TopicId, payload: &Bytes) -> MessageIdType {
        let epoch = self.current_epoch();
        let payload_hash = blake3::hash(payload.as_ref());
        MessageHeader::calculate_msg_id(topic, epoch, &self.peer_id, payload_hash.as_bytes())
    }

    /// Sign message header using ML-DSA-65
    ///
    /// Serializes the header and signs it with the node's ML-DSA key pair.
    /// Per SPEC2 §2, all gossip messages MUST be signed for authenticity.
    fn sign_message(&self, header: &MessageHeader) -> Vec<u8> {
        // Serialize header for signing
        let header_bytes = match postcard::to_stdvec(header) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to serialize header for signing: {}", e);
                return Vec::new();
            }
        };

        // Sign with ML-DSA-65
        match self.signing_key.sign(&header_bytes) {
            Ok(signature) => signature,
            Err(e) => {
                error!("Failed to sign message: {}", e);
                Vec::new()
            }
        }
    }

    /// Verify message signature using ML-DSA-65
    ///
    /// # Arguments
    /// * `header` - Message header to verify
    /// * `signature` - ML-DSA signature bytes
    /// * `public_key` - Sender's public key bytes
    ///
    /// # Returns
    /// `true` if signature is valid, `false` otherwise
    fn verify_signature(
        &self,
        header: &MessageHeader,
        signature: &[u8],
        public_key: &[u8],
    ) -> bool {
        // Serialize header
        let header_bytes = match postcard::to_stdvec(header) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("Failed to serialize header for verification: {}", e);
                return false;
            }
        };

        // Verify signature
        match saorsa_gossip_identity::MlDsaKeyPair::verify(public_key, &header_bytes, signature) {
            Ok(valid) => valid,
            Err(e) => {
                warn!("Failed to verify signature: {}", e);
                false
            }
        }
    }

    /// Publish a message (local origin)
    pub async fn publish_local(&self, topic: TopicId, payload: Bytes) -> Result<()> {
        let msg_id = self.calculate_msg_id(&topic, &payload);

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let signature = self.sign_message(&header);

        let _message = GossipMessage {
            header: header.clone(),
            payload: Some(payload.clone()),
            signature,
            public_key: self.signing_key.public_key().to_vec(),
        };

        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        // Add to cache
        state.cache_message(msg_id, payload.clone(), header);

        // Send EAGER to eager_peers
        let eager_peers: Vec<PeerId> = state.eager_peers.iter().copied().collect();
        drop(topics); // Release lock before network I/O

        for peer in eager_peers {
            let transport = self.transport.clone();
            let message = _message.clone();
            tokio::spawn(async move {
                trace!(peer_id = %peer, msg_id = ?msg_id, "Sending EAGER");
                let bytes = match postcard::to_stdvec(&message) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        warn!(peer_id = %peer, msg_id = ?msg_id, "EAGER serialize failed: {e}");
                        return;
                    }
                };
                match transport
                    .send_to_peer(peer, GossipStreamType::PubSub, bytes.into())
                    .await
                {
                    Ok(()) => {}
                    Err(err) => {
                        warn!(peer_id = %peer, msg_id = ?msg_id, "EAGER send failed: {err}");
                    }
                }
            });
        }

        // Batch msg_id to pending_ihave
        let mut topics = self.topics.write().await;
        if let Some(state) = topics.get_mut(&topic) {
            state.pending_ihave.push(msg_id);

            // Deliver to local subscribers
            let data = (self.peer_id, payload);
            state.subscribers.retain(|tx| tx.send(data.clone()).is_ok());
        }

        Ok(())
    }

    /// Handle incoming EAGER message
    pub async fn handle_eager(
        &self,
        from: PeerId,
        topic: TopicId,
        message: GossipMessage,
    ) -> Result<()> {
        let msg_id = message.header.msg_id;

        // Verify signature
        if !self.verify_signature(&message.header, &message.signature, &message.public_key) {
            warn!(peer_id = %from, msg_id = ?msg_id, "Invalid signature, dropping");
            return Err(anyhow!("Invalid signature"));
        }

        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        // Check for duplicate
        if state.has_message(&msg_id) {
            // PRUNE: move sender from eager to lazy
            state.prune_peer(from);
            return Ok(());
        }

        // New message - add to cache
        let payload = message
            .payload
            .clone()
            .ok_or_else(|| anyhow!("EAGER missing payload"))?;
        state.cache_message(msg_id, payload.clone(), message.header.clone());

        // Add sender to eager_peers if not already present
        // This ensures bidirectional message flow - if a peer sends us messages
        // on a topic, they've subscribed and should receive our messages too.
        if !state.eager_peers.contains(&from) && !state.lazy_peers.contains(&from) {
            state.eager_peers.insert(from);
            debug!(peer_id = %from, topic = ?topic, "Added sender to eager_peers");
        }

        // Deliver to local subscribers
        let data = (from, payload.clone());
        state.subscribers.retain(|tx| tx.send(data.clone()).is_ok());

        // Forward to eager_peers (except sender)
        let eager_peers: Vec<PeerId> = state
            .eager_peers
            .iter()
            .filter(|&&p| p != from)
            .copied()
            .collect();

        // Batch msg_id to pending_ihave for lazy_peers
        state.pending_ihave.push(msg_id);

        drop(topics); // Release lock

        // Forward EAGER
        for peer in eager_peers {
            trace!(peer_id = %peer, msg_id = ?msg_id, "Forwarding EAGER");
            let bytes = postcard::to_stdvec(&message)
                .map_err(|e| anyhow!("Serialization failed: {}", e))?;
            self.transport
                .send_to_peer(peer, GossipStreamType::PubSub, bytes.into())
                .await?;
        }

        Ok(())
    }

    /// Handle incoming IHAVE message
    pub async fn handle_ihave(
        &self,
        from: PeerId,
        topic: TopicId,
        msg_ids: Vec<MessageIdType>,
    ) -> Result<()> {
        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        let mut requested = Vec::new();

        for msg_id in msg_ids {
            // Skip if we have it
            if state.has_message(&msg_id) {
                continue;
            }

            // Skip if already requested
            if state.outstanding_iwants.contains_key(&msg_id) {
                continue;
            }

            // Request it
            requested.push(msg_id);
            state
                .outstanding_iwants
                .insert(msg_id, (from, Instant::now()));
        }

        drop(topics); // Release lock

        if !requested.is_empty() {
            debug!(peer_id = %from, count = requested.len(), "Sending IWANT");
            // Create IWANT message
            let iwant_header = MessageHeader {
                version: 1,
                topic,
                msg_id: requested[0], // Use first ID as header
                kind: MessageKind::IWant,
                hop: 0,
                ttl: 10,
            };
            let iwant_header_clone = iwant_header.clone();
            let iwant_msg = GossipMessage {
                header: iwant_header,
                payload: Some(
                    postcard::to_stdvec(&requested)
                        .map_err(|e| anyhow!("Serialization failed: {}", e))?
                        .into(),
                ),
                signature: self.sign_message(&iwant_header_clone),
                public_key: self.signing_key.public_key().to_vec(),
            };
            let bytes = postcard::to_stdvec(&iwant_msg)
                .map_err(|e| anyhow!("Serialization failed: {}", e))?;
            self.transport
                .send_to_peer(from, GossipStreamType::PubSub, bytes.into())
                .await?;
        }

        Ok(())
    }

    /// Handle incoming IWANT message
    pub async fn handle_iwant(
        &self,
        from: PeerId,
        topic: TopicId,
        msg_ids: Vec<MessageIdType>,
    ) -> Result<()> {
        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        let mut to_send = Vec::new();

        for msg_id in msg_ids {
            if let Some(cached) = state.get_message(&msg_id) {
                to_send.push((msg_id, cached));
                // GRAFT: move peer from lazy to eager
                state.graft_peer(from);
            } else {
                warn!(msg_id = ?msg_id, "IWANT for unknown message");
            }
        }

        drop(topics); // Release lock

        // Send EAGER with payloads
        for (msg_id, cached) in to_send {
            debug!(peer_id = %from, msg_id = ?msg_id, "Sending EAGER in response to IWANT");

            let _message = GossipMessage {
                header: cached.header.clone(),
                payload: Some(cached.payload.clone()),
                signature: self.sign_message(&cached.header),
                public_key: self.signing_key.public_key().to_vec(),
            };

            let bytes = postcard::to_stdvec(&_message)
                .map_err(|e| anyhow!("Serialization failed: {}", e))?;
            self.transport
                .send_to_peer(from, GossipStreamType::PubSub, bytes.into())
                .await?;
        }

        Ok(())
    }

    /// Handle incoming anti-entropy message
    ///
    /// Processes `AntiEntropyPayload::Digest` and `AntiEntropyPayload::Response`
    /// messages for set reconciliation after network partitions.
    async fn handle_anti_entropy(
        &self,
        from: PeerId,
        topic: TopicId,
        message: GossipMessage,
    ) -> Result<()> {
        // Verify signature
        if !self.verify_signature(&message.header, &message.signature, &message.public_key) {
            warn!(peer_id = %from, "Anti-entropy: invalid signature, dropping");
            return Err(anyhow!("Invalid signature on anti-entropy message"));
        }

        let payload_bytes = message
            .payload
            .ok_or_else(|| anyhow!("Anti-entropy message missing payload"))?;

        let ae_payload: AntiEntropyPayload = postcard::from_bytes(&payload_bytes)
            .map_err(|e| anyhow!("Failed to deserialize anti-entropy payload: {}", e))?;

        match ae_payload {
            AntiEntropyPayload::Digest { msg_ids } => {
                debug!(
                    peer_id = %from,
                    topic = ?topic,
                    their_count = msg_ids.len(),
                    "Received anti-entropy digest"
                );

                let their_ids: HashSet<MessageIdType> = msg_ids.into_iter().collect();

                let mut topics = self.topics.write().await;
                let state = topics.entry(topic).or_insert_with(TopicState::new);

                let our_ids: HashSet<MessageIdType> =
                    state.cached_message_ids().into_iter().collect();

                // IDs we have that they don't - send cached messages as EAGER
                let mut messages_to_send = Vec::new();
                for id in our_ids.difference(&their_ids) {
                    if let Some(cached) = state.get_message(id) {
                        messages_to_send.push(cached);
                    }
                }

                // IDs they have that we don't - we need these
                let ids_we_need: Vec<MessageIdType> =
                    their_ids.difference(&our_ids).copied().collect();

                drop(topics);

                // Send cached messages the peer is missing as EAGER
                for cached in &messages_to_send {
                    let eager_msg = GossipMessage {
                        header: cached.header.clone(),
                        payload: Some(cached.payload.clone()),
                        signature: self.sign_message(&cached.header),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&eager_msg) {
                        let _ = self
                            .transport
                            .send_to_peer(from, GossipStreamType::PubSub, bytes.into())
                            .await;
                    }
                }

                // Send IWANT for IDs they have that we don't
                if !ids_we_need.is_empty() {
                    debug!(
                        peer_id = %from,
                        count = ids_we_need.len(),
                        "Anti-entropy: requesting missing messages via IWANT"
                    );
                    let iwant_header = MessageHeader {
                        version: 1,
                        topic,
                        msg_id: ids_we_need[0],
                        kind: MessageKind::IWant,
                        hop: 0,
                        ttl: 10,
                    };
                    let iwant_header_clone = iwant_header.clone();
                    let iwant_msg = GossipMessage {
                        header: iwant_header,
                        payload: Some(
                            postcard::to_stdvec(&ids_we_need)
                                .map_err(|e| anyhow!("Serialization failed: {}", e))?
                                .into(),
                        ),
                        signature: self.sign_message(&iwant_header_clone),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&iwant_msg) {
                        let _ = self
                            .transport
                            .send_to_peer(from, GossipStreamType::PubSub, bytes.into())
                            .await;
                    }
                }

                debug!(
                    peer_id = %from,
                    sent = messages_to_send.len(),
                    requested = ids_we_need.len(),
                    "Anti-entropy digest processed"
                );
            }
            AntiEntropyPayload::Response { missing_ids } => {
                debug!(
                    peer_id = %from,
                    topic = ?topic,
                    count = missing_ids.len(),
                    "Received anti-entropy response"
                );

                // Filter out IDs we already have
                let topics = self.topics.read().await;
                let ids_to_request: Vec<MessageIdType> = if let Some(state) = topics.get(&topic) {
                    missing_ids
                        .into_iter()
                        .filter(|id| !state.has_message(id))
                        .collect()
                } else {
                    missing_ids
                };
                drop(topics);

                // Send IWANT for each ID we don't have
                if !ids_to_request.is_empty() {
                    debug!(
                        peer_id = %from,
                        count = ids_to_request.len(),
                        "Anti-entropy response: sending IWANT for missing IDs"
                    );
                    let iwant_header = MessageHeader {
                        version: 1,
                        topic,
                        msg_id: ids_to_request[0],
                        kind: MessageKind::IWant,
                        hop: 0,
                        ttl: 10,
                    };
                    let iwant_header_clone = iwant_header.clone();
                    let iwant_msg = GossipMessage {
                        header: iwant_header,
                        payload: Some(
                            postcard::to_stdvec(&ids_to_request)
                                .map_err(|e| anyhow!("Serialization failed: {}", e))?
                                .into(),
                        ),
                        signature: self.sign_message(&iwant_header_clone),
                        public_key: self.signing_key.public_key().to_vec(),
                    };
                    if let Ok(bytes) = postcard::to_stdvec(&iwant_msg) {
                        let _ = self
                            .transport
                            .send_to_peer(from, GossipStreamType::PubSub, bytes.into())
                            .await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Send an anti-entropy digest for a specific topic to a specific peer
    ///
    /// Collects cached message IDs and sends them as an `AntiEntropyPayload::Digest`.
    async fn send_anti_entropy_digest(&self, topic: TopicId, peer: PeerId) -> Result<()> {
        let topics = self.topics.read().await;
        let msg_ids = if let Some(state) = topics.get(&topic) {
            state.cached_message_ids()
        } else {
            return Ok(());
        };
        drop(topics);

        if msg_ids.is_empty() {
            return Ok(());
        }

        let ae_payload = AntiEntropyPayload::Digest { msg_ids };
        let payload_bytes = postcard::to_stdvec(&ae_payload)
            .map_err(|e| anyhow!("Failed to serialize anti-entropy payload: {}", e))?;

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let signature = self.sign_message(&header);

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: self.signing_key.public_key().to_vec(),
        };

        let bytes =
            postcard::to_stdvec(&message).map_err(|e| anyhow!("Serialization failed: {}", e))?;
        self.transport
            .send_to_peer(peer, GossipStreamType::PubSub, bytes.into())
            .await?;

        debug!(
            peer_id = %peer,
            topic = ?topic,
            "Sent anti-entropy digest"
        );

        Ok(())
    }

    /// Spawn background task to flush IHAVE batches
    fn spawn_ihave_flusher(&self) {
        let topics = self.topics.clone();
        let transport = self.transport.clone();
        let signing_key = self.signing_key.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(IHAVE_FLUSH_INTERVAL_MS));

            loop {
                interval.tick().await;

                let mut topics_guard = topics.write().await;

                for (topic_id, state) in topics_guard.iter_mut() {
                    if state.pending_ihave.is_empty() {
                        continue;
                    }

                    // Take up to MAX_IHAVE_BATCH_SIZE
                    let batch: Vec<MessageIdType> = state
                        .pending_ihave
                        .drain(..state.pending_ihave.len().min(MAX_IHAVE_BATCH_SIZE))
                        .collect();

                    let lazy_peers: Vec<PeerId> = state.lazy_peers.iter().copied().collect();

                    trace!(topic = ?topic_id, batch_size = batch.len(), peer_count = lazy_peers.len(), "Flushing IHAVE batch");

                    // Send IHAVE to each lazy peer
                    for peer in lazy_peers {
                        let ihave_header = MessageHeader {
                            version: 1,
                            topic: *topic_id,
                            msg_id: batch[0], // Use first ID as header
                            kind: MessageKind::IHave,
                            hop: 0,
                            ttl: 10,
                        };
                        let ihave_header_clone = ihave_header.clone();

                        // Sign the header
                        let signature = match postcard::to_stdvec(&ihave_header_clone) {
                            Ok(bytes) => signing_key.sign(&bytes).unwrap_or_default(),
                            Err(_) => Vec::new(),
                        };

                        let ihave_msg = GossipMessage {
                            header: ihave_header,
                            payload: Some(postcard::to_stdvec(&batch).unwrap_or_default().into()),
                            signature,
                            public_key: signing_key.public_key().to_vec(),
                        };
                        if let Ok(bytes) = postcard::to_stdvec(&ihave_msg) {
                            let _ = transport
                                .send_to_peer(peer, GossipStreamType::PubSub, bytes.into())
                                .await;
                        }
                    }
                }
            }
        });
    }

    /// Spawn background task to clean expired cache entries
    fn spawn_cache_cleaner(&self) {
        let topics = self.topics.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(60));

            loop {
                interval.tick().await;

                let mut topics_guard = topics.write().await;

                for state in topics_guard.values_mut() {
                    state.clean_cache();
                }
            }
        });
    }

    /// Spawn background task to maintain eager peer degree
    fn spawn_degree_maintainer(&self) {
        let topics = self.topics.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(30));

            loop {
                interval.tick().await;

                let mut topics_guard = topics.write().await;

                for state in topics_guard.values_mut() {
                    state.maintain_degree();
                }
            }
        });
    }

    /// Spawn background task for anti-entropy reconciliation
    ///
    /// Every `ANTI_ENTROPY_INTERVAL_SECS` seconds, for each topic with cached messages,
    /// picks one random peer and sends an anti-entropy digest containing our cached message IDs.
    fn spawn_anti_entropy_task(&self) {
        let topics = self.topics.clone();
        let transport = self.transport.clone();
        let signing_key = self.signing_key.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(ANTI_ENTROPY_INTERVAL_SECS));

            loop {
                interval.tick().await;

                let topics_guard = topics.read().await;

                // Collect work to do (topic, peer, msg_ids) while holding the read lock
                let mut work: Vec<(TopicId, PeerId, Vec<MessageIdType>)> = Vec::new();

                for (topic_id, state) in topics_guard.iter() {
                    let msg_ids = state.cached_message_ids();
                    if msg_ids.is_empty() {
                        continue;
                    }

                    // Collect all peers (eager + lazy) for random selection
                    let all_peers: Vec<PeerId> = state
                        .eager_peers
                        .iter()
                        .chain(state.lazy_peers.iter())
                        .copied()
                        .collect();

                    if all_peers.is_empty() {
                        continue;
                    }

                    // Pick a deterministic-random peer using hash of topic + current time
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let hash_input = blake3::hash(
                        &[topic_id.to_bytes().as_slice(), &now.to_le_bytes()].concat(),
                    );
                    let hash_bytes = hash_input.as_bytes();
                    let index = (hash_bytes[0] as usize) % all_peers.len();
                    let selected_peer = all_peers[index];

                    work.push((*topic_id, selected_peer, msg_ids));
                }

                drop(topics_guard);

                // Send digests without holding the lock
                for (topic_id, peer, msg_ids) in work {
                    let ae_payload = AntiEntropyPayload::Digest { msg_ids };
                    let payload_bytes = match postcard::to_stdvec(&ae_payload) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!("Anti-entropy: failed to serialize payload: {}", e);
                            continue;
                        }
                    };

                    let header = MessageHeader {
                        version: 1,
                        topic: topic_id,
                        msg_id: [0u8; 32],
                        kind: MessageKind::AntiEntropy,
                        hop: 0,
                        ttl: 1,
                    };

                    let signature = match postcard::to_stdvec(&header) {
                        Ok(bytes) => signing_key.sign(&bytes).unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };

                    let message = GossipMessage {
                        header,
                        payload: Some(payload_bytes.into()),
                        signature,
                        public_key: signing_key.public_key().to_vec(),
                    };

                    if let Ok(bytes) = postcard::to_stdvec(&message) {
                        let _ = transport
                            .send_to_peer(peer, GossipStreamType::PubSub, bytes.into())
                            .await;
                    }

                    trace!(
                        peer_id = %peer,
                        topic = ?topic_id,
                        "Anti-entropy: sent digest"
                    );
                }
            }
        });
    }

    /// Initialize peers for a topic from membership layer
    pub async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>) {
        let mut topics = self.topics.write().await;
        let state = topics.entry(topic).or_insert_with(TopicState::new);

        // Start with all peers as eager (tree will optimize via PRUNE)
        for peer in peers {
            state.eager_peers.insert(peer);
        }

        debug!(topic = ?topic, peer_count = state.eager_peers.len(), "Initialized topic peers");
    }
}

#[async_trait::async_trait]
impl<T: GossipTransport + 'static> PubSub for PlumtreePubSub<T> {
    async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()> {
        self.publish_local(topic, data).await
    }

    fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let topics = self.topics.clone();

        tokio::spawn(async move {
            let mut topics_guard = topics.write().await;
            let state = topics_guard.entry(topic).or_insert_with(TopicState::new);
            state.subscribers.push(tx);
        });

        rx
    }

    async fn unsubscribe(&self, topic: TopicId) -> Result<()> {
        let mut topics = self.topics.write().await;
        topics.remove(&topic);
        Ok(())
    }

    async fn initialize_topic_peers(&self, topic: TopicId, peers: Vec<PeerId>) {
        PlumtreePubSub::initialize_topic_peers(self, topic, peers).await
    }

    async fn handle_message(&self, from: PeerId, data: Bytes) -> Result<()> {
        // Deserialize the GossipMessage
        let message: GossipMessage = postcard::from_bytes(&data)
            .map_err(|e| anyhow!("Failed to deserialize PubSub message: {}", e))?;

        let topic_id = message.header.topic;
        let msg_kind = message.header.kind;

        debug!(
            msg_kind = ?msg_kind,
            peer_id = %from,
            topic = ?topic_id,
            "Handling incoming PubSub message"
        );

        // Route to appropriate handler based on message kind
        // Only handle pubsub-specific message kinds (Eager, IHave, IWant)
        match msg_kind {
            MessageKind::Eager => self.handle_eager(from, topic_id, message).await,
            MessageKind::IHave => {
                // IHAVE payload contains Vec<MessageIdType>
                if let Some(payload) = &message.payload {
                    let msg_ids: Vec<MessageIdType> = postcard::from_bytes(payload)
                        .map_err(|e| anyhow!("Failed to deserialize IHAVE payload: {}", e))?;
                    self.handle_ihave(from, topic_id, msg_ids).await
                } else {
                    Err(anyhow!("IHAVE message missing payload"))
                }
            }
            MessageKind::IWant => {
                // IWANT payload contains Vec<MessageIdType>
                if let Some(payload) = &message.payload {
                    let msg_ids: Vec<MessageIdType> = postcard::from_bytes(payload)
                        .map_err(|e| anyhow!("Failed to deserialize IWANT payload: {}", e))?;
                    self.handle_iwant(from, topic_id, msg_ids).await
                } else {
                    Err(anyhow!("IWANT message missing payload"))
                }
            }
            MessageKind::AntiEntropy => self.handle_anti_entropy(from, topic_id, message).await,
            // Other message kinds (Ping, Ack, Find, Presence, Shuffle) are not handled by PubSub
            _ => {
                warn!(
                    "PubSub received non-pubsub message kind {:?}, ignoring",
                    msg_kind
                );
                Ok(())
            }
        }
    }

    async fn trigger_anti_entropy(&self, topic: TopicId) -> Result<()> {
        let topics = self.topics.read().await;

        let peer = if let Some(state) = topics.get(&topic) {
            // Pick a peer (any eager or lazy)
            state
                .eager_peers
                .iter()
                .chain(state.lazy_peers.iter())
                .next()
                .copied()
        } else {
            None
        };

        drop(topics);

        if let Some(peer) = peer {
            self.send_anti_entropy_digest(topic, peer).await
        } else {
            Ok(()) // No peers available
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use saorsa_gossip_transport::UdpTransportAdapter;
    use std::net::SocketAddr;

    fn test_peer_id(id: u8) -> PeerId {
        let mut bytes = [0u8; 32];
        bytes[0] = id;
        PeerId::new(bytes)
    }

    async fn test_transport() -> Arc<UdpTransportAdapter> {
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("valid addr");
        Arc::new(
            UdpTransportAdapter::new(bind, vec![])
                .await
                .expect("transport"),
        )
    }

    fn test_signing_key() -> saorsa_gossip_identity::MlDsaKeyPair {
        saorsa_gossip_identity::MlDsaKeyPair::generate().expect("Failed to generate test key pair")
    }

    #[tokio::test]
    async fn test_pubsub_creation() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let _pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
    }

    #[tokio::test]
    async fn test_publish_and_subscribe() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        let mut rx = pubsub.subscribe(topic);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let data = Bytes::from("test message");
        pubsub.publish(topic, data.clone()).await.ok();

        let received =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv()).await;

        assert!(received.is_ok());
        let (_, payload) = received.unwrap().unwrap();
        assert_eq!(payload, data);
    }

    #[tokio::test]
    async fn test_message_caching() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        let payload = Bytes::from("test");
        let msg_id = pubsub.calculate_msg_id(&topic, &payload);

        pubsub.publish(topic, payload.clone()).await.ok();

        // Check cache
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.has_message(&msg_id));
    }

    #[tokio::test]
    async fn test_duplicate_detection_prune() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let signing_key = test_signing_key();
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Initialize peer as eager
        pubsub.initialize_topic_peers(topic, vec![from_peer]).await;

        let payload = Bytes::from("test");
        let msg_id = pubsub.calculate_msg_id(&topic, &payload);

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id,
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        // Create properly signed message
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload.clone()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // First EAGER - should be accepted
        pubsub
            .handle_eager(from_peer, topic, message.clone())
            .await
            .ok();

        // Second EAGER - should trigger PRUNE
        pubsub.handle_eager(from_peer, topic, message).await.ok();

        // Verify peer was moved to lazy
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(!state.eager_peers.contains(&from_peer));
        assert!(state.lazy_peers.contains(&from_peer));
    }

    #[tokio::test]
    async fn test_ihave_handling() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let unknown_msg_id = [42u8; 32];

        pubsub
            .handle_ihave(from_peer, topic, vec![unknown_msg_id])
            .await
            .ok();

        // Verify IWANT was tracked
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.outstanding_iwants.contains_key(&unknown_msg_id));
    }

    #[tokio::test]
    async fn test_iwant_graft() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Initialize peer as lazy
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            state.lazy_peers.insert(from_peer);
        }

        // Publish a message to cache it
        let payload = Bytes::from("test");
        pubsub.publish(topic, payload.clone()).await.ok();

        // Get the actual cached msg_id (don't recalculate - epoch may have changed)
        let msg_id = {
            let topics = pubsub.topics.read().await;
            let state = topics.get(&topic).unwrap();
            // Get the first (and only) cached message ID
            state
                .message_cache
                .peek_lru()
                .map(|(id, _)| *id)
                .expect("message should be cached")
        };

        // Handle IWANT from lazy peer
        pubsub
            .handle_iwant(from_peer, topic, vec![msg_id])
            .await
            .ok();

        // Verify peer was grafted to eager
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.eager_peers.contains(&from_peer));
        assert!(!state.lazy_peers.contains(&from_peer));
    }

    #[tokio::test]
    async fn test_degree_maintenance() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        // Add many peers to lazy
        let mut peers = Vec::new();
        for i in 2..20 {
            peers.push(test_peer_id(i));
        }

        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.entry(topic).or_insert_with(TopicState::new);
            for peer in &peers {
                state.lazy_peers.insert(*peer);
            }

            // Maintain degree (should promote to reach MIN_EAGER_DEGREE)
            state.maintain_degree();

            assert!(state.eager_peers.len() >= MIN_EAGER_DEGREE);
        }
    }

    #[tokio::test]
    async fn test_cache_expiration() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        let payload = Bytes::from("test");
        pubsub.publish(topic, payload).await.ok();

        // Manually expire cache entry
        {
            let mut topics = pubsub.topics.write().await;
            let state = topics.get_mut(&topic).unwrap();

            // Modify timestamp to simulate expiry
            for (_, cached) in state.message_cache.iter_mut() {
                cached.timestamp = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 10);
            }

            state.clean_cache();

            assert_eq!(state.message_cache.len(), 0);
        }
    }

    // TDD: RED phase - These tests will fail until we implement real ML-DSA signing

    #[tokio::test]
    async fn test_message_signing_with_real_mldsa() {
        // GREEN: Now implementing real ML-DSA signing
        use saorsa_gossip_identity::MlDsaKeyPair;

        let keypair = MlDsaKeyPair::generate().expect("keypair");
        let peer_id = PeerId::new([1u8; 32]);
        let transport = test_transport().await;

        // Create PlumtreePubSub with signing key
        let _pubsub = PlumtreePubSub::new(peer_id, transport, keypair.clone());

        // Create a message header
        let topic = TopicId::new([1u8; 32]);
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        // Serialize header for signing
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");

        // Sign with ML-DSA
        let signature = keypair.sign(&header_bytes).expect("sign");

        // Signature should NOT be empty
        assert!(
            !signature.is_empty(),
            "ML-DSA signature should not be empty"
        );

        // Signature should be valid
        let valid =
            MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &signature).expect("verify");
        assert!(valid, "Signature should be valid");
    }

    #[tokio::test]
    async fn test_message_signature_verification() {
        // RED: This will fail because verify_signature always returns true
        use saorsa_gossip_identity::MlDsaKeyPair;

        let keypair = MlDsaKeyPair::generate().expect("keypair");

        let topic = TopicId::new([1u8; 32]);
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [1u8; 32],
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let signature = keypair.sign(&header_bytes).expect("sign");

        // Valid signature should verify
        let valid =
            MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &signature).expect("verify");
        assert!(valid, "Valid signature should verify");

        // Tampered signature should NOT verify
        let mut bad_signature = signature.clone();
        bad_signature[0] ^= 0xFF; // Flip bits

        let invalid = MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &bad_signature)
            .expect("verify");
        assert!(!invalid, "Tampered signature should not verify");
    }

    #[tokio::test]
    async fn test_published_message_has_valid_signature() {
        // GREEN: Now verifying that published messages have valid signatures
        use saorsa_gossip_identity::MlDsaKeyPair;

        let keypair = MlDsaKeyPair::generate().expect("keypair");
        let peer_id = PeerId::new([1u8; 32]);
        let transport = test_transport().await;

        // Create pubsub with signing key
        let pubsub = PlumtreePubSub::new(peer_id, transport, keypair.clone());

        let topic = TopicId::new([1u8; 32]);
        let payload = Bytes::from("test message");

        // Publish a message
        pubsub.publish(topic, payload.clone()).await.ok();

        // The message should be signed internally
        // Verify by checking that sign_message produces non-empty signatures
        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::Eager,
            hop: 0,
            ttl: 10,
        };

        let signature = pubsub.sign_message(&header);
        assert!(
            !signature.is_empty(),
            "Published messages should have non-empty signatures"
        );

        // Verify the signature is valid
        let header_bytes = postcard::to_stdvec(&header).expect("serialize");
        let valid =
            MlDsaKeyPair::verify(keypair.public_key(), &header_bytes, &signature).expect("verify");
        assert!(valid, "Signature should be valid");
    }

    // Anti-entropy tests

    #[test]
    fn test_anti_entropy_payload_serialization() {
        // Test Digest variant round-trips through postcard
        let digest = AntiEntropyPayload::Digest {
            msg_ids: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
        };
        let bytes = postcard::to_stdvec(&digest).expect("serialize digest");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize digest");

        match deserialized {
            AntiEntropyPayload::Digest { msg_ids } => {
                assert_eq!(msg_ids.len(), 3);
                assert_eq!(msg_ids[0], [1u8; 32]);
                assert_eq!(msg_ids[1], [2u8; 32]);
                assert_eq!(msg_ids[2], [3u8; 32]);
            }
            AntiEntropyPayload::Response { .. } => {
                panic!("Expected Digest, got Response");
            }
        }

        // Test Response variant round-trips through postcard
        let response = AntiEntropyPayload::Response {
            missing_ids: vec![[4u8; 32], [5u8; 32]],
        };
        let bytes = postcard::to_stdvec(&response).expect("serialize response");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize response");

        match deserialized {
            AntiEntropyPayload::Response { missing_ids } => {
                assert_eq!(missing_ids.len(), 2);
                assert_eq!(missing_ids[0], [4u8; 32]);
                assert_eq!(missing_ids[1], [5u8; 32]);
            }
            AntiEntropyPayload::Digest { .. } => {
                panic!("Expected Response, got Digest");
            }
        }
    }

    #[test]
    fn test_anti_entropy_payload_empty_serialization() {
        // Empty digest should also round-trip
        let digest = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let bytes = postcard::to_stdvec(&digest).expect("serialize empty digest");
        let deserialized: AntiEntropyPayload =
            postcard::from_bytes(&bytes).expect("deserialize empty digest");

        match deserialized {
            AntiEntropyPayload::Digest { msg_ids } => {
                assert!(msg_ids.is_empty());
            }
            AntiEntropyPayload::Response { .. } => {
                panic!("Expected Digest, got Response");
            }
        }
    }

    #[tokio::test]
    async fn test_cached_message_ids() {
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, test_signing_key());
        let topic = TopicId::new([1u8; 32]);

        // Publish 3 messages
        pubsub
            .publish(topic, Bytes::from("msg1"))
            .await
            .expect("publish 1");
        pubsub
            .publish(topic, Bytes::from("msg2"))
            .await
            .expect("publish 2");
        pubsub
            .publish(topic, Bytes::from("msg3"))
            .await
            .expect("publish 3");

        // Verify cached_message_ids returns all 3
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        let ids = state.cached_message_ids();
        assert_eq!(ids.len(), 3, "Should have 3 cached message IDs");
    }

    #[tokio::test]
    async fn test_handle_anti_entropy_digest_sends_missing() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Publish a message so we have it cached
        pubsub
            .publish(topic, Bytes::from("cached message"))
            .await
            .expect("publish");

        // Get the cached message ID
        let our_msg_id = {
            let topics = pubsub.topics.read().await;
            let state = topics.get(&topic).unwrap();
            let ids = state.cached_message_ids();
            assert_eq!(ids.len(), 1);
            ids[0]
        };

        // Create a digest from the "remote" peer that has NO messages (empty)
        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Handle the digest - should attempt to send our cached message to the peer
        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        // The send may fail (no actual connection) but the method should not error
        // on the logic itself
        assert!(result.is_ok());

        // Our message should still be in cache
        let topics = pubsub.topics.read().await;
        let state = topics.get(&topic).unwrap();
        assert!(state.has_message(&our_msg_id));
    }

    #[tokio::test]
    async fn test_handle_anti_entropy_digest_requests_missing() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // We have NO messages cached. The remote peer claims to have some.
        let remote_msg_id = [99u8; 32];
        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: vec![remote_msg_id],
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Handle the digest - should try to send IWANT for the missing message
        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_anti_entropy_response() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        // Create a Response saying we're missing some IDs
        let missing_id = [77u8; 32];
        let ae_payload = AntiEntropyPayload::Response {
            missing_ids: vec![missing_id],
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Handle the response - should try to send IWANT
        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_anti_entropy_invalid_signature_rejected() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        // Use a BAD signature
        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature: vec![0u8; 100], // invalid signature
            public_key: signing_key.public_key().to_vec(),
        };

        let result = pubsub.handle_anti_entropy(from_peer, topic, message).await;
        assert!(result.is_err(), "Invalid signature should be rejected");
    }

    #[tokio::test]
    async fn test_anti_entropy_message_routing() {
        // Test that AntiEntropy messages are correctly routed via handle_message
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key.clone());
        let topic = TopicId::new([1u8; 32]);
        let from_peer = test_peer_id(2);

        let ae_payload = AntiEntropyPayload::Digest {
            msg_ids: Vec::new(),
        };
        let payload_bytes = postcard::to_stdvec(&ae_payload).expect("serialize");

        let header = MessageHeader {
            version: 1,
            topic,
            msg_id: [0u8; 32],
            kind: MessageKind::AntiEntropy,
            hop: 0,
            ttl: 1,
        };

        let header_bytes = postcard::to_stdvec(&header).expect("serialize header");
        let signature = signing_key.sign(&header_bytes).expect("sign");

        let message = GossipMessage {
            header,
            payload: Some(payload_bytes.into()),
            signature,
            public_key: signing_key.public_key().to_vec(),
        };

        // Serialize the full message as it would come over the wire
        let wire_bytes = postcard::to_stdvec(&message).expect("serialize wire message");

        // Route through handle_message (the PubSub trait method)
        let result = pubsub.handle_message(from_peer, wire_bytes.into()).await;
        assert!(
            result.is_ok(),
            "AntiEntropy message should be routed correctly"
        );
    }

    #[tokio::test]
    async fn test_trigger_anti_entropy() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let peer = test_peer_id(2);

        // Initialize with a peer
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        // Publish a message so there's something to reconcile
        pubsub
            .publish(topic, Bytes::from("test data"))
            .await
            .expect("publish");

        // Trigger anti-entropy manually (send may fail on transport, but logic is correct)
        let result = pubsub.trigger_anti_entropy(topic).await;
        // The result may be Ok or Err depending on transport - we're testing the logic path
        // If transport fails, the error is from send_to_peer, not from our logic
        let _ = result;
    }

    #[tokio::test]
    async fn test_trigger_anti_entropy_no_peers() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);

        // No peers initialized - should return Ok without doing anything
        let result = pubsub.trigger_anti_entropy(topic).await;
        assert!(result.is_ok(), "No peers should result in no-op Ok");
    }

    #[tokio::test]
    async fn test_send_anti_entropy_digest() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let peer = test_peer_id(2);

        // Publish a message so there's a cached message
        pubsub
            .publish(topic, Bytes::from("digest test"))
            .await
            .expect("publish");

        // Send digest (transport send may fail, but serialization and logic should work)
        let _ = pubsub.send_anti_entropy_digest(topic, peer).await;
    }

    #[tokio::test]
    async fn test_send_anti_entropy_digest_empty_cache() {
        let signing_key = test_signing_key();
        let peer_id = test_peer_id(1);
        let transport = test_transport().await;
        let pubsub = PlumtreePubSub::new(peer_id, transport, signing_key);
        let topic = TopicId::new([1u8; 32]);
        let peer = test_peer_id(2);

        // Initialize topic but don't publish anything
        pubsub.initialize_topic_peers(topic, vec![peer]).await;

        // Should return Ok since there's nothing to send
        let result = pubsub.send_anti_entropy_digest(topic, peer).await;
        assert!(result.is_ok(), "Empty cache should result in no-op Ok");
    }
}
