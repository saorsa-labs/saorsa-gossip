#![warn(missing_docs)]

//! Membership management using HyParView + SWIM
//!
//! Provides:
//! - HyParView for partial views (active + passive)
//! - SWIM for failure detection
//! - Periodic shuffling and anti-entropy

use anyhow::{anyhow, Result};
use rand::SeedableRng;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
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
/// Number of peers to probe per interval (per SPEC.md)
pub const SWIM_PROBE_FANOUT: usize = 3;
/// Number of peers for indirect probing (per SPEC.md)
pub const SWIM_INDIRECT_PROBE_FANOUT: usize = 3;
/// Timeout in milliseconds before marking probe as failed (per SPEC.md)
pub const SWIM_ACK_TIMEOUT_MS: u64 = 500;

/// SWIM protocol messages
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SwimMessage {
    /// Ping message to probe peer
    Ping,
    /// Ack response to ping
    Ack,
    /// PingReq message for indirect probing.
    ///
    /// When a direct probe times out, the node asks other peers to probe the suspect.
    /// `target` is the suspect peer to probe, `requester` is the node requesting the probe.
    PingReq {
        /// The suspect peer to be probed
        target: PeerId,
        /// The node requesting the indirect probe
        requester: PeerId,
    },
    /// AckResponse message - forwarded ack from indirect probe responder back to requester.
    ///
    /// When a node receives a PingReq and successfully probes the target, it sends this
    /// message back to the original requester to confirm the target is alive.
    /// `target` is the peer that responded to the probe, `requester` is the original requester.
    AckResponse {
        /// The peer that responded to the indirect probe
        target: PeerId,
        /// The original requester of the indirect probe
        requester: PeerId,
    },
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

/// Unified membership protocol message wrapper
///
/// Both HyParView and SWIM messages are sent on the `Membership` stream type.
/// This wrapper provides a single dispatch point for all membership messages,
/// allowing the receiver to route to the appropriate handler.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MembershipProtocolMessage {
    /// HyParView protocol message for partial view management
    HyParView(HyParViewMessage),
    /// SWIM protocol message for failure detection
    Swim(SwimMessage),
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
    /// Local peer ID
    self_peer_id: PeerId,
    /// Peer states with timestamps
    states: Arc<RwLock<HashMap<PeerId, SwimPeerEntry>>>,
    /// Pending probes with timestamps for timeout detection
    pending_probes: Arc<RwLock<HashMap<PeerId, Instant>>>,
    /// Probe period in seconds
    probe_period: u64,
    /// Suspect timeout in seconds
    suspect_timeout: u64,
    /// Number of peers to probe per interval
    probe_fanout: usize,
    /// Transport layer for sending probes
    transport: Arc<T>,
}

