#![warn(missing_docs)]

//! Membership management using HyParView + SWIM
//!
//! Provides:
//! - HyParView for partial views (active + passive)
//! - SWIM for failure detection
//! - Periodic shuffling and anti-entropy

use anyhow::{anyhow, Result};
use saorsa_gossip_transport::{GossipTransport, StreamType};
use saorsa_gossip_types::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time;
use tracing::{debug, trace, warn};

/// Default active view degree (8-12 peers)
pub const DEFAULT_ACTIVE_DEGREE: usize = 8;
/// Maximum active view degree
pub const MAX_ACTIVE_DEGREE: usize = 12;
/// Default passive view degree (64-128 peers)
pub const DEFAULT_PASSIVE_DEGREE: usize = 64;
/// Maximum passive view degree
pub const MAX_PASSIVE_DEGREE: usize = 128;
/// Shuffle period in seconds (per SPEC.md)
pub const SHUFFLE_PERIOD_SECS: u64 = 30;
/// SWIM probe interval (per SPEC.md)
pub const SWIM_PROBE_INTERVAL_SECS: u64 = 1;
/// SWIM suspect timeout (per SPEC.md)
pub const SWIM_SUSPECT_TIMEOUT_SECS: u64 = 3;

/// SWIM protocol messages
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SwimMessage {
    /// Ping message to probe peer
    Ping,
    /// Ack response to ping
    Ack,
}

/// Active random walk length for JOIN (per HyParView paper)
pub const ACTIVE_RANDOM_WALK_LENGTH: usize = 6;
/// Passive random walk length for SHUFFLE (per HyParView paper)
pub const PASSIVE_RANDOM_WALK_LENGTH: usize = 3;
/// Number of peers to include in shuffle from active view
pub const SHUFFLE_ACTIVE_SIZE: usize = 3;
/// Number of peers to include in shuffle from passive view
pub const SHUFFLE_PASSIVE_SIZE: usize = 4;

/// HyParView neighbor priority
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NeighborPriority {
    /// High priority - accepting node has empty active view
    High,
    /// Low priority - normal neighbor request
    Low,
}

/// HyParView protocol messages
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HyParViewMessage {
    /// Join request from new node
    Join {
        /// The peer ID of the joining node
        sender: PeerId,
        /// Time-to-live for random walk
        ttl: usize,
    },
    /// ForwardJoin - forwarded join request
    ForwardJoin {
        /// Original sender who forwarded
        sender: PeerId,
        /// The new peer trying to join
        new_peer: PeerId,
        /// Remaining TTL for random walk
        ttl: usize,
    },
    /// Neighbor request
    Neighbor {
        /// Sender's peer ID
        sender: PeerId,
        /// Priority of the request
        priority: NeighborPriority,
    },
    /// Response to Neighbor request
    NeighborReply {
        /// Whether the request was accepted
        accepted: bool,
    },
    /// Shuffle request with peer list
    Shuffle {
        /// Sender's peer ID
        sender: PeerId,
        /// List of peers to exchange
        peers: Vec<PeerId>,
        /// Remaining TTL for random walk
        ttl: usize,
    },
    /// Response to Shuffle request
    ShuffleReply {
        /// List of peers in response
        peers: Vec<PeerId>,
    },
    /// Disconnect notification
    Disconnect,
}

/// Membership management trait
#[async_trait::async_trait]
pub trait Membership: Send + Sync {
    /// Join the overlay network with seed peers
    async fn join(&self, seeds: Vec<String>) -> Result<()>;

    /// Get the active view (peers for routing)
    fn active_view(&self) -> Vec<PeerId>;

    /// Get the passive view (peers for healing)
    fn passive_view(&self) -> Vec<PeerId>;

    /// Add a peer to the active view
    async fn add_active(&self, peer: PeerId) -> Result<()>;

    /// Remove a peer from the active view
    async fn remove_active(&self, peer: PeerId) -> Result<()>;

    /// Promote a peer from passive to active view
    async fn promote(&self, peer: PeerId) -> Result<()>;
}

/// Peer state for SWIM failure detection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// Peer is alive and responding
    Alive,
    /// Peer is suspected of failure
    Suspect,
    /// Peer is confirmed dead
    Dead,
}

