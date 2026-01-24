//! BLE Transport Adapter Stub for Saorsa Gossip
//!
//! This module provides a simulated Bluetooth Low Energy transport for testing
//! multi-transport scenarios. It simulates the constrained characteristics of
//! BLE communication:
//!
//! - **MTU Limit**: 512 bytes maximum message size
//! - **Latency**: 50-150ms simulated round-trip time
//! - **Capabilities**: Low-latency control only (no bulk transfer)
//!
//! This is a **stub implementation** for benchmarking and testing. It does not
//! perform actual BLE communication.
//!
//! # Deprecation Notice
//!
//! This stub is deprecated. For real BLE transport, use ant-quic 0.20+ with the
//! `ble` feature enabled, which provides a production-ready BLE implementation:
//!
//! ```ignore
//! use ant_quic::transport::ble::BleTransport;
//! ```
//!
//! # Example
//!
//! ```ignore
//! use saorsa_gossip_transport::BleTransportAdapter;
//!
//! let ble = BleTransportAdapter::new(peer_id);
//! let caps = ble.capabilities();
//! assert_eq!(caps.max_message_size, 512);
//! ```

// Allow deprecated usage of TransportAdapter within this module
#![allow(deprecated)]

use crate::error::{TransportError, TransportResult};
use crate::{GossipStreamType, TransportAdapter, TransportCapabilities};
use async_trait::async_trait;
use bytes::Bytes;
use saorsa_gossip_types::PeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::debug;

/// BLE transport characteristics
const BLE_MTU: usize = 512;
const BLE_MIN_LATENCY_MS: u64 = 50;
const BLE_MAX_LATENCY_MS: u64 = 150;
const BLE_CHANNEL_CAPACITY: usize = 100;

/// Configuration for BLE transport adapter stub.
#[deprecated(
    since = "0.4.0",
    note = "Use ant_quic::transport::ble::BleConfig instead for production BLE support."
)]
#[derive(Debug, Clone)]
pub struct BleTransportAdapterConfig {
    /// Simulated minimum latency in milliseconds.
    pub min_latency_ms: u64,
    /// Simulated maximum latency in milliseconds.
    pub max_latency_ms: u64,
    /// Maximum message size (MTU).
    pub mtu: usize,
    /// Channel capacity for message queue.
    pub channel_capacity: usize,
}

impl Default for BleTransportAdapterConfig {
    fn default() -> Self {
        Self {
            min_latency_ms: BLE_MIN_LATENCY_MS,
            max_latency_ms: BLE_MAX_LATENCY_MS,
            mtu: BLE_MTU,
            channel_capacity: BLE_CHANNEL_CAPACITY,
        }
    }
}

impl BleTransportAdapterConfig {
    /// Create a new BLE configuration with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the simulated latency range.
    #[must_use]
    pub fn with_latency(mut self, min_ms: u64, max_ms: u64) -> Self {
        self.min_latency_ms = min_ms;
        self.max_latency_ms = max_ms;
        self
    }

    /// Set the maximum message size (MTU).
    #[must_use]
    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }
}

/// Simulated BLE transport adapter for testing multi-transport scenarios.
///
/// This stub does not perform actual BLE communication. Instead, it:
/// - Simulates BLE latency characteristics
/// - Enforces MTU limits
/// - Provides appropriate transport capabilities for routing tests
///
/// Use this adapter with [`TransportMultiplexer`] to test fallback behavior
/// and capability-based routing.
///
/// # Deprecation Notice
///
/// This stub is deprecated. For production BLE support, use ant-quic 0.20+
/// with the `ble` feature enabled.
///
/// [`TransportMultiplexer`]: crate::TransportMultiplexer
#[deprecated(
    since = "0.4.0",
    note = "Use ant_quic::transport::ble::BleTransport instead for production BLE support. \
            This stub remains available for testing constrained link simulation."
)]
pub struct BleTransportAdapter {
    /// Local peer ID.
    peer_id: PeerId,
    /// Configuration.
    config: BleTransportAdapterConfig,
    /// Connected peers (simulated).
    peers: Arc<RwLock<HashMap<PeerId, SocketAddr>>>,
    /// Message queue sender.
    send_tx: mpsc::Sender<(PeerId, GossipStreamType, Bytes)>,
    /// Message queue receiver.
    recv_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<(PeerId, GossipStreamType, Bytes)>>>,
    /// Statistics: messages sent.
    messages_sent: Arc<RwLock<u64>>,
    /// Statistics: messages received.
    messages_received: Arc<RwLock<u64>>,
    /// Statistics: bytes sent.
    bytes_sent: Arc<RwLock<u64>>,
}

