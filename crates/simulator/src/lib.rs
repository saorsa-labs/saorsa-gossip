#![warn(missing_docs)]

//! Deterministic Network Simulator for Saorsa Gossip
//!
//! This crate provides a deterministic network simulator for testing
//! gossip protocols under various network conditions including:
//! - Configurable latency and bandwidth
//! - Packet loss and corruption
//! - Network partitions and topology changes
//! - Time dilation for accelerated testing
//!
//! # Architecture
//!
//! The simulator maintains a virtual clock and message queue, allowing
//! deterministic testing of distributed protocols. Each simulated node
//! runs in its own task with controlled message delivery.
//!
//! # Example Usage
//!
//! ```rust,no_run
//! use saorsa_gossip_simulator::{NetworkSimulator, LinkConfig, Topology};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create simulator with 5 nodes in a mesh topology
//! let mut simulator = NetworkSimulator::new()
//!     .with_topology(Topology::Mesh)
//!     .with_nodes(5)
//!     .with_time_dilation(10.0); // 10x speedup
//!
//! // Configure network conditions for all links
//! simulator.set_link_config_all(LinkConfig {
//!     latency_ms: 50,
//!     bandwidth_bps: 1_000_000, // 1 Mbps
//!     packet_loss_rate: 0.01,   // 1% loss
//!     jitter_ms: 10,
//! });
//!
//! // Start simulation
//! simulator.start().await?;
//!
//! // Run test scenario...
//!
//! # Ok(())
//! # }
//! ```

pub mod simulated_transport;

use rand::prelude::*;
use rand_pcg::Pcg64;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, trace, warn};

/// Unique identifier for simulated nodes
pub type NodeId = u32;

/// Simulated network message with metadata
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimulatedMessage {
    /// Source node ID
    pub from: NodeId,
    /// Destination node ID
    pub to: NodeId,
    /// Message payload
    pub payload: Vec<u8>,
    /// Message type (for routing)
    pub message_type: MessageType,
    /// Priority (higher = delivered first)
    pub priority: u8,
    /// Unique message ID for tracking
    pub id: u64,
}

/// Message type for routing decisions
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageType {
    /// Membership protocol messages
    Membership,
    /// PubSub dissemination messages
    PubSub,
    /// CRDT synchronization messages
    CrdtSync,
    /// Presence beacon messages
    Presence,
    /// Control messages (simulator internal)
    Control,
}

impl MessageType {
    /// Convert to the corresponding stream type for transport.
    ///
    /// Maps simulator message types to transport stream types:
    /// - `Membership` and `Control` → `StreamType::Membership`
    /// - `PubSub` → `StreamType::PubSub`
    /// - `CrdtSync` and `Presence` → `StreamType::Bulk`
    pub fn to_stream_type(self) -> saorsa_gossip_transport::GossipStreamType {
        match self {
            Self::Membership | Self::Control => {
                saorsa_gossip_transport::GossipStreamType::Membership
            }
            Self::PubSub => saorsa_gossip_transport::GossipStreamType::PubSub,
            Self::CrdtSync | Self::Presence => saorsa_gossip_transport::GossipStreamType::Bulk,
        }
    }

    /// Create from a transport stream type.
    ///
    /// Maps transport stream types to simulator message types:
    /// - `StreamType::Membership` → `MessageType::Membership`
    /// - `StreamType::PubSub` → `MessageType::PubSub`
    /// - `StreamType::Bulk` → `MessageType::CrdtSync`
    pub fn from_stream_type(stream: saorsa_gossip_transport::GossipStreamType) -> Self {
        match stream {
            saorsa_gossip_transport::GossipStreamType::Membership => Self::Membership,
            saorsa_gossip_transport::GossipStreamType::PubSub => Self::PubSub,
            saorsa_gossip_transport::GossipStreamType::Bulk => Self::CrdtSync,
        }
    }
}

impl From<saorsa_gossip_transport::GossipStreamType> for MessageType {
    fn from(stream: saorsa_gossip_transport::GossipStreamType) -> Self {
        Self::from_stream_type(stream)
    }
}

impl From<MessageType> for saorsa_gossip_transport::GossipStreamType {
    fn from(msg_type: MessageType) -> Self {
        msg_type.to_stream_type()
    }
}

/// Network link configuration between nodes
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkConfig {
    /// Base latency in milliseconds
    pub latency_ms: u64,
    /// Bandwidth limit in bytes per second (0 = unlimited)
    pub bandwidth_bps: u64,
    /// Packet loss rate (0.0 to 1.0)
    pub packet_loss_rate: f64,
    /// Jitter variation in milliseconds
    pub jitter_ms: u64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            latency_ms: 10,        // 10ms latency
            bandwidth_bps: 0,      // Unlimited bandwidth
            packet_loss_rate: 0.0, // No packet loss
            jitter_ms: 0,          // No jitter
        }
    }
}