/// SWIM peer entry with timestamp
#[derive(Clone, Debug)]
struct SwimPeerEntry {
    state: PeerState,
    last_update: Instant,
}

/// SWIM failure detector
pub struct SwimDetector<T: GossipTransport + 'static> {
    /// Peer states with timestamps
    states: Arc<RwLock<HashMap<PeerId, SwimPeerEntry>>>,
    /// Probe period in seconds
    probe_period: u64,
    /// Suspect timeout in seconds
    suspect_timeout: u64,
    /// Transport layer for sending probes
    transport: Arc<T>,
}

impl<T: GossipTransport + 'static> SwimDetector<T> {
    /// Create a new SWIM detector
    pub fn new(probe_period: u64, suspect_timeout: u64, transport: Arc<T>) -> Self {
        let detector = Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            probe_period,
            suspect_timeout,
            transport,
        };

        // Start background probing task
        detector.spawn_probe_task();
        detector.spawn_suspect_timeout_task();

        detector
    }

    /// Mark a peer as alive
    pub async fn mark_alive(&self, peer: PeerId) {
        let mut states = self.states.write().await;
        states.insert(
            peer,
            SwimPeerEntry {
                state: PeerState::Alive,
                last_update: Instant::now(),
            },
        );
        trace!(peer_id = %peer, "SWIM: Marked peer as alive");
    }

    /// Mark a peer as suspect
    pub async fn mark_suspect(&self, peer: PeerId) {
        let mut states = self.states.write().await;
        if let Some(entry) = states.get_mut(&peer) {
            if entry.state == PeerState::Alive {
                entry.state = PeerState::Suspect;
                entry.last_update = Instant::now();
                debug!(peer_id = %peer, "SWIM: Marked peer as suspect");
            }
        }
    }

    /// Mark a peer as dead
    pub async fn mark_dead(&self, peer: PeerId) {
        let mut states = self.states.write().await;
        states.insert(
            peer,
            SwimPeerEntry {
                state: PeerState::Dead,
                last_update: Instant::now(),
            },
        );
        warn!(peer_id = %peer, "SWIM: Marked peer as dead");
    }

    /// Get the state of a peer
    pub async fn get_state(&self, peer: &PeerId) -> Option<PeerState> {
        let states = self.states.read().await;
        states.get(peer).map(|entry| entry.state)
    }

    /// Get all peers in a specific state
    pub async fn get_peers_in_state(&self, state: PeerState) -> Vec<PeerId> {
        let states = self.states.read().await;
        states
            .iter()
            .filter(|(_, entry)| entry.state == state)
            .map(|(peer, _)| *peer)
            .collect()
    }

    /// Remove a peer from tracking
    pub async fn remove_peer(&self, peer: &PeerId) {
        let mut states = self.states.write().await;
        states.remove(peer);
    }

    /// Get the probe period
    pub fn probe_period(&self) -> u64 {
        self.probe_period
    }

    /// Get the suspect timeout
    pub fn suspect_timeout(&self) -> u64 {
        self.suspect_timeout
    }

    /// Spawn background task to probe random peers
    fn spawn_probe_task(&self) {
        let states = self.states.clone();
        let probe_period = self.probe_period;
        let transport = self.transport.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(probe_period));

            loop {
                interval.tick().await;

                let states_guard = states.read().await;
                let alive_peers: Vec<PeerId> = states_guard
                    .iter()
                    .filter(|(_, entry)| entry.state == PeerState::Alive)
                    .map(|(peer, _)| *peer)
                    .collect();
                drop(states_guard);

                if let Some(&peer) = alive_peers.first() {
                    // Send PING to peer via transport
                    trace!(peer_id = %peer, "SWIM: Probing peer");
                    let ping_msg = SwimMessage::Ping;
                    if let Ok(bytes) = bincode::serialize(&ping_msg) {
                        let _ = transport
                            .send_to_peer(peer, StreamType::Membership, bytes.into())
                            .await;
                    }
                    // Note: Response handling would mark peer alive/suspect
                    // For now, we'll rely on manual state updates
                }
            }
        });
    }

    /// Spawn background task to check suspect timeouts
    fn spawn_suspect_timeout_task(&self) {
        let states = self.states.clone();
        let suspect_timeout = self.suspect_timeout;

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(1));

            loop {
                interval.tick().await;

                let mut states_guard = states.write().await;
                let now = Instant::now();

                // Find suspects that have timed out
                let mut to_mark_dead = Vec::new();
                for (peer, entry) in states_guard.iter() {
                    if entry.state == PeerState::Suspect {
                        let elapsed = now.duration_since(entry.last_update);
                        if elapsed > Duration::from_secs(suspect_timeout) {
                            to_mark_dead.push(*peer);
                        }
                    }
                }

                // Mark timed-out suspects as dead
                for peer in to_mark_dead {
                    states_guard.insert(
                        peer,
                        SwimPeerEntry {
                            state: PeerState::Dead,
                            last_update: now,
                        },
                    );
                    warn!(peer_id = %peer, "SWIM: Suspect timeout → marked dead");
                }
            }
        });
    }
}

