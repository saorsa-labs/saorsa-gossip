//! UDP Transport Adapter for Saorsa Gossip
//!
//! This module provides a production-ready QUIC-over-UDP transport using ant-quic.
//! It is the primary transport adapter for gossip communication.
//!
//! Features:
//! - Full QUIC multiplexing for membership/pubsub/bulk streams
//! - NAT traversal with hole punching
//! - Post-quantum cryptography (PQC) support via ML-KEM-768
//! - Connection pooling and management

use anyhow::{anyhow, Result};
use bytes::Bytes;
use saorsa_gossip_types::PeerId as GossipPeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tracing::{debug, error, info, warn};

use crate::{BootstrapCache, GossipStreamType, GossipTransport};

// Import ant-quic types (v0.14+ API)
use ant_quic::transport::TransportAddr;
use ant_quic::{MlDsaPublicKey, MlDsaSecretKey, Node, NodeConfig, PeerId as AntPeerId};

// Re-export key utils for tests
#[cfg(test)]
use ant_quic::{derive_peer_id_from_public_key, generate_ml_dsa_keypair};

/// Configuration for Ant-QUIC transport
#[derive(Debug, Clone)]
pub struct UdpTransportAdapterConfig {
    /// Local address to bind to
    pub bind_addr: SocketAddr,
    /// List of known peer addresses for initial discovery
    pub known_peers: Vec<SocketAddr>,
    /// Channel capacity for backpressure (default: 10,000 messages)
    pub channel_capacity: usize,
    /// Maximum bytes to read per stream (default: 100 MB)
    pub stream_read_limit: usize,
    /// Maximum number of peers to track (default: 1,000)
    pub max_peers: usize,
    /// Maximum number of concurrent sends (default: 64)
    pub max_inflight_sends: usize,
    /// Send timeout for a single message (default: 15 seconds)
    pub send_timeout: Duration,
    /// Optional ML-DSA keypair bytes (public_key, secret_key) for identity persistence
    /// If not provided, a fresh keypair is generated.
    /// This ensures the transport peer ID matches the application's identity peer ID.
    pub keypair: Option<(Vec<u8>, Vec<u8>)>,
}

impl UdpTransportAdapterConfig {
    /// Create a new configuration with required fields and sensible defaults
    pub fn new(bind_addr: SocketAddr, known_peers: Vec<SocketAddr>) -> Self {
        Self {
            bind_addr,
            known_peers,
            channel_capacity: 10_000,
            stream_read_limit: 100 * 1024 * 1024, // 100 MB
            max_peers: 1_000,
            max_inflight_sends: 64,
            send_timeout: Duration::from_secs(15),
            keypair: None,
        }
    }

    /// Set the ML-DSA keypair for identity persistence
    /// This ensures the transport peer ID matches the application's identity peer ID.
    pub fn with_keypair(mut self, public_key: Vec<u8>, secret_key: Vec<u8>) -> Self {
        self.keypair = Some((public_key, secret_key));
        self
    }

    /// Set channel capacity for backpressure
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    /// Set stream read limit
    pub fn with_stream_read_limit(mut self, limit: usize) -> Self {
        self.stream_read_limit = limit;
        self
    }

    /// Set maximum number of peers to track
    pub fn with_max_peers(mut self, max: usize) -> Self {
        self.max_peers = max;
        self
    }

    /// Set maximum number of concurrent sends
    pub fn with_max_inflight_sends(mut self, max: usize) -> Self {
        self.max_inflight_sends = max.max(1);
        self
    }

    /// Set send timeout for a single message
    pub fn with_send_timeout(mut self, timeout: Duration) -> Self {
        self.send_timeout = timeout;
        self
    }
}

/// Ant-QUIC transport implementation
///
/// Uses the ant-quic Node API for symmetric P2P networking with NAT traversal.
/// All nodes can both connect to peers and accept connections.
pub struct UdpTransportAdapter {
    /// The underlying ant-quic P2P node
    node: Arc<Node>,
    /// Incoming message channel (bounded for backpressure)
    recv_tx: mpsc::Sender<(GossipPeerId, GossipStreamType, Bytes)>,
    recv_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<(GossipPeerId, GossipStreamType, Bytes)>>>,
    /// Local peer ID (ant-quic format)
    ant_peer_id: AntPeerId,
    /// Local peer ID (gossip format)
    gossip_peer_id: GossipPeerId,
    /// Track connected peers with their addresses and last seen time
    connected_peers: Arc<RwLock<HashMap<GossipPeerId, (SocketAddr, Instant)>>>,
    /// Bootstrap peer IDs mapped to their addresses
    bootstrap_peer_ids: Arc<RwLock<HashMap<SocketAddr, GossipPeerId>>>,
    /// Optional bootstrap cache for persistent peer storage (using ant-quic's cache)
    bootstrap_cache: Option<Arc<BootstrapCache>>,
    /// Configuration
    config: UdpTransportAdapterConfig,
    /// Limit concurrent send operations to avoid exhausting QUIC stream budgets.
    send_semaphore: Arc<Semaphore>,
    /// Serialize sends per peer to avoid exhausting per-connection stream budgets.
    peer_send_locks: Arc<RwLock<HashMap<GossipPeerId, Arc<Mutex<()>>>>>,
}

impl UdpTransportAdapter {
    /// Create a new Ant-QUIC transport
    ///
    /// # Arguments
    /// * `bind_addr` - Local address to bind to
    /// * `known_peers` - List of known peer addresses for initial discovery
    pub async fn new(bind_addr: SocketAddr, known_peers: Vec<SocketAddr>) -> Result<Self> {
        let config = UdpTransportAdapterConfig::new(bind_addr, known_peers);
        Self::with_config(config, None).await
    }

    /// Create a new Ant-QUIC transport with optional bootstrap cache
    ///
    /// # Arguments
    /// * `bind_addr` - Local address to bind to
    /// * `known_peers` - List of known peer addresses
    /// * `bootstrap_cache` - Optional bootstrap cache for persistent peer storage
    pub async fn new_with_cache(
        bind_addr: SocketAddr,
        known_peers: Vec<SocketAddr>,
        bootstrap_cache: Option<Arc<BootstrapCache>>,
    ) -> Result<Self> {
        let config = UdpTransportAdapterConfig::new(bind_addr, known_peers);
        Self::with_config(config, bootstrap_cache).await
    }

