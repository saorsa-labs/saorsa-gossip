#![warn(missing_docs)]

//! QUIC transport adapter for Saorsa Gossip
//!
//! Provides QUIC transport with:
//! - Three control streams: `mship`, `pubsub`, `bulk`
//! - 0-RTT resumption where safe
//! - Path migration by default
//! - PQC handshake with ant-quic
//!
//! # SharedTransport Integration
//!
//! This crate provides [`GossipProtocolHandler`] for use with `ant-quic`'s
//! [`SharedTransport`]. The handler processes all gossip stream types (Membership,
//! PubSub, GossipBulk) and routes them to the appropriate internal handlers.
//!
//! # Peer Caching
//!
//! This crate uses ant-quic's `BootstrapCache` for persistent peer storage
//! with epsilon-greedy selection for balanced exploration and exploitation.

mod ant_quic_transport;
mod protocol_handler;

pub use ant_quic_transport::{AntQuicTransport, AntQuicTransportConfig};
pub use protocol_handler::{
    BulkHandler, GossipMessage, GossipProtocolHandler, MembershipHandler, PubSubHandler,
};

// Re-export ant-quic's bootstrap cache as our peer cache
pub use ant_quic::{
    BootstrapCache, BootstrapCacheConfig, BootstrapCacheConfigBuilder, CacheEvent, CacheStats,
};

// Re-export ant-quic types for convenience
pub use ant_quic::{
    LinkError, LinkResult, PeerId as AntPeerId, ProtocolHandler, ProtocolHandlerExt,
    SharedTransport, StreamType, TransportError,
};

use anyhow::Result;
use saorsa_gossip_types::PeerId;
use std::net::SocketAddr;
use tokio::sync::mpsc;

/// Stream type identifiers for QUIC streams (legacy wrapper)
///
/// This type provides a simpler interface for gossip-specific stream types.
/// For the full stream type enum, use [`ant_quic::StreamType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GossipStreamType {
    /// Membership stream for HyParView+SWIM
    Membership,
    /// Pub/sub stream for Plumtree control
    PubSub,
    /// Bulk stream for payloads and CRDT deltas
    Bulk,
}

impl GossipStreamType {
    /// Decode stream type from wire format byte
    ///
    /// Returns `None` for unknown stream type values.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Membership),
            1 => Some(Self::PubSub),
            2 => Some(Self::Bulk),
            _ => None,
        }
    }

    /// Encode stream type to wire format byte
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Membership => 0,
            Self::PubSub => 1,
            Self::Bulk => 2,
        }
    }

    /// Convert to the ant-quic stream type.
    pub fn to_ant_quic(self) -> StreamType {
        match self {
            Self::Membership => StreamType::Membership,
            Self::PubSub => StreamType::PubSub,
            Self::Bulk => StreamType::GossipBulk,
        }
    }

    /// Convert from the ant-quic stream type.
    ///
    /// Returns `None` for non-gossip stream types.
    pub fn from_ant_quic(st: StreamType) -> Option<Self> {
        match st {
            StreamType::Membership => Some(Self::Membership),
            StreamType::PubSub => Some(Self::PubSub),
            StreamType::GossipBulk => Some(Self::Bulk),
            _ => None,
        }
    }
}

/// QUIC transport trait for dial/listen operations
#[async_trait::async_trait]
pub trait GossipTransport: Send + Sync {
    /// Dial a peer and establish QUIC connection
    async fn dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()>;

    /// Dial a bootstrap node directly by address (no coordinator needed)
    /// Returns the PeerId of the connected bootstrap node
    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId>;

    /// Listen on a socket address for incoming connections
    async fn listen(&self, bind: SocketAddr) -> Result<()>;

    /// Close the transport
    async fn close(&self) -> Result<()>;

    /// Send data to a specific peer on a specific stream type
    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: bytes::Bytes,
    ) -> Result<()>;

    /// Receive a message from any peer on any stream
    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, bytes::Bytes)>;
}

// Blanket implementation for Arc<T> to allow calling trait methods through Arc
#[async_trait::async_trait]
impl<T: GossipTransport + ?Sized> GossipTransport for std::sync::Arc<T> {
    async fn dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()> {
        (**self).dial(peer, addr).await
    }

    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId> {
        (**self).dial_bootstrap(addr).await
    }

    async fn listen(&self, bind: SocketAddr) -> Result<()> {
        (**self).listen(bind).await
    }

    async fn close(&self) -> Result<()> {
        (**self).close().await
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: bytes::Bytes,
    ) -> Result<()> {
        (**self).send_to_peer(peer, stream_type, data).await
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, bytes::Bytes)> {
        (**self).receive_message().await
    }
}

/// Transport configuration
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Enable 0-RTT resumption
    pub enable_0rtt: bool,
    /// Enable path migration
    pub enable_migration: bool,
    /// Maximum idle timeout in seconds
    pub max_idle_timeout: u64,
    /// Keep-alive interval in seconds
    pub keep_alive_interval: u64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            enable_0rtt: true,
            enable_migration: true,
            max_idle_timeout: 30,
            keep_alive_interval: 10,
        }
    }
}

