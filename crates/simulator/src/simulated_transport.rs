//! Simulated Transport for Gossip Protocols
//!
//! This module provides a transport implementation that integrates
//! the network simulator with actual gossip protocols, allowing
//! realistic chaos testing of membership, pubsub, and CRDT protocols.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};

use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use saorsa_gossip_types::PeerId;

use crate::{MessageType, NetworkSimulator, NodeId, SimulatedMessage};

/// Maps PeerId to simulator NodeId
type PeerMap = HashMap<PeerId, NodeId>;
type NodePeerMap = HashMap<NodeId, PeerId>;

/// Simulated transport that bridges simulator and gossip protocols
pub struct SimulatedGossipTransport {
    /// This peer's ID
    peer_id: PeerId,
    /// This peer's simulator node ID
    node_id: NodeId,
    /// Reference to the network simulator
    simulator: Arc<RwLock<NetworkSimulator>>,
    /// Map of peer IDs to node IDs
    peer_to_node: Arc<RwLock<PeerMap>>,
    /// Channel for receiving messages
    receiver: Arc<Mutex<mpsc::UnboundedReceiver<(PeerId, GossipStreamType, Bytes)>>>,
    /// Channel for sending to receiver
    sender: mpsc::UnboundedSender<(PeerId, GossipStreamType, Bytes)>,
    /// Listening state
    listening: Arc<RwLock<bool>>,
}

impl SimulatedGossipTransport {
    /// Create a new simulated transport
    pub fn new(
        peer_id: PeerId,
        node_id: NodeId,
        simulator: Arc<RwLock<NetworkSimulator>>,
        peer_to_node: Arc<RwLock<PeerMap>>,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        Self {
            peer_id,
            node_id,
            simulator,
            peer_to_node,
            receiver: Arc::new(Mutex::new(receiver)),
            sender,
            listening: Arc::new(RwLock::new(false)),
        }
    }

    /// Get the channel sender for message delivery
    pub fn get_sender(&self) -> mpsc::UnboundedSender<(PeerId, GossipStreamType, Bytes)> {
        self.sender.clone()
    }
}

#[async_trait::async_trait]
impl GossipTransport for SimulatedGossipTransport {
    async fn dial(&self, peer: PeerId, _addr: SocketAddr) -> Result<()> {
        // In simulation, dialing just records the peer mapping
        // The simulator already has all nodes connected based on topology
        let node_map = self.peer_to_node.read().await;
        if !node_map.contains_key(&peer) {
            drop(node_map);
            // Register the peer if not already known
            // This would normally be done during network initialization
        }
        Ok(())
    }

    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId> {
        // In simulation, generate a deterministic peer ID from address
        let mut id_bytes = [0u8; 32];
        let addr_bytes = addr.to_string();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            addr_bytes.hash(&mut hasher);
            hasher.finish()
        };
        id_bytes[..8].copy_from_slice(&hash.to_le_bytes());
        let peer_id = PeerId::new(id_bytes);
        Ok(peer_id)
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<()> {
        // Mark as listening
        *self.listening.write().await = true;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        *self.listening.write().await = false;
        Ok(())
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        // Look up the target node ID
        let node_map = self.peer_to_node.read().await;
        let target_node = node_map
            .get(&peer)
            .ok_or_else(|| anyhow!("Unknown peer: {:?}", peer))?;

        // Send through the simulator using From trait conversion
        let message_type: MessageType = stream_type.into();
        let sim = self.simulator.read().await;
        sim.send_message(self.node_id, *target_node, data.to_vec(), message_type)
            .await?;

        Ok(())
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        // Wait for a message from the channel
        let mut receiver = self.receiver.lock().await;
        receiver
            .recv()
            .await
            .ok_or_else(|| anyhow!("Transport closed"))
    }
}

/// Builder for creating a simulated gossip network
pub struct SimulatedGossipNetwork {
    simulator: Arc<RwLock<NetworkSimulator>>,
    peer_to_node: Arc<RwLock<PeerMap>>,
    node_to_peer: Arc<RwLock<NodePeerMap>>,
    transports: Vec<SimulatedGossipTransport>,
}