impl BleTransportAdapter {
    /// Create a new BLE transport adapter stub with default configuration.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The local peer ID for this transport
    #[must_use]
    pub fn new(peer_id: PeerId) -> Self {
        Self::with_config(peer_id, BleTransportAdapterConfig::default())
    }

    /// Create a new BLE transport adapter stub with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The local peer ID for this transport
    /// * `config` - BLE configuration settings
    #[must_use]
    pub fn with_config(peer_id: PeerId, config: BleTransportAdapterConfig) -> Self {
        let (send_tx, recv_rx) = mpsc::channel(config.channel_capacity);

        Self {
            peer_id,
            config,
            peers: Arc::new(RwLock::new(HashMap::new())),
            send_tx,
            recv_rx: Arc::new(tokio::sync::Mutex::new(recv_rx)),
            messages_sent: Arc::new(RwLock::new(0)),
            messages_received: Arc::new(RwLock::new(0)),
            bytes_sent: Arc::new(RwLock::new(0)),
        }
    }

    /// Simulate BLE latency by sleeping for a random duration.
    async fn simulate_latency(&self) {
        let min = self.config.min_latency_ms;
        let max = self.config.max_latency_ms;
        let latency = if max > min {
            // Simple pseudo-random latency within range
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            min + (now % (max - min))
        } else {
            min
        };
        tokio::time::sleep(Duration::from_millis(latency)).await;
    }

    /// Get the number of messages sent through this transport.
    pub async fn messages_sent(&self) -> u64 {
        *self.messages_sent.read().await
    }

    /// Get the number of messages received through this transport.
    pub async fn messages_received(&self) -> u64 {
        *self.messages_received.read().await
    }

    /// Get the total bytes sent through this transport.
    pub async fn bytes_sent(&self) -> u64 {
        *self.bytes_sent.read().await
    }

    /// Inject a message into the receive queue (for testing).
    ///
    /// This allows tests to simulate receiving messages from remote peers.
    pub async fn inject_message(
        &self,
        from_peer: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> TransportResult<()> {
        self.send_tx
            .send((from_peer, stream_type, data))
            .await
            .map_err(|_| TransportError::Closed)?;

        let mut received = self.messages_received.write().await;
        *received += 1;

        Ok(())
    }
}

#[async_trait]
impl TransportAdapter for BleTransportAdapter {
    fn local_peer_id(&self) -> PeerId {
        self.peer_id
    }

    async fn dial(&self, addr: SocketAddr) -> TransportResult<PeerId> {
        // Simulate connection latency
        self.simulate_latency().await;

        // Generate a deterministic peer ID from the address (for testing)
        let mut bytes = [0u8; 32];
        let addr_bytes = format!("{}", addr);
        let hash = addr_bytes.as_bytes();
        for (i, b) in hash.iter().enumerate() {
            bytes[i % 32] ^= *b;
        }
        let remote_peer_id = PeerId::new(bytes);

        // Record as connected
        let mut peers = self.peers.write().await;
        peers.insert(remote_peer_id, addr);

        debug!(peer = ?remote_peer_id, addr = ?addr, "BLE stub: dialed peer");
        Ok(remote_peer_id)
    }

    async fn send(
        &self,
        peer_id: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> TransportResult<()> {
        // Check MTU limit
        if data.len() > self.config.mtu {
            return Err(TransportError::MtuExceeded {
                size: data.len(),
                mtu: self.config.mtu,
            });
        }

        // Simulate send latency
        self.simulate_latency().await;

        // Update statistics
        {
            let mut sent = self.messages_sent.write().await;
            *sent += 1;
        }
        {
            let mut bytes = self.bytes_sent.write().await;
            *bytes += data.len() as u64;
        }

        debug!(
            peer = ?peer_id,
            stream = ?stream_type,
            size = data.len(),
            "BLE stub: sent message"
        );

        Ok(())
    }

    async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)> {
        let mut rx = self.recv_rx.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }

    async fn close(&self) -> TransportResult<()> {
        let mut peers = self.peers.write().await;
        peers.clear();
        debug!("BLE stub: closed");
        Ok(())
    }

