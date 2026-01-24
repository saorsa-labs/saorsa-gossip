#![warn(missing_docs)]
#![deny(clippy::panic)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

//! Multi-transport adapter for Saorsa Gossip
//!
//! Provides transport abstraction with:
//! - [`TransportAdapter`] trait for pluggable transports
//! - [`TransportMultiplexer`] for capability-based routing
//! - UDP/QUIC as the default transport via [`UdpTransportAdapter`]
//! - Three control streams: `mship`, `pubsub`, `bulk`
//! - 0-RTT resumption where safe
//! - Path migration by default
//! - PQC handshake with ant-quic
//!
//! # Transport Multiplexing
//!
//! The [`TransportMultiplexer`] routes messages to appropriate transports based
//! on capability requirements. Use [`TransportRequest`] to specify routing needs:
//!
//! ```ignore
//! // Request low-latency transport for control messages
//! let request = TransportRequest::low_latency_control();
//! transport.send_with_request(peer, stream_type, data, &request).await?;
//!
//! // Request bulk transfer for large CRDT payloads
//! let request = TransportRequest::bulk_transfer();
//! transport.send_with_request(peer, stream_type, data, &request).await?;
//!
//! // Custom request with preferences and exclusions
//! let request = TransportRequest::new()
//!     .require(TransportCapability::LowLatencyControl)
//!     .prefer(TransportDescriptor::Udp)
//!     .exclude(TransportDescriptor::Lora);
//! ```
//!
//! The membership module uses `low_latency_control()` for HyParView and SWIM
//! messages, while the pubsub module uses `bulk_transfer()` for large payloads.
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

mod ble_transport_adapter;
mod error;
mod multiplexed_transport;
mod multiplexer;
mod protocol_handler;
mod udp_transport_adapter;

pub use ble_transport_adapter::{BleTransportAdapter, BleTransportAdapterConfig};
pub use error::{TransportError as GossipTransportError, TransportResult as GossipTransportResult};

pub use multiplexed_transport::MultiplexedGossipTransport;
pub use multiplexer::{
    TransportCapability, TransportDescriptor, TransportMultiplexer, TransportRegistry,
    TransportRequest,
};

pub use udp_transport_adapter::{UdpTransportAdapter, UdpTransportAdapterConfig};

// Deprecated aliases for backward compatibility
#[deprecated(since = "0.3.0", note = "Use UdpTransportAdapter instead")]
/// Deprecated alias for [`UdpTransportAdapter`].
pub type AntQuicTransport = UdpTransportAdapter;

#[deprecated(since = "0.3.0", note = "Use UdpTransportAdapterConfig instead")]
/// Deprecated alias for [`UdpTransportAdapterConfig`].
pub type AntQuicTransportConfig = UdpTransportAdapterConfig;
pub use protocol_handler::{
    BulkHandler, GossipMessage, GossipProtocolHandler, MembershipHandler, PubSubHandler,
};

// Re-export ant-quic's bootstrap cache as our peer cache
pub use ant_quic::bootstrap_cache::{
    CachedPeer, ConnectionOutcome as BootstrapConnectionOutcome,
    ConnectionStats as BootstrapConnectionStats, NatType as BootstrapNatType, PeerCapabilities,
    PeerSource, RelayPathHint,
};
pub use ant_quic::{
    BootstrapCache, BootstrapCacheConfig, BootstrapCacheConfigBuilder, CacheEvent, CacheStats,
};

// Re-export ant-quic types for convenience
pub use ant_quic as quic;
pub use ant_quic::{
    LinkError, LinkResult, PeerId as AntPeerId, ProtocolHandler, ProtocolHandlerExt,
    SharedTransport, StreamType, TransportError,
};

// Re-export ant-quic transport infrastructure (v0.20+)
// These types enable native multi-transport routing via ant-quic's native registry
// and ConnectionRouter, reducing the need for saorsa-gossip's custom multiplexer.
//
// Note: We alias ant-quic's TransportRegistry as AntTransportRegistry to avoid
// collision with saorsa-gossip's own TransportRegistry (which will be deprecated
// in Phase 3.2 once we fully migrate to ant-quic's routing).
pub use ant_quic::transport::{
    LoRaParams as AntLoRaParams, TransportAddr, TransportCapabilities as AntTransportCapabilities,
    TransportProvider, TransportRegistry as AntTransportRegistry, TransportType as AntTransportType,
};

use anyhow::Result;
use bytes::Bytes;
use error::TransportResult;
use saorsa_gossip_types::PeerId;
use std::net::SocketAddr;

// ============================================================================
// TransportAdapter - Low-level transport abstraction
// ============================================================================

/// Capabilities of a transport implementation.
///
/// This struct describes the characteristics and limitations of a specific
/// transport, allowing higher-level code to make routing decisions based on
/// transport properties.
#[derive(Debug, Clone, Default)]
pub struct TransportCapabilities {
    /// Whether this transport supports broadcast/multicast.
    pub supports_broadcast: bool,
    /// Maximum message size in bytes that can be sent atomically.
    pub max_message_size: usize,
    /// Typical one-way latency in milliseconds.
    pub typical_latency_ms: u32,
    /// Whether the transport provides reliable delivery.
    pub is_reliable: bool,
    /// Human-readable transport name (e.g., "UDP/QUIC", "BLE", "LoRa").
    pub name: &'static str,
}

