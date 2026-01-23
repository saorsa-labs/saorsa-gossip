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

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use saorsa_gossip_types::PeerId;
use tracing::{debug, info, warn};

use crate::error::TransportResult;
use crate::{
    GossipStreamType, GossipTransport, TransportAdapter, TransportDescriptor, TransportMultiplexer,
    TransportRequest,
};

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

#[async_trait::async_trait]
impl GossipTransport for MultiplexedGossipTransport {
    async fn dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()> {
        debug!(
            "MultiplexedGossipTransport: dialing peer {} at {}",
            peer, addr
        );

        // Get the default transport for dialing
        let transport = self
            .multiplexer
            .get_default_transport()
            .await
            .ok_or_else(|| anyhow!("No default transport configured"))?;

        // Dial using the adapter
        let connected_peer = transport.dial(addr).await.map_err(|e| anyhow!("{}", e))?;

        // Verify we connected to the expected peer
        if connected_peer != peer {
            warn!(
                "Connected to peer {} at {} but expected {}",
                connected_peer, addr, peer
            );
            return Err(anyhow!(
                "Connected to unexpected peer {} when dialing {}",
                connected_peer,
                peer
            ));
        }

        info!("Successfully dialed peer {} at {}", peer, addr);
        Ok(())
    }

    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId> {
        debug!("MultiplexedGossipTransport: dialing bootstrap at {}", addr);

        // Get the default transport for bootstrap dialing
        let transport = self
            .multiplexer
            .get_default_transport()
            .await
            .ok_or_else(|| anyhow!("No default transport configured"))?;

        // Dial and return the peer ID
        let peer_id = transport.dial(addr).await.map_err(|e| anyhow!("{}", e))?;

        info!(
            "Successfully connected to bootstrap at {} (peer: {})",
            addr, peer_id
        );
        Ok(peer_id)
    }

    async fn listen(&self, bind: SocketAddr) -> Result<()> {
        // For multiplexed transport, listening is handled by the individual transports
        // during their initialization. This is a no-op for most transports.
        debug!("MultiplexedGossipTransport: listen called for {} (no-op, handled by individual transports)", bind);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        info!("MultiplexedGossipTransport: closing all transports");

        // Get all registered transports and close them
        let descriptors = self.multiplexer.available_transports().await;

        for descriptor in descriptors {
            if let Some(transport) = self.multiplexer.get_transport(&descriptor).await {
                if let Err(e) = transport.close().await {
                    warn!("Error closing {:?} transport: {}", descriptor, e);
                }
            }
        }

        info!("All transports closed");
        Ok(())
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        debug!(
            "MultiplexedGossipTransport: sending {} bytes to {} on {:?}",
            data.len(),
            peer,
            stream_type
        );

        // Select the appropriate transport based on stream type
        let transport = self
            .multiplexer
            .select_transport_for_stream(stream_type)
            .await
            .map_err(|e| anyhow!("{}", e))?;

        // Send using the selected transport
        transport
            .send(peer, stream_type, data)
            .await
            .map_err(|e| anyhow!("{}", e))?;

        Ok(())
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        // Receive from the default transport
        // Future enhancement: multiplex receive from all transports
        let transport = self
            .multiplexer
            .get_default_transport()
            .await
            .ok_or_else(|| anyhow!("No default transport configured"))?;

        let (peer_id, stream_type, data) = transport.recv().await.map_err(|e| anyhow!("{}", e))?;

        debug!(
            "MultiplexedGossipTransport: received {} bytes from {} on {:?}",
            data.len(),
            peer_id,
            stream_type
        );

        Ok((peer_id, stream_type, data))
    }

    async fn send_with_request(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
        request: &TransportRequest,
    ) -> Result<()> {
        debug!(
            "MultiplexedGossipTransport: sending {} bytes to {} on {:?} with request {:?}",
            data.len(),
            peer,
            stream_type,
            request
        );

        // Try to select transport based on capability requirements
        let transport = match self.multiplexer.select_transport(request).await {
            Ok(t) => t,
            Err(e) => {
                // Fall back to stream-type based routing if no transport matches requirements
                debug!(
                    "No transport matches request {:?}, falling back to stream routing: {}",
                    request, e
                );
                self.multiplexer
                    .select_transport_for_stream(stream_type)
                    .await
                    .map_err(|e| anyhow!("{}", e))?
            }
        };

        // Send using the selected transport
        transport
            .send(peer, stream_type, data)
            .await
            .map_err(|e| anyhow!("{}", e))?;

        Ok(())
    }
}