    /// Create a new Ant-QUIC transport with custom configuration
    ///
    /// # Arguments
    /// * `config` - Transport configuration
    /// * `bootstrap_cache` - Optional bootstrap cache for persistent peer storage
    pub async fn with_config(
        config: UdpTransportAdapterConfig,
        bootstrap_cache: Option<Arc<BootstrapCache>>,
    ) -> Result<Self> {
        info!(
            "Creating Ant-QUIC transport at {} with {} known peers",
            config.bind_addr,
            config.known_peers.len()
        );
        info!(
            "Config: channel_capacity={}, max_peers={}, stream_read_limit={}",
            config.channel_capacity, config.max_peers, config.stream_read_limit
        );

        // Create NodeConfig with our settings
        let mut node_config_builder = NodeConfig::builder()
            .bind_addr(config.bind_addr)
            .known_peers(config.known_peers.clone());

        // If a keypair is provided, use it to ensure peer ID consistency with application identity
        if let Some((pub_key_bytes, sec_key_bytes)) = &config.keypair {
            let pub_key = MlDsaPublicKey::from_bytes(pub_key_bytes)
                .map_err(|e| anyhow!("Invalid ML-DSA public key: {}", e))?;
            let sec_key = MlDsaSecretKey::from_bytes(sec_key_bytes)
                .map_err(|e| anyhow!("Invalid ML-DSA secret key: {}", e))?;
            node_config_builder = node_config_builder.keypair(pub_key, sec_key);
            info!("Using provided ML-DSA keypair for transport identity");
        }

        let node_config = node_config_builder.build();

        // Create the Node
        let node = Node::with_config(node_config)
            .await
            .map_err(|e| anyhow!("Failed to create Node: {}", e))?;

        let ant_peer_id = node.peer_id();
        let gossip_peer_id = ant_peer_id_to_gossip(&ant_peer_id);

        info!("Peer ID: {:?}", ant_peer_id);

        // Create bounded channel for backpressure
        let (recv_tx, recv_rx) = mpsc::channel(config.channel_capacity);

        let transport = Self {
            node: Arc::new(node),
            recv_tx,
            recv_rx: Arc::new(tokio::sync::Mutex::new(recv_rx)),
            ant_peer_id,
            gossip_peer_id,
            connected_peers: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_peer_ids: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_cache: bootstrap_cache.clone(),
            config: config.clone(),
            send_semaphore: Arc::new(Semaphore::new(config.max_inflight_sends)),
            peer_send_locks: Arc::new(RwLock::new(HashMap::new())),
        };

        // Start receiving loop
        transport.spawn_receiver();

        // Connect to known peers in the background (non-blocking)
        // This allows the transport to start immediately while connections establish
        if !config.known_peers.is_empty() {
            let peer_count = config.known_peers.len();
            let node_clone = transport.node.clone();
            let known_peers = config.known_peers.clone();
            let peers_map = Arc::clone(&transport.connected_peers);
            let max_peers = transport.config.max_peers;
            info!(
                "Spawning background task to connect to {} known peer(s)...",
                peer_count
            );

            tokio::spawn(async move {
                let mut connected = 0usize;
                for addr in known_peers {
                    let already_connected = node_clone
                        .connected_peers()
                        .await
                        .iter()
                        .any(|peer| peer.remote_addr == TransportAddr::Udp(addr));

                    if already_connected {
                        debug!("Already connected to known peer {}", addr);
                        connected += 1;
                        continue;
                    }

                    match node_clone.connect_addr(addr).await {
                        Ok(_) => {
                            connected += 1;
                            if let Some(peer_conn) = node_clone
                                .connected_peers()
                                .await
                                .into_iter()
                                .find(|conn| conn.remote_addr == TransportAddr::Udp(addr))
                            {
                                let gossip_id = ant_peer_id_to_gossip(&peer_conn.peer_id);
                                add_peer_with_lru(&peers_map, gossip_id, addr, max_peers).await;
                            }
                            info!("Connected to known peer {}", addr);
                        }
                        Err(e) => {
                            warn!("Failed to connect to known peer {}: {}", addr, e);
                        }
                    }
                }

                info!("✓ Connected to {}/{} known peer(s)", connected, peer_count);
            });
        }

        Ok(transport)
    }

    /// Get local peer ID (gossip format)
    pub fn peer_id(&self) -> GossipPeerId {
        self.gossip_peer_id
    }

    /// Get local ant-quic peer ID
    pub fn ant_peer_id(&self) -> AntPeerId {
        self.ant_peer_id
    }