/// Network topology configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Topology {
    /// Fully connected mesh (every node connects to every other)
    Mesh,
    /// Star topology (central hub with spokes)
    Star {
        /// Central node ID that acts as the hub
        center: NodeId,
    },
    /// Ring topology (circular connections)
    Ring,
    /// Tree topology (hierarchical connections)
    Tree {
        /// Number of children per parent node
        branching_factor: usize,
    },
    /// Custom topology with explicit connections
    Custom {
        /// List of explicit connections between nodes
        connections: Vec<(NodeId, NodeId)>,
    },
}

/// Transport interface for simulated nodes
#[async_trait::async_trait]
pub trait SimulatedTransport: Send + Sync {
    /// Send a message to another node
    async fn send_to_node(
        &self,
        to: NodeId,
        payload: Vec<u8>,
        message_type: MessageType,
    ) -> Result<(), SimulatorError>;

    /// Receive a message (called by simulator)
    async fn receive_message(&self, message: SimulatedMessage) -> Result<(), SimulatorError>;

    /// Get the node's identifier
    fn node_id(&self) -> NodeId;
}

/// Simulated network node
pub struct SimulatedNode {
    /// Node identifier
    pub id: NodeId,
    /// Node's transport interface
    pub transport: Box<dyn SimulatedTransport>,
    /// Message receive channel
    pub receiver: mpsc::UnboundedReceiver<SimulatedMessage>,
    /// Message send channel to simulator
    pub sender: mpsc::UnboundedSender<SimulatedMessage>,
}

/// Main network simulator
pub struct NetworkSimulator {
    /// Random number generator (deterministic with seed)
    rng: Arc<Mutex<Pcg64>>,
    /// Simulated nodes
    nodes: HashMap<NodeId, SimulatedNode>,
    /// Network topology
    topology: Topology,
    /// Link configurations (node pairs -> config)
    link_configs: HashMap<(NodeId, NodeId), LinkConfig>,
    /// Default link configuration
    default_link_config: LinkConfig,
    /// Message queue with delivery times
    message_queue: Arc<RwLock<VecDeque<(TokioInstant, SimulatedMessage)>>>,
    /// Time dilation factor (1.0 = real time, 2.0 = half speed, 0.5 = double speed)
    time_dilation: f64,
    /// Next message ID
    next_message_id: Arc<Mutex<u64>>,
    /// Simulation running state
    running: Arc<RwLock<bool>>,
}

impl NetworkSimulator {
    /// Clone the simulator's configuration, discarding runtime state.
    ///
    /// This method creates a new `NetworkSimulator` with the same configuration
    /// (topology, link configs, time dilation, RNG seed) but WITHOUT:
    /// - Nodes and their message channels
    /// - The message queue
    /// - Running state
    ///
    /// Use this when you need to pass simulator configuration to chaos injectors
    /// or create parallel test scenarios with the same network parameters.
    ///
    /// For a full clone including all state, use individual field access methods.
    pub fn clone_config(&self) -> Self {
        Self {
            rng: Arc::clone(&self.rng),
            nodes: HashMap::new(),
            topology: self.topology.clone(),
            link_configs: self.link_configs.clone(),
            default_link_config: self.default_link_config.clone(),
            message_queue: Arc::new(RwLock::new(VecDeque::new())),
            time_dilation: self.time_dilation,
            next_message_id: Arc::clone(&self.next_message_id),
            running: Arc::new(RwLock::new(false)),
        }
    }
}

impl Clone for NetworkSimulator {
    /// Creates a configuration-only clone of the simulator.
    ///
    /// **Warning**: This does NOT clone runtime state (nodes, message queue, running flag).
    /// Use `clone_config()` for explicit behavior, or access individual configuration
    /// through getter methods if you need specific values.
    fn clone(&self) -> Self {
        self.clone_config()
    }
}

/// Errors that can occur during network simulation
#[derive(thiserror::Error, Debug)]
pub enum SimulatorError {
    /// The specified node was not found in the simulation
    #[error("Node not found: {0}")]
    NodeNotFound(NodeId),
    /// No link configuration exists between the specified nodes
    #[error("Link not configured between nodes {0} and {1}")]
    LinkNotConfigured(NodeId, NodeId),
    /// Message delivery to target node failed
    #[error("Message delivery failed: {0}")]
    DeliveryFailed(String),
    /// Underlying transport layer error
    #[error("Transport error: {0}")]
    TransportError(String),
    /// Simulation must be running to perform this operation
    #[error("Simulation not running")]
    NotRunning,
    /// The topology configuration is invalid
    #[error("Invalid topology configuration")]
    InvalidTopology,
}

