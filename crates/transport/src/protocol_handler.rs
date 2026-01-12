// Copyright (c) 2026 Saorsa Labs Limited
//
// Licensed under the MIT OR Apache-2.0 license

//! Protocol handler for gossip streams over SharedTransport.
//!
//! This module provides [`GossipProtocolHandler`] which implements the
//! [`saorsa_transport::ProtocolHandler`] trait to handle gossip protocol
//! streams (Membership, PubSub, GossipBulk) over the shared transport layer.

use async_trait::async_trait;
use bytes::Bytes;
use saorsa_transport::{PeerId, ProtocolHandler, StreamType, TransportResult};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

/// Message received from a gossip stream.
#[derive(Debug, Clone)]
pub struct GossipMessage {
    /// The peer that sent this message.
    pub peer: PeerId,
    /// The stream type this message came from.
    pub stream_type: StreamType,
    /// The message payload.
    pub data: Bytes,
}

/// Handler for gossip protocol streams.
///
/// Routes incoming streams to the appropriate internal handler based on
/// stream type: Membership, PubSub, or GossipBulk.
///
/// # Example
///
/// ```rust,ignore
/// use saorsa_gossip_transport::GossipProtocolHandler;
/// use saorsa_transport::{SharedTransport, ProtocolHandlerExt};
///
/// let (handler, rx) = GossipProtocolHandler::new();
/// transport.register_handler(handler.boxed()).await;
///
/// // Receive messages from the handler
/// while let Some(msg) = rx.recv().await {
///     match msg.stream_type {
///         StreamType::Membership => handle_membership(msg),
///         StreamType::PubSub => handle_pubsub(msg),
///         StreamType::GossipBulk => handle_bulk(msg),
///         _ => {}
///     }
/// }
/// ```
pub struct GossipProtocolHandler {
    /// Channel to send received messages to the application layer.
    message_tx: mpsc::UnboundedSender<GossipMessage>,
    /// Membership-specific handler (optional, for direct routing).
    membership_handler: Option<Arc<dyn MembershipHandler>>,
    /// PubSub-specific handler (optional, for direct routing).
    pubsub_handler: Option<Arc<dyn PubSubHandler>>,
    /// Bulk data handler (optional, for direct routing).
    bulk_handler: Option<Arc<dyn BulkHandler>>,
}

/// Trait for handling membership protocol messages.
#[async_trait]
pub trait MembershipHandler: Send + Sync {
    /// Handle a membership message from a peer.
    async fn handle_membership(&self, peer: PeerId, data: Bytes) -> TransportResult<Option<Bytes>>;
}

/// Trait for handling pubsub protocol messages.
#[async_trait]
pub trait PubSubHandler: Send + Sync {
    /// Handle a pubsub message from a peer.
    async fn handle_pubsub(&self, peer: PeerId, data: Bytes) -> TransportResult<Option<Bytes>>;
}

/// Trait for handling bulk data transfers.
#[async_trait]
pub trait BulkHandler: Send + Sync {
    /// Handle bulk data from a peer.
    async fn handle_bulk(&self, peer: PeerId, data: Bytes) -> TransportResult<Option<Bytes>>;
}

impl GossipProtocolHandler {
    /// Create a new gossip protocol handler.
    ///
    /// Returns the handler and a receiver for incoming messages.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<GossipMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handler = Self {
            message_tx: tx,
            membership_handler: None,
            pubsub_handler: None,
            bulk_handler: None,
        };
        (handler, rx)
    }

    /// Create a handler with direct routing to specific handlers.
    ///
    /// When handlers are set, messages are routed directly to them
    /// instead of going through the message channel.
    pub fn with_handlers(
        membership: Option<Arc<dyn MembershipHandler>>,
        pubsub: Option<Arc<dyn PubSubHandler>>,
        bulk: Option<Arc<dyn BulkHandler>>,
    ) -> (Self, mpsc::UnboundedReceiver<GossipMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handler = Self {
            message_tx: tx,
            membership_handler: membership,
            pubsub_handler: pubsub,
            bulk_handler: bulk,
        };
        (handler, rx)
    }

    /// Set the membership handler for direct routing.
    pub fn set_membership_handler(&mut self, handler: Arc<dyn MembershipHandler>) {
        self.membership_handler = Some(handler);
    }

    /// Set the pubsub handler for direct routing.
    pub fn set_pubsub_handler(&mut self, handler: Arc<dyn PubSubHandler>) {
        self.pubsub_handler = Some(handler);
    }

    /// Set the bulk handler for direct routing.
    pub fn set_bulk_handler(&mut self, handler: Arc<dyn BulkHandler>) {
        self.bulk_handler = Some(handler);
    }

    /// Handle a membership stream message.
    async fn handle_membership_internal(
        &self,
        peer: PeerId,
        data: Bytes,
    ) -> TransportResult<Option<Bytes>> {
        trace!(peer = ?peer, len = data.len(), "handling membership message");

        // If we have a direct handler, use it
        if let Some(ref handler) = self.membership_handler {
            return handler.handle_membership(peer, data).await;
        }

        // Otherwise, send to the channel
        self.send_message(peer, StreamType::Membership, data);
        Ok(None)
    }

    /// Handle a pubsub stream message.
    async fn handle_pubsub_internal(
        &self,
        peer: PeerId,
        data: Bytes,
    ) -> TransportResult<Option<Bytes>> {
        trace!(peer = ?peer, len = data.len(), "handling pubsub message");

        // If we have a direct handler, use it
        if let Some(ref handler) = self.pubsub_handler {
            return handler.handle_pubsub(peer, data).await;
        }

        // Otherwise, send to the channel
        self.send_message(peer, StreamType::PubSub, data);
        Ok(None)
    }

    /// Handle a bulk stream message.
    async fn handle_bulk_internal(
        &self,
        peer: PeerId,
        data: Bytes,
    ) -> TransportResult<Option<Bytes>> {
        trace!(peer = ?peer, len = data.len(), "handling bulk message");

        // If we have a direct handler, use it
        if let Some(ref handler) = self.bulk_handler {
            return handler.handle_bulk(peer, data).await;
        }

        // Otherwise, send to the channel
        self.send_message(peer, StreamType::GossipBulk, data);
        Ok(None)
    }

    /// Send a message to the channel.
    fn send_message(&self, peer: PeerId, stream_type: StreamType, data: Bytes) {
        let msg = GossipMessage {
            peer,
            stream_type,
            data,
        };
        if let Err(e) = self.message_tx.send(msg) {
            warn!("failed to send gossip message to channel: {}", e);
        }
    }
}