/// Mock QUIC transport implementation (placeholder for ant-quic)
///
/// This is a testing mock that uses channels to simulate transport operations.
/// For production, use `AntQuicTransport` from `ant_quic_transport` module.
pub struct QuicTransport {
    connection_tx: mpsc::UnboundedSender<(PeerId, SocketAddr)>,
    connection_rx: mpsc::UnboundedReceiver<(PeerId, SocketAddr)>,
    send_tx: mpsc::UnboundedSender<(PeerId, GossipStreamType, bytes::Bytes)>,
    recv_tx: mpsc::UnboundedSender<(PeerId, GossipStreamType, bytes::Bytes)>,
    recv_rx: mpsc::UnboundedReceiver<(PeerId, GossipStreamType, bytes::Bytes)>,
}

impl QuicTransport {
    /// Create a new QUIC transport with the given configuration
    pub fn new(_config: TransportConfig) -> Self {
        let (connection_tx, connection_rx) = mpsc::unbounded_channel();
        let (send_tx, _send_rx) = mpsc::unbounded_channel();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        Self {
            connection_tx,
            connection_rx,
            send_tx,
            recv_tx,
            recv_rx,
        }
    }

    /// Get a receiver for incoming connections
    pub fn connection_receiver(&mut self) -> &mut mpsc::UnboundedReceiver<(PeerId, SocketAddr)> {
        &mut self.connection_rx
    }

    /// Get a sender for simulating received messages (for testing)
    pub fn get_recv_tx(&self) -> mpsc::UnboundedSender<(PeerId, GossipStreamType, bytes::Bytes)> {
        self.recv_tx.clone()
    }

    /// Get a receiver for messages (for testing)
    pub fn message_receiver(
        &mut self,
    ) -> &mut mpsc::UnboundedReceiver<(PeerId, GossipStreamType, bytes::Bytes)> {
        &mut self.recv_rx
    }
}

#[async_trait::async_trait]
impl GossipTransport for QuicTransport {
    async fn dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()> {
        self.connection_tx
            .send((peer, addr))
            .map_err(|e| anyhow::anyhow!("Failed to send connection: {}", e))?;
        Ok(())
    }

    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId> {
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
        self.connection_tx
            .send((peer_id, addr))
            .map_err(|e| anyhow::anyhow!("Failed to send connection: {}", e))?;
        Ok(peer_id)
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: bytes::Bytes,
    ) -> Result<()> {
        self.send_tx
            .send((peer, stream_type, data))
            .map_err(|e| anyhow::anyhow!("Failed to send to peer: {}", e))?;
        Ok(())
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, bytes::Bytes)> {
        self.recv_tx
            .send((
                PeerId::new([0u8; 32]),
                GossipStreamType::PubSub,
                bytes::Bytes::new(),
            ))
            .ok();
        Err(anyhow::anyhow!("No messages available"))
    }
}

/// Stream multiplexer for QUIC streams
pub struct StreamMultiplexer {
    membership_tx: mpsc::UnboundedSender<bytes::Bytes>,
    pubsub_tx: mpsc::UnboundedSender<bytes::Bytes>,
    bulk_tx: mpsc::UnboundedSender<bytes::Bytes>,
}

impl StreamMultiplexer {
    /// Create a new stream multiplexer
    pub fn new() -> (Self, StreamReceivers) {
        let (membership_tx, membership_rx) = mpsc::unbounded_channel();
        let (pubsub_tx, pubsub_rx) = mpsc::unbounded_channel();
        let (bulk_tx, bulk_rx) = mpsc::unbounded_channel();
        let mux = Self {
            membership_tx,
            pubsub_tx,
            bulk_tx,
        };
        let receivers = StreamReceivers {
            membership_rx,
            pubsub_rx,
            bulk_rx,
        };
        (mux, receivers)
    }

    /// Send data on the specified stream type
    pub fn send(&self, stream_type: GossipStreamType, data: bytes::Bytes) -> Result<()> {
        let tx = match stream_type {
            GossipStreamType::Membership => &self.membership_tx,
            GossipStreamType::PubSub => &self.pubsub_tx,
            GossipStreamType::Bulk => &self.bulk_tx,
        };
        tx.send(data)
            .map_err(|e| anyhow::anyhow!("Failed to send on {:?} stream: {}", stream_type, e))
    }
}

impl Default for StreamMultiplexer {
    fn default() -> Self {
        Self::new().0
    }
}

/// Stream receivers for each stream type
pub struct StreamReceivers {
    /// Membership stream receiver
    pub membership_rx: mpsc::UnboundedReceiver<bytes::Bytes>,
    /// Pub/sub stream receiver
    pub pubsub_rx: mpsc::UnboundedReceiver<bytes::Bytes>,
    /// Bulk stream receiver
    pub bulk_rx: mpsc::UnboundedReceiver<bytes::Bytes>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_config_defaults() {
        let config = TransportConfig::default();
        assert!(config.enable_0rtt);
        assert!(config.enable_migration);
        assert_eq!(config.max_idle_timeout, 30);
        assert_eq!(config.keep_alive_interval, 10);
    }