    /// Get list of connected peers
    ///
    /// Returns a vector of (PeerId, SocketAddr) tuples for all currently connected peers.
    /// Connections are tracked internally and expired after 5 minutes of inactivity.
    pub async fn connected_peers(&self) -> Vec<(GossipPeerId, SocketAddr)> {
        let ant_peers = self.node.connected_peers().await;
        let now = Instant::now();
        const PEER_TTL: Duration = Duration::from_secs(300);

        {
            let mut peers = self.connected_peers.write().await;
            // Refresh any active peers from the node.
            for conn in &ant_peers {
                let gossip_id = ant_peer_id_to_gossip(&conn.peer_id);
                let addr = conn.remote_addr.to_synthetic_socket_addr();
                if addr_is_usable(&addr) {
                    peers.insert(gossip_id, (addr, now));
                } else if let Some(existing_addr) = peers
                    .get(&gossip_id)
                    .map(|(existing_addr, _)| *existing_addr)
                {
                    peers.insert(gossip_id, (existing_addr, now));
                } else {
                    warn!("Ignoring unusable peer addr {} for {}", addr, gossip_id);
                }
            }
            // Drop only peers that have been inactive for a while.
            peers.retain(|_, (_addr, last_seen)| now.duration_since(*last_seen) <= PEER_TTL);
        }

        ant_peers
            .into_iter()
            .filter_map(|conn| {
                let addr = conn.remote_addr.to_synthetic_socket_addr();
                if addr_is_usable(&addr) {
                    Some((ant_peer_id_to_gossip(&conn.peer_id), addr))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get bootstrap peer ID by address
    pub async fn get_bootstrap_peer_id(&self, addr: SocketAddr) -> Option<GossipPeerId> {
        self.bootstrap_peer_ids.read().await.get(&addr).copied()
    }

    /// List all bootstrap peers
    pub async fn list_bootstrap_peers(&self) -> Vec<(SocketAddr, GossipPeerId)> {
        self.bootstrap_peer_ids
            .read()
            .await
            .iter()
            .map(|(addr, peer_id)| (*addr, *peer_id))
            .collect()
    }

    /// Get peer ID for any connected peer
    pub async fn get_connected_peer_id(&self, addr: SocketAddr) -> Option<GossipPeerId> {
        // Check bootstrap peers first
        if let Some(peer_id) = self.bootstrap_peer_ids.read().await.get(&addr) {
            return Some(*peer_id);
        }

        // Check regular connected peers
        self.connected_peers
            .read()
            .await
            .iter()
            .find(|(_, (peer_addr, _))| *peer_addr == addr)
            .map(|(peer_id, _)| *peer_id)
    }

    /// Get reference to bootstrap cache if configured
    pub fn bootstrap_cache(&self) -> Option<&Arc<BootstrapCache>> {
        self.bootstrap_cache.as_ref()
    }

    /// Get the external/reflexive address as observed by remote peers
    ///
    /// This returns the public address of this endpoint as seen by other peers.
    /// Returns `None` if no external address has been discovered yet.
    pub fn get_external_address(&self) -> Option<SocketAddr> {
        self.node.external_addr()
    }

    /// Get access to the underlying ant-quic Node for direct operations
    ///
    /// This enables using the same QUIC endpoint for:
    /// - NAT traversal coordination
    /// - Direct connectivity testing
    /// - Hole punching
    /// - All P2P operations
    ///
    /// By exposing the node, callers can use a single QUIC endpoint for both
    /// gossip protocol messages AND direct P2P operations, avoiding the need
    /// for multiple listening ports.
    pub fn node(&self) -> Arc<Node> {
        Arc::clone(&self.node)
    }

    /// Spawn background task to receive incoming messages
    ///
    /// This spawns two tasks:
    /// 1. A receiver task that calls `node.recv()` to receive messages from ALL connected peers
    /// 2. An acceptor task that calls `node.accept()` to track incoming connections
    ///
    /// IMPORTANT: The receiver must start immediately, not wait for accept(), because:
    /// - `node.recv()` receives from ALL connected peers (both inbound and outbound)
    /// - If we only dial out (never receive incoming), we still need to receive messages
    /// - Waiting for accept() would block receiving on outbound-only connections
    fn spawn_receiver(&self) {
        let max_peers = self.config.max_peers;

        // Task 1: Receive messages from ALL connected peers (inbound and outbound)
        // This must start immediately, not wait for accept()
        let node_recv = Arc::clone(&self.node);
        let recv_tx = self.recv_tx.clone();
        let peers_recv = Arc::clone(&self.connected_peers);

        tokio::spawn(async move {
            info!("Ant-QUIC receiver task started (global message receiver)");

            loop {
                match node_recv.recv().await {
                    Ok((from_peer_id, data)) => {
                        if data.is_empty() {
                            continue;
                        }

                        let from_gossip_id = ant_peer_id_to_gossip(&from_peer_id);

                        // Parse stream type from first byte
                        let stream_type =
                            match data.first().and_then(|&b| GossipStreamType::from_byte(b)) {
                                Some(st) => st,
                                None => {
                                    if let Some(&b) = data.first() {
                                        warn!("Unknown stream type byte: {}", b);
                                    }
                                    continue;
                                }
                            };

                        // Extract payload (skip first byte)
                        let payload = if data.len() > 1 {
                            Bytes::copy_from_slice(&data[1..])
                        } else {
                            Bytes::new()
                        };

                        // Update peer tracking - only update timestamp if already known
                        update_peer_last_seen(&peers_recv, from_gossip_id).await;

                        // Forward to recv channel
                        if let Err(e) = recv_tx.send((from_gossip_id, stream_type, payload)).await {
                            error!("Failed to forward message: {}", e);
                            break;
                        }

                        debug!(
                            "Forwarded {} bytes ({:?}) from {:?}",
                            data.len() - 1,
                            stream_type,
                            from_gossip_id
                        );
                    }
                    Err(e) => {
                        // Timeout is expected, just continue
                        debug!("Receive timeout or error: {}", e);
                    }
                }
            }
        });

        // Task 2: Accept incoming connections and track them
        // This runs separately from receiving - it only tracks peer addresses
        let node_accept = Arc::clone(&self.node);
        let peers_accept = Arc::clone(&self.connected_peers);

        tokio::spawn(async move {
            info!("Ant-QUIC acceptor task started (incoming connection handler)");

            loop {
                if let Some(peer_conn) = node_accept.accept().await {
                    let peer_id = peer_conn.peer_id;
                    let transport_addr = peer_conn.remote_addr;
                    // Convert TransportAddr to SocketAddr for peer tracking
                    let peer_addr = transport_addr.to_synthetic_socket_addr();
                    let gossip_peer_id = ant_peer_id_to_gossip(&peer_id);

                    info!("Accepted connection from {:?} at {}", peer_id, peer_addr);

                    // Track the peer if the address is usable.
                    if addr_is_usable(&peer_addr) {
                        add_peer_with_lru(&peers_accept, gossip_peer_id, peer_addr, max_peers)
                            .await;
                    } else {
                        warn!(
                            "Skipping unusable peer addr {} for {}",
                            peer_addr, gossip_peer_id
                        );
                    }
                }

                // Small delay to prevent busy loop
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    /// Add or update a peer in the connected peers map with LRU eviction
    async fn add_peer(&self, peer_id: GossipPeerId, addr: SocketAddr) {
        if !addr_is_usable(&addr) {
            warn!("Skipping unusable peer addr {} for {}", addr, peer_id);
            return;
        }
        add_peer_with_lru(&self.connected_peers, peer_id, addr, self.config.max_peers).await;
    }

    /// Remove a peer from the connected peers map
    async fn remove_peer(&self, peer_id: &GossipPeerId) {
        let mut peers = self.connected_peers.write().await;
        if peers.remove(peer_id).is_some() {
            debug!("Removed peer {:?} after connection failure", peer_id);
        }
        self.peer_send_locks.write().await.remove(peer_id);
    }

    async fn cached_peer_addr(&self, peer_id: GossipPeerId) -> Option<SocketAddr> {
        {
            let peers = self.connected_peers.read().await;
            if let Some((addr, _)) = peers.get(&peer_id) {
                if addr_is_usable(addr) {
                    return Some(*addr);
                }
            }
        }

        let cache = self.bootstrap_cache.as_ref()?;
        let ant_peer_id = gossip_peer_id_to_ant(&peer_id);
        let cached = cache.get_peer(&ant_peer_id).await?;
        let addr = cached
            .addresses
            .iter()
            .copied()
            .find(addr_is_usable)
            .or_else(|| {
                cached
                    .capabilities
                    .external_addresses
                    .iter()
                    .copied()
                    .find(addr_is_usable)
            })?;

        self.add_peer(peer_id, addr).await;
        Some(addr)
    }

    async fn peer_send_lock(&self, peer: GossipPeerId) -> Arc<Mutex<()>> {
        let mut locks = self.peer_send_locks.write().await;
        locks
            .entry(peer)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn reconnect_peer(&self, peer: GossipPeerId, addr: SocketAddr) -> Result<()> {
        debug!("Attempting reconnect to peer {} at {}", peer, addr);
        match tokio::time::timeout(Duration::from_secs(2), self.node.connect_addr(addr)).await {
            Ok(Ok(peer_conn)) => {
                let gossip_id = ant_peer_id_to_gossip(&peer_conn.peer_id);
                if gossip_id != peer {
                    warn!(
                        "Reconnect to {} returned unexpected peer {}",
                        addr, gossip_id
                    );
                    self.remove_peer(&peer).await;
                    return Err(anyhow!(
                        "Reconnect returned unexpected peer {} for {}",
                        gossip_id,
                        peer
                    ));
                }
                self.add_peer(peer, addr).await;
                Ok(())
            }
            Ok(Err(err)) => Err(anyhow!(
                "Reconnect to {} for peer {} failed: {}",
                addr,
                peer,
                err
            )),
            Err(_) => Err(anyhow!("Reconnect to {} for peer {} timed out", addr, peer)),
        }
    }
}

fn addr_is_usable(addr: &SocketAddr) -> bool {
    !addr.ip().is_unspecified() && addr.port() != 0
}

/// Add a peer with LRU eviction (standalone helper for use in spawned tasks)
async fn add_peer_with_lru(
    peers: &Arc<RwLock<HashMap<GossipPeerId, (SocketAddr, Instant)>>>,
    peer_id: GossipPeerId,
    addr: SocketAddr,
    max_peers: usize,
) {
    let mut peer_map = peers.write().await;

    // If at capacity and this is a new peer, evict the oldest one
    if peer_map.len() >= max_peers && !peer_map.contains_key(&peer_id) {
        if let Some((oldest_peer_id, _)) = peer_map
            .iter()
            .min_by_key(|(_peer_id, (_addr, last_seen))| last_seen)
            .map(|(peer_id, data)| (*peer_id, data))
        {
            peer_map.remove(&oldest_peer_id);
            info!(
                "Evicted oldest peer {:?} to make room for {:?} (limit: {})",
                oldest_peer_id, peer_id, max_peers
            );
        }
    }

    peer_map.insert(peer_id, (addr, Instant::now()));
}

/// Update last_seen timestamp for an existing peer (if known)
async fn update_peer_last_seen(
    peers: &Arc<RwLock<HashMap<GossipPeerId, (SocketAddr, Instant)>>>,
    peer_id: GossipPeerId,
) {
    let mut peer_map = peers.write().await;
    if let Some((addr, _)) = peer_map.get(&peer_id) {
        let addr = *addr;
        peer_map.insert(peer_id, (addr, Instant::now()));
    }
    // If peer is not known, we can't add them without their address
}

/// Convert ant-quic PeerId to Gossip PeerId
fn ant_peer_id_to_gossip(ant_id: &AntPeerId) -> GossipPeerId {
    // ant-quic PeerId is a 32-byte array, same as GossipPeerId
    GossipPeerId::new(ant_id.0)
}

/// Convert Gossip PeerId to ant-quic PeerId
fn gossip_peer_id_to_ant(gossip_id: &GossipPeerId) -> AntPeerId {
    AntPeerId(gossip_id.to_bytes())
}

#[async_trait::async_trait]
impl GossipTransport for UdpTransportAdapter {
    async fn dial(&self, peer: GossipPeerId, addr: SocketAddr) -> Result<()> {
        info!("Dialing peer {} at {}", peer, addr);

        // Connect to the peer by address
        let start = Instant::now();
        let ant_peer_id = gossip_peer_id_to_ant(&peer);

        if self.node.is_connected(&ant_peer_id).await {
            info!("Already connected to peer {} at {}", peer, addr);
            self.add_peer(peer, addr).await;
            return Ok(());
        }

        match self.node.connect_addr(addr).await {
            Ok(peer_conn) => {
                let gossip_id = ant_peer_id_to_gossip(&peer_conn.peer_id);
                let rtt_ms = start.elapsed().as_millis() as u32;
                info!(
                    "Successfully connected to peer {} at {} (rtt: {}ms)",
                    gossip_id, addr, rtt_ms
                );

                if gossip_id != peer {
                    warn!(
                        "Connected to peer {} at {} but expected {}",
                        gossip_id, addr, peer
                    );
                    self.remove_peer(&peer).await;
                    return Err(anyhow!(
                        "Connected to unexpected peer {} when dialing {}",
                        gossip_id,
                        peer
                    ));
                }

                // Track the connection
                self.add_peer(gossip_id, addr).await;

                // Update bootstrap cache if present
                if let Some(cache) = &self.bootstrap_cache {
                    let ant_peer_id = gossip_peer_id_to_ant(&gossip_id);
                    cache.record_success(&ant_peer_id, rtt_ms).await;
                }

                Ok(())
            }
            Err(e) => {
                warn!("Failed to connect to peer at {}: {}", addr, e);

                // Record failure in bootstrap cache
                if let Some(cache) = &self.bootstrap_cache {
                    let ant_peer_id = gossip_peer_id_to_ant(&peer);
                    cache.record_failure(&ant_peer_id).await;
                }

                self.remove_peer(&peer).await;
                Err(anyhow!("Failed to connect to peer: {}", e))
            }
        }
    }

    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<GossipPeerId> {
        info!("Dialing bootstrap node at {}", addr);

        let start = Instant::now();
        match self.node.connect_addr(addr).await {
            Ok(peer_conn) => {
                let gossip_peer_id = ant_peer_id_to_gossip(&peer_conn.peer_id);
                let rtt_ms = start.elapsed().as_millis() as u32;

                info!(
                    "Successfully connected to bootstrap {} (PeerId: {}, rtt: {}ms)",
                    addr, gossip_peer_id, rtt_ms
                );

                // Store bootstrap peer ID
                self.bootstrap_peer_ids
                    .write()
                    .await
                    .insert(addr, gossip_peer_id);

                // Also track in connected_peers
                self.add_peer(gossip_peer_id, addr).await;

                // Update bootstrap cache if present
                if let Some(cache) = &self.bootstrap_cache {
                    let ant_peer_id = gossip_peer_id_to_ant(&gossip_peer_id);
                    cache.record_success(&ant_peer_id, rtt_ms).await;
                }

                Ok(gossip_peer_id)
            }
            Err(e) => {
                warn!("Failed to connect to bootstrap {}: {}", addr, e);
                Err(anyhow!("Failed to connect to bootstrap: {}", e))
            }
        }
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<()> {
        // Node handles listening automatically
        info!("Ant-QUIC node is listening (handled by Node)");
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        info!("Closing Ant-QUIC transport");
        // Node cleanup is handled by Drop
        Ok(())
    }

    async fn send_to_peer(
        &self,
        peer: GossipPeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        let _send_permit = self
            .send_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow!("Send semaphore closed"))?;
        let peer_lock = self.peer_send_lock(peer).await;
        let _peer_guard = peer_lock.lock().await;
        debug!(
            "Sending {} bytes to peer {} on {:?} stream",
            data.len(),
            peer,
            stream_type
        );

        // Refresh connected peers from the underlying node to avoid stale state.
        let _ = self.connected_peers().await;

        // Convert gossip PeerId to ant-quic PeerId
        let ant_peer_id = gossip_peer_id_to_ant(&peer);

        if !self.node.is_connected(&ant_peer_id).await {
            if let Some(addr) = self.cached_peer_addr(peer).await {
                debug!(
                    "Peer {} not connected; attempting reconnect to {}",
                    peer, addr
                );
                if let Err(err) = self.reconnect_peer(peer, addr).await {
                    warn!("Reconnect to {} for peer {} failed: {}", addr, peer, err);
                    return Err(err);
                }
            } else {
                return Err(anyhow!(
                    "Peer {} not connected and no cached address is available",
                    peer
                ));
            }
        }

        // Prepare message: [stream_type_byte | data]
        let mut buf = Vec::with_capacity(1 + data.len());
        buf.push(stream_type.to_byte());
        buf.extend_from_slice(&data);

        // Send via the node
        let mut last_err = match tokio::time::timeout(
            self.config.send_timeout,
            self.node.send(&ant_peer_id, &buf),
        )
        .await
        {
            Ok(Ok(())) => {
                info!("Successfully sent {} bytes to peer {}", buf.len(), peer);
                return Ok(());
            }
            Ok(Err(e)) => {
                warn!("Failed to send to peer {}: {}", peer, e);
                anyhow!("Failed to send to peer {}: {}", peer, e)
            }
            Err(_) => {
                warn!("Send to peer {} timed out", peer);
                anyhow!("Send to peer {} timed out", peer)
            }
        };

        let retry_addr = self.cached_peer_addr(peer).await;
        if let Some(addr) = retry_addr {
            debug!("Retrying send to peer {} via reconnect to {}", peer, addr);
            let _ = self.node.disconnect(&ant_peer_id).await;
            if let Err(err) = self.reconnect_peer(peer, addr).await {
                warn!(
                    "Retry reconnect to {} for peer {} failed: {}",
                    addr, peer, err
                );
                return Err(err);
            }
            match tokio::time::timeout(self.config.send_timeout, self.node.send(&ant_peer_id, &buf))
                .await
            {
                Ok(Ok(())) => {
                    info!(
                        "Successfully sent {} bytes to peer {} after reconnect",
                        buf.len(),
                        peer
                    );
                    return Ok(());
                }
                Ok(Err(e)) => {
                    warn!("Retry send to peer {} failed: {}", peer, e);
                    last_err = anyhow!("Retry send to peer {} failed: {}", peer, e);
                }
                Err(_) => {
                    warn!("Retry send to peer {} timed out", peer);
                    last_err = anyhow!("Retry send to peer {} timed out", peer);
                }
            }
        } else {
            return Err(anyhow!(
                "Failed to send to peer {} (no cached address): {}",
                peer,
                last_err
            ));
        }

        Err(last_err)
    }

    async fn receive_message(&self) -> Result<(GossipPeerId, GossipStreamType, Bytes)> {
        let mut recv_rx = self.recv_rx.lock().await;

        recv_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("Receive channel closed"))
    }

    fn local_peer_id(&self) -> GossipPeerId {
        self.gossip_peer_id
    }
}

// =============================================================================
// TransportAdapter Implementation
// =============================================================================

use crate::error::{TransportError, TransportResult};
use crate::{TransportAdapter, TransportCapabilities};

#[async_trait::async_trait]
impl TransportAdapter for UdpTransportAdapter {
    fn local_peer_id(&self) -> GossipPeerId {
        self.gossip_peer_id
    }

    async fn dial(&self, addr: SocketAddr) -> TransportResult<GossipPeerId> {
        debug!("TransportAdapter::dial to {}", addr);

        let start = Instant::now();
        if let Some((peer_id, _)) = self
            .connected_peers()
            .await
            .into_iter()
            .find(|(_peer_id, peer_addr)| *peer_addr == addr)
        {
            debug!("Already connected to peer {} at {}", peer_id, addr);
            return Ok(peer_id);
        }

        match self.node.connect_addr(addr).await {
            Ok(peer_conn) => {
                let gossip_peer_id = ant_peer_id_to_gossip(&peer_conn.peer_id);
                let rtt_ms = start.elapsed().as_millis() as u32;
                info!(
                    "Connected to peer {} at {} (rtt: {}ms)",
                    gossip_peer_id, addr, rtt_ms
                );

                // Track the connection
                self.add_peer(gossip_peer_id, addr).await;

                // Update bootstrap cache if present
                if let Some(cache) = &self.bootstrap_cache {
                    let ant_peer_id = gossip_peer_id_to_ant(&gossip_peer_id);
                    cache.record_success(&ant_peer_id, rtt_ms).await;
                }

                Ok(gossip_peer_id)
            }
            Err(e) => {
                warn!("Failed to dial {}: {}", addr, e);

                Err(TransportError::DialFailed {
                    addr,
                    source: anyhow!("{}", e),
                })
            }
        }
    }

    async fn send(
        &self,
        peer_id: GossipPeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> TransportResult<()> {
        let _send_permit =
            self.send_semaphore
                .acquire()
                .await
                .map_err(|_| TransportError::SendFailed {
                    peer_id,
                    source: anyhow!("Send semaphore closed"),
                })?;
        let peer_lock = self.peer_send_lock(peer_id).await;
        let _peer_guard = peer_lock.lock().await;
        let ant_peer_id = gossip_peer_id_to_ant(&peer_id);

        // Refresh connected peers from the underlying node to avoid stale state.
        let _ = self.connected_peers().await;

        if !self.node.is_connected(&ant_peer_id).await {
            if let Some(addr) = self.cached_peer_addr(peer_id).await {
                debug!(
                    "Peer {} not connected; attempting reconnect to {}",
                    peer_id, addr
                );
                if let Err(err) = self.reconnect_peer(peer_id, addr).await {
                    warn!("Reconnect to {} for peer {} failed: {}", addr, peer_id, err);
                    return Err(TransportError::SendFailed {
                        peer_id,
                        source: err,
                    });
                }
            } else {
                return Err(TransportError::SendFailed {
                    peer_id,
                    source: anyhow!(
                        "Peer {} not connected and no cached address is available",
                        peer_id
                    ),
                });
            }
        }

        // Prepend stream type byte to payload (same format as GossipTransport)
        let mut payload = Vec::with_capacity(1 + data.len());
        payload.push(stream_type.to_byte());
        payload.extend_from_slice(&data);

        // Use ant-quic's send() with peer ID and raw bytes
        let mut last_err = match tokio::time::timeout(
            self.config.send_timeout,
            self.node.send(&ant_peer_id, &payload),
        )
        .await
        {
            Ok(Ok(())) => {
                debug!(
                    "Sent {} bytes to peer {} on {:?}",
                    data.len(),
                    peer_id,
                    stream_type
                );
                return Ok(());
            }
            Ok(Err(e)) => {
                warn!("Failed to send to peer {}: {}", peer_id, e);
                anyhow!("{}", e)
            }
            Err(_) => {
                warn!("Send to peer {} timed out", peer_id);
                anyhow!("Send to peer timed out")
            }
        };

        let retry_addr = self.cached_peer_addr(peer_id).await;
        if let Some(addr) = retry_addr {
            debug!(
                "Retrying send to peer {} via reconnect to {}",
                peer_id, addr
            );
            let _ = self.node.disconnect(&ant_peer_id).await;
            if let Err(err) = self.reconnect_peer(peer_id, addr).await {
                warn!(
                    "Retry reconnect to {} for peer {} failed: {}",
                    addr, peer_id, err
                );
                return Err(TransportError::SendFailed {
                    peer_id,
                    source: err,
                });
            }
            match tokio::time::timeout(
                self.config.send_timeout,
                self.node.send(&ant_peer_id, &payload),
            )
            .await
            {
                Ok(Ok(())) => {
                    debug!(
                        "Sent {} bytes to peer {} on {:?} after reconnect",
                        data.len(),
                        peer_id,
                        stream_type
                    );
                    return Ok(());
                }
                Ok(Err(e)) => {
                    warn!("Retry send to peer {} failed: {}", peer_id, e);
                    last_err = anyhow!("{}", e);
                }
                Err(_) => {
                    warn!("Retry send to peer {} timed out", peer_id);
                    last_err = anyhow!("Retry send to peer timed out");
                }
            }
        } else {
            return Err(TransportError::SendFailed {
                peer_id,
                source: anyhow!(
                    "Failed to send to peer {} (no cached address): {}",
                    peer_id,
                    last_err
                ),
            });
        }

        Err(TransportError::SendFailed {
            peer_id,
            source: last_err,
        })
    }

    async fn recv(&self) -> TransportResult<(GossipPeerId, GossipStreamType, Bytes)> {
        let mut recv_rx = self.recv_rx.lock().await;

        recv_rx.recv().await.ok_or_else(|| TransportError::Closed)
    }

    async fn close(&self) -> TransportResult<()> {
        info!("Closing TransportAdapter");
        // The underlying node will be closed when dropped
        // For now, just clear tracked peers
        let mut peers = self.connected_peers.write().await;
        peers.clear();
        Ok(())
    }

    async fn connected_peers(&self) -> Vec<(GossipPeerId, SocketAddr)> {
        self.connected_peers().await
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            supports_broadcast: false,
            max_message_size: self.config.stream_read_limit,
            typical_latency_ms: 50, // Typical UDP/QUIC latency
            is_reliable: true,      // QUIC provides reliable delivery
            name: "UDP/QUIC",
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ==========================================================================
    // UdpTransportAdapter Creation Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_udp_transport_adapter_creation() {
        let bind_addr = "127.0.0.1:0".parse().expect("Invalid address");
        let transport = UdpTransportAdapter::new(bind_addr, vec![])
            .await
            .expect("Failed to create transport");

        assert_ne!(transport.peer_id(), GossipPeerId::new([0u8; 32]));
    }

    #[tokio::test]
    async fn test_udp_transport_adapter_with_config() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let config = UdpTransportAdapterConfig {
            bind_addr,
            known_peers: vec![],
            channel_capacity: 10_000,
            stream_read_limit: 100 * 1024 * 1024,
            max_peers: 1_000,
            max_inflight_sends: 64,
            send_timeout: Duration::from_secs(15),
            keypair: None,
        };
        let transport = UdpTransportAdapter::with_config(config, None)
            .await
            .expect("Failed to create transport with config");

        assert_ne!(transport.peer_id(), GossipPeerId::new([0u8; 32]));
    }

    #[tokio::test]
    async fn test_udp_transport_adapter_unique_peer_ids() {
        // Create two transports and ensure they have unique peer IDs
        let addr1: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let addr2: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");

        let transport1 = UdpTransportAdapter::new(addr1, vec![])
            .await
            .expect("Failed to create transport1");
        let transport2 = UdpTransportAdapter::new(addr2, vec![])
            .await
            .expect("Failed to create transport2");

        assert_ne!(
            transport1.peer_id(),
            transport2.peer_id(),
            "Each transport should have a unique peer ID"
        );
    }

    #[tokio::test]
    async fn test_udp_transport_adapter_peer_id_consistency() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let transport = UdpTransportAdapter::new(bind_addr, vec![])
            .await
            .expect("Failed to create transport");

        // Peer ID should be consistent across multiple calls
        let id1 = transport.peer_id();
        let id2 = transport.peer_id();
        assert_eq!(id1, id2, "Peer ID should be consistent");
    }

    // ==========================================================================
    // UdpTransportAdapterConfig Tests
    // ==========================================================================

    #[test]
    fn test_udp_transport_adapter_config_defaults() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("valid address");
        let config = UdpTransportAdapterConfig {
            bind_addr,
            known_peers: vec![],
            channel_capacity: 10_000,
            stream_read_limit: 100 * 1024 * 1024,
            max_peers: 1_000,
            max_inflight_sends: 64,
            send_timeout: Duration::from_secs(15),
            keypair: None,
        };
        assert_eq!(config.channel_capacity, 10_000);
        assert_eq!(config.max_peers, 1_000);
        assert!(config.keypair.is_none());
    }

    #[test]
    fn test_udp_transport_adapter_config_with_known_peers() {
        let bind_addr: SocketAddr = "127.0.0.1:9000".parse().expect("valid address");
        let peer1: SocketAddr = "192.168.1.1:9000".parse().expect("valid address");
        let peer2: SocketAddr = "192.168.1.2:9000".parse().expect("valid address");

        let config = UdpTransportAdapterConfig {
            bind_addr,
            known_peers: vec![peer1, peer2],
            channel_capacity: 5_000,
            stream_read_limit: 50 * 1024 * 1024,
            max_peers: 500,
            max_inflight_sends: 64,
            send_timeout: Duration::from_secs(15),
            keypair: None,
        };

        assert_eq!(config.known_peers.len(), 2);
        assert_eq!(config.channel_capacity, 5_000);
        assert_eq!(config.max_peers, 500);
    }

    #[test]
    fn test_udp_transport_adapter_config_with_keypair_bytes() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("valid address");
        // Test that config can hold keypair bytes (actual key generation tested separately)
        let test_public_key = vec![1u8; 2592]; // ML-DSA-65 public key size
        let test_secret_key = vec![2u8; 4032]; // ML-DSA-65 secret key size

        let config = UdpTransportAdapterConfig {
            bind_addr,
            known_peers: vec![],
            channel_capacity: 10_000,
            stream_read_limit: 100 * 1024 * 1024,
            max_peers: 1_000,
            max_inflight_sends: 64,
            send_timeout: Duration::from_secs(15),
            keypair: Some((test_public_key.clone(), test_secret_key.clone())),
        };

        assert!(config.keypair.is_some());
        let (pk, sk) = config.keypair.unwrap();
        assert_eq!(pk.len(), 2592);
        assert_eq!(sk.len(), 4032);
    }

    // ==========================================================================
    // Peer ID Conversion Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_peer_id_conversion() {
        // Generate test peer ID using ML-DSA keypair
        let (public_key, _secret_key) =
            generate_ml_dsa_keypair().expect("Failed to generate keypair");
        let ant_id = derive_peer_id_from_public_key(&public_key);

        // Convert to gossip and back
        let gossip_id = ant_peer_id_to_gossip(&ant_id);
        let ant_id_back = gossip_peer_id_to_ant(&gossip_id);

        assert_eq!(ant_id, ant_id_back);
    }

    #[tokio::test]
    async fn test_peer_id_conversion_deterministic() {
        let (public_key, _secret_key) =
            generate_ml_dsa_keypair().expect("Failed to generate keypair");
        let ant_id = derive_peer_id_from_public_key(&public_key);

        // Multiple conversions should produce same result
        let gossip_id1 = ant_peer_id_to_gossip(&ant_id);
        let gossip_id2 = ant_peer_id_to_gossip(&ant_id);
        assert_eq!(gossip_id1, gossip_id2);
    }

    #[test]
    fn test_gossip_peer_id_to_ant_deterministic() {
        let gossip_id = GossipPeerId::new([42u8; 32]);

        let ant_id1 = gossip_peer_id_to_ant(&gossip_id);
        let ant_id2 = gossip_peer_id_to_ant(&gossip_id);
        assert_eq!(ant_id1, ant_id2);
    }

    #[test]
    fn test_peer_id_conversion_preserves_bytes() {
        let original_bytes = [123u8; 32];
        let gossip_id = GossipPeerId::new(original_bytes);
        let ant_id = gossip_peer_id_to_ant(&gossip_id);
        let recovered = ant_peer_id_to_gossip(&ant_id);

        assert_eq!(gossip_id, recovered);
    }

    // ==========================================================================
    // StreamType Encoding Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_stream_type_encoding() {
        // Test to_byte()
        assert_eq!(GossipStreamType::Membership.to_byte(), 0u8);
        assert_eq!(GossipStreamType::PubSub.to_byte(), 1u8);
        assert_eq!(GossipStreamType::Bulk.to_byte(), 2u8);

        // Test from_byte()
        assert_eq!(
            GossipStreamType::from_byte(0),
            Some(GossipStreamType::Membership)
        );
        assert_eq!(
            GossipStreamType::from_byte(1),
            Some(GossipStreamType::PubSub)
        );
        assert_eq!(GossipStreamType::from_byte(2), Some(GossipStreamType::Bulk));
        assert_eq!(GossipStreamType::from_byte(3), None);
        assert_eq!(GossipStreamType::from_byte(255), None);

        // Test round-trip
        for st in [
            GossipStreamType::Membership,
            GossipStreamType::PubSub,
            GossipStreamType::Bulk,
        ] {
            assert_eq!(GossipStreamType::from_byte(st.to_byte()), Some(st));
        }
    }

    #[test]
    fn test_stream_type_all_variants_have_unique_bytes() {
        let membership = GossipStreamType::Membership.to_byte();
        let pubsub = GossipStreamType::PubSub.to_byte();
        let bulk = GossipStreamType::Bulk.to_byte();

        assert_ne!(membership, pubsub);
        assert_ne!(membership, bulk);
        assert_ne!(pubsub, bulk);
    }

    // ==========================================================================
    // ML-DSA Key Generation Tests
    // ==========================================================================

    #[test]
    fn test_ml_dsa_keypair_generation_succeeds() {
        let result = generate_ml_dsa_keypair();
        assert!(result.is_ok(), "Key generation should succeed");
    }

    #[test]
    fn test_derive_peer_id_deterministic() {
        // Generate keypair and derive peer ID twice - should be same
        let (public_key, _secret_key) =
            generate_ml_dsa_keypair().expect("Failed to generate keypair");

        let id1 = derive_peer_id_from_public_key(&public_key);
        let id2 = derive_peer_id_from_public_key(&public_key);

        assert_eq!(id1, id2, "Same public key should produce same peer ID");
    }

    #[test]
    fn test_derive_peer_id_different_keys_produce_different_ids() {
        let (pk1, _) = generate_ml_dsa_keypair().expect("keypair 1");
        let (pk2, _) = generate_ml_dsa_keypair().expect("keypair 2");

        let id1 = derive_peer_id_from_public_key(&pk1);
        let id2 = derive_peer_id_from_public_key(&pk2);

        assert_ne!(id1, id2, "Different keys should produce different peer IDs");
    }

    // ==========================================================================
    // Transport Close Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_udp_transport_adapter_close() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let transport = UdpTransportAdapter::new(bind_addr, vec![])
            .await
            .expect("Failed to create transport");

        let result = GossipTransport::close(&transport).await;
        assert!(result.is_ok(), "Close should succeed");
    }

    #[tokio::test]
    async fn test_udp_transport_adapter_close_idempotent() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let transport = UdpTransportAdapter::new(bind_addr, vec![])
            .await
            .expect("Failed to create transport");

        // Close twice - should be safe (using GossipTransport trait)
        let result1: Result<()> = GossipTransport::close(&transport).await;
        let result2: Result<()> = GossipTransport::close(&transport).await;
        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }

    // ==========================================================================
    // Integration Tests (Ignored by default)
    // ==========================================================================

    #[tokio::test]
    #[ignore] // Integration test - requires running ant-quic nodes
    async fn test_two_node_communication() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Once;
        use tokio::time::{sleep, timeout, Duration};

        fn init_tracing() {
            static INIT: Once = Once::new();
            INIT.call_once(|| {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::from_default_env()
                            .add_directive("info".parse().expect("directive")),
                    )
                    .with_test_writer()
                    .try_init();
            });
        }