    async fn connected_peers(&self) -> Vec<(PeerId, SocketAddr)> {
        let peers = self.peers.read().await;
        peers.iter().map(|(id, addr)| (*id, *addr)).collect()
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            supports_broadcast: false,
            max_message_size: self.config.mtu,
            typical_latency_ms: ((self.config.min_latency_ms + self.config.max_latency_ms) / 2)
                as u32,
            is_reliable: true,
            name: "BLE (stub)",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn test_peer_id() -> PeerId {
        PeerId::new([1u8; 32])
    }

    #[test]
    fn test_ble_config_default() {
        let config = BleTransportAdapterConfig::default();
        assert_eq!(config.mtu, BLE_MTU);
        assert_eq!(config.min_latency_ms, BLE_MIN_LATENCY_MS);
        assert_eq!(config.max_latency_ms, BLE_MAX_LATENCY_MS);
    }

    #[test]
    fn test_ble_config_builder() {
        let config = BleTransportAdapterConfig::new()
            .with_latency(10, 20)
            .with_mtu(256);

        assert_eq!(config.min_latency_ms, 10);
        assert_eq!(config.max_latency_ms, 20);
        assert_eq!(config.mtu, 256);
    }

    #[tokio::test]
    async fn test_ble_adapter_creation() {
        let adapter = BleTransportAdapter::new(test_peer_id());
        assert_eq!(adapter.local_peer_id(), test_peer_id());
    }

    #[tokio::test]
    async fn test_ble_capabilities() {
        let adapter = BleTransportAdapter::new(test_peer_id());
        let caps = adapter.capabilities();

        assert_eq!(caps.max_message_size, BLE_MTU);
        assert!(!caps.supports_broadcast);
        assert!(caps.is_reliable);
        assert_eq!(caps.name, "BLE (stub)");
    }

    #[tokio::test]
    async fn test_ble_mtu_enforcement() {
        let config = BleTransportAdapterConfig::new()
            .with_latency(0, 0) // No latency for faster test
            .with_mtu(100);

        let adapter = BleTransportAdapter::with_config(test_peer_id(), config);
        let remote = PeerId::new([2u8; 32]);

        // Small message should succeed
        let small_data = Bytes::from(vec![0u8; 50]);
        let result = adapter
            .send(remote, GossipStreamType::Membership, small_data)
            .await;
        assert!(result.is_ok());

        // Large message should fail
        let large_data = Bytes::from(vec![0u8; 200]);
        let result = adapter
            .send(remote, GossipStreamType::Membership, large_data)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ble_dial() {
        let config = BleTransportAdapterConfig::new().with_latency(0, 0);
        let adapter = BleTransportAdapter::with_config(test_peer_id(), config);

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let peer_id = adapter.dial(addr).await.unwrap();

        // Should have recorded the peer
        let peers = adapter.connected_peers().await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, peer_id);
    }

    #[tokio::test]
    async fn test_ble_statistics() {
        let config = BleTransportAdapterConfig::new().with_latency(0, 0);
        let adapter = BleTransportAdapter::with_config(test_peer_id(), config);
        let remote = PeerId::new([2u8; 32]);

        assert_eq!(adapter.messages_sent().await, 0);
        assert_eq!(adapter.bytes_sent().await, 0);

        let data = Bytes::from(vec![0u8; 100]);
        adapter
            .send(remote, GossipStreamType::Membership, data)
            .await
            .unwrap();

        assert_eq!(adapter.messages_sent().await, 1);
        assert_eq!(adapter.bytes_sent().await, 100);
    }

    #[tokio::test]
    async fn test_ble_inject_message() {
        let config = BleTransportAdapterConfig::new().with_latency(0, 0);
        let adapter = BleTransportAdapter::with_config(test_peer_id(), config);
        let remote = PeerId::new([2u8; 32]);

        // Inject a message
        let data = Bytes::from("test message");
        adapter
            .inject_message(remote, GossipStreamType::Membership, data.clone())
            .await
            .unwrap();

        // Receive should get the injected message
        let (peer, stream, received) = adapter.recv().await.unwrap();
        assert_eq!(peer, remote);
        assert_eq!(stream, GossipStreamType::Membership);
        assert_eq!(received, data);
    }

    #[tokio::test]
    async fn test_ble_close() {
        let config = BleTransportAdapterConfig::new().with_latency(0, 0);
        let adapter = BleTransportAdapter::with_config(test_peer_id(), config);

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        adapter.dial(addr).await.unwrap();
        assert_eq!(adapter.connected_peers().await.len(), 1);

        adapter.close().await.unwrap();
        assert_eq!(adapter.connected_peers().await.len(), 0);
    }
}