    #[test]
    fn test_transport_config_custom() {
        let config = TransportConfig {
            enable_0rtt: false,
            enable_migration: false,
            max_idle_timeout: 60,
            keep_alive_interval: 5,
        };
        assert!(!config.enable_0rtt);
        assert!(!config.enable_migration);
        assert_eq!(config.max_idle_timeout, 60);
        assert_eq!(config.keep_alive_interval, 5);
    }

    #[test]
    fn test_stream_type_from_byte_valid() {
        assert_eq!(
            GossipStreamType::from_byte(0),
            Some(GossipStreamType::Membership)
        );
        assert_eq!(
            GossipStreamType::from_byte(1),
            Some(GossipStreamType::PubSub)
        );
        assert_eq!(GossipStreamType::from_byte(2), Some(GossipStreamType::Bulk));
    }

    #[test]
    fn test_stream_type_from_byte_invalid() {
        assert_eq!(GossipStreamType::from_byte(3), None);
        assert_eq!(GossipStreamType::from_byte(100), None);
        assert_eq!(GossipStreamType::from_byte(255), None);
    }

    #[test]
    fn test_stream_type_to_byte() {
        assert_eq!(GossipStreamType::Membership.to_byte(), 0);
        assert_eq!(GossipStreamType::PubSub.to_byte(), 1);
        assert_eq!(GossipStreamType::Bulk.to_byte(), 2);
    }

    #[test]
    fn test_stream_type_roundtrip() {
        for stream_type in [
            GossipStreamType::Membership,
            GossipStreamType::PubSub,
            GossipStreamType::Bulk,
        ] {
            let byte = stream_type.to_byte();
            let recovered = GossipStreamType::from_byte(byte);
            assert_eq!(recovered, Some(stream_type));
        }
    }

    #[test]
    fn test_stream_type_to_ant_quic() {
        assert_eq!(
            GossipStreamType::Membership.to_ant_quic(),
            StreamType::Membership
        );
        assert_eq!(GossipStreamType::PubSub.to_ant_quic(), StreamType::PubSub);
        assert_eq!(GossipStreamType::Bulk.to_ant_quic(), StreamType::GossipBulk);
    }

    #[test]
    fn test_stream_type_from_ant_quic() {
        assert_eq!(
            GossipStreamType::from_ant_quic(StreamType::Membership),
            Some(GossipStreamType::Membership)
        );
        assert_eq!(
            GossipStreamType::from_ant_quic(StreamType::PubSub),
            Some(GossipStreamType::PubSub)
        );
        assert_eq!(
            GossipStreamType::from_ant_quic(StreamType::GossipBulk),
            Some(GossipStreamType::Bulk)
        );
        assert_eq!(GossipStreamType::from_ant_quic(StreamType::DhtQuery), None);
    }

    #[tokio::test]
    async fn test_quic_transport_creation() {
        let config = TransportConfig::default();
        let _transport = QuicTransport::new(config);
    }

    #[tokio::test]
    async fn test_transport_dial() {
        let config = TransportConfig::default();
        let transport = QuicTransport::new(config);
        let peer_id = PeerId::new([1u8; 32]);
        let addr = "127.0.0.1:8080".parse().expect("valid address");
        let result = transport.dial(peer_id, addr).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_transport_listen() {
        let config = TransportConfig::default();
        let transport = QuicTransport::new(config);
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid address");
        let result = transport.listen(addr).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_transport_close() {
        let config = TransportConfig::default();
        let transport = QuicTransport::new(config);
        let result = transport.close().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_multiplexer() {
        let (mux, mut receivers) = StreamMultiplexer::new();
        let test_data = bytes::Bytes::from("test");
        mux.send(GossipStreamType::Membership, test_data.clone())
            .ok();
        let received = receivers.membership_rx.recv().await;
        assert!(received.is_some());
        assert_eq!(received.unwrap(), test_data);
    }

    #[tokio::test]
    async fn test_stream_multiplexer_pubsub() {
        let (mux, mut receivers) = StreamMultiplexer::new();
        let test_data = bytes::Bytes::from("pubsub message");
        mux.send(GossipStreamType::PubSub, test_data.clone()).ok();
        let received = receivers.pubsub_rx.recv().await;
        assert!(received.is_some());
        assert_eq!(received.unwrap(), test_data);
    }

    #[tokio::test]
    async fn test_stream_multiplexer_bulk() {
        let (mux, mut receivers) = StreamMultiplexer::new();
        let test_data = bytes::Bytes::from("bulk data");
        mux.send(GossipStreamType::Bulk, test_data.clone()).ok();
        let received = receivers.bulk_rx.recv().await;
        assert!(received.is_some());
        assert_eq!(received.unwrap(), test_data);
    }

    #[tokio::test]
    async fn test_arc_transport_dial() {
        let config = TransportConfig::default();
        let transport = std::sync::Arc::new(QuicTransport::new(config));
        let peer_id = PeerId::new([1u8; 32]);
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("valid address");
        let result = transport.dial(peer_id, addr).await;
        assert!(result.is_ok());
    }
}
