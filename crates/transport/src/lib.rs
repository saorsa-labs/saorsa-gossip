#![warn(missing_docs)]
#![deny(clippy::panic)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

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
mod error;
mod protocol_handler;

pub use error::{TransportError as GossipTransportError, TransportResult as GossipTransportResult};

pub use ant_quic_transport::{AntQuicTransport, AntQuicTransportConfig};
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

use anyhow::Result;
use saorsa_gossip_types::PeerId;
use std::net::SocketAddr;
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