        init_tracing();

        // Dynamic port allocation to avoid conflicts
        let base_port = 20000
            + (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() % 1000)
                .unwrap_or(0) as u16);

        // Create first node
        let node1_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), base_port);
        let node1 = UdpTransportAdapter::new(node1_addr, vec![])
            .await
            .expect("Failed to create node1");

        // Give node1 time to start
        sleep(Duration::from_millis(100)).await;

        // Create second node (also includes node1 as known peer)
        let node2_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), base_port + 1);
        let node2 = UdpTransportAdapter::new(node2_addr, vec![node1_addr])
            .await
            .expect("Failed to create node2");

        // Give nodes time to establish connection
        sleep(Duration::from_millis(500)).await;

        // Test sending from node2 to node1
        let test_data = Bytes::from("Hello, QUIC!");
        let node1_peer_id = node1.peer_id();

        // Dial node1 from node2 (using GossipTransport trait)
        GossipTransport::dial(&node2, node1_peer_id, node1_addr)
            .await
            .expect("Failed to dial node1");

        // Give connection time to establish and be accepted
        let start = std::time::Instant::now();
        let timeout_wait = Duration::from_secs(5);
        loop {
            if start.elapsed() > timeout_wait {
                let node1_peers = node1.connected_peers().await;
                let node2_peers = node2.connected_peers().await;
                panic!(
                    "Timed out waiting for node1 to accept node2. node1_peers={:?} node2_peers={:?}",
                    node1_peers, node2_peers
                );
            }

            let node1_peers = node1.connected_peers().await;
            if node1_peers
                .iter()
                .any(|(peer_id, _)| *peer_id == node2.peer_id())
            {
                break;
            }

            sleep(Duration::from_millis(100)).await;
        }

        let node1_ant_peers = node1.node().connected_peers().await;
        let node2_ant_peers = node2.node().connected_peers().await;
        tracing::info!(
            node1_peers = ?node1_ant_peers,
            node2_peers = ?node2_ant_peers,
            "Ant-QUIC connected peers after accept"
        );

        // Send message
        node2
            .send_to_peer(node1_peer_id, GossipStreamType::PubSub, test_data.clone())
            .await
            .expect("Failed to send message");

        // Receive message on node1 with timeout
        let result = timeout(Duration::from_secs(5), node1.receive_message()).await;

        match result {
            Ok(Ok((peer_id, stream_type, data))) => {
                assert_eq!(peer_id, node2.peer_id());
                assert_eq!(stream_type, GossipStreamType::PubSub);
                assert_eq!(data, test_data);
            }
            Ok(Err(e)) => panic!("Receive error: {}", e),
            Err(_) => panic!("Receive timeout"),
        }
    }
}