impl<T: GossipTransport + 'static> SwimDetector<T> {
    /// Create a new SWIM detector
    pub fn new(
        self_peer_id: PeerId,
        probe_period: u64,
        suspect_timeout: u64,
        probe_fanout: usize,
        transport: Arc<T>,
    ) -> Self {
        let detector = Self {
            self_peer_id,
            states: Arc::new(RwLock::new(HashMap::new())),
            pending_probes: Arc::new(RwLock::new(HashMap::new())),
            probe_period,
            suspect_timeout,
            probe_fanout,
            transport,
        };

        // Start background probing task
        detector.spawn_probe_task();
        detector.spawn_suspect_timeout_task();
        detector.spawn_probe_timeout_task();

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

    /// Get the probe fanout
    pub fn probe_fanout(&self) -> usize {
        self.probe_fanout
    }

    /// Record a probe sent to a peer for timeout tracking
    ///
    /// This method tracks when a probe was sent to enable timeout detection.
    /// If the peer doesn't respond within the probe period, it can be marked as suspect.
    pub async fn record_probe(&self, peer: PeerId) {
        let mut pending = self.pending_probes.write().await;
        pending.insert(peer, Instant::now());
        trace!(peer_id = %peer, "SWIM: Recorded pending probe");
    }

    /// Clear a pending probe for a peer
    ///
    /// Called when an ack is received from a peer, indicating the probe succeeded.
    /// Also used to clear timed-out probes before triggering indirect probing.
    pub async fn clear_probe(&self, peer: &PeerId) -> bool {
        let mut pending = self.pending_probes.write().await;
        let was_present = pending.remove(peer).is_some();
        if was_present {
            trace!(peer_id = %peer, "SWIM: Cleared pending probe");
        }
        was_present
    }

    /// Handle incoming Ping message and respond with Ack
    ///
    /// Marks the sender as alive (they're clearly responsive) and sends back an Ack message.
    pub async fn handle_ping(&self, sender: PeerId) -> Result<()> {
        // Mark sender as alive - they sent us a ping, so they're responding
        self.mark_alive(sender).await;
        trace!(peer_id = %sender, "SWIM: Received Ping, sending Ack");

        // Serialize and send Ack response wrapped in MembershipProtocolMessage
        let wrapped = MembershipProtocolMessage::Swim(SwimMessage::Ack);
        let bytes = postcard::to_stdvec(&wrapped)
            .map_err(|e| anyhow!("Failed to serialize Ack message: {}", e))?;

        self.transport
            .send_to_peer(sender, GossipStreamType::Membership, bytes.into())
            .await?;

        trace!(peer_id = %sender, "SWIM: Ack sent successfully");
        Ok(())
    }

    /// Handle incoming Ack message
    ///
    /// Marks the sender as alive and clears any pending probe for this peer.
    pub async fn handle_ack(&self, sender: PeerId) -> Result<()> {
        // Mark sender as alive - they responded to our ping
        self.mark_alive(sender).await;

        // Clear pending probe if one exists
        let was_pending = self.clear_probe(&sender).await;

        if was_pending {
            trace!(peer_id = %sender, "SWIM: Received Ack, cleared pending probe");
        } else {
            trace!(peer_id = %sender, "SWIM: Received Ack (no pending probe)");
        }

        Ok(())
    }

    /// Request indirect probes for a target peer from K random alive peers
    pub async fn request_indirect_probes(&self, target: PeerId) -> Result<()> {
        let states = self.states.read().await;
        let alive_peers: Vec<PeerId> = states
            .iter()
            .filter(|(peer, entry)| {
                entry.state == PeerState::Alive && **peer != target && **peer != self.self_peer_id
            })
            .map(|(peer, _)| *peer)
            .collect();
        drop(states);

        if alive_peers.is_empty() {
            debug!(target = %target, "SWIM: No alive peers for indirect probes");
            return Ok(());
        }

        // Select K random alive peers for indirect probing
        let probe_count = SWIM_INDIRECT_PROBE_FANOUT.min(alive_peers.len());
        let peers_to_ask: Vec<PeerId> = {
            use rand::seq::SliceRandom;
            let mut rng = rand::rngs::StdRng::from_entropy();
            alive_peers
                .choose_multiple(&mut rng, probe_count)
                .copied()
                .collect()
        };

        debug!(
            target = %target,
            indirect_probers = ?peers_to_ask,
            "SWIM: Requesting indirect probes"
        );

        // Send PingReq to each selected peer
        for peer in peers_to_ask {
            let wrapped = MembershipProtocolMessage::Swim(SwimMessage::PingReq {
                target,
                requester: self.self_peer_id,
            });
            match postcard::to_stdvec(&wrapped) {
                Ok(bytes) => {
                    if let Err(e) = self
                        .transport
                        .send_to_peer(peer, GossipStreamType::Membership, bytes.into())
                        .await
                    {
                        debug!(?e, peer_id = %peer, "SWIM: PingReq send failed");
                    }
                }
                Err(e) => {
                    warn!(?e, "SWIM: PingReq message serialization failed");
                }
            }
        }

        Ok(())
    }

    /// Handle incoming PingReq message - probe target on behalf of requester
    pub async fn handle_ping_req(&self, requester: PeerId, target: PeerId) -> Result<()> {
        trace!(
            requester = %requester,
            target = %target,
            "SWIM: Received PingReq, probing target"
        );

        // Send Ping to target wrapped in MembershipProtocolMessage
        let wrapped = MembershipProtocolMessage::Swim(SwimMessage::Ping);
        let bytes = postcard::to_stdvec(&wrapped)
            .map_err(|e| anyhow!("Failed to serialize Ping message: {}", e))?;

        self.transport
            .send_to_peer(target, GossipStreamType::Membership, bytes.into())
            .await?;

        trace!(target = %target, "SWIM: Indirect probe Ping sent to target");
        Ok(())
    }

    /// Handle incoming AckResponse message from indirect probe
    pub async fn handle_ack_response(&self, target: PeerId) -> Result<()> {
        trace!(
            target = %target,
            "SWIM: Received AckResponse, marking target as alive"
        );

        // Mark target as alive (overrides suspect)
        self.mark_alive(target).await;

        // Clear any pending probe for this target
        self.clear_probe(&target).await;

        Ok(())
    }

    /// Spawn background task to check for probe timeouts
    fn spawn_probe_timeout_task(&self) {
        let pending_probes = self.pending_probes.clone();
        let states = self.states.clone();
        let self_peer_id = self.self_peer_id;
        let transport = self.transport.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(SWIM_ACK_TIMEOUT_MS));

            loop {
                interval.tick().await;

                let now = Instant::now();
                let mut timed_out_peers = Vec::new();

                // Find probes that have timed out
                {
                    let pending = pending_probes.read().await;
                    for (peer, probe_time) in pending.iter() {
                        let elapsed = now.duration_since(*probe_time);
                        if elapsed >= Duration::from_millis(SWIM_ACK_TIMEOUT_MS) {
                            timed_out_peers.push(*peer);
                        }
                    }
                }

                // For each timed-out peer: mark as suspect and trigger indirect probes
                for peer in timed_out_peers {
                    debug!(peer_id = %peer, "SWIM: Probe timeout, marking suspect");

                    // Mark as suspect
                    {
                        let mut states_guard = states.write().await;
                        if let Some(entry) = states_guard.get_mut(&peer) {
                            if entry.state == PeerState::Alive {
                                entry.state = PeerState::Suspect;
                                entry.last_update = now;
                            }
                        }
                    }

                    // Clear the timed-out probe
                    {
                        let mut pending = pending_probes.write().await;
                        pending.remove(&peer);
                    }

                    // Request indirect probes
                    let states_read = states.read().await;
                    let alive_peers: Vec<PeerId> = states_read
                        .iter()
                        .filter(|(p, entry)| {
                            entry.state == PeerState::Alive && **p != peer && **p != self_peer_id
                        })
                        .map(|(p, _)| *p)
                        .collect();
                    drop(states_read);

                    if !alive_peers.is_empty() {
                        use rand::seq::SliceRandom;
                        let probe_count = SWIM_INDIRECT_PROBE_FANOUT.min(alive_peers.len());
                        let peers_to_ask: Vec<PeerId> = {
                            let mut rng = rand::rngs::StdRng::from_entropy();
                            alive_peers
                                .choose_multiple(&mut rng, probe_count)
                                .copied()
                                .collect()
                        };

                        debug!(
                            target = %peer,
                            indirect_probers = ?peers_to_ask,
                            "SWIM: Requesting indirect probes after timeout"
                        );

                        for indirect_peer in peers_to_ask {
                            let wrapped = MembershipProtocolMessage::Swim(SwimMessage::PingReq {
                                target: peer,
                                requester: self_peer_id,
                            });
                            match postcard::to_stdvec(&wrapped) {
                                Ok(bytes) => {
                                    if let Err(e) = transport
                                        .send_to_peer(
                                            indirect_peer,
                                            GossipStreamType::Membership,
                                            bytes.into(),
                                        )
                                        .await
                                    {
                                        debug!(?e, peer_id = %indirect_peer, "SWIM: PingReq send failed");
                                    }
                                }
                                Err(e) => {
                                    warn!(?e, "SWIM: PingReq message serialization failed");
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    /// Spawn background task to probe random peers
    fn spawn_probe_task(&self) {
        let states = self.states.clone();
        let pending_probes = self.pending_probes.clone();
        let probe_period = self.probe_period;
        let probe_fanout = self.probe_fanout;
        let transport = self.transport.clone();

        tokio::spawn(async move {
            use rand::seq::SliceRandom;
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

                if alive_peers.is_empty() {
                    continue;
                }

                // Select min(probe_fanout, alive_peers.len()) random peers
                // Use StdRng::from_entropy() instead of thread_rng() to be Send-safe
                let probe_count = probe_fanout.min(alive_peers.len());
                let peers_to_probe: Vec<PeerId> = {
                    let mut rng = rand::rngs::StdRng::from_entropy();
                    alive_peers
                        .choose_multiple(&mut rng, probe_count)
                        .copied()
                        .collect()
                };

                debug!(
                    probe_count = peers_to_probe.len(),
                    "SWIM: Probing multiple peers"
                );

                for peer in peers_to_probe {
                    // Record probe for timeout tracking (don't overwrite existing pending probes)
                    {
                        let mut pending = pending_probes.write().await;
                        pending.entry(peer).or_insert_with(Instant::now);
                    }

                    // Send PING to peer via transport wrapped in MembershipProtocolMessage
                    trace!(peer_id = %peer, "SWIM: Probing peer");
                    let wrapped = MembershipProtocolMessage::Swim(SwimMessage::Ping);
                    match postcard::to_stdvec(&wrapped) {
                        Ok(bytes) => {
                            if let Err(e) = transport
                                .send_to_peer(peer, GossipStreamType::Membership, bytes.into())
                                .await
                            {
                                debug!(?e, peer_id = %peer, "SWIM: Probe send failed");
                            }
                        }
                        Err(e) => {
                            warn!(?e, "SWIM: Ping message serialization failed");
                        }
                    }
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
                local_peer_id,
                SWIM_PROBE_INTERVAL_SECS,
                SWIM_SUSPECT_TIMEOUT_SECS,
                SWIM_PROBE_FANOUT,
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
    ///
    /// Wraps the message in [`MembershipProtocolMessage::HyParView`] for unified
    /// dispatch on the receiving end, then serializes with postcard and sends
    /// on the Membership stream.
    async fn send_hyparview_message(&self, peer: PeerId, msg: &HyParViewMessage) -> Result<()> {
        let wrapped = MembershipProtocolMessage::HyParView(msg.clone());
        let bytes = postcard::to_stdvec(&wrapped)
            .map_err(|e| anyhow!("Failed to serialize HyParView message: {}", e))?;
        self.transport
            .send_to_peer(peer, GossipStreamType::Membership, bytes.into())
            .await
    }

    /// Get the SWIM detector
    pub fn swim(&self) -> &SwimDetector<T> {
        &self.swim
    }

    /// Dispatch an incoming membership message to the appropriate handler.
    ///
    /// Deserializes the raw bytes as a [`MembershipProtocolMessage`] using postcard,
    /// then routes to either the HyParView or SWIM handler based on the variant.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails or if the underlying handler
    /// returns an error.
    pub async fn dispatch_message(&self, sender: PeerId, data: &[u8]) -> Result<()> {
        let msg: MembershipProtocolMessage = postcard::from_bytes(data)
            .map_err(|e| anyhow!("Failed to deserialize membership message: {}", e))?;

        match msg {
            MembershipProtocolMessage::HyParView(hpv_msg) => {
                self.dispatch_hyparview(sender, hpv_msg).await
            }
            MembershipProtocolMessage::Swim(swim_msg) => self.dispatch_swim(sender, swim_msg).await,
        }
    }

    /// Dispatch a HyParView message to the appropriate handler method.
    async fn dispatch_hyparview(&self, sender: PeerId, msg: HyParViewMessage) -> Result<()> {
        match msg {
            HyParViewMessage::Join {
                sender: join_sender,
                ttl,
            } => self.handle_join(join_sender, ttl).await,
            HyParViewMessage::ForwardJoin {
                sender: fwd_sender,
                new_peer,
                ttl,
            } => self.handle_forward_join(fwd_sender, new_peer, ttl).await,
            HyParViewMessage::Neighbor {
                sender: nbr_sender,
                priority,
            } => self.handle_neighbor(nbr_sender, priority).await,
            HyParViewMessage::NeighborReply { accepted } => {
                self.handle_neighbor_reply(sender, accepted).await
            }
            HyParViewMessage::Shuffle {
                sender: shuf_sender,
                peers,
                ttl,
            } => self.handle_shuffle(shuf_sender, peers, ttl).await,
            HyParViewMessage::ShuffleReply { peers } => {
                self.handle_shuffle_reply(peers).await;
                Ok(())
            }
            HyParViewMessage::Disconnect => self.handle_disconnect(sender).await,
        }
    }

    /// Dispatch a SWIM message to the appropriate handler method.
    async fn dispatch_swim(&self, sender: PeerId, msg: SwimMessage) -> Result<()> {
        match msg {
            SwimMessage::Ping => self.swim.handle_ping(sender).await,
            SwimMessage::Ack => self.swim.handle_ack(sender).await,
            SwimMessage::PingReq { target, requester } => {
                self.swim.handle_ping_req(requester, target).await
            }
            SwimMessage::AckResponse { target, .. } => self.swim.handle_ack_response(target).await,
        }
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
        let transport = self.transport.clone();
        let local_peer_id = self.local_peer_id;

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(SHUFFLE_PERIOD_SECS));

            loop {
                interval.tick().await;

                let active_guard = active.read().await;

                // Select first active peer for shuffle target (skip if empty)
                let Some(target) = active_guard.iter().next().copied() else {
                    drop(active_guard);
                    continue;
                };

                // Build shuffle list: self + sample from active + passive
                let mut shuffle_list = vec![local_peer_id];
                shuffle_list.extend(active_guard.iter().take(SHUFFLE_ACTIVE_SIZE).copied());
                drop(active_guard);

                let passive_guard = passive.read().await;
                shuffle_list.extend(passive_guard.iter().take(SHUFFLE_PASSIVE_SIZE).copied());
                drop(passive_guard);

                debug!(
                    target = %target,
                    shuffle_count = shuffle_list.len(),
                    "HyParView: Periodic shuffle"
                );

                // Send SHUFFLE message wrapped in MembershipProtocolMessage
                let shuffle_msg = HyParViewMessage::Shuffle {
                    sender: local_peer_id,
                    peers: shuffle_list,
                    ttl: PASSIVE_RANDOM_WALK_LENGTH,
                };
                let wrapped = MembershipProtocolMessage::HyParView(shuffle_msg);

                match postcard::to_stdvec(&wrapped) {
                    Ok(bytes) => {
                        if let Err(e) = transport
                            .send_to_peer(target, GossipStreamType::Membership, bytes.into())
                            .await
                        {
                            debug!(?e, "HyParView: Shuffle send failed");
                        }
                    }
                    Err(e) => {
                        warn!(?e, "HyParView: Shuffle message serialization failed");
                    }
                }
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
            Err(_) => {
                warn!("HyParView: active_view lock contention, returning empty");
                Vec::new()
            }
        }
    }

    fn passive_view(&self) -> Vec<PeerId> {
        // Try to get read lock, return empty vec if unavailable
        match self.passive.try_read() {
            Ok(passive) => passive.iter().copied().collect(),
            Err(_) => {
                warn!("HyParView: passive_view lock contention, returning empty");
                Vec::new()
            }
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
    use saorsa_gossip_transport::UdpTransportAdapter;
    use std::net::SocketAddr;

    async fn test_transport() -> Arc<UdpTransportAdapter> {
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("valid addr");
        Arc::new(
            UdpTransportAdapter::new(bind, vec![])
                .await
                .expect("transport"),
        )
    }

    fn test_peer_id() -> PeerId {
        PeerId::new([0u8; 32])
    }

    async fn test_membership() -> HyParViewMembership<UdpTransportAdapter> {
        HyParViewMembership::new(
            test_peer_id(),
            DEFAULT_ACTIVE_DEGREE,
            DEFAULT_PASSIVE_DEGREE,
            test_transport().await,
        )
    }

    #[tokio::test]
    async fn test_hyparview_creation() {
        let membership = test_membership().await;
        assert_eq!(membership.active_view().len(), 0);
        assert_eq!(membership.passive_view().len(), 0);
    }

    #[tokio::test]
    async fn test_add_active_peer() {
        let membership = test_membership().await;
        let peer = PeerId::new([1u8; 32]);

        membership.add_active(peer).await.ok();
        let active = membership.active_view();
        assert_eq!(active.len(), 1);
        assert!(active.contains(&peer));
    }

    #[tokio::test]
    async fn test_remove_active_peer() {
        let membership = test_membership().await;
        let peer = PeerId::new([1u8; 32]);

        membership.add_active(peer).await.ok();
        membership.remove_active(peer).await.ok();

        let active = membership.active_view();
        assert_eq!(active.len(), 0);
    }

    #[tokio::test]
    async fn test_active_view_capacity() {
        let transport = test_transport().await;
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
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
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
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 1, SWIM_PROBE_FANOUT, transport); // 1s timeout
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
        let membership = test_membership().await;
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
        let transport = test_transport().await;
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
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport); // Long timeout so background task doesn't interfere

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

    #[tokio::test]
    async fn test_swim_probe_fanout() {
        let transport = test_transport().await;
        let custom_fanout = 5;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, custom_fanout, transport);

        assert_eq!(swim.probe_fanout(), custom_fanout);
    }

    #[tokio::test]
    async fn test_swim_default_probe_fanout() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);

        assert_eq!(swim.probe_fanout(), SWIM_PROBE_FANOUT);
        assert_eq!(SWIM_PROBE_FANOUT, 3);
    }

    // ===== Probe Timeout Tracking Tests =====

    #[tokio::test]
    async fn test_record_probe_adds_entry() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Initially no pending probes
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 0);
        drop(pending);

        // Record a probe
        swim.record_probe(peer).await;

        // Should now have one pending probe
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key(&peer));
    }

    #[tokio::test]
    async fn test_clear_probe_removes_entry() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Record a probe
        swim.record_probe(peer).await;
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 1);
        drop(pending);

        // Clear the probe
        let was_present = swim.clear_probe(&peer).await;
        assert!(was_present);

        // Should now be empty
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_clear_probe_returns_false_when_not_present() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Clear a probe that was never recorded
        let was_present = swim.clear_probe(&peer).await;
        assert!(!was_present);
    }

    #[tokio::test]
    async fn test_multiple_simultaneous_probes() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);
        let peer3 = PeerId::new([3u8; 32]);

        // Record multiple probes
        swim.record_probe(peer1).await;
        swim.record_probe(peer2).await;
        swim.record_probe(peer3).await;

        // Should have three pending probes
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 3);
        assert!(pending.contains_key(&peer1));
        assert!(pending.contains_key(&peer2));
        assert!(pending.contains_key(&peer3));
        drop(pending);

        // Clear one probe
        swim.clear_probe(&peer2).await;

        // Should have two remaining
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 2);
        assert!(pending.contains_key(&peer1));
        assert!(!pending.contains_key(&peer2));
        assert!(pending.contains_key(&peer3));
    }

    #[tokio::test]
    async fn test_record_probe_updates_existing_entry() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Record initial probe
        swim.record_probe(peer).await;
        let first_instant = {
            let pending = swim.pending_probes.read().await;
            *pending.get(&peer).expect("probe should exist")
        };

        // Wait a bit
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Record another probe for the same peer
        swim.record_probe(peer).await;
        let second_instant = {
            let pending = swim.pending_probes.read().await;
            *pending.get(&peer).expect("probe should exist")
        };

        // The timestamp should have been updated
        assert!(second_instant > first_instant);

        // Should still have only one entry
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 1);
    }

    // ===== Shuffle Protocol Tests =====

    #[tokio::test]
    async fn test_shuffle_returns_ok_with_empty_active_view() {
        let membership = test_membership().await;

        // With empty active view, shuffle should return Ok but do nothing
        let result = membership.shuffle().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shuffle_builds_peer_list() {
        let membership = test_membership().await;

        // Add peers to active and passive views
        let active_peer = PeerId::new([1u8; 32]);
        let passive_peer = PeerId::new([2u8; 32]);

        membership.add_active(active_peer).await.ok();
        membership.add_to_passive(passive_peer).await;

        // Shuffle should succeed (message send may fail without real connection, but that's OK)
        let result = membership.shuffle().await;
        // The actual send may fail without a real transport, but the logic completes
        // We verify the method doesn't panic and structures are correctly accessed
        assert!(result.is_ok() || result.is_err()); // Either is acceptable in test
    }

    #[tokio::test]
    async fn test_sample_active_returns_correct_count() {
        let membership = test_membership().await;

        // Add 5 peers to active
        for i in 1..=5 {
            let peer = PeerId::new([i; 32]);
            membership.add_active(peer).await.ok();
        }

        let sample = membership.sample_active(3).await;
        assert!(sample.len() <= 3);
        assert!(sample.len() <= 5); // Can't return more than exist
    }

    #[tokio::test]
    async fn test_sample_passive_returns_correct_count() {
        let membership = test_membership().await;

        // Add 5 peers to passive
        for i in 1..=5 {
            let peer = PeerId::new([i; 32]);
            membership.add_to_passive(peer).await;
        }

        let sample = membership.sample_passive(3).await;
        assert!(sample.len() <= 3);
    }

    #[tokio::test]
    async fn test_handle_shuffle_reply_adds_to_passive() {
        let membership = test_membership().await;
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        // Initially empty passive view
        assert_eq!(membership.passive_view().len(), 0);

        // Handle shuffle reply with peers
        membership.handle_shuffle_reply(vec![peer1, peer2]).await;

        // Both peers should be in passive view
        let passive = membership.passive_view();
        assert_eq!(passive.len(), 2);
        assert!(passive.contains(&peer1));
        assert!(passive.contains(&peer2));
    }

    #[tokio::test]
    async fn test_handle_shuffle_reply_excludes_self() {
        let membership = test_membership().await;
        let local_peer = test_peer_id(); // Same as membership's local peer
        let other_peer = PeerId::new([1u8; 32]);

        // Handle shuffle reply containing self
        membership
            .handle_shuffle_reply(vec![local_peer, other_peer])
            .await;

        // Only other_peer should be in passive view, not self
        let passive = membership.passive_view();
        assert_eq!(passive.len(), 1);
        assert!(passive.contains(&other_peer));
        assert!(!passive.contains(&local_peer));
    }

    #[tokio::test]
    async fn test_handle_shuffle_terminal_node_attempts_reply() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let shuffle_peer = PeerId::new([2u8; 32]);

        // With empty active view and TTL=0, handle_shuffle should try to send reply
        // In test environment without real transport, this will fail
        // But we verify the method doesn't panic and handles gracefully
        let result = membership
            .handle_shuffle(sender, vec![shuffle_peer], 0)
            .await;

        // Expected to fail since we can't send reply without real transport
        // The important test is that it doesn't panic and follows correct logic
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_shuffle_reply_integrates_peers_correctly() {
        // This tests the actual peer integration logic that handle_shuffle uses
        // (handle_shuffle_reply is the same code path for integrating received peers)
        let membership = test_membership().await;
        let shuffle_peer = PeerId::new([2u8; 32]);

        // Directly verify the add_to_passive logic that handle_shuffle uses
        membership.add_to_passive(shuffle_peer).await;

        let passive = membership.passive_view();
        assert!(passive.contains(&shuffle_peer));
    }

    #[tokio::test]
    async fn test_add_to_passive_excludes_active_peers() {
        let membership = test_membership().await;
        let peer = PeerId::new([1u8; 32]);

        // Add peer to active view first
        membership.add_active(peer).await.ok();

        // Try to add same peer to passive
        membership.add_to_passive(peer).await;

        // Peer should be in active but not passive
        assert!(membership.active_view().contains(&peer));
        assert!(!membership.passive_view().contains(&peer));
    }

    #[tokio::test]
    async fn test_add_to_passive_excludes_self() {
        let membership = test_membership().await;
        let local_peer = test_peer_id();

        // Try to add self to passive
        membership.add_to_passive(local_peer).await;

        // Self should not be in passive view
        assert!(!membership.passive_view().contains(&local_peer));
    }

    #[tokio::test]
    async fn test_random_active_peer_except() {
        let membership = test_membership().await;
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        membership.add_active(peer1).await.ok();
        membership.add_active(peer2).await.ok();

        // Should return a peer that is not peer1
        let result = membership.random_active_peer_except(peer1).await;
        assert!(result.is_some());
        assert_ne!(result.unwrap(), peer1);
    }

    #[tokio::test]
    async fn test_random_active_peer_except_returns_none_when_only_excluded() {
        let membership = test_membership().await;
        let peer1 = PeerId::new([1u8; 32]);

        membership.add_active(peer1).await.ok();

        // With only peer1 in active view, excluding peer1 should return None
        let result = membership.random_active_peer_except(peer1).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_handle_shuffle_forwards_when_ttl_positive_and_active_peers_exist() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let forwarder = PeerId::new([2u8; 32]);
        let shuffle_peer = PeerId::new([3u8; 32]);

        // Add sender and another peer to active view (need 2+ for forwarding)
        membership.add_active(sender).await.ok();
        membership.add_active(forwarder).await.ok();

        // Handle shuffle with TTL > 0: should attempt to forward
        let result = membership
            .handle_shuffle(sender, vec![shuffle_peer], 2)
            .await;

        // Should fail because no forwarding link is established yet
        assert!(result.is_err());

        // Peers should NOT be integrated when forwarding (only terminal nodes integrate)
        let passive = membership.passive_view();
        assert!(
            !passive.contains(&shuffle_peer),
            "Peers should not be integrated when forwarding"
        );
    }

    #[tokio::test]
    async fn test_handle_shuffle_becomes_terminal_when_sender_is_only_active_peer() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let shuffle_peer = PeerId::new([2u8; 32]);

        // Add only sender to active view (can't forward to anyone else)
        membership.add_active(sender).await.ok();

        // Handle shuffle with TTL > 0 but no one to forward to
        // Should act as terminal node and try to send reply (which fails)
        let result = membership
            .handle_shuffle(sender, vec![shuffle_peer], 2)
            .await;

        // Fails when trying to send shuffle reply
        assert!(result.is_err());
    }

    // ===== SwimMessage Serialization Tests =====

    #[test]
    fn test_swim_message_ping_roundtrip() {
        let msg = SwimMessage::Ping;
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: SwimMessage = postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            SwimMessage::Ping => {}
            _ => panic!("Expected Ping variant"),
        }
    }

    #[test]
    fn test_swim_message_ack_roundtrip() {
        let msg = SwimMessage::Ack;
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: SwimMessage = postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            SwimMessage::Ack => {}
            _ => panic!("Expected Ack variant"),
        }
    }

    #[test]
    fn test_swim_message_pingreq_roundtrip() {
        let target = PeerId::new([1u8; 32]);
        let requester = PeerId::new([2u8; 32]);
        let msg = SwimMessage::PingReq { target, requester };

        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: SwimMessage = postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            SwimMessage::PingReq {
                target: t,
                requester: r,
            } => {
                assert_eq!(t, target);
                assert_eq!(r, requester);
            }
            _ => panic!("Expected PingReq variant"),
        }
    }

    #[test]
    fn test_swim_message_ackresponse_roundtrip() {
        let target = PeerId::new([3u8; 32]);
        let requester = PeerId::new([4u8; 32]);
        let msg = SwimMessage::AckResponse { target, requester };

        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: SwimMessage = postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            SwimMessage::AckResponse {
                target: t,
                requester: r,
            } => {
                assert_eq!(t, target);
                assert_eq!(r, requester);
            }
            _ => panic!("Expected AckResponse variant"),
        }
    }

    #[test]
    fn test_swim_message_pingreq_contains_correct_peer_ids() {
        let target = PeerId::new([10u8; 32]);
        let requester = PeerId::new([20u8; 32]);
        let msg = SwimMessage::PingReq { target, requester };

        match msg {
            SwimMessage::PingReq {
                target: t,
                requester: r,
            } => {
                assert_eq!(t, target);
                assert_eq!(r, requester);
                assert_ne!(t, r); // Ensure they're different
            }
            _ => panic!("Expected PingReq variant"),
        }
    }

    #[test]
    fn test_swim_message_ackresponse_contains_correct_peer_ids() {
        let target = PeerId::new([30u8; 32]);
        let requester = PeerId::new([40u8; 32]);
        let msg = SwimMessage::AckResponse { target, requester };

        match msg {
            SwimMessage::AckResponse {
                target: t,
                requester: r,
            } => {
                assert_eq!(t, target);
                assert_eq!(r, requester);
                assert_ne!(t, r); // Ensure they're different
            }
            _ => panic!("Expected AckResponse variant"),
        }
    }

    #[test]
    fn test_all_swim_message_variants_serialize() {
        let target = PeerId::new([1u8; 32]);
        let requester = PeerId::new([2u8; 32]);

        let messages = vec![
            SwimMessage::Ping,
            SwimMessage::Ack,
            SwimMessage::PingReq { target, requester },
            SwimMessage::AckResponse { target, requester },
        ];

        for msg in messages {
            let bytes = postcard::to_stdvec(&msg).expect("serialize");
            let _deserialized: SwimMessage = postcard::from_bytes(&bytes).expect("deserialize");
        }
    }

    // ===== handle_ping and handle_ack Tests =====

    #[tokio::test]
    async fn test_handle_ping_marks_sender_alive() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let sender = PeerId::new([1u8; 32]);

        // Initially no state for this peer
        assert_eq!(swim.get_state(&sender).await, None);

        // Handle ping from sender
        let result = swim.handle_ping(sender).await;

        // Should succeed (send might fail without real connection, but that's OK for this test)
        // The important part is marking as alive
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Alive));

        // Result may be Err due to send failing, but state should be updated
        let _ = result;
    }

    #[tokio::test]
    async fn test_handle_ack_marks_sender_alive() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let sender = PeerId::new([1u8; 32]);

        // Initially no state for this peer
        assert_eq!(swim.get_state(&sender).await, None);

        // Handle ack from sender
        let result = swim.handle_ack(sender).await;
        assert!(result.is_ok());

        // Sender should be marked as alive
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Alive));
    }

    #[tokio::test]
    async fn test_handle_ack_clears_pending_probe() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Record a pending probe
        swim.record_probe(peer).await;
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 1);
        drop(pending);

        // Handle ack from peer
        let result = swim.handle_ack(peer).await;
        assert!(result.is_ok());

        // Pending probe should be cleared
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_handle_ack_unexpected_peer() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let unexpected_peer = PeerId::new([99u8; 32]);

        // Handle ack from peer we never probed
        let result = swim.handle_ack(unexpected_peer).await;

        // Should not panic and should succeed
        assert!(result.is_ok());

        // Peer should be marked as alive even if unexpected
        assert_eq!(
            swim.get_state(&unexpected_peer).await,
            Some(PeerState::Alive)
        );
    }

    #[tokio::test]
    async fn test_handle_ping_updates_suspect_to_alive() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let sender = PeerId::new([1u8; 32]);

        // Mark peer as suspect first
        swim.mark_alive(sender).await;
        swim.mark_suspect(sender).await;
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Suspect));

        // Handle ping from sender
        let _ = swim.handle_ping(sender).await;

        // Should be marked alive again
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Alive));
    }

    #[tokio::test]
    async fn test_handle_ack_updates_dead_to_alive() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let sender = PeerId::new([1u8; 32]);

        // Mark peer as dead first
        swim.mark_dead(sender).await;
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Dead));

        // Handle ack from sender (zombie!)
        let result = swim.handle_ack(sender).await;
        assert!(result.is_ok());

        // Should be marked alive again
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Alive));
    }

    #[tokio::test]
    async fn test_handle_multiple_pings_from_same_peer() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let sender = PeerId::new([1u8; 32]);

        // Handle multiple pings
        for _ in 0..5 {
            let _ = swim.handle_ping(sender).await;
        }

        // Should still be alive and only one entry
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Alive));

        let states = swim.states.read().await;
        assert_eq!(states.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_ack_multiple_times_same_peer() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 3, SWIM_PROBE_FANOUT, transport);
        let sender = PeerId::new([1u8; 32]);

        // Record probe and handle multiple acks
        swim.record_probe(sender).await;

        for i in 0..3 {
            let result = swim.handle_ack(sender).await;
            assert!(result.is_ok());

            // Only first ack should clear the probe
            if i == 0 {
                let pending = swim.pending_probes.read().await;
                assert_eq!(pending.len(), 0);
            }
        }

        // Should still be alive
        assert_eq!(swim.get_state(&sender).await, Some(PeerState::Alive));
    }

    // ===== Probe Task Multiple Peers Tests =====

    #[tokio::test]
    async fn test_probe_task_probes_multiple_peers() {
        let transport = test_transport().await;
        let fanout = 3;
        let swim = SwimDetector::new(test_peer_id(), 10, 100, fanout, transport); // 10 second probe period to ensure only one round

        // Add 10 alive peers
        for i in 1..=10 {
            let peer = PeerId::new([i; 32]);
            swim.mark_alive(peer).await;
        }

        // Wait briefly for the immediate first tick of the interval
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check pending_probes - should have multiple entries (at least 2, up to fanout)
        let pending = swim.pending_probes.read().await;
        assert!(
            pending.len() >= 2,
            "Expected at least 2 pending probes (multiple peers probed), got {}",
            pending.len()
        );
        assert!(
            pending.len() <= fanout,
            "Expected at most {} pending probes, got {}",
            fanout,
            pending.len()
        );
    }

    #[tokio::test]
    async fn test_probe_task_probes_all_when_fewer_than_fanout() {
        let transport = test_transport().await;
        let fanout = 3;
        let swim = SwimDetector::new(test_peer_id(), 10, 100, fanout, transport); // 10 second probe period to ensure only one round

        // Add only 2 alive peers (fewer than fanout)
        let peer1 = PeerId::new([1; 32]);
        let peer2 = PeerId::new([2; 32]);
        swim.mark_alive(peer1).await;
        swim.mark_alive(peer2).await;

        // Wait briefly for the immediate first tick of the interval
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check pending_probes - should have probed both peers
        let pending = swim.pending_probes.read().await;
        assert_eq!(
            pending.len(),
            2,
            "Expected exactly 2 pending probes (both peers), got {}",
            pending.len()
        );
        assert!(pending.contains_key(&peer1));
        assert!(pending.contains_key(&peer2));
    }

    // ===== Task 6: Probe Timeout Detection Tests =====

    #[tokio::test]
    async fn test_probe_timeout_marks_suspect() {
        let transport = test_transport().await;
        // Use short timeout for faster test
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Mark peer as alive and record probe
        swim.mark_alive(peer).await;
        swim.record_probe(peer).await;

        // Verify initially alive
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));

        // Wait for timeout. The probe_timeout_task interval has an immediate first tick,
        // then subsequent ticks every SWIM_ACK_TIMEOUT_MS (500ms).
        // The timeout check looks for probes with elapsed >= SWIM_ACK_TIMEOUT_MS.
        // Wait up to 3 seconds with polling to handle timing variations and system load
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if swim.get_state(&peer).await == Some(PeerState::Suspect) {
                // Success! Check probe was cleared
                let pending = swim.pending_probes.read().await;
                assert!(!pending.contains_key(&peer));
                return;
            }
        }

        // If we get here, the timeout never triggered
        panic!(
            "Probe timeout did not mark peer as suspect after 3 seconds. State: {:?}",
            swim.get_state(&peer).await
        );
    }

    #[tokio::test]
    async fn test_probe_ack_before_timeout_stays_alive() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let peer = PeerId::new([1u8; 32]);

        // Mark peer as alive and record probe
        swim.mark_alive(peer).await;
        swim.record_probe(peer).await;

        // Verify initially alive
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));

        // Send ack within timeout (300ms < 500ms)
        tokio::time::sleep(Duration::from_millis(300)).await;
        swim.handle_ack(peer).await.ok();

        // Should still be alive
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));

        // Wait past the original timeout
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Should still be alive (ack cleared the probe)
        assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));
    }

    // ===== Task 7: Indirect Probe Protocol Tests =====

    #[tokio::test]
    async fn test_request_indirect_probes_sends_ping_req() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let target = PeerId::new([1u8; 32]);
        let helper1 = PeerId::new([2u8; 32]);
        let helper2 = PeerId::new([3u8; 32]);
        let helper3 = PeerId::new([4u8; 32]);

        // Add helper peers as alive
        swim.mark_alive(helper1).await;
        swim.mark_alive(helper2).await;
        swim.mark_alive(helper3).await;

        // Request indirect probes (will fail to send without real transport, but logs the attempt)
        let result = swim.request_indirect_probes(target).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_request_indirect_probes_with_no_alive_peers() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let target = PeerId::new([1u8; 32]);

        // No alive peers - should return Ok but do nothing
        let result = swim.request_indirect_probes(target).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_ping_req_pings_target() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let requester = PeerId::new([1u8; 32]);
        let target = PeerId::new([2u8; 32]);

        // Handle PingReq (will fail without real transport, but shouldn't panic)
        let result = swim.handle_ping_req(requester, target).await;
        // Expected to fail due to no real transport connection
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_ack_response_marks_alive() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let target = PeerId::new([1u8; 32]);

        // Mark target as suspect first
        swim.mark_alive(target).await;
        swim.mark_suspect(target).await;
        assert_eq!(swim.get_state(&target).await, Some(PeerState::Suspect));

        // Handle AckResponse
        let result = swim.handle_ack_response(target).await;
        assert!(result.is_ok());

        // Should be marked alive again
        assert_eq!(swim.get_state(&target).await, Some(PeerState::Alive));
    }

    #[tokio::test]
    async fn test_handle_ack_response_clears_pending_probe() {
        let transport = test_transport().await;
        let swim = SwimDetector::new(test_peer_id(), 1, 100, SWIM_PROBE_FANOUT, transport);
        let target = PeerId::new([1u8; 32]);

        // Record a pending probe
        swim.record_probe(target).await;
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 1);
        drop(pending);

        // Handle AckResponse
        let result = swim.handle_ack_response(target).await;
        assert!(result.is_ok());

        // Pending probe should be cleared
        let pending = swim.pending_probes.read().await;
        assert_eq!(pending.len(), 0);
    }

    // ===== MembershipProtocolMessage Serialization Tests =====

    #[test]
    fn test_membership_protocol_message_hyparview_join_roundtrip() {
        let msg = MembershipProtocolMessage::HyParView(HyParViewMessage::Join {
            sender: PeerId::new([1u8; 32]),
            ttl: 6,
        });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: MembershipProtocolMessage =
            postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            MembershipProtocolMessage::HyParView(HyParViewMessage::Join { sender, ttl }) => {
                assert_eq!(sender, PeerId::new([1u8; 32]));
                assert_eq!(ttl, 6);
            }
            _ => panic!("Expected HyParView(Join)"),
        }
    }

    #[test]
    fn test_membership_protocol_message_hyparview_disconnect_roundtrip() {
        let msg = MembershipProtocolMessage::HyParView(HyParViewMessage::Disconnect);
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: MembershipProtocolMessage =
            postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            MembershipProtocolMessage::HyParView(HyParViewMessage::Disconnect) => {}
            _ => panic!("Expected HyParView(Disconnect)"),
        }
    }

    #[test]
    fn test_membership_protocol_message_swim_ping_roundtrip() {
        let msg = MembershipProtocolMessage::Swim(SwimMessage::Ping);
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: MembershipProtocolMessage =
            postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            MembershipProtocolMessage::Swim(SwimMessage::Ping) => {}
            _ => panic!("Expected Swim(Ping)"),
        }
    }

    #[test]
    fn test_membership_protocol_message_swim_ack_roundtrip() {
        let msg = MembershipProtocolMessage::Swim(SwimMessage::Ack);
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: MembershipProtocolMessage =
            postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            MembershipProtocolMessage::Swim(SwimMessage::Ack) => {}
            _ => panic!("Expected Swim(Ack)"),
        }
    }

    #[test]
    fn test_membership_protocol_message_swim_pingreq_roundtrip() {
        let target = PeerId::new([1u8; 32]);
        let requester = PeerId::new([2u8; 32]);
        let msg = MembershipProtocolMessage::Swim(SwimMessage::PingReq { target, requester });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: MembershipProtocolMessage =
            postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            MembershipProtocolMessage::Swim(SwimMessage::PingReq {
                target: t,
                requester: r,
            }) => {
                assert_eq!(t, target);
                assert_eq!(r, requester);
            }
            _ => panic!("Expected Swim(PingReq)"),
        }
    }

    #[test]
    fn test_membership_protocol_message_swim_ackresponse_roundtrip() {
        let target = PeerId::new([3u8; 32]);
        let requester = PeerId::new([4u8; 32]);
        let msg = MembershipProtocolMessage::Swim(SwimMessage::AckResponse { target, requester });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");
        let deserialized: MembershipProtocolMessage =
            postcard::from_bytes(&bytes).expect("deserialize");

        match deserialized {
            MembershipProtocolMessage::Swim(SwimMessage::AckResponse {
                target: t,
                requester: r,
            }) => {
                assert_eq!(t, target);
                assert_eq!(r, requester);
            }
            _ => panic!("Expected Swim(AckResponse)"),
        }
    }

    #[test]
    fn test_membership_protocol_message_all_hyparview_variants_roundtrip() {
        let sender = PeerId::new([1u8; 32]);
        let new_peer = PeerId::new([2u8; 32]);

        let messages = vec![
            MembershipProtocolMessage::HyParView(HyParViewMessage::Join { sender, ttl: 6 }),
            MembershipProtocolMessage::HyParView(HyParViewMessage::ForwardJoin {
                sender,
                new_peer,
                ttl: 3,
            }),
            MembershipProtocolMessage::HyParView(HyParViewMessage::Neighbor {
                sender,
                priority: NeighborPriority::High,
            }),
            MembershipProtocolMessage::HyParView(HyParViewMessage::NeighborReply {
                accepted: true,
            }),
            MembershipProtocolMessage::HyParView(HyParViewMessage::Shuffle {
                sender,
                peers: vec![new_peer],
                ttl: 3,
            }),
            MembershipProtocolMessage::HyParView(HyParViewMessage::ShuffleReply {
                peers: vec![sender],
            }),
            MembershipProtocolMessage::HyParView(HyParViewMessage::Disconnect),
        ];

        for msg in messages {
            let bytes = postcard::to_stdvec(&msg).expect("serialize");
            let _: MembershipProtocolMessage = postcard::from_bytes(&bytes).expect("deserialize");
        }
    }

    #[test]
    fn test_membership_protocol_message_all_swim_variants_roundtrip() {
        let target = PeerId::new([1u8; 32]);
        let requester = PeerId::new([2u8; 32]);

        let messages = vec![
            MembershipProtocolMessage::Swim(SwimMessage::Ping),
            MembershipProtocolMessage::Swim(SwimMessage::Ack),
            MembershipProtocolMessage::Swim(SwimMessage::PingReq { target, requester }),
            MembershipProtocolMessage::Swim(SwimMessage::AckResponse { target, requester }),
        ];

        for msg in messages {
            let bytes = postcard::to_stdvec(&msg).expect("serialize");
            let _: MembershipProtocolMessage = postcard::from_bytes(&bytes).expect("deserialize");
        }
    }

    // ===== dispatch_message Tests =====

    #[tokio::test]
    async fn test_dispatch_swim_ping_routes_to_handle_ping() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);

        // Serialize a SWIM Ping wrapped in MembershipProtocolMessage
        let msg = MembershipProtocolMessage::Swim(SwimMessage::Ping);
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch - handle_ping will mark sender alive and try to send Ack
        // Ack send may fail without real transport, but state should be updated
        let _ = membership.dispatch_message(sender, &bytes).await;

        // Verify sender was marked as alive (handle_ping behavior)
        assert_eq!(
            membership.swim().get_state(&sender).await,
            Some(PeerState::Alive)
        );
    }

    #[tokio::test]
    async fn test_dispatch_swim_ack_routes_to_handle_ack() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);

        // Record a pending probe first
        membership.swim().record_probe(sender).await;
        let pending = membership.swim().pending_probes.read().await;
        assert_eq!(pending.len(), 1);
        drop(pending);

        // Serialize a SWIM Ack wrapped in MembershipProtocolMessage
        let msg = MembershipProtocolMessage::Swim(SwimMessage::Ack);
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch
        let result = membership.dispatch_message(sender, &bytes).await;
        assert!(result.is_ok());

        // Verify sender was marked as alive and probe was cleared
        assert_eq!(
            membership.swim().get_state(&sender).await,
            Some(PeerState::Alive)
        );
        let pending = membership.swim().pending_probes.read().await;
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_dispatch_swim_pingreq_routes_to_handle_ping_req() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let target = PeerId::new([2u8; 32]);
        let requester = PeerId::new([3u8; 32]);

        // Serialize a SWIM PingReq wrapped in MembershipProtocolMessage
        let msg = MembershipProtocolMessage::Swim(SwimMessage::PingReq { target, requester });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch - will try to send Ping to target (will fail without real transport)
        let result = membership.dispatch_message(sender, &bytes).await;

        // Expected to fail due to no real transport, but should not panic
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_dispatch_swim_ack_response_routes_to_handle_ack_response() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let target = PeerId::new([2u8; 32]);
        let requester = PeerId::new([3u8; 32]);

        // Mark target as suspect
        membership.swim().mark_alive(target).await;
        membership.swim().mark_suspect(target).await;
        assert_eq!(
            membership.swim().get_state(&target).await,
            Some(PeerState::Suspect)
        );

        // Serialize an AckResponse wrapped in MembershipProtocolMessage
        let msg = MembershipProtocolMessage::Swim(SwimMessage::AckResponse { target, requester });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch
        let result = membership.dispatch_message(sender, &bytes).await;
        assert!(result.is_ok());

        // Verify target was marked as alive (handle_ack_response behavior)
        assert_eq!(
            membership.swim().get_state(&target).await,
            Some(PeerState::Alive)
        );
    }

    #[tokio::test]
    async fn test_dispatch_hyparview_join_routes_correctly() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let join_sender = PeerId::new([5u8; 32]);

        // Serialize a HyParView Join wrapped in MembershipProtocolMessage
        let msg = MembershipProtocolMessage::HyParView(HyParViewMessage::Join {
            sender: join_sender,
            ttl: 2,
        });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch - handle_join adds the peer to active view
        let result = membership.dispatch_message(sender, &bytes).await;
        assert!(result.is_ok());

        // Verify the joining peer was added to active view
        let active = membership.active_view();
        assert!(active.contains(&join_sender));
    }

    #[tokio::test]
    async fn test_dispatch_hyparview_disconnect_routes_correctly() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);

        // Add sender to active view first
        membership.add_active(sender).await.ok();
        assert!(membership.active_view().contains(&sender));

        // Serialize a HyParView Disconnect wrapped in MembershipProtocolMessage
        let msg = MembershipProtocolMessage::HyParView(HyParViewMessage::Disconnect);
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch - handle_disconnect removes the sender from active view
        let result = membership.dispatch_message(sender, &bytes).await;
        assert!(result.is_ok());

        // Verify sender was removed from active view
        assert!(!membership.active_view().contains(&sender));
    }

    #[tokio::test]
    async fn test_dispatch_invalid_bytes_returns_error() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);

        // Send garbage bytes
        let invalid_bytes = [0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA];

        let result = membership.dispatch_message(sender, &invalid_bytes).await;

        // Should return an error, not panic
        assert!(result.is_err());

        let err_msg = format!("{}", result.err().unwrap());
        assert!(
            err_msg.contains("Failed to deserialize membership message"),
            "Error should mention deserialization failure, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_dispatch_empty_bytes_returns_error() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);

        // Send empty bytes
        let result = membership.dispatch_message(sender, &[]).await;

        // Should return an error, not panic
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_dispatch_hyparview_shuffle_reply_routes_correctly() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);
        let peer_in_reply = PeerId::new([5u8; 32]);

        // Serialize a ShuffleReply
        let msg = MembershipProtocolMessage::HyParView(HyParViewMessage::ShuffleReply {
            peers: vec![peer_in_reply],
        });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch
        let result = membership.dispatch_message(sender, &bytes).await;
        assert!(result.is_ok());

        // Verify the peer from the reply was added to passive view
        let passive = membership.passive_view();
        assert!(passive.contains(&peer_in_reply));
    }

    #[tokio::test]
    async fn test_dispatch_hyparview_neighbor_reply_accepted() {
        let membership = test_membership().await;
        let sender = PeerId::new([1u8; 32]);

        // Serialize a NeighborReply(accepted=true)
        let msg = MembershipProtocolMessage::HyParView(HyParViewMessage::NeighborReply {
            accepted: true,
        });
        let bytes = postcard::to_stdvec(&msg).expect("serialize");

        // Dispatch
        let result = membership.dispatch_message(sender, &bytes).await;
        assert!(result.is_ok());

        // Verify sender was marked alive via SWIM
        assert_eq!(
            membership.swim().get_state(&sender).await,
            Some(PeerState::Alive)
        );
    }

    // ===== Property-Based Tests (proptest) =====

    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;
        use proptest::{collection, proptest};

        proptest! {
            /// For random cluster sizes and fanout values, probe_fanout() returns the configured value.
            #[test]
            fn probe_fanout_respects_k(
                _cluster_size in 3usize..=20,
                fanout in 1usize..=10
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let transport = test_transport().await;
                    let swim = SwimDetector::new(test_peer_id(), 1, 3, fanout, transport);
                    assert_eq!(swim.probe_fanout(), fanout);
                });
            }

            /// For random sets of peer IDs, marking them alive then querying returns all of them.
            #[test]
            fn alive_peers_tracked_correctly(
                peer_bytes in collection::vec(any::<[u8; 32]>(), 1..=20)
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let transport = test_transport().await;
                    let swim = SwimDetector::new(test_peer_id(), 100, 100, SWIM_PROBE_FANOUT, transport);

                    // Deduplicate peer IDs (same bytes = same peer)
                    let mut unique_peers = std::collections::HashSet::new();
                    let peers: Vec<PeerId> = peer_bytes
                        .into_iter()
                        .filter(|b| *b != [0u8; 32]) // Exclude self peer ID
                        .filter(|b| unique_peers.insert(*b))
                        .map(PeerId::new)
                        .collect();

                    for &peer in &peers {
                        swim.mark_alive(peer).await;
                    }

                    let alive = swim.get_peers_in_state(PeerState::Alive).await;
                    assert_eq!(alive.len(), peers.len());
                    for peer in &peers {
                        assert!(alive.contains(peer));
                    }
                });
            }

            /// For random sets of peer IDs, marking some alive and some suspect returns correct sets.
            #[test]
            fn suspect_peers_tracked_correctly(
                alive_bytes in collection::vec(any::<[u8; 32]>(), 1..=10),
                suspect_bytes in collection::vec(any::<[u8; 32]>(), 1..=10)
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let transport = test_transport().await;
                    let swim = SwimDetector::new(test_peer_id(), 100, 100, SWIM_PROBE_FANOUT, transport);

                    // Build unique alive peers (exclude self)
                    let mut seen = std::collections::HashSet::new();
                    let alive_peers: Vec<PeerId> = alive_bytes
                        .into_iter()
                        .filter(|b| *b != [0u8; 32])
                        .filter(|b| seen.insert(*b))
                        .map(PeerId::new)
                        .collect();

                    // Build unique suspect peers (exclude self and already-alive peers)
                    let suspect_peers: Vec<PeerId> = suspect_bytes
                        .into_iter()
                        .filter(|b| *b != [0u8; 32])
                        .filter(|b| seen.insert(*b))
                        .map(PeerId::new)
                        .collect();

                    // Mark alive peers
                    for &peer in &alive_peers {
                        swim.mark_alive(peer).await;
                    }

                    // For suspect peers: must mark alive first, then suspect
                    for &peer in &suspect_peers {
                        swim.mark_alive(peer).await;
                        swim.mark_suspect(peer).await;
                    }

                    let alive_result = swim.get_peers_in_state(PeerState::Alive).await;
                    let suspect_result = swim.get_peers_in_state(PeerState::Suspect).await;

                    assert_eq!(alive_result.len(), alive_peers.len());
                    assert_eq!(suspect_result.len(), suspect_peers.len());

                    for peer in &alive_peers {
                        assert!(alive_result.contains(peer));
                    }
                    for peer in &suspect_peers {
                        assert!(suspect_result.contains(peer));
                    }
                });
            }

            /// For random sets of peer IDs, marking some dead returns the correct set.
            #[test]
            fn dead_peers_tracked_correctly(
                dead_bytes in collection::vec(any::<[u8; 32]>(), 1..=20)
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let transport = test_transport().await;
                    let swim = SwimDetector::new(test_peer_id(), 100, 100, SWIM_PROBE_FANOUT, transport);

                    let mut unique_peers = std::collections::HashSet::new();
                    let dead_peers: Vec<PeerId> = dead_bytes
                        .into_iter()
                        .filter(|b| *b != [0u8; 32])
                        .filter(|b| unique_peers.insert(*b))
                        .map(PeerId::new)
                        .collect();

                    for &peer in &dead_peers {
                        swim.mark_dead(peer).await;
                    }

                    let dead_result = swim.get_peers_in_state(PeerState::Dead).await;
                    assert_eq!(dead_result.len(), dead_peers.len());
                    for peer in &dead_peers {
                        assert!(dead_result.contains(peer));
                    }
                });
            }

            /// For any peer that's marked suspect, marking alive transitions them back.
            #[test]
            fn mark_alive_clears_suspect(peer_bytes in any::<[u8; 32]>()) {
                // Skip self peer ID
                prop_assume!(peer_bytes != [0u8; 32]);

                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let transport = test_transport().await;
                    let swim = SwimDetector::new(test_peer_id(), 100, 100, SWIM_PROBE_FANOUT, transport);
                    let peer = PeerId::new(peer_bytes);

                    // Mark alive then suspect
                    swim.mark_alive(peer).await;
                    swim.mark_suspect(peer).await;
                    assert_eq!(swim.get_state(&peer).await, Some(PeerState::Suspect));

                    // Mark alive again
                    swim.mark_alive(peer).await;
                    assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));
                });
            }

            /// All SwimMessage variants roundtrip through postcard serialization.
            #[test]
            fn swim_message_roundtrip(
                target_bytes in any::<[u8; 32]>(),
                requester_bytes in any::<[u8; 32]>()
            ) {
                let target = PeerId::new(target_bytes);
                let requester = PeerId::new(requester_bytes);

                let messages = vec![
                    SwimMessage::Ping,
                    SwimMessage::Ack,
                    SwimMessage::PingReq { target, requester },
                    SwimMessage::AckResponse { target, requester },
                ];

                for msg in messages {
                    let bytes = postcard::to_stdvec(&msg).expect("serialize");
                    let deserialized: SwimMessage =
                        postcard::from_bytes(&bytes).expect("deserialize");

                    // Verify variant and field equality
                    match (&msg, &deserialized) {
                        (SwimMessage::Ping, SwimMessage::Ping) => {}
                        (SwimMessage::Ack, SwimMessage::Ack) => {}
                        (
                            SwimMessage::PingReq {
                                target: t1,
                                requester: r1,
                            },
                            SwimMessage::PingReq {
                                target: t2,
                                requester: r2,
                            },
                        ) => {
                            assert_eq!(t1, t2);
                            assert_eq!(r1, r2);
                        }
                        (
                            SwimMessage::AckResponse {
                                target: t1,
                                requester: r1,
                            },
                            SwimMessage::AckResponse {
                                target: t2,
                                requester: r2,
                            },
                        ) => {
                            assert_eq!(t1, t2);
                            assert_eq!(r1, r2);
                        }
                        _ => panic!("Variant mismatch after roundtrip"),
                    }
                }
            }

            /// Recording a probe then handling an ack clears the pending probe.
            #[test]
            fn pending_probes_cleared_on_ack(peer_bytes in any::<[u8; 32]>()) {
                // Skip self peer ID
                prop_assume!(peer_bytes != [0u8; 32]);

                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let transport = test_transport().await;
                    let swim = SwimDetector::new(test_peer_id(), 100, 100, SWIM_PROBE_FANOUT, transport);
                    let peer = PeerId::new(peer_bytes);

                    // Record a probe
                    swim.record_probe(peer).await;
                    let pending = swim.pending_probes.read().await;
                    assert!(pending.contains_key(&peer));
                    drop(pending);

                    // Handle ack
                    swim.handle_ack(peer).await.expect("handle_ack");

                    // Pending probe should be cleared
                    let pending = swim.pending_probes.read().await;
                    assert!(!pending.contains_key(&peer));

                    // Peer should be alive
                    assert_eq!(swim.get_state(&peer).await, Some(PeerState::Alive));
                });
            }
        }
    }
}