/// HyParView membership implementation
pub struct HyParViewMembership<T: GossipTransport + 'static> {
    /// Local peer ID
    local_peer_id: PeerId,
    /// Active view (for routing)
    active: Arc<RwLock<HashSet<PeerId>>>,
    /// Passive view (for healing)
    passive: Arc<RwLock<HashSet<PeerId>>>,
    /// Peer addresses for connection
    peer_addrs: Arc<RwLock<HashMap<PeerId, std::net::SocketAddr>>>,
    /// SWIM failure detector
    swim: SwimDetector<T>,
    /// Active view degree
    active_degree: usize,
    /// Passive view degree
    passive_degree: usize,
    /// Transport layer for sending messages
    transport: Arc<T>,
}

impl<T: GossipTransport + 'static> HyParViewMembership<T> {
    /// Create a new HyParView membership manager
    pub fn new(
        local_peer_id: PeerId,
        active_degree: usize,
        passive_degree: usize,
        transport: Arc<T>,
    ) -> Self {
        let membership = Self {
            local_peer_id,
            active: Arc::new(RwLock::new(HashSet::new())),
            passive: Arc::new(RwLock::new(HashSet::new())),
            peer_addrs: Arc::new(RwLock::new(HashMap::new())),
            swim: SwimDetector::new(
                SWIM_PROBE_INTERVAL_SECS,
                SWIM_SUSPECT_TIMEOUT_SECS,
                transport.clone(),
            ),
            active_degree,
            passive_degree,
            transport,
        };

        // Start background shuffle task
        membership.spawn_shuffle_task();
        membership.spawn_degree_maintenance_task();

        membership
    }

    /// Get local peer ID
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Store peer address
    pub async fn store_peer_addr(&self, peer: PeerId, addr: std::net::SocketAddr) {
        let mut addrs = self.peer_addrs.write().await;
        addrs.insert(peer, addr);
    }

    /// Get peer address
    pub async fn get_peer_addr(&self, peer: &PeerId) -> Option<std::net::SocketAddr> {
        let addrs = self.peer_addrs.read().await;
        addrs.get(peer).copied()
    }

    /// Add peer to passive view (without duplicating in active)
    pub async fn add_to_passive(&self, peer: PeerId) {
        if peer == self.local_peer_id {
            return;
        }

        let active = self.active.read().await;
        if active.contains(&peer) {
            return;
        }
        drop(active);

        let mut passive = self.passive.write().await;
        if passive.len() < MAX_PASSIVE_DEGREE {
            passive.insert(peer);
            trace!(peer_id = %peer, "Added to passive view");
        }
    }

    /// Select random peer from active view (excluding specified peer)
    pub async fn random_active_peer_except(&self, exclude: PeerId) -> Option<PeerId> {
        let active = self.active.read().await;
        active.iter().find(|&&p| p != exclude).copied()
    }

    /// Sample random peers from active view
    pub async fn sample_active(&self, count: usize) -> Vec<PeerId> {
        let active = self.active.read().await;
        active.iter().take(count).copied().collect()
    }

    /// Sample random peers from passive view
    pub async fn sample_passive(&self, count: usize) -> Vec<PeerId> {
        let passive = self.passive.read().await;
        passive.iter().take(count).copied().collect()
    }

    /// Send a HyParView message to a peer
    async fn send_hyparview_message(&self, peer: PeerId, msg: &HyParViewMessage) -> Result<()> {
        let bytes = bincode::serialize(msg)
            .map_err(|e| anyhow!("Failed to serialize HyParView message: {}", e))?;
        self.transport
            .send_to_peer(peer, StreamType::Membership, bytes.into())
            .await
    }

    /// Get the SWIM detector
    pub fn swim(&self) -> &SwimDetector<T> {
        &self.swim
    }

    /// Shuffle the passive view with a random peer (full HyParView protocol)
    pub async fn shuffle(&self) -> Result<()> {
        let active = self.active.read().await;
        if active.is_empty() {
            return Ok(());
        }

        // Select random active peer for shuffle target
        let target = *active
            .iter()
            .next()
            .ok_or_else(|| anyhow!("No active peers"))?;
        drop(active);

        // Build shuffle list: self + random sample from active + passive
        let mut shuffle_list = vec![self.local_peer_id];
        shuffle_list.extend(self.sample_active(SHUFFLE_ACTIVE_SIZE).await);
        shuffle_list.extend(self.sample_passive(SHUFFLE_PASSIVE_SIZE).await);

        debug!(
            target = %target,
            shuffle_count = shuffle_list.len(),
            "HyParView: Initiating shuffle"
        );

        // Send SHUFFLE message with random walk TTL
        let shuffle_msg = HyParViewMessage::Shuffle {
            sender: self.local_peer_id,
            peers: shuffle_list,
            ttl: PASSIVE_RANDOM_WALK_LENGTH,
        };
        self.send_hyparview_message(target, &shuffle_msg).await
    }

    /// Handle incoming SHUFFLE message
    pub async fn handle_shuffle(
        &self,
        sender: PeerId,
        peers: Vec<PeerId>,
        ttl: usize,
    ) -> Result<()> {
        let active = self.active.read().await;

        // If TTL > 0 and we have active peers besides sender, forward
        if ttl > 0 && active.len() > 1 {
            if let Some(next) = active.iter().find(|&&p| p != sender).copied() {
                drop(active);
                let forward_msg = HyParViewMessage::Shuffle {
                    sender,
                    peers,
                    ttl: ttl - 1,
                };
                return self.send_hyparview_message(next, &forward_msg).await;
            }
        }
        drop(active);

        // Terminal node: exchange views
        let reply_peers = self.sample_passive(peers.len()).await;

        // Send shuffle reply
        let reply_msg = HyParViewMessage::ShuffleReply { peers: reply_peers };
        self.send_hyparview_message(sender, &reply_msg).await?;

        // Integrate received peers into passive view
        for peer in peers {
            if peer != self.local_peer_id {
                self.add_to_passive(peer).await;
            }
        }

        Ok(())
    }

    /// Handle incoming SHUFFLE_REPLY message
    pub async fn handle_shuffle_reply(&self, peers: Vec<PeerId>) {
        for peer in peers {
            if peer != self.local_peer_id {
                self.add_to_passive(peer).await;
            }
        }
    }

    /// Handle incoming JOIN message (as a contact node)
    pub async fn handle_join(&self, new_peer: PeerId, ttl: usize) -> Result<()> {
        debug!(
            new_peer = %new_peer,
            ttl = ttl,
            "HyParView: Received JOIN request"
        );

        // Add new peer to active view
        self.add_active(new_peer).await?;

        // Forward JOIN to all active peers (except the new peer) with decremented TTL
        if ttl > 0 {
            let active = self.active.read().await;
            let forward_targets: Vec<PeerId> =
                active.iter().filter(|&&p| p != new_peer).copied().collect();
            drop(active);

            for target in forward_targets {
                let forward_msg = HyParViewMessage::ForwardJoin {
                    sender: self.local_peer_id,
                    new_peer,
                    ttl: ttl - 1,
                };
                // Best effort - don't fail the whole join if one forward fails
                let _ = self.send_hyparview_message(target, &forward_msg).await;
            }
        }

        Ok(())
    }

    /// Handle incoming FORWARDJOIN message
    pub async fn handle_forward_join(
        &self,
        _sender: PeerId,
        new_peer: PeerId,
        ttl: usize,
    ) -> Result<()> {
        let active = self.active.read().await;
        let active_count = active.len();
        drop(active);

        // If TTL is 0 or active view has room, accept the new peer
        if ttl == 0 || active_count < DEFAULT_ACTIVE_DEGREE {
            debug!(
                new_peer = %new_peer,
                reason = if ttl == 0 { "TTL expired" } else { "active view has room" },
                "HyParView: Accepting FORWARDJOIN"
            );

            // Add to active and send NEIGHBOR request
            self.add_active(new_peer).await?;

            let neighbor_msg = HyParViewMessage::Neighbor {
                sender: self.local_peer_id,
                priority: if active_count == 0 {
                    NeighborPriority::High
                } else {
                    NeighborPriority::Low
                },
            };
            self.send_hyparview_message(new_peer, &neighbor_msg).await?;
        } else {
            // Forward to random active peer with decremented TTL
            if let Some(next) = self.random_active_peer_except(new_peer).await {
                let forward_msg = HyParViewMessage::ForwardJoin {
                    sender: self.local_peer_id,
                    new_peer,
                    ttl: ttl - 1,
                };
                self.send_hyparview_message(next, &forward_msg).await?;
            }
        }

        // At TTL == PASSIVE_RANDOM_WALK_LENGTH, add to passive view
        if ttl == PASSIVE_RANDOM_WALK_LENGTH {
            self.add_to_passive(new_peer).await;
        }

        Ok(())
    }

    /// Handle incoming NEIGHBOR request
    pub async fn handle_neighbor(&self, sender: PeerId, priority: NeighborPriority) -> Result<()> {
        let active = self.active.read().await;
        let active_count = active.len();
        drop(active);

        // Accept if high priority, or if we have room
        let accepted = priority == NeighborPriority::High || active_count < MAX_ACTIVE_DEGREE;

        if accepted {
            self.add_active(sender).await?;
            debug!(peer = %sender, "HyParView: Accepted NEIGHBOR request");
        } else {
            debug!(peer = %sender, "HyParView: Rejected NEIGHBOR request (at capacity)");
        }

        let reply_msg = HyParViewMessage::NeighborReply { accepted };
        self.send_hyparview_message(sender, &reply_msg).await
    }

    /// Handle incoming NEIGHBOR_REPLY message
    pub async fn handle_neighbor_reply(&self, sender: PeerId, accepted: bool) -> Result<()> {
        if accepted {
            debug!(peer = %sender, "HyParView: NEIGHBOR accepted");
            self.swim.mark_alive(sender).await;
        } else {
            debug!(peer = %sender, "HyParView: NEIGHBOR rejected");
            // Move from active to passive
            {
                let mut active = self.active.write().await;
                if active.remove(&sender) {
                    drop(active);
                    self.add_to_passive(sender).await;
                }
            }
        }
        Ok(())
    }

    /// Handle incoming DISCONNECT message
    pub async fn handle_disconnect(&self, sender: PeerId) -> Result<()> {
        debug!(peer = %sender, "HyParView: Received DISCONNECT");
        self.remove_active(sender).await?;

        // Try to promote from passive to maintain active view size
        let passive = self.passive.read().await;
        if let Some(&candidate) = passive.iter().next() {
            drop(passive);
            self.promote(candidate).await?;
        }

        Ok(())
    }

    /// Maintain active and passive view degrees
    #[cfg(test)]
    async fn maintain_degrees(&self) {
        let mut active = self.active.write().await;
        let mut passive = self.passive.write().await;

        // Enforce active degree limits (8-12)
        if active.len() < DEFAULT_ACTIVE_DEGREE && !passive.is_empty() {
            // Promote from passive
            let to_promote = DEFAULT_ACTIVE_DEGREE - active.len();
            let peers: Vec<PeerId> = passive.iter().take(to_promote).copied().collect();

            for peer in peers {
                passive.remove(&peer);
                active.insert(peer);
                debug!(peer_id = %peer, "Promoted from passive to active");
            }
        } else if active.len() > MAX_ACTIVE_DEGREE {
            // Demote to passive
            let to_demote = active.len() - MAX_ACTIVE_DEGREE;
            let peers: Vec<PeerId> = active.iter().take(to_demote).copied().collect();

            for peer in peers {
                active.remove(&peer);
                if passive.len() < MAX_PASSIVE_DEGREE {
                    passive.insert(peer);
                    debug!(peer_id = %peer, "Demoted from active to passive");
                }
            }
        }

        // Enforce passive degree limit (max 128)
        if passive.len() > MAX_PASSIVE_DEGREE {
            let to_remove = passive.len() - MAX_PASSIVE_DEGREE;
            let peers: Vec<PeerId> = passive.iter().take(to_remove).copied().collect();

            for peer in peers {
                passive.remove(&peer);
                trace!(peer_id = %peer, "Removed from passive view (over capacity)");
            }
        }
    }

    /// Spawn background task for periodic shuffling
    fn spawn_shuffle_task(&self) {
        let active = self.active.clone();
        let passive = self.passive.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(SHUFFLE_PERIOD_SECS));

            loop {
                interval.tick().await;

                let active_guard = active.read().await;
                let passive_guard = passive.read().await;

                if !active_guard.is_empty() {
                    debug!(
                        active_count = active_guard.len(),
                        passive_count = passive_guard.len(),
                        "HyParView: Periodic shuffle tick"
                    );
                }

                // TODO: Actual shuffle implementation requires transport
                drop(active_guard);
                drop(passive_guard);
            }
        });
    }

    /// Spawn background task for degree maintenance
    fn spawn_degree_maintenance_task(&self) {
        let active = self.active.clone();
        let passive = self.passive.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(10));

            loop {
                interval.tick().await;

                let mut active_guard = active.write().await;
                let mut passive_guard = passive.write().await;

                let active_count = active_guard.len();
                let passive_count = passive_guard.len();

                // Promote from passive if active is low
                if active_count < DEFAULT_ACTIVE_DEGREE && !passive_guard.is_empty() {
                    let to_promote = DEFAULT_ACTIVE_DEGREE - active_count;
                    let peers: Vec<PeerId> =
                        passive_guard.iter().take(to_promote).copied().collect();

                    for peer in peers {
                        passive_guard.remove(&peer);
                        active_guard.insert(peer);
                        debug!(peer_id = %peer, "Degree maintenance: promoted to active");
                    }
                }

                // Demote to passive if active is high
                if active_count > MAX_ACTIVE_DEGREE {
                    let to_demote = active_count - MAX_ACTIVE_DEGREE;
                    let peers: Vec<PeerId> = active_guard.iter().take(to_demote).copied().collect();

                    for peer in peers {
                        active_guard.remove(&peer);
                        if passive_guard.len() < MAX_PASSIVE_DEGREE {
                            passive_guard.insert(peer);
                            debug!(peer_id = %peer, "Degree maintenance: demoted to passive");
                        }
                    }
                }

                // Trim passive if over capacity
                if passive_count > MAX_PASSIVE_DEGREE {
                    let to_remove = passive_count - MAX_PASSIVE_DEGREE;
                    let peers: Vec<PeerId> =
                        passive_guard.iter().take(to_remove).copied().collect();

                    for peer in peers {
                        passive_guard.remove(&peer);
                        trace!(peer_id = %peer, "Degree maintenance: removed from passive");
                    }
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl<T: GossipTransport + 'static> Membership for HyParViewMembership<T> {
    async fn join(&self, seeds: Vec<String>) -> Result<()> {
        use std::net::SocketAddr;

        if seeds.is_empty() {
            debug!("JOIN: No seeds provided, operating as bootstrap node");
            return Ok(());
        }

        // Exponential backoff parameters
        const INITIAL_DELAY_MS: u64 = 100;
        const MAX_DELAY_MS: u64 = 30_000;
        const MAX_RETRIES: usize = 10;

        for seed in seeds {
            // Parse the seed address
            let addr: SocketAddr = match seed.parse() {
                Ok(a) => a,
                Err(e) => {
                    warn!(seed = %seed, error = %e, "JOIN: Invalid seed address");
                    continue;
                }
            };

            let mut delay_ms = INITIAL_DELAY_MS;
            let mut connected = false;

            for attempt in 0..MAX_RETRIES {
                debug!(
                    seed = %seed,
                    attempt = attempt + 1,
                    "JOIN: Connecting to bootstrap node"
                );

                // Connect via transport and get peer ID
                match self.transport.dial_bootstrap(addr).await {
                    Ok(seed_peer_id) => {
                        debug!(
                            seed = %seed,
                            peer_id = %seed_peer_id,
                            "JOIN: Connected to bootstrap"
                        );

                        // Store peer address
                        self.store_peer_addr(seed_peer_id, addr).await;

                        // Send JOIN message
                        let join_msg = HyParViewMessage::Join {
                            sender: self.local_peer_id,
                            ttl: ACTIVE_RANDOM_WALK_LENGTH,
                        };

                        if let Err(e) = self.send_hyparview_message(seed_peer_id, &join_msg).await {
                            warn!(error = %e, "JOIN: Failed to send JOIN message");
                            continue;
                        }

                        // Add seed to active view immediately (optimistic)
                        self.add_active(seed_peer_id).await?;
                        connected = true;
                        break;
                    }
                    Err(e) => {
                        warn!(
                            seed = %seed,
                            attempt = attempt + 1,
                            error = %e,
                            delay_ms = delay_ms,
                            "JOIN: Connection failed, will retry"
                        );

                        // Exponential backoff with jitter
                        let jitter =
                            (rand::random::<u64>() % (delay_ms / 4)).saturating_sub(delay_ms / 8);
                        tokio::time::sleep(Duration::from_millis(delay_ms + jitter)).await;
                        delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
                    }
                }
            }

            if connected {
                // Successfully joined via this seed
                debug!(seed = %seed, "JOIN: Successfully joined network");
                return Ok(());
            }
        }

        Err(anyhow!("JOIN: Failed to connect to any seed nodes"))
    }

    fn active_view(&self) -> Vec<PeerId> {
        // Try to get read lock, return empty vec if unavailable
        match self.active.try_read() {
            Ok(active) => active.iter().copied().collect(),
            Err(_) => Vec::new(),
        }
    }

    fn passive_view(&self) -> Vec<PeerId> {
        // Try to get read lock, return empty vec if unavailable
        match self.passive.try_read() {
            Ok(passive) => passive.iter().copied().collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn add_active(&self, peer: PeerId) -> Result<()> {
        let mut active = self.active.write().await;

        // If active view is full, demote one peer to passive
        if active.len() >= self.active_degree {
            if let Some(&to_demote) = active.iter().next() {
                active.remove(&to_demote);
                // Move to passive view
                let mut passive = self.passive.write().await;
                if passive.len() < self.passive_degree {
                    passive.insert(to_demote);
                    debug!(peer_id = %to_demote, "Demoted to passive (active view full)");
                }
            }
        }

        active.insert(peer);
        drop(active); // Release lock before async call

        self.swim.mark_alive(peer).await;
        debug!(peer_id = %peer, "Added to active view");

        Ok(())
    }

    async fn remove_active(&self, peer: PeerId) -> Result<()> {
        let mut active = self.active.write().await;
        let removed = active.remove(&peer);
        drop(active);

        if removed {
            self.swim.mark_dead(peer).await;
            debug!(peer_id = %peer, "Removed from active view");
        }

        Ok(())
    }

    async fn promote(&self, peer: PeerId) -> Result<()> {
        let mut passive = self.passive.write().await;
        let was_passive = passive.remove(&peer);
        drop(passive); // Release lock before calling add_active

        if was_passive {
            self.add_active(peer).await?;
            debug!(peer_id = %peer, "Promoted from passive to active");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saorsa_gossip_transport::{QuicTransport, TransportConfig};

    fn test_transport() -> Arc<QuicTransport> {
        Arc::new(QuicTransport::new(TransportConfig::default()))
    }

    fn test_peer_id() -> PeerId {
        PeerId::new([0u8; 32])
    }

    fn test_membership() -> HyParViewMembership<QuicTransport> {
        HyParViewMembership::new(
            test_peer_id(),
            DEFAULT_ACTIVE_DEGREE,
            DEFAULT_PASSIVE_DEGREE,
            test_transport(),
        )
    }

    #[tokio::test]
    async fn test_hyparview_creation() {
        let membership = test_membership();
        assert_eq!(membership.active_view().len(), 0);
        assert_eq!(membership.passive_view().len(), 0);
    }

    #[tokio::test]
    async fn test_add_active_peer() {
        let membership = test_membership();
        let peer = PeerId::new([1u8; 32]);

        membership.add_active(peer).await.ok();
        let active = membership.active_view();
        assert_eq!(active.len(), 1);
        assert!(active.contains(&peer));
    }

    #[tokio::test]
    async fn test_remove_active_peer() {
        let membership = test_membership();
        let peer = PeerId::new([1u8; 32]);

        membership.add_active(peer).await.ok();
        membership.remove_active(peer).await.ok();

        let active = membership.active_view();
        assert_eq!(active.len(), 0);
    }

    #[tokio::test]
    async fn test_active_view_capacity() {
        let transport = test_transport();
        let membership = HyParViewMembership::new(test_peer_id(), 3, 10, transport);

        // Add 5 peers (more than capacity)
        for i in 0..5 {
            let peer = PeerId::new([i; 32]);
            membership.add_active(peer).await.ok();
        }

        // Should only have 3 in active (capacity limit)
        let active = membership.active_view();
        assert_eq!(active.len(), 3);

        // Others should be in passive
        let passive = membership.passive_view();
        assert_eq!(passive.len(), 2);
    }

    #[tokio::test]
    async fn test_swim_states() {
        let transport = test_transport();
        let swim = SwimDetector::new(1, 3, transport);
        let peer = PeerId::new([1u8; 32]);

        swim.mark_alive(peer).await;
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));

        swim.mark_suspect(peer).await;
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Suspect));

        swim.mark_dead(peer).await;
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Dead));
    }

    #[tokio::test]
    async fn test_swim_suspect_timeout() {
        let transport = test_transport();
        let swim = SwimDetector::new(1, 1, transport); // 1s timeout
        let peer = PeerId::new([1u8; 32]);

        swim.mark_alive(peer).await;
        swim.mark_suspect(peer).await;

        // Wait for timeout
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Should be marked dead automatically
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Dead));
    }

    #[tokio::test]
    async fn test_promote_from_passive() {
        let membership = test_membership();
        let peer = PeerId::new([1u8; 32]);

        // Add to passive
        {
            let mut passive = membership.passive.write().await;
            passive.insert(peer);
        }

        // Promote to active
        membership.promote(peer).await.ok();

        let active = membership.active_view();
        let passive = membership.passive_view();

        assert!(active.contains(&peer));
        assert!(!passive.contains(&peer));
    }

    #[tokio::test]
    async fn test_degree_maintenance() {
        let transport = test_transport();
        let membership = HyParViewMembership::new(test_peer_id(), 5, 20, transport);

        // Add many peers to passive
        for i in 0..15 {
            let peer = PeerId::new([i; 32]);
            let mut passive = membership.passive.write().await;
            passive.insert(peer);
        }

        // Run maintenance
        membership.maintain_degrees().await;

        // Should have promoted some to active
        let active = membership.active_view();
        assert!(active.len() >= 5);
        assert!(active.len() <= 12);
    }

    #[tokio::test]
    async fn test_get_peers_in_state() {
        let transport = test_transport();
        let swim = SwimDetector::new(1, 100, transport); // Long timeout so background task doesn't interfere

        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);
        let peer3 = PeerId::new([3u8; 32]);

        swim.mark_alive(peer1).await;
        swim.mark_alive(peer2).await; // Start as alive
        swim.mark_suspect(peer2).await; // Then mark suspect
        swim.mark_dead(peer3).await;

        let alive = swim.get_peers_in_state(PeerState::Alive).await;
        let suspects = swim.get_peers_in_state(PeerState::Suspect).await;
        let dead = swim.get_peers_in_state(PeerState::Dead).await;

        assert_eq!(alive.len(), 1);
        assert_eq!(suspects.len(), 1);
        assert_eq!(dead.len(), 1);

        assert!(alive.contains(&peer1));
        assert!(suspects.contains(&peer2));
        assert!(dead.contains(&peer3));
    }
}