impl std::fmt::Debug for MultiplexedGossipTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiplexedGossipTransport")
            .field("local_peer_id", &self.local_peer_id)
            .finish_non_exhaustive()
    }
}

#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TransportResult;
    use crate::TransportCapabilities;
    use async_trait::async_trait;

    // ========================================================================
    // Mock Transport Adapter for Testing
    // ========================================================================

    struct MockTransportAdapter {
        peer_id: PeerId,
        capabilities: TransportCapabilities,
    }

    impl MockTransportAdapter {
        fn new_with_peer_id(peer_id: PeerId) -> Self {
            Self {
                peer_id,
                capabilities: TransportCapabilities {
                    supports_broadcast: false,
                    max_message_size: 100 * 1024 * 1024,
                    typical_latency_ms: 50,
                    is_reliable: true,
                    name: "MockUDP",
                },
            }
        }
    }

    #[async_trait]
    impl TransportAdapter for MockTransportAdapter {
        fn local_peer_id(&self) -> PeerId {
            self.peer_id
        }

        async fn dial(&self, _addr: std::net::SocketAddr) -> TransportResult<PeerId> {
            // Return a known peer ID for testing
            Ok(PeerId::new([42u8; 32]))
        }

        async fn send(
            &self,
            _peer_id: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> TransportResult<()> {
            Ok(())
        }

        async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)> {
            Ok((
                PeerId::new([99u8; 32]),
                GossipStreamType::Membership,
                Bytes::from("test message"),
            ))
        }

        async fn close(&self) -> TransportResult<()> {
            Ok(())
        }

        async fn connected_peers(&self) -> Vec<(PeerId, std::net::SocketAddr)> {
            vec![]
        }

        fn capabilities(&self) -> TransportCapabilities {
            self.capabilities.clone()
        }
    }

    // ========================================================================
    // Basic Struct Tests
    // ========================================================================

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

    // ========================================================================
    // from_adapter Builder Tests
    // ========================================================================

    #[tokio::test]
    async fn test_from_adapter_creates_transport() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        assert_eq!(transport.local_peer_id(), peer_id);
    }

    #[tokio::test]
    async fn test_from_adapter_sets_default_transport() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // The default transport should be set
        let default = transport.multiplexer().get_default_transport().await;
        assert!(default.is_some());
    }

    // ========================================================================
    // GossipTransport Trait Method Tests
    // ========================================================================

    #[tokio::test]
    async fn test_dial_bootstrap_uses_default_transport() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // dial_bootstrap should work and return a peer ID
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let result = transport.dial_bootstrap(addr).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PeerId::new([42u8; 32])); // From mock
    }

    #[tokio::test]
    async fn test_dial_verifies_peer_id() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // dial should fail if expected peer doesn't match connected peer
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let wrong_peer = PeerId::new([99u8; 32]); // Different from mock's [42u8; 32]
        let result = transport.dial(wrong_peer, addr).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_dial_succeeds_with_matching_peer() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // dial should succeed if expected peer matches mock's returned peer
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let expected_peer = PeerId::new([42u8; 32]); // Matches mock's dial return
        let result = transport.dial(expected_peer, addr).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_listen_is_noop() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // listen should succeed (it's a no-op)
        let addr: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
        let result = transport.listen(addr).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_close_succeeds() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // close should succeed
        let result = transport.close().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_peer_routes_by_stream_type() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // send_to_peer should route through the multiplexer
        let target_peer = PeerId::new([50u8; 32]);
        let data = Bytes::from("test data");
        let result = transport
            .send_to_peer(target_peer, GossipStreamType::Membership, data)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_receive_message_from_default_transport() {
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // receive_message should return data from the mock
        let result = transport.receive_message().await;
        assert!(result.is_ok());
        let (recv_peer_id, stream_type, data) = result.unwrap();
        assert_eq!(recv_peer_id, PeerId::new([99u8; 32])); // From mock's recv
        assert_eq!(stream_type, GossipStreamType::Membership);
        assert_eq!(&data[..], b"test message");
    }

    // ========================================================================
    // Error Case Tests
    // ========================================================================

    #[tokio::test]
    async fn test_dial_fails_without_default_transport() {
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));
        let transport = MultiplexedGossipTransport::new(multiplexer, peer_id);

        // dial should fail when no default transport is configured
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let result = transport.dial(peer_id, addr).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No default transport"));
    }

    #[tokio::test]
    async fn test_receive_fails_without_default_transport() {
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));
        let transport = MultiplexedGossipTransport::new(multiplexer, peer_id);

        // receive should fail when no default transport is configured
        let result = transport.receive_message().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No default transport"));
    }

    // ========================================================================
    // Integration Tests for Multi-Transport Scenarios
    // ========================================================================

    /// Mock BLE transport adapter for testing multi-transport routing
    struct MockBleTransportAdapter {
        peer_id: PeerId,
        closed: std::sync::atomic::AtomicBool,
    }

    impl MockBleTransportAdapter {
        fn new_with_peer_id(peer_id: PeerId) -> Self {
            Self {
                peer_id,
                closed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn is_closed(&self) -> bool {
            self.closed.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TransportAdapter for MockBleTransportAdapter {
        fn local_peer_id(&self) -> PeerId {
            self.peer_id
        }

        async fn dial(&self, _addr: std::net::SocketAddr) -> TransportResult<PeerId> {
            Ok(PeerId::new([77u8; 32])) // Different from UDP mock
        }

        async fn send(
            &self,
            _peer_id: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> TransportResult<()> {
            Ok(())
        }

        async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)> {
            Ok((
                PeerId::new([88u8; 32]),
                GossipStreamType::PubSub,
                Bytes::from("ble message"),
            ))
        }

        async fn close(&self) -> TransportResult<()> {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn connected_peers(&self) -> Vec<(PeerId, std::net::SocketAddr)> {
            vec![]
        }

        fn capabilities(&self) -> TransportCapabilities {
            TransportCapabilities {
                supports_broadcast: true, // BLE can do local broadcast
                max_message_size: 512,    // BLE has smaller MTU
                typical_latency_ms: 10,   // Low latency
                is_reliable: false,
                name: "MockBLE",
            }
        }
    }

    /// Trackable mock adapter to verify close() is called
    struct TrackableMockTransportAdapter {
        inner: MockTransportAdapter,
        closed: std::sync::atomic::AtomicBool,
    }

    impl TrackableMockTransportAdapter {
        fn new_with_peer_id(peer_id: PeerId) -> Self {
            Self {
                inner: MockTransportAdapter::new_with_peer_id(peer_id),
                closed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn is_closed(&self) -> bool {
            self.closed.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TransportAdapter for TrackableMockTransportAdapter {
        fn local_peer_id(&self) -> PeerId {
            self.inner.local_peer_id()
        }

        async fn dial(&self, addr: std::net::SocketAddr) -> TransportResult<PeerId> {
            self.inner.dial(addr).await
        }

        async fn send(
            &self,
            peer_id: PeerId,
            stream_type: GossipStreamType,
            data: Bytes,
        ) -> TransportResult<()> {
            self.inner.send(peer_id, stream_type, data).await
        }

        async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)> {
            self.inner.recv().await
        }

        async fn close(&self) -> TransportResult<()> {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn connected_peers(&self) -> Vec<(PeerId, std::net::SocketAddr)> {
            self.inner.connected_peers().await
        }

        fn capabilities(&self) -> TransportCapabilities {
            self.inner.capabilities()
        }
    }

    #[tokio::test]
    async fn test_multi_transport_routing() {
        // Test that multiple transports can be registered and used
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));

        // Register UDP transport
        let udp_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));
        multiplexer
            .register_transport(TransportDescriptor::Udp, udp_adapter)
            .await
            .expect("UDP registration should succeed");

        // Register BLE transport
        let ble_adapter = Arc::new(MockBleTransportAdapter::new_with_peer_id(peer_id));
        multiplexer
            .register_transport(TransportDescriptor::Ble, ble_adapter)
            .await
            .expect("BLE registration should succeed");

        // Set UDP as default
        multiplexer
            .set_default_transport(TransportDescriptor::Udp)
            .await
            .expect("Setting default should succeed");

        let transport = MultiplexedGossipTransport::new(multiplexer.clone(), peer_id);

        // Verify both transports are available
        let available = multiplexer.available_transports().await;
        assert_eq!(available.len(), 2);
        assert!(available.contains(&TransportDescriptor::Udp));
        assert!(available.contains(&TransportDescriptor::Ble));

        // Send should work (routes through multiplexer)
        let result = transport
            .send_to_peer(
                PeerId::new([1u8; 32]),
                GossipStreamType::Membership,
                Bytes::from("test"),
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_fallback_when_preferred_unavailable() {
        // Test that the system falls back when a preferred transport isn't available
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));

        // Only register UDP transport (no BLE)
        let udp_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));
        multiplexer
            .register_transport(TransportDescriptor::Udp, udp_adapter)
            .await
            .expect("UDP registration should succeed");

        multiplexer
            .set_default_transport(TransportDescriptor::Udp)
            .await
            .expect("Setting default should succeed");

        let transport = MultiplexedGossipTransport::new(multiplexer.clone(), peer_id);

        // Even without BLE, sending should work because it falls back to UDP
        let result = transport
            .send_to_peer(
                PeerId::new([1u8; 32]),
                GossipStreamType::PubSub,
                Bytes::from("pubsub message"),
            )
            .await;
        assert!(result.is_ok());

        // dial_bootstrap should also work with just UDP
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let result = transport.dial_bootstrap(addr).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_backward_compatibility_single_transport() {
        // Test that single-transport mode (backward compatibility) works correctly
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        // Use from_adapter to create single-transport setup
        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // All GossipTransport operations should work
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();

        // dial_bootstrap
        let peer_id_result = transport.dial_bootstrap(addr).await;
        assert!(peer_id_result.is_ok());

        // dial (with matching peer)
        let expected_peer = PeerId::new([42u8; 32]);
        let dial_result = transport.dial(expected_peer, addr).await;
        assert!(dial_result.is_ok());

        // listen (no-op)
        let listen_result = transport.listen(addr).await;
        assert!(listen_result.is_ok());

        // send_to_peer
        let send_result = transport
            .send_to_peer(
                PeerId::new([1u8; 32]),
                GossipStreamType::Bulk,
                Bytes::from("bulk data"),
            )
            .await;
        assert!(send_result.is_ok());

        // receive_message
        let recv_result = transport.receive_message().await;
        assert!(recv_result.is_ok());

        // close
        let close_result = transport.close().await;
        assert!(close_result.is_ok());
    }

    #[tokio::test]
    async fn test_close_closes_all_transports() {
        // Test that close() properly closes all registered transports
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));

        // Register trackable UDP transport
        let udp_adapter = Arc::new(TrackableMockTransportAdapter::new_with_peer_id(peer_id));
        let udp_clone = udp_adapter.clone();
        multiplexer
            .register_transport(TransportDescriptor::Udp, udp_adapter)
            .await
            .expect("UDP registration should succeed");

        // Register trackable BLE transport
        let ble_adapter = Arc::new(MockBleTransportAdapter::new_with_peer_id(peer_id));
        let ble_clone = ble_adapter.clone();
        multiplexer
            .register_transport(TransportDescriptor::Ble, ble_adapter)
            .await
            .expect("BLE registration should succeed");

        multiplexer
            .set_default_transport(TransportDescriptor::Udp)
            .await
            .expect("Setting default should succeed");

        let transport = MultiplexedGossipTransport::new(multiplexer, peer_id);

        // Verify transports are not closed yet
        assert!(!udp_clone.is_closed());
        assert!(!ble_clone.is_closed());

        // Close the multiplexed transport
        let result = transport.close().await;
        assert!(result.is_ok());

        // Verify both transports were closed
        assert!(udp_clone.is_closed());
        assert!(ble_clone.is_closed());
    }

    #[tokio::test]
    async fn test_send_routes_all_stream_types() {
        // Test that all stream types can be sent successfully
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        let target = PeerId::new([1u8; 32]);

        // Test Membership stream
        let result = transport
            .send_to_peer(
                target,
                GossipStreamType::Membership,
                Bytes::from("membership"),
            )
            .await;
        assert!(result.is_ok());

        // Test PubSub stream
        let result = transport
            .send_to_peer(target, GossipStreamType::PubSub, Bytes::from("pubsub"))
            .await;
        assert!(result.is_ok());

        // Test Bulk stream
        let result = transport
            .send_to_peer(target, GossipStreamType::Bulk, Bytes::from("bulk"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_clone_transport() {
        // Test that MultiplexedGossipTransport can be cloned and both instances work
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // Clone the transport
        let transport_clone = transport.clone();

        // Both should have the same peer ID
        assert_eq!(transport.local_peer_id(), transport_clone.local_peer_id());

        // Both should share the same multiplexer
        assert!(Arc::ptr_eq(
            transport.multiplexer(),
            transport_clone.multiplexer()
        ));

        // Both should be able to send
        let target = PeerId::new([1u8; 32]);
        let result1 = transport
            .send_to_peer(target, GossipStreamType::Membership, Bytes::from("test1"))
            .await;
        let result2 = transport_clone
            .send_to_peer(target, GossipStreamType::Membership, Bytes::from("test2"))
            .await;
        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }

    // ========================================================================
    // send_with_request Tests (Capability-Based Routing)
    // ========================================================================

    #[tokio::test]
    async fn test_send_with_low_latency_request() {
        // Test that send_with_request routes using low latency capability
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        let target = PeerId::new([1u8; 32]);
        let request = TransportRequest::low_latency_control();
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Membership,
                Bytes::from("control message"),
                &request,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_bulk_request() {
        // Test that send_with_request routes using bulk transfer capability
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        let target = PeerId::new([1u8; 32]);
        let request = TransportRequest::bulk_transfer();
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Bulk,
                Bytes::from("large payload data"),
                &request,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_request_falls_back_when_capability_unavailable() {
        // Test fallback to stream-type routing when no transport has the requested capability
        let peer_id = test_peer_id();
        let multiplexer = Arc::new(TransportMultiplexer::new(peer_id));

        // Register a transport that doesn't have OfflineReady capability
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));
        multiplexer
            .register_transport(TransportDescriptor::Udp, mock_adapter)
            .await
            .expect("registration should succeed");
        multiplexer
            .set_default_transport(TransportDescriptor::Udp)
            .await
            .expect("setting default should succeed");

        let transport = MultiplexedGossipTransport::new(multiplexer, peer_id);

        // Request offline_ready capability (not supported by mock)
        let target = PeerId::new([1u8; 32]);
        let request = TransportRequest::offline_ready();

        // Should fall back to stream-type routing and succeed
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Membership,
                Bytes::from("data"),
                &request,
            )
            .await;
        assert!(result.is_ok(), "Should fall back to default transport");
    }

    #[tokio::test]
    async fn test_send_with_request_uses_default_when_no_match() {
        // Test that send_with_request uses default transport when no capability match
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // Create a request with multiple requirements that may not be fully met
        let target = PeerId::new([1u8; 32]);
        let request = TransportRequest::new()
            .require(crate::TransportCapability::LowLatencyControl)
            .require(crate::TransportCapability::OfflineReady); // Unlikely combination

        // Should still succeed via fallback
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Membership,
                Bytes::from("data"),
                &request,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_peer_still_works() {
        // Test backward compatibility - send_to_peer should continue to work
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        let target = PeerId::new([1u8; 32]);

        // Old API should still work
        let result = transport
            .send_to_peer(target, GossipStreamType::Membership, Bytes::from("legacy"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_default_implementation_ignores_request() {
        // Test that the default trait implementation ignores the request parameter
        // This test verifies backward compatibility with non-multiplexed transports
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        // Even with an impossible request, should succeed
        let target = PeerId::new([1u8; 32]);
        let request = TransportRequest::new()
            .require(crate::TransportCapability::Broadcast)
            .exclude(TransportDescriptor::Udp)
            .exclude(TransportDescriptor::Ble)
            .exclude(TransportDescriptor::Lora);

        // Should fall back and succeed
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Membership,
                Bytes::from("fallback test"),
                &request,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_request_chainable_constructors() {
        // Test that the chainable constructors work correctly in send_with_request
        let peer_id = test_peer_id();
        let mock_adapter = Arc::new(MockTransportAdapter::new_with_peer_id(peer_id));

        let transport =
            MultiplexedGossipTransport::from_adapter(mock_adapter, TransportDescriptor::Udp)
                .await
                .expect("from_adapter should succeed");

        let target = PeerId::new([1u8; 32]);

        // Test low_latency_control with prefer
        let request = TransportRequest::low_latency_control().prefer(TransportDescriptor::Udp);
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Membership,
                Bytes::from("test"),
                &request,
            )
            .await;
        assert!(result.is_ok());

        // Test bulk_transfer with exclude
        let request = TransportRequest::bulk_transfer().exclude(TransportDescriptor::Ble);
        let result = transport
            .send_with_request(
                target,
                GossipStreamType::Bulk,
                Bytes::from("bulk test"),
                &request,
            )
            .await;
        assert!(result.is_ok());
    }
}