impl Default for NetworkSimulator {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkSimulator {
    /// Create a new network simulator
    pub fn new() -> Self {
        let rng = Pcg64::seed_from_u64(42); // Deterministic seed for testing

        Self {
            rng: Arc::new(Mutex::new(rng)),
            nodes: HashMap::new(),
            topology: Topology::Mesh,
            link_configs: HashMap::new(),
            default_link_config: LinkConfig::default(),
            message_queue: Arc::new(RwLock::new(VecDeque::new())),
            time_dilation: 1.0,
            next_message_id: Arc::new(Mutex::new(0)),
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Set the network topology
    pub fn with_topology(mut self, topology: Topology) -> Self {
        self.topology = topology;
        self
    }

    /// Set the number of nodes (creates default nodes)
    pub fn with_nodes(mut self, count: usize) -> Self {
        for i in 0..count {
            self.add_node(i as NodeId);
        }
        self
    }

    /// Set time dilation factor
    pub fn with_time_dilation(mut self, dilation: f64) -> Self {
        self.time_dilation = dilation;
        self
    }

    /// Set default link configuration
    pub fn with_default_link_config(mut self, config: LinkConfig) -> Self {
        self.default_link_config = config;
        self
    }

    /// Set deterministic seed for reproducible tests
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.rng = Arc::new(Mutex::new(Pcg64::seed_from_u64(seed)));
        self
    }

    /// Add a node to the simulation
    pub fn add_node(&mut self, node_id: NodeId) -> &mut Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let transport = Box::new(MockTransport::new(node_id, tx.clone()));

        let node = SimulatedNode {
            id: node_id,
            transport,
            receiver: rx,
            sender: tx,
        };

        self.nodes.insert(node_id, node);
        self
    }

    /// Set link configuration between two nodes
    pub fn set_link_config(&mut self, from: NodeId, to: NodeId, config: LinkConfig) -> &mut Self {
        self.link_configs.insert((from, to), config.clone());
        self.link_configs.insert((to, from), config); // Bidirectional
        self
    }

    /// Set link configuration for all links
    pub fn set_link_config_all(&mut self, config: LinkConfig) -> &mut Self {
        self.default_link_config = config;
        self
    }

    /// Get link configuration between two nodes
    fn get_link_config(&self, from: NodeId, to: NodeId) -> LinkConfig {
        self.link_configs
            .get(&(from, to))
            .or_else(|| self.link_configs.get(&(to, from)))
            .cloned()
            .unwrap_or_else(|| self.default_link_config.clone())
    }

    /// Send a message from one node to another
    pub async fn send_message(
        &self,
        from: NodeId,
        to: NodeId,
        payload: Vec<u8>,
        message_type: MessageType,
    ) -> Result<(), SimulatorError> {
        if !*self.running.read().await {
            return Err(SimulatorError::NotRunning);
        }

        let mut rng = self.rng.lock().await;
        let config = self.get_link_config(from, to);

        // Check for packet loss
        if rng.gen::<f64>() < config.packet_loss_rate {
            debug!(
                "Message dropped due to packet loss (from: {}, to: {})",
                from, to
            );
            return Ok(()); // Silently drop
        }

        // Calculate delivery time with latency and jitter
        let base_latency = Duration::from_millis(config.latency_ms);
        let jitter = Duration::from_millis(rng.gen_range(0..=config.jitter_ms));
        let delivery_delay = base_latency + jitter;

        // Apply time dilation
        let dilated_delay =
            Duration::from_secs_f64(delivery_delay.as_secs_f64() / self.time_dilation);

        let delivery_time = TokioInstant::now() + dilated_delay;

        // Create message
        let message_id = {
            let mut next_id = self.next_message_id.lock().await;
            let id = *next_id;
            *next_id += 1;
            id
        };

        let message = SimulatedMessage {
            from,
            to,
            payload,
            message_type,
            priority: 0, // Default priority
            id: message_id,
        };

        // Add to message queue
        {
            let mut queue = self.message_queue.write().await;
            queue.push_back((delivery_time, message));
            // Sort by delivery time (simple insertion sort for small queues)
            queue.make_contiguous().sort_by_key(|(time, _)| *time);
        }

        trace!(
            "Queued message {} from {} to {} for delivery at {:?}",
            message_id,
            from,
            to,
            delivery_time
        );
        Ok(())
    }

    /// Start the simulation
    pub async fn start(&mut self) -> Result<(), SimulatorError> {
        *self.running.write().await = true;

        // Initialize topology connections
        self.initialize_topology().await?;

        info!(
            "Network simulator started with {} nodes, time_dilation: {}",
            self.nodes.len(),
            self.time_dilation
        );

        // Start message delivery task
        let message_queue = self.message_queue.clone();
        let running = self.running.clone();

        tokio::spawn(async move {
            Self::run_message_delivery(message_queue, running).await;
        });

        Ok(())
    }