impl SimulatedGossipNetwork {
    /// Create a new simulated gossip network
    pub fn new(simulator: NetworkSimulator) -> Self {
        Self {
            simulator: Arc::new(RwLock::new(simulator)),
            peer_to_node: Arc::new(RwLock::new(HashMap::new())),
            node_to_peer: Arc::new(RwLock::new(HashMap::new())),
            transports: Vec::new(),
        }
    }

    /// Add a peer to the network
    pub async fn add_peer(&mut self, peer_id: PeerId, node_id: NodeId) -> SimulatedGossipTransport {
        // Register the mapping
        self.peer_to_node.write().await.insert(peer_id, node_id);
        self.node_to_peer.write().await.insert(node_id, peer_id);

        // Create the transport
        SimulatedGossipTransport::new(
            peer_id,
            node_id,
            Arc::clone(&self.simulator),
            Arc::clone(&self.peer_to_node),
        )
    }

    /// Start the simulator
    pub async fn start(&mut self) -> Result<()> {
        self.simulator
            .write()
            .await
            .start()
            .await
            .map_err(|e| anyhow!("{:?}", e))
    }

    /// Stop the simulator
    pub async fn stop(&mut self) -> Result<()> {
        self.simulator
            .write()
            .await
            .stop()
            .await
            .map_err(|e| anyhow!("{:?}", e))
    }

    /// Get a reference to the simulator for chaos injection
    pub fn simulator(&self) -> Arc<RwLock<NetworkSimulator>> {
        Arc::clone(&self.simulator)
    }

    /// Deliver a message to a specific peer's transport
    /// This is called by the simulator when a message is ready for delivery
    pub async fn deliver_message(
        &self,
        to_node: NodeId,
        from_node: NodeId,
        message: SimulatedMessage,
    ) -> Result<()> {
        // Look up the peer IDs
        let node_map = self.node_to_peer.read().await;
        let to_peer = node_map
            .get(&to_node)
            .ok_or_else(|| anyhow!("Unknown node: {}", to_node))?;
        let from_peer = node_map
            .get(&from_node)
            .ok_or_else(|| anyhow!("Unknown node: {}", from_node))?;

        // Find the transport and deliver
        for transport in &self.transports {
            if transport.peer_id == *to_peer {
                // Use From trait conversion for message type to stream type
                let stream_type: GossipStreamType = message.message_type.into();
                let bytes = Bytes::from(message.payload);
                transport.sender.send((*from_peer, stream_type, bytes))?;
                return Ok(());
            }
        }

        Err(anyhow!("Transport not found for peer: {:?}", to_peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NetworkSimulator, Topology};

    #[tokio::test]
    async fn test_simulated_transport_creation() {
        let simulator = NetworkSimulator::new()
            .with_nodes(2)
            .with_topology(Topology::Mesh);

        let mut network = SimulatedGossipNetwork::new(simulator);

        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        let transport1 = network.add_peer(peer1, 0).await;
        let transport2 = network.add_peer(peer2, 1).await;

        assert_eq!(transport1.peer_id, peer1);
        assert_eq!(transport2.peer_id, peer2);
    }

    #[tokio::test]
    async fn test_simulated_transport_send_receive() {
        let simulator = NetworkSimulator::new()
            .with_nodes(2)
            .with_topology(Topology::Mesh);

        let mut network = SimulatedGossipNetwork::new(simulator);
        network.start().await.unwrap();

        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        let transport1 = network.add_peer(peer1, 0).await;
        let _transport2 = network.add_peer(peer2, 1).await;

        // Send a message from peer1 to peer2
        let message = Bytes::from("Hello, peer2!");
        transport1
            .send_to_peer(peer2, GossipStreamType::PubSub, message.clone())
            .await
            .unwrap();

        // Note: In a real integration, the simulator's message delivery
        // would trigger the receive. For this test, we verify the send succeeds.
    }
}
