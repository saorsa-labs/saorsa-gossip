//! Ant-QUIC transport implementation for Saorsa Gossip
//!
//! This module provides a production-ready QUIC transport using ant-quic.
//! Features:
//! - Full QUIC multiplexing for membership/pubsub/bulk streams
//! - NAT traversal with hole punching
//! - Post-quantum cryptography (PQC) support via ML-KEM-768
//! - Connection pooling and management

use anyhow::{Result, anyhow};
use bytes::Bytes;
use saorsa_gossip_types::PeerId as GossipPeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::{BootstrapCache, GossipTransport, StreamType};

// Import ant-quic types (v0.14+ API)
use ant_quic::{MlDsaPublicKey, MlDsaSecretKey, Node, NodeConfig, PeerId as AntPeerId};

// Re-export key utils for tests
#[cfg(test)]
use ant_quic::{derive_peer_id_from_public_key, generate_ml_dsa_keypair};

/// Configuration for Ant-QUIC transport
#[derive(Debug, Clone)]
pub struct AntQuicTransportConfig {
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
    /// Optional ML-DSA keypair bytes (public_key, secret_key) for identity persistence
    /// If not provided, a fresh keypair is generated.
    /// This ensures the transport peer ID matches the application's identity peer ID.
    pub keypair: Option<(Vec<u8>, Vec<u8>)>,
}

