//! Multiplexed Transport for Saorsa Gossip
//!
//! This module provides [`MultiplexedGossipTransport`] which manages multiple
//! transport adapters through a [`TransportMultiplexer`]. It allows the gossip
//! protocol to communicate over multiple transport layers (UDP, BLE, LoRa)
//! simultaneously, routing messages based on capability requirements.
//!
//! # Example
//!
//! ```ignore
//! use saorsa_gossip_transport::{
//!     MultiplexedGossipTransport, TransportMultiplexer, UdpTransportAdapter,
//! };
//! use std::sync::Arc;
//!
//! // Create a multiplexer with UDP transport
//! let udp = Arc::new(UdpTransportAdapter::new(addr).await?);
//! let multiplexer = TransportMultiplexer::new(udp.local_peer_id());
//! multiplexer.register_transport(TransportDescriptor::Udp, udp.clone()).await?;
//! multiplexer.set_default_transport(TransportDescriptor::Udp).await?;
//!
//! // Wrap in MultiplexedGossipTransport
//! let transport = MultiplexedGossipTransport::new(
//!     Arc::new(multiplexer),
//!     local_peer_id,
//! );
//! ```

use std::sync::Arc;

use saorsa_gossip_types::PeerId;

use crate::error::TransportResult;
use crate::{TransportAdapter, TransportDescriptor, TransportMultiplexer};

/// A multiplexed gossip transport that manages multiple transport adapters.
///
/// This struct coordinates communication across multiple transport layers,
/// allowing the gossip protocol to utilize multiple network interfaces
/// and transport protocols (UDP/QUIC, BLE, LoRa) simultaneously.
///
/// The multiplexer routes messages to the appropriate transport based on
/// capability requirements (e.g., low-latency for membership, bulk for CRDTs).
#[derive(Clone)]
pub struct MultiplexedGossipTransport {
    /// The underlying transport multiplexer managing multiple adapters
    multiplexer: Arc<TransportMultiplexer>,
    /// The local peer ID for this transport
    local_peer_id: PeerId,
}

impl MultiplexedGossipTransport {
    /// Creates a new multiplexed gossip transport.
    ///
    /// # Arguments
    ///
    /// * `multiplexer` - The transport multiplexer managing multiple adapters
    /// * `local_peer_id` - The local peer ID for this transport instance
    ///
    /// # Returns
    ///
    /// A new `MultiplexedGossipTransport` instance.
    pub fn new(multiplexer: Arc<TransportMultiplexer>, local_peer_id: PeerId) -> Self {
        Self {
            multiplexer,
            local_peer_id,
        }
    }

    /// Returns the local peer ID for this transport.
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Returns a reference to the underlying multiplexer.
    pub fn multiplexer(&self) -> &Arc<TransportMultiplexer> {
        &self.multiplexer
    }

    /// Creates a multiplexed transport from a single transport adapter.
    ///
    /// This is a convenience builder method that creates a multiplexer
    /// with a single transport adapter registered as the default, then
    /// wraps it in a `MultiplexedGossipTransport`.
    ///
    /// # Arguments
    ///
    /// * `adapter` - The transport adapter to use
    /// * `descriptor` - The descriptor for this transport type
    ///
    /// # Returns
    ///
    /// A new `MultiplexedGossipTransport` instance, or a transport error
    /// if the multiplexer cannot be created.
    ///
    /// # Errors
    ///
    /// Returns a transport error if registration fails.
    pub async fn from_adapter(
        adapter: Arc<dyn TransportAdapter>,
        descriptor: TransportDescriptor,
    ) -> TransportResult<Self> {
        let local_peer_id = adapter.local_peer_id();
        let multiplexer = TransportMultiplexer::new(local_peer_id);

        // Register as default transport
        multiplexer
            .register_transport(descriptor.clone(), adapter)
            .await?;
        multiplexer.set_default_transport(descriptor).await?;

        Ok(Self::new(Arc::new(multiplexer), local_peer_id))
    }
}

impl std::fmt::Debug for MultiplexedGossipTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiplexedGossipTransport")
            .field("local_peer_id", &self.local_peer_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer_id() -> PeerId {
        PeerId::new([0u8; 32])
    }

    #[test]
    fn test_multiplexed_transport_creation() {
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));
        let transport = MultiplexedGossipTransport::new(multiplexer, peer_id);

        assert_eq!(transport.local_peer_id(), peer_id);
    }

    #[test]
    fn test_multiplexed_transport_debug() {
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));
        let transport = MultiplexedGossipTransport::new(multiplexer, peer_id);

        let debug_str = format!("{:?}", transport);
        assert!(debug_str.contains("MultiplexedGossipTransport"));
    }

    #[test]
    fn test_multiplexer_accessor() {
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));
        let transport = MultiplexedGossipTransport::new(multiplexer.clone(), peer_id);

        assert!(Arc::ptr_eq(transport.multiplexer(), &multiplexer));
    }
}