impl Default for GossipProtocolHandler {
    fn default() -> Self {
        Self::new().0
    }
}

#[async_trait]
impl ProtocolHandler for GossipProtocolHandler {
    fn stream_types(&self) -> &[StreamType] {
        // Handle all three gossip stream types
        StreamType::gossip_types()
    }

    async fn handle_stream(
        &self,
        peer: PeerId,
        stream_type: StreamType,
        data: Bytes,
    ) -> TransportResult<Option<Bytes>> {
        debug!(
            peer = ?peer,
            stream_type = %stream_type,
            len = data.len(),
            "received gossip stream"
        );

        match stream_type {
            StreamType::Membership => self.handle_membership_internal(peer, data).await,
            StreamType::PubSub => self.handle_pubsub_internal(peer, data).await,
            StreamType::GossipBulk => self.handle_bulk_internal(peer, data).await,
            _ => {
                warn!(
                    stream_type = %stream_type,
                    "received unexpected stream type in gossip handler"
                );
                Ok(None)
            }
        }
    }

    async fn handle_datagram(
        &self,
        peer: PeerId,
        stream_type: StreamType,
        data: Bytes,
    ) -> TransportResult<()> {
        // For datagrams, we just send to the channel without expecting a response
        trace!(
            peer = ?peer,
            stream_type = %stream_type,
            len = data.len(),
            "received gossip datagram"
        );

        self.send_message(peer, stream_type, data);
        Ok(())
    }

    async fn shutdown(&self) -> TransportResult<()> {
        debug!("shutting down gossip protocol handler");
        Ok(())
    }

    fn name(&self) -> &str {
        "GossipProtocolHandler"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handler_stream_types() {
        let (handler, _rx) = GossipProtocolHandler::new();
        let types = handler.stream_types();

        assert_eq!(types.len(), 3);
        assert!(types.contains(&StreamType::Membership));
        assert!(types.contains(&StreamType::PubSub));
        assert!(types.contains(&StreamType::GossipBulk));
    }

    #[tokio::test]
    async fn test_handler_name() {
        let (handler, _rx) = GossipProtocolHandler::new();
        assert_eq!(handler.name(), "GossipProtocolHandler");
    }

    #[tokio::test]
    async fn test_handle_membership() {
        let (handler, mut rx) = GossipProtocolHandler::new();
        let peer = PeerId::from([1u8; 32]);
        let data = Bytes::from_static(b"membership data");

        let result = handler
            .handle_stream(peer, StreamType::Membership, data.clone())
            .await;
        assert!(result.is_ok());

        let msg = rx.recv().await.expect("should receive message");
        assert_eq!(msg.peer, peer);
        assert_eq!(msg.stream_type, StreamType::Membership);
        assert_eq!(msg.data, data);
    }

    #[tokio::test]
    async fn test_handle_pubsub() {
        let (handler, mut rx) = GossipProtocolHandler::new();
        let peer = PeerId::from([2u8; 32]);
        let data = Bytes::from_static(b"pubsub data");

        let result = handler
            .handle_stream(peer, StreamType::PubSub, data.clone())
            .await;
        assert!(result.is_ok());

        let msg = rx.recv().await.expect("should receive message");
        assert_eq!(msg.peer, peer);
        assert_eq!(msg.stream_type, StreamType::PubSub);
        assert_eq!(msg.data, data);
    }

    #[tokio::test]
    async fn test_handle_bulk() {
        let (handler, mut rx) = GossipProtocolHandler::new();
        let peer = PeerId::from([3u8; 32]);
        let data = Bytes::from_static(b"bulk data");

        let result = handler
            .handle_stream(peer, StreamType::GossipBulk, data.clone())
            .await;
        assert!(result.is_ok());

        let msg = rx.recv().await.expect("should receive message");
        assert_eq!(msg.peer, peer);
        assert_eq!(msg.stream_type, StreamType::GossipBulk);
        assert_eq!(msg.data, data);
    }

    #[tokio::test]
    async fn test_handle_datagram() {
        let (handler, mut rx) = GossipProtocolHandler::new();
        let peer = PeerId::from([4u8; 32]);
        let data = Bytes::from_static(b"datagram");

        let result = handler
            .handle_datagram(peer, StreamType::Membership, data.clone())
            .await;
        assert!(result.is_ok());

        let msg = rx.recv().await.expect("should receive message");
        assert_eq!(msg.peer, peer);
        assert_eq!(msg.stream_type, StreamType::Membership);
        assert_eq!(msg.data, data);
    }

    #[tokio::test]
    async fn test_shutdown() {
        let (handler, _rx) = GossipProtocolHandler::new();
        let result = handler.shutdown().await;
        assert!(result.is_ok());
    }
}