/// Low-level transport adapter trait.
///
/// This trait abstracts the underlying transport mechanism (QUIC, BLE, LoRa, etc.)
/// and provides a common interface for sending and receiving gossip messages.
/// Unlike [`GossipTransport`], this trait uses [`TransportResult`] for error handling
/// and focuses on transport-level operations.
///
/// # Implementors
///
/// - [`UdpTransportAdapter`] - QUIC over UDP (default)
/// - Future: `BleTransportAdapter` - Bluetooth Low Energy
/// - Future: `LoraTransportAdapter` - LoRa radio
///
/// # Example
///
/// ```ignore
/// use saorsa_gossip_transport::{TransportAdapter, TransportCapabilities};
///
/// async fn send_message<T: TransportAdapter>(transport: &T, peer: PeerId, data: Bytes) {
///     let caps = transport.capabilities();
///     if data.len() <= caps.max_message_size {
///         transport.send(peer, GossipStreamType::Membership, data).await?;
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait TransportAdapter: Send + Sync {
    /// Returns the local peer ID for this transport.
    fn local_peer_id(&self) -> PeerId;

    /// Dials a remote address and returns the peer ID of the connected node.
    ///
    /// # Errors
    ///
    /// Returns [`GossipTransportError::DialFailed`] if the connection cannot be established.
    async fn dial(&self, addr: SocketAddr) -> TransportResult<PeerId>;

    /// Sends data to a peer with the given stream type.
    ///
    /// # Errors
    ///
    /// Returns [`GossipTransportError::SendFailed`] if the message cannot be delivered.
    async fn send(
        &self,
        peer_id: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> TransportResult<()>;

    /// Receives the next message from any connected peer.
    ///
    /// This method blocks until a message is available or an error occurs.
    ///
    /// # Errors
    ///
    /// Returns [`GossipTransportError::ReceiveFailed`] on receive errors.
    /// Returns [`GossipTransportError::Closed`] if the transport is closed.
    async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)>;

    /// Closes the transport and releases all resources.
    ///
    /// # Errors
    ///
    /// Returns an error if cleanup fails.
    async fn close(&self) -> TransportResult<()>;

    /// Returns a list of currently connected peers with their addresses.
    async fn connected_peers(&self) -> Vec<(PeerId, SocketAddr)>;

    /// Returns the capabilities of this transport.
    fn capabilities(&self) -> TransportCapabilities;
}

// Blanket implementation for Arc<T> to allow calling trait methods through Arc
#[async_trait::async_trait]
impl<T: TransportAdapter + ?Sized> TransportAdapter for std::sync::Arc<T> {
    fn local_peer_id(&self) -> PeerId {
        (**self).local_peer_id()
    }

    async fn dial(&self, addr: SocketAddr) -> TransportResult<PeerId> {
        (**self).dial(addr).await
    }

    async fn send(
        &self,
        peer_id: PeerId,
        stream_type: GossipStreamType,
        data: Bytes,
    ) -> TransportResult<()> {
        (**self).send(peer_id, stream_type, data).await
    }

    async fn recv(&self) -> TransportResult<(PeerId, GossipStreamType, Bytes)> {
        (**self).recv().await
    }

    async fn close(&self) -> TransportResult<()> {
        (**self).close().await
    }

    async fn connected_peers(&self) -> Vec<(PeerId, SocketAddr)> {
        (**self).connected_peers().await
    }

    fn capabilities(&self) -> TransportCapabilities {
        (**self).capabilities()
    }
}

// ============================================================================
// GossipStreamType
// ============================================================================

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

    /// Send data to a peer with specific transport capability requirements.
    ///
    /// This method allows specifying capability requirements for transport
    /// selection. If the requirements cannot be met, the implementation should
    /// fall back to the default transport.
    ///
    /// The default implementation ignores the request and delegates to `send_to_peer`,
    /// which routes based on stream type only. This maintains backward compatibility
    /// with implementations that don't support capability-based routing.
    ///
    /// # Arguments
    ///
    /// * `peer` - The target peer ID
    /// * `stream_type` - The stream type for the message
    /// * `data` - The message payload
    /// * `request` - Transport capability requirements
    ///
    /// # Example
    ///
    /// ```ignore
    /// use saorsa_gossip_transport::{GossipTransport, TransportRequest, TransportCapability};
    ///
    /// // Request low-latency transport for control messages
    /// let request = TransportRequest::new()
    ///     .require(TransportCapability::LowLatencyControl);
    ///
    /// transport.send_with_request(peer, stream_type, data, &request).await?;
    /// ```
    async fn send_with_request(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: bytes::Bytes,
        _request: &TransportRequest,
    ) -> Result<()> {
        // Default: ignore request, route by stream type only
        self.send_to_peer(peer, stream_type, data).await
    }
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

    async fn send_with_request(
        &self,
        peer: PeerId,
        stream_type: GossipStreamType,
        data: bytes::Bytes,
        request: &TransportRequest,
    ) -> Result<()> {
        (**self)
            .send_with_request(peer, stream_type, data, request)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