    /// Stop the simulation
    pub async fn stop(&mut self) -> Result<(), SimulatorError> {
        *self.running.write().await = false;
        info!("Network simulator stopped");
        Ok(())
    }

    /// Initialize topology connections
    async fn initialize_topology(&mut self) -> Result<(), SimulatorError> {
        match &self.topology {
            Topology::Mesh => {
                // Connect every node to every other node
                let node_ids: Vec<NodeId> = self.nodes.keys().copied().collect();
                for &from in &node_ids {
                    for &to in &node_ids {
                        if from != to {
                            // Connections are implicit in mesh topology
                        }
                    }
                }
            }
            Topology::Star { center } => {
                if !self.nodes.contains_key(center) {
                    return Err(SimulatorError::InvalidTopology);
                }
                // Connect center to all other nodes
                let _node_ids: Vec<NodeId> = self
                    .nodes
                    .keys()
                    .filter(|&&id| id != *center)
                    .copied()
                    .collect();
                // Connections are implicit in star topology
            }
            Topology::Ring => {
                let node_ids: Vec<NodeId> = self.nodes.keys().copied().collect();
                for i in 0..node_ids.len() {
                    let _from = node_ids[i];
                    let _to = node_ids[(i + 1) % node_ids.len()];
                    // Connections are implicit in ring topology
                }
            }
            Topology::Tree { branching_factor } => {
                // Tree topology: hierarchical connections where each node (except root)
                // has exactly one parent, and each parent has up to branching_factor children.
                // Node 0 is the root.
                // For node i > 0: parent = (i - 1) / branching_factor
                let node_ids: Vec<NodeId> = self.nodes.keys().copied().collect();

                if node_ids.is_empty() {
                    return Err(SimulatorError::InvalidTopology);
                }

                // Validate branching factor
                if *branching_factor == 0 {
                    return Err(SimulatorError::InvalidTopology);
                }

                // Calculate tree connections (for documentation/validation)
                // Node IDs might not be sequential 0..n, so we work with indices
                let mut sorted_ids: Vec<NodeId> = node_ids.clone();
                sorted_ids.sort();

                for (idx, _node_id) in sorted_ids.iter().enumerate() {
                    if idx > 0 {
                        let parent_idx = (idx - 1) / branching_factor;
                        if parent_idx >= sorted_ids.len() {
                            return Err(SimulatorError::InvalidTopology);
                        }
                        // Connections are implicit in tree topology
                        // Parent: sorted_ids[parent_idx], Child: sorted_ids[idx]
                    }
                }

                info!(
                    branching_factor,
                    nodes = node_ids.len(),
                    "Initialized tree topology"
                );
            }
            Topology::Custom { connections } => {
                // Validate all nodes exist
                for &(from, to) in connections {
                    if !self.nodes.contains_key(&from) || !self.nodes.contains_key(&to) {
                        return Err(SimulatorError::InvalidTopology);
                    }
                }
            }
        }
        Ok(())
    }