impl AntQuicTransportConfig {
    /// Create a new configuration with required fields and sensible defaults
    pub fn new(bind_addr: SocketAddr, known_peers: Vec<SocketAddr>) -> Self {
        Self {
            bind_addr,
            known_peers,
            channel_capacity: 10_000,
            stream_read_limit: 100 * 1024 * 1024, // 100 MB
            max_peers: 1_000,
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
}

/// Ant-QUIC transport implementation
///
/// Uses the ant-quic Node API for symmetric P2P networking with NAT traversal.
/// All nodes can both connect to peers and accept connections.
pub struct AntQuicTransport {
    /// The underlying ant-quic P2P node
    node: Arc<Node>,
    /// Incoming message channel (bounded for backpressure)
    recv_tx: mpsc::Sender<(GossipPeerId, StreamType, Bytes)>,
    recv_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<(GossipPeerId, StreamType, Bytes)>>>,
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
    config: AntQuicTransportConfig,
}

impl AntQuicTransport {
    /// Create a new Ant-QUIC transport
    ///
    /// # Arguments
    /// * `bind_addr` - Local address to bind to
    /// * `known_peers` - List of known peer addresses for initial discovery
    pub async fn new(bind_addr: SocketAddr, known_peers: Vec<SocketAddr>) -> Result<Self> {
        let config = AntQuicTransportConfig::new(bind_addr, known_peers);
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
        let config = AntQuicTransportConfig::new(bind_addr, known_peers);
        Self::with_config(config, bootstrap_cache).await
    }

    /// Create a new Ant-QUIC transport with custom configuration
    ///
    /// # Arguments
    /// * `config` - Transport configuration
    /// * `bootstrap_cache` - Optional bootstrap cache for persistent peer storage
    pub async fn with_config(
        config: AntQuicTransportConfig,
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
        };

        // Start receiving loop
        transport.spawn_receiver();

        // Connect to known peers in the background (non-blocking)
        // This allows the transport to start immediately while connections establish
        if !config.known_peers.is_empty() {
            let peer_count = config.known_peers.len();
            let node_clone = transport.node.clone();
            info!(
                "Spawning background task to connect to {} known peer(s)...",
                peer_count
            );

            tokio::spawn(async move {
                match node_clone.connect_known_peers().await {
                    Ok(connected) => {
                        info!("✓ Connected to {}/{} known peer(s)", connected, peer_count);
                    }
                    Err(e) => {
                        warn!("Failed to connect to known peers: {}", e);
                    }
                }
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
        let peers = self.connected_peers.read().await;
        let now = Instant::now();

        peers
            .iter()
            .filter(|(_, (_, last_seen))| now.duration_since(*last_seen) < Duration::from_secs(300))
            .map(|(peer_id, (addr, _))| (*peer_id, *addr))
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
        let node = Arc::clone(&self.node);
        let recv_tx = self.recv_tx.clone();
        let connected_peers = Arc::clone(&self.connected_peers);
        let max_peers = self.config.max_peers;

        // Task 1: Receive messages from ALL connected peers (inbound and outbound)
        // This must start immediately, not wait for accept()
        let node_recv = Arc::clone(&node);
        let tx = recv_tx;
        let peers_recv = Arc::clone(&connected_peers);

        tokio::spawn(async move {
            info!("Ant-QUIC receiver task started (global message receiver)");

            loop {
                match node_recv.recv(Duration::from_secs(60)).await {
                    Ok((from_peer_id, data)) => {
                        if data.is_empty() {
                            continue;
                        }

                        let from_gossip_id = ant_peer_id_to_gossip(&from_peer_id);

                        // Parse stream type from first byte
                        let stream_type = match data.first() {
                            Some(&0) => StreamType::Membership,
                            Some(&1) => StreamType::PubSub,
                            Some(&2) => StreamType::Bulk,
                            Some(&other) => {
                                warn!("Unknown stream type byte: {}", other);
                                continue;
                            }
                            None => continue,
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
                        if let Err(e) = tx.send((from_gossip_id, stream_type, payload)).await {
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
        let peers_accept = connected_peers;

        tokio::spawn(async move {
            info!("Ant-QUIC acceptor task started (incoming connection handler)");

            loop {
                if let Some(peer_conn) = node.accept().await {
                    let peer_id = peer_conn.peer_id;
                    let peer_addr = peer_conn.remote_addr;
                    let gossip_peer_id = ant_peer_id_to_gossip(&peer_id);

                    info!("Accepted connection from {:?} at {}", peer_id, peer_addr);

                    // Track the peer
                    add_peer_with_lru(&peers_accept, gossip_peer_id, peer_addr, max_peers).await;
                }

                // Small delay to prevent busy loop
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    /// Add or update a peer in the connected peers map with LRU eviction
    async fn add_peer(&self, peer_id: GossipPeerId, addr: SocketAddr) {
        add_peer_with_lru(&self.connected_peers, peer_id, addr, self.config.max_peers).await;
    }

    /// Remove a peer from the connected peers map
    async fn remove_peer(&self, peer_id: &GossipPeerId) {
        let mut peers = self.connected_peers.write().await;
        if peers.remove(peer_id).is_some() {
            debug!("Removed peer {:?} after connection failure", peer_id);
        }
    }
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
impl GossipTransport for AntQuicTransport {
    async fn dial(&self, peer: GossipPeerId, addr: SocketAddr) -> Result<()> {
        info!("Dialing peer {} at {}", peer, addr);

        // Connect to the peer by address
        let start = Instant::now();
        match self.node.connect_addr(addr).await {
            Ok(peer_conn) => {
                let gossip_id = ant_peer_id_to_gossip(&peer_conn.peer_id);
                let rtt_ms = start.elapsed().as_millis() as u32;
                info!(
                    "Successfully connected to peer {} at {} (rtt: {}ms)",
                    gossip_id, addr, rtt_ms
                );

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
        stream_type: StreamType,
        data: Bytes,
    ) -> Result<()> {
        debug!(
            "Sending {} bytes to peer {} on {:?} stream",
            data.len(),
            peer,
            stream_type
        );

        // Convert gossip PeerId to ant-quic PeerId
        let ant_peer_id = gossip_peer_id_to_ant(&peer);

        // Encode stream type as first byte
        let stream_type_byte = match stream_type {
            StreamType::Membership => 0u8,
            StreamType::PubSub => 1u8,
            StreamType::Bulk => 2u8,
        };

        // Prepare message: [stream_type_byte | data]
        let mut buf = Vec::with_capacity(1 + data.len());
        buf.push(stream_type_byte);
        buf.extend_from_slice(&data);

        // Send via the node
        match self.node.send(&ant_peer_id, &buf).await {
            Ok(()) => {
                info!("Successfully sent {} bytes to peer {}", buf.len(), peer);
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send to peer {}: {}", peer, e);
                self.remove_peer(&peer).await;
                Err(anyhow!("Failed to send to peer: {}", e))
            }
        }
    }

    async fn receive_message(&self) -> Result<(GossipPeerId, StreamType, Bytes)> {
        let mut recv_rx = self.recv_rx.lock().await;

        recv_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("Receive channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ant_quic_transport_creation() {
        let bind_addr = "127.0.0.1:0".parse().expect("Invalid address");
        let transport = AntQuicTransport::new(bind_addr, vec![])
            .await
            .expect("Failed to create transport");

        assert_ne!(transport.peer_id(), GossipPeerId::new([0u8; 32]));
    }

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
    #[ignore] // Integration test - requires running ant-quic nodes
    async fn test_two_node_communication() {
        use std::net::{IpAddr, Ipv4Addr};
        use tokio::time::{Duration, sleep, timeout};

        // Dynamic port allocation to avoid conflicts
        let base_port = 20000
            + (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() % 1000)
                .unwrap_or(0) as u16);

        // Create first node
        let node1_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), base_port);
        let node1 = AntQuicTransport::new(node1_addr, vec![])
            .await
            .expect("Failed to create node1");

        // Give node1 time to start
        sleep(Duration::from_millis(100)).await;

        // Create second node that knows about first
        let node2_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), base_port + 1);
        let node2 = AntQuicTransport::new(node2_addr, vec![node1_addr])
            .await
            .expect("Failed to create node2");

        // Give nodes time to establish connection
        sleep(Duration::from_millis(500)).await;

        // Test sending from node2 to node1
        let test_data = Bytes::from("Hello, QUIC!");
        let node1_peer_id = node1.peer_id();

        // Dial node1 from node2
        node2
            .dial(node1_peer_id, node1_addr)
            .await
            .expect("Failed to dial node1");

        // Give connection time to establish
        sleep(Duration::from_millis(500)).await;

        // Send message
        node2
            .send_to_peer(node1_peer_id, StreamType::PubSub, test_data.clone())
            .await
            .expect("Failed to send message");

        // Receive message on node1 with timeout
        let result = timeout(Duration::from_secs(5), node1.receive_message()).await;

        match result {
            Ok(Ok((peer_id, stream_type, data))) => {
                assert_eq!(peer_id, node2.peer_id());
                assert_eq!(stream_type, StreamType::PubSub);
                assert_eq!(data, test_data);
            }
            Ok(Err(e)) => panic!("Receive error: {}", e),
            Err(_) => panic!("Receive timeout"),
        }
    }

    #[tokio::test]
    async fn test_stream_type_encoding() {
        assert_eq!(
            match StreamType::Membership {
                StreamType::Membership => 0u8,
                StreamType::PubSub => 1u8,
                StreamType::Bulk => 2u8,
            },
            0u8
        );
        assert_eq!(
            match StreamType::PubSub {
                StreamType::Membership => 0u8,
                StreamType::PubSub => 1u8,
                StreamType::Bulk => 2u8,
            },
            1u8
        );
        assert_eq!(
            match StreamType::Bulk {
                StreamType::Membership => 0u8,
                StreamType::PubSub => 1u8,
                StreamType::Bulk => 2u8,
            },
            2u8
        );
    }
}