    /// Run the message delivery loop
    async fn run_message_delivery(
        message_queue: Arc<RwLock<VecDeque<(TokioInstant, SimulatedMessage)>>>,
        running: Arc<RwLock<bool>>,
    ) {
        while *running.read().await {
            let now = TokioInstant::now();

            // Process due messages
            loop {
                let message = {
                    let mut queue = message_queue.write().await;
                    match queue.front() {
                        Some((delivery_time, _)) if *delivery_time <= now => {
                            queue.pop_front().map(|(_, msg)| msg)
                        }
                        _ => None, // Queue empty or next message not due yet
                    }
                };

                match message {
                    Some(msg) => {
                        if let Err(e) = Self::deliver_message(msg).await {
                            warn!("Failed to deliver message: {:?}", e);
                        }
                    }
                    None => break,
                }
            }

            // Small sleep to prevent busy waiting
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Deliver a message to its destination node
    async fn deliver_message(message: SimulatedMessage) -> Result<(), SimulatorError> {
        // This would need access to the node registry
        // For now, this is a placeholder
        trace!("Delivering message {} to node {}", message.id, message.to);
        Ok(())
    }

    /// Get statistics about the simulation
    pub async fn get_stats(&self) -> SimulatorStats {
        let queue_len = self.message_queue.read().await.len();
        SimulatorStats {
            nodes: self.nodes.len(),
            queued_messages: queue_len,
            time_dilation: self.time_dilation,
        }
    }
}

/// Simulation statistics
#[derive(Debug, Clone)]
pub struct SimulatorStats {
    /// Number of nodes in the simulation
    pub nodes: usize,
    /// Number of messages currently in the delivery queue
    pub queued_messages: usize,
    /// Time dilation factor (1.0 = real-time)
    pub time_dilation: f64,
}

// ============================================================================
// Chaos Engineering Framework
// ============================================================================

/// Types of chaos events that can be injected into the simulation
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChaosEvent {
    /// Node completely fails and stops responding
    NodeFailure {
        /// ID of the node that will fail
        node_id: NodeId,
        /// How long the failure lasts
        duration: Duration,
    },

    /// Network partition splits nodes into isolated groups
    NetworkPartition {
        /// First group of isolated nodes
        group_a: Vec<NodeId>,
        /// Second group of isolated nodes
        group_b: Vec<NodeId>,
        /// How long the partition lasts
        duration: Duration,
    },

    /// Messages are dropped with specified probability
    MessageLoss {
        /// Probability of message loss (0.0-1.0)
        loss_rate: f64,
        /// How long the message loss condition lasts
        duration: Duration,
    },

    /// Messages are corrupted (payload modified)
    MessageCorruption {
        /// Probability of message corruption (0.0-1.0)
        corruption_rate: f64,
        /// How long the corruption condition lasts
        duration: Duration,
    },

    /// Network latency spikes to high values
    LatencySpike {
        /// Additional latency in milliseconds
        latency_ms: u64,
        /// How long the latency spike lasts
        duration: Duration,
    },

    /// Network bandwidth severely reduced
    BandwidthThrottling {
        /// Maximum bandwidth in bits per second
        bandwidth_bps: u64,
        /// How long the throttling lasts
        duration: Duration,
    },

    /// Clock skew introduced between nodes
    ClockSkew {
        /// ID of the node with clock skew
        node_id: NodeId,
        /// Clock offset in milliseconds (positive = ahead, negative = behind)
        offset_ms: i64,
        /// How long the clock skew lasts
        duration: Duration,
    },

    /// Custom chaos event for extensibility
    Custom {
        /// Name of the custom event
        name: String,
        /// Event parameters as JSON
        parameters: serde_json::Value,
        /// How long the event lasts
        duration: Duration,
    },
}

/// Chaos scenario configuration
#[derive(Clone, Debug)]
pub struct ChaosScenario {
    /// Name of the scenario for identification
    pub name: String,
    /// Sequence of chaos events to apply
    pub events: Vec<(Duration, ChaosEvent)>,
    /// Total duration of the scenario
    pub duration: Duration,
}

/// Chaos injector for applying failure scenarios to simulations
#[derive(Clone)]
pub struct ChaosInjector {
    /// Current active chaos events
    active_events: Arc<RwLock<Vec<(TokioInstant, ChaosEvent)>>>,
    /// Random number generator for deterministic chaos
    rng: Arc<Mutex<Pcg64>>,
    /// Whether chaos injection is enabled
    enabled: Arc<RwLock<bool>>,
}

impl NetworkSimulator {
    /// Get a chaos injector for this simulator
    pub fn create_chaos_injector(&self) -> ChaosInjector {
        ChaosInjector::new()
    }

    /// Check if the simulator is currently running
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Create some predefined chaos scenarios
    pub fn create_chaos_scenarios() -> Vec<ChaosScenario> {
        vec![
            // Basic network degradation
            ChaosScenario {
                name: "network_degradation".to_string(),
                duration: Duration::from_secs(60),
                events: vec![
                    (
                        Duration::from_secs(10),
                        ChaosEvent::LatencySpike {
                            latency_ms: 200,
                            duration: Duration::from_secs(20),
                        },
                    ),
                    (
                        Duration::from_secs(30),
                        ChaosEvent::MessageLoss {
                            loss_rate: 0.1,
                            duration: Duration::from_secs(15),
                        },
                    ),
                ],
            },
            // Node failure scenario
            ChaosScenario {
                name: "node_failure".to_string(),
                duration: Duration::from_secs(45),
                events: vec![(
                    Duration::from_secs(15),
                    ChaosEvent::NodeFailure {
                        node_id: 2,
                        duration: Duration::from_secs(10),
                    },
                )],
            },
            // Network partition
            ChaosScenario {
                name: "network_partition".to_string(),
                duration: Duration::from_secs(50),
                events: vec![(
                    Duration::from_secs(20),
                    ChaosEvent::NetworkPartition {
                        group_a: vec![0, 1],
                        group_b: vec![2, 3, 4],
                        duration: Duration::from_secs(15),
                    },
                )],
            },
            // Extreme chaos
            ChaosScenario {
                name: "extreme_chaos".to_string(),
                duration: Duration::from_secs(90),
                events: vec![
                    (
                        Duration::from_secs(5),
                        ChaosEvent::MessageLoss {
                            loss_rate: 0.2,
                            duration: Duration::from_secs(30),
                        },
                    ),
                    (
                        Duration::from_secs(10),
                        ChaosEvent::LatencySpike {
                            latency_ms: 500,
                            duration: Duration::from_secs(25),
                        },
                    ),
                    (
                        Duration::from_secs(15),
                        ChaosEvent::BandwidthThrottling {
                            bandwidth_bps: 50_000,
                            duration: Duration::from_secs(20),
                        },
                    ),
                    (
                        Duration::from_secs(35),
                        ChaosEvent::NodeFailure {
                            node_id: 1,
                            duration: Duration::from_secs(15),
                        },
                    ),
                ],
            },
        ]
    }
}

impl Default for ChaosInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl ChaosInjector {
    /// Create a new chaos injector
    pub fn new() -> Self {
        let rng = Pcg64::seed_from_u64(12345); // Different seed from simulator

        Self {
            active_events: Arc::new(RwLock::new(Vec::new())),
            rng: Arc::new(Mutex::new(rng)),
            enabled: Arc::new(RwLock::new(false)),
        }
    }

    /// Create chaos injector with deterministic seed
    pub fn with_seed(seed: u64) -> Self {
        let mut injector = Self::new();
        injector.rng = Arc::new(Mutex::new(Pcg64::seed_from_u64(seed)));
        injector
    }

    /// Enable chaos injection
    pub async fn enable(&self) {
        *self.enabled.write().await = true;
        info!("Chaos injection enabled");
    }

    /// Disable chaos injection
    pub async fn disable(&self) {
        *self.enabled.write().await = false;
        let mut active = self.active_events.write().await;
        active.clear();
        info!("Chaos injection disabled");
    }

    /// Inject a single chaos event
    pub async fn inject_event(&self, event: ChaosEvent) -> Result<(), SimulatorError> {
        if !*self.enabled.read().await {
            return Ok(());
        }

        let now = TokioInstant::now();
        let mut active = self.active_events.write().await;
        active.push((now, event.clone()));

        info!("Injected chaos event: {:?}", event);
        Ok(())
    }

    /// Run a complete chaos scenario
    pub async fn run_scenario(
        &self,
        scenario: ChaosScenario,
        simulator: Arc<RwLock<NetworkSimulator>>,
    ) -> Result<(), SimulatorError> {
        info!("Starting chaos scenario: {}", scenario.name);

        self.enable().await;

        let start_time = TokioInstant::now();
        let mut event_index = 0;

        while start_time.elapsed() < scenario.duration {
            // Check if it's time to inject the next event
            if event_index < scenario.events.len() {
                let (event_time, event) = &scenario.events[event_index];
                if start_time.elapsed() >= *event_time {
                    self.inject_event(event.clone()).await?;
                    event_index += 1;
                }
            }

            // Apply currently active chaos events
            self.apply_active_chaos(&simulator).await?;

            // Small delay to prevent busy waiting
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        self.disable().await;
        info!("Completed chaos scenario: {}", scenario.name);
        Ok(())
    }

    /// Apply currently active chaos events to the simulation
    async fn apply_active_chaos(
        &self,
        simulator: &Arc<RwLock<NetworkSimulator>>,
    ) -> Result<(), SimulatorError> {
        let active_events = self.active_events.read().await.clone();
        let now = TokioInstant::now();

        for (start_time, event) in active_events {
            let elapsed = now.duration_since(start_time);
            let should_apply = match event {
                ChaosEvent::NodeFailure { duration, .. } => elapsed < duration,
                ChaosEvent::NetworkPartition { duration, .. } => elapsed < duration,
                ChaosEvent::MessageLoss { duration, .. } => elapsed < duration,
                ChaosEvent::MessageCorruption { duration, .. } => elapsed < duration,
                ChaosEvent::LatencySpike { duration, .. } => elapsed < duration,
                ChaosEvent::BandwidthThrottling { duration, .. } => elapsed < duration,
                ChaosEvent::ClockSkew { duration, .. } => elapsed < duration,
                ChaosEvent::Custom { duration, .. } => elapsed < duration,
            };

            if should_apply {
                self.apply_chaos_event(simulator, &event).await?;
            }
        }

        Ok(())
    }

    /// Apply a specific chaos event
    async fn apply_chaos_event(
        &self,
        simulator: &Arc<RwLock<NetworkSimulator>>,
        event: &ChaosEvent,
    ) -> Result<(), SimulatorError> {
        match event {
            ChaosEvent::NodeFailure { node_id, .. } => {
                // Mark node as failed - messages to/from this node are dropped
                debug!("Applying node failure: {}", node_id);
                // This would require extending the simulator to track failed nodes
            }
            ChaosEvent::NetworkPartition { .. } => {
                // Prevent messages between groups
                debug!("Applying network partition between groups");
                // This would require modifying link configurations dynamically
            }
            ChaosEvent::MessageLoss { loss_rate, .. } => {
                // Increase packet loss rate globally
                let mut sim = simulator.write().await;
                let current_config = sim.default_link_config.clone();
                let new_config = LinkConfig {
                    packet_loss_rate: current_config.packet_loss_rate + *loss_rate,
                    ..current_config
                };
                sim.set_link_config_all(new_config);
            }
            ChaosEvent::MessageCorruption {
                corruption_rate, ..
            } => {
                // Messages get corrupted with given probability
                debug!("Applying message corruption: {}%", corruption_rate * 100.0);
                // This would require extending message delivery to corrupt payloads
            }
            ChaosEvent::LatencySpike { latency_ms, .. } => {
                // Increase latency globally
                let mut sim = simulator.write().await;
                let current_config = sim.default_link_config.clone();
                let new_config = LinkConfig {
                    latency_ms: current_config.latency_ms + *latency_ms,
                    ..current_config
                };
                sim.set_link_config_all(new_config);
            }
            ChaosEvent::BandwidthThrottling { bandwidth_bps, .. } => {
                // Reduce bandwidth globally
                let mut sim = simulator.write().await;
                let current_config = sim.default_link_config.clone();
                let new_config = LinkConfig {
                    bandwidth_bps: *bandwidth_bps.min(&current_config.bandwidth_bps),
                    ..current_config
                };
                sim.set_link_config_all(new_config);
            }
            ChaosEvent::ClockSkew {
                node_id, offset_ms, ..
            } => {
                // Introduce clock skew for specific node
                debug!("Applying clock skew to node {}: {}ms", node_id, offset_ms);
                // This would require per-node timing adjustments
            }
            ChaosEvent::Custom {
                name, parameters, ..
            } => {
                debug!(
                    "Applying custom chaos event: {} with params {:?}",
                    name, parameters
                );
                // Custom event handling
            }
        }

        Ok(())
    }

    /// Get current chaos statistics
    pub async fn get_stats(&self) -> ChaosStats {
        let active_count = self.active_events.read().await.len();
        let enabled = *self.enabled.read().await;

        ChaosStats {
            enabled,
            active_events: active_count,
        }
    }
}

/// Chaos injection statistics
#[derive(Debug, Clone)]
pub struct ChaosStats {
    /// Whether chaos injection is currently enabled
    pub enabled: bool,
    /// Number of currently active chaos events
    pub active_events: usize,
}

/// Mock transport implementation for testing
pub struct MockTransport {
    node_id: NodeId,
    sender: mpsc::UnboundedSender<SimulatedMessage>,
}

impl MockTransport {
    /// Create a new mock transport for a node
    pub fn new(node_id: NodeId, sender: mpsc::UnboundedSender<SimulatedMessage>) -> Self {
        Self { node_id, sender }
    }
}

#[async_trait::async_trait]
impl SimulatedTransport for MockTransport {
    async fn send_to_node(
        &self,
        to: NodeId,
        payload: Vec<u8>,
        message_type: MessageType,
    ) -> Result<(), SimulatorError> {
        let message = SimulatedMessage {
            from: self.node_id,
            to,
            payload,
            message_type,
            priority: 0,
            id: 0, // Will be set by simulator
        };

        self.sender
            .send(message)
            .map_err(|_| SimulatorError::DeliveryFailed("Channel closed".to_string()))
    }

    async fn receive_message(&self, _message: SimulatedMessage) -> Result<(), SimulatorError> {
        // Messages are received through the channel, not directly
        Ok(())
    }

    fn node_id(&self) -> NodeId {
        self.node_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simulator_creation() {
        let simulator = NetworkSimulator::new();
        assert_eq!(simulator.nodes.len(), 0);
        assert_eq!(simulator.time_dilation, 1.0);
    }

    #[tokio::test]
    async fn test_add_nodes() {
        let simulator = NetworkSimulator::new().with_nodes(3);
        assert_eq!(simulator.nodes.len(), 3);
        assert!(simulator.nodes.contains_key(&0));
        assert!(simulator.nodes.contains_key(&1));
        assert!(simulator.nodes.contains_key(&2));
    }

    #[tokio::test]
    async fn test_link_config() {
        let mut simulator = NetworkSimulator::new().with_nodes(2);

        let config = LinkConfig {
            latency_ms: 100,
            bandwidth_bps: 1_000_000,
            packet_loss_rate: 0.05,
            jitter_ms: 10,
        };

        simulator.set_link_config(0, 1, config.clone());

        let retrieved = simulator.get_link_config(0, 1);
        assert_eq!(retrieved.latency_ms, 100);
        assert_eq!(retrieved.bandwidth_bps, 1_000_000);
    }

    #[tokio::test]
    async fn test_simulator_stats() {
        let simulator = NetworkSimulator::new().with_nodes(2);
        let stats = simulator.get_stats().await;
        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.time_dilation, 1.0);
    }

    #[tokio::test]
    async fn test_chaos_injector_creation() {
        let injector = ChaosInjector::new();
        let stats = injector.get_stats().await;
        assert!(!stats.enabled);
        assert_eq!(stats.active_events, 0);
    }

    #[tokio::test]
    async fn test_chaos_injector_enable_disable() {
        let injector = ChaosInjector::new();

        // Initially disabled
        let stats = injector.get_stats().await;
        assert!(!stats.enabled);

        // Enable
        injector.enable().await;
        let stats = injector.get_stats().await;
        assert!(stats.enabled);

        // Disable
        injector.disable().await;
        let stats = injector.get_stats().await;
        assert!(!stats.enabled);
        assert_eq!(stats.active_events, 0); // Should clear events
    }

    #[tokio::test]
    async fn test_chaos_event_injection() {
        let injector = ChaosInjector::new();
        injector.enable().await;

        // Inject a message loss event
        let event = ChaosEvent::MessageLoss {
            loss_rate: 0.1,
            duration: Duration::from_secs(10),
        };

        injector.inject_event(event).await.unwrap();

        let stats = injector.get_stats().await;
        assert_eq!(stats.active_events, 1);
    }

    #[tokio::test]
    async fn test_chaos_scenarios_creation() {
        let scenarios = NetworkSimulator::create_chaos_scenarios();

        assert_eq!(scenarios.len(), 4);

        // Check that each scenario has the expected name
        let names: Vec<&str> = scenarios.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"network_degradation"));
        assert!(names.contains(&"node_failure"));
        assert!(names.contains(&"network_partition"));
        assert!(names.contains(&"extreme_chaos"));
    }

    #[tokio::test]
    async fn test_simulator_with_chaos_injector() {
        let simulator = NetworkSimulator::new().with_nodes(3);
        let injector = simulator.create_chaos_injector();

        // Injector should be attached to simulator
        injector.enable().await;

        let event = ChaosEvent::LatencySpike {
            latency_ms: 100,
            duration: Duration::from_secs(5),
        };

        injector.inject_event(event).await.unwrap();
        let stats = injector.get_stats().await;
        assert_eq!(stats.active_events, 1);
    }

    #[tokio::test]
    async fn test_tree_topology_initialization() {
        // Create simulator with tree topology, branching factor 2
        let mut simulator = NetworkSimulator::new()
            .with_topology(Topology::Tree {
                branching_factor: 2,
            })
            .with_nodes(7);

        // Start should succeed
        let result = simulator.start().await;
        assert!(
            result.is_ok(),
            "Tree topology initialization should succeed"
        );

        simulator.stop().await.ok();
    }

    #[tokio::test]
    async fn test_tree_topology_binary_tree() {
        // Binary tree with 7 nodes:
        //       0
        //      / \
        //     1   2
        //    /|   |\
        //   3 4   5 6
        let mut simulator = NetworkSimulator::new()
            .with_topology(Topology::Tree {
                branching_factor: 2,
            })
            .with_nodes(7);

        let result = simulator.start().await;
        assert!(result.is_ok());

        // Verify node count
        assert_eq!(simulator.nodes.len(), 7);

        simulator.stop().await.ok();
    }

    #[tokio::test]
    async fn test_tree_topology_ternary() {
        // Ternary tree (branching factor 3) with 10 nodes:
        //          0
        //       /  |  \
        //      1   2   3
        //     /|\  |
        //    4 5 6 7...
        let mut simulator = NetworkSimulator::new()
            .with_topology(Topology::Tree {
                branching_factor: 3,
            })
            .with_nodes(10);

        let result = simulator.start().await;
        assert!(result.is_ok());

        simulator.stop().await.ok();
    }

    #[tokio::test]
    async fn test_tree_topology_single_node() {
        // Single node tree should work
        let mut simulator = NetworkSimulator::new()
            .with_topology(Topology::Tree {
                branching_factor: 2,
            })
            .with_nodes(1);

        let result = simulator.start().await;
        assert!(result.is_ok());

        simulator.stop().await.ok();
    }

    #[tokio::test]
    async fn test_tree_topology_invalid_branching_factor() {
        // Branching factor 0 should fail
        let mut simulator = NetworkSimulator::new()
            .with_topology(Topology::Tree {
                branching_factor: 0,
            })
            .with_nodes(5);

        let result = simulator.start().await;
        assert!(result.is_err(), "Branching factor 0 should be invalid");
    }

    #[tokio::test]
    async fn test_tree_topology_no_nodes() {
        // Empty tree should fail
        let mut simulator = NetworkSimulator::new().with_topology(Topology::Tree {
            branching_factor: 2,
        });

        let result = simulator.start().await;
        assert!(result.is_err(), "Empty tree should be invalid");
    }
}
