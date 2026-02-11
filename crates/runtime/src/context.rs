//! GossipContext - Simplified configuration for the gossip runtime.
//!
//! This module provides [`GossipContext`], a high-level configuration builder that
//! simplifies setting up a gossip runtime with transport configuration. It handles
//! the complexity of creating transport adapters automatically.
//!
//! # Example
//!
//! ```ignore
//! use std::net::SocketAddr;
//! use saorsa_gossip_runtime::GossipContext;
//!
//! // Simple setup with defaults
//! let runtime = GossipContext::with_defaults()
//!     .with_bind_addr("0.0.0.0:10000".parse().unwrap())
//!     .build()
//!     .await?;
//!
//! // Custom UDP configuration
//! let runtime = GossipContext::new()
//!     .with_bind_addr("0.0.0.0:10000".parse().unwrap())
//!     .with_known_peers(vec!["192.168.1.1:10000".parse().unwrap()])
//!     .with_channel_capacity(20_000)
//!     .with_max_peers(500)
//!     .build()
//!     .await?;
//! ```

use anyhow::{Context, Result};
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_transport::{UdpTransportAdapter, UdpTransportAdapterConfig};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::runtime::{GossipRuntime, GossipRuntimeBuilder};

/// Central configuration structure for gossip protocol runtime.
///
/// `GossipContext` provides a simplified API for configuring the gossip runtime
/// without needing to manually create transports. It handles all the
/// wiring automatically while exposing the most commonly needed configuration options.
///
/// # Configuration Options
///
/// - **Bind address**: Where to listen for incoming connections
/// - **Known peers**: Bootstrap peers to connect to on startup
/// - **Identity**: Cryptographic identity (auto-generated if not provided)
/// - **Channel capacity**: Size of internal message buffers
/// - **Max peers**: Maximum number of concurrent peer connections
///
/// # Example
///
/// ```ignore
/// use saorsa_gossip_runtime::GossipContext;
///
/// let runtime = GossipContext::with_defaults()
///     .with_bind_addr("0.0.0.0:10000".parse()?)
///     .build()
///     .await?;
/// ```
#[derive(Clone, Debug)]
pub struct GossipContext {
    /// Address to bind the gossip instance to
    bind_addr: SocketAddr,

    /// Known peer addresses for bootstrap
    known_peers: Vec<SocketAddr>,

    /// Cryptographic identity for signing messages
    identity: Option<MlDsaKeyPair>,

    /// Capacity of message channels between runtime components
    channel_capacity: usize,

    /// Maximum number of peers this node will maintain connections to
    max_peers: usize,

    /// Stream read limit for the UDP transport
    stream_read_limit: usize,
}

impl Default for GossipContext {
    /// Creates a default GossipContext bound to 0.0.0.0:0.
    fn default() -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            known_peers: Vec::new(),
            identity: None,
            channel_capacity: 10_000,
            max_peers: 1_000,
            stream_read_limit: 100 * 1024 * 1024, // 100 MB
        }
    }
}

impl GossipContext {
    /// Creates a new GossipContext with default values.
    ///
    /// Use builder methods to customize the configuration before calling [`build`](Self::build).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a GossipContext with sensible defaults for immediate use.
    ///
    /// This is equivalent to [`GossipContext::new()`](Self::new) but more explicit
    /// about the intent.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::default()
    }

    /// Sets the bind address for the transport.
    ///
    /// # Arguments
    ///
    /// * `addr` - The socket address to bind to
    #[must_use]
    pub fn with_bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Sets the list of known peers for bootstrap.
    ///
    /// These peers will be contacted on startup to join the gossip network.
    ///
    /// # Arguments
    ///
    /// * `peers` - List of peer addresses to connect to
    #[must_use]
    pub fn with_known_peers(mut self, peers: Vec<SocketAddr>) -> Self {
        self.known_peers = peers;
        self
    }

    /// Adds a single known peer for bootstrap.
    ///
    /// # Arguments
    ///
    /// * `addr` - Address of a peer to connect to
    #[must_use]
    pub fn add_known_peer(mut self, addr: SocketAddr) -> Self {
        self.known_peers.push(addr);
        self
    }

    /// Sets the cryptographic identity.
    ///
    /// If not set, a new identity will be generated automatically.
    ///
    /// # Arguments
    ///
    /// * `identity` - The ML-DSA key pair for signing messages
    #[must_use]
    pub fn with_identity(mut self, identity: MlDsaKeyPair) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Sets the capacity of internal message channels.
    ///
    /// Higher values allow more buffering but consume more memory.
    /// Default is 10,000.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Number of messages to buffer
    #[must_use]
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    /// Sets the maximum number of concurrent peer connections.
    ///
    /// Default is 1,000.
    ///
    /// # Arguments
    ///
    /// * `max` - Maximum number of peers
    #[must_use]
    pub fn with_max_peers(mut self, max: usize) -> Self {
        self.max_peers = max;
        self
    }

    /// Sets the maximum bytes to read per stream.
    ///
    /// Default is 100 MB.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum bytes per stream
    #[must_use]
    pub fn with_stream_read_limit(mut self, limit: usize) -> Self {
        self.stream_read_limit = limit;
        self
    }

    /// Returns the bind address.
    #[must_use]
    pub fn bind_addr(&self) -> &SocketAddr {
        &self.bind_addr
    }

    /// Returns the list of known peers.
    #[must_use]
    pub fn known_peers(&self) -> &[SocketAddr] {
        &self.known_peers
    }

    /// Returns the channel capacity.
    #[must_use]
    pub fn channel_capacity(&self) -> usize {
        self.channel_capacity
    }

    /// Returns the max peers setting.
    #[must_use]
    pub fn max_peers(&self) -> usize {
        self.max_peers
    }

    /// Returns the stream read limit in bytes.
    #[must_use]
    pub fn stream_read_limit(&self) -> usize {
        self.stream_read_limit
    }

    /// Builds the GossipRuntime from this context.
    ///
    /// This method creates a UDP transport with the configured settings
    /// and initializes the full gossip runtime stack.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The transport fails to bind to the specified address
    /// - Identity generation fails (if no identity was provided)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use saorsa_gossip_runtime::GossipContext;
    ///
    /// let runtime = GossipContext::with_defaults()
    ///     .with_bind_addr("0.0.0.0:10000".parse()?)
    ///     .build()
    ///     .await?;
    /// ```
    pub async fn build(self) -> Result<GossipRuntime> {
        // Create UDP transport config with context settings
        let udp_config = UdpTransportAdapterConfig::new(self.bind_addr, self.known_peers.clone())
            .with_channel_capacity(self.channel_capacity)
            .with_max_peers(self.max_peers)
            .with_stream_read_limit(self.stream_read_limit);

        // Get identity (generate if not provided)
        let identity = match self.identity {
            Some(id) => id,
            None => MlDsaKeyPair::generate().context("failed to generate identity keypair")?,
        };

        // Create UDP transport adapter
        let udp_adapter = Arc::new(
            UdpTransportAdapter::with_config(udp_config, None)
                .await
                .context("failed to create UDP transport adapter")?,
        );

        // Use the standard builder with our configured transport
        GossipRuntimeBuilder::new()
            .bind_addr(self.bind_addr)
            .known_peers(self.known_peers)
            .identity(identity)
            .with_transport(udp_adapter)
            .build()
            .await
            .context("failed to build GossipRuntime")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use saorsa_gossip_transport::GossipTransport;

    #[test]
    fn test_default_context() {
        let ctx = GossipContext::default();
        assert_eq!(ctx.channel_capacity(), 10_000);
        assert_eq!(ctx.max_peers(), 1_000);
        assert!(ctx.known_peers().is_empty());
    }

    #[test]
    fn test_new_context() {
        let ctx = GossipContext::new();
        assert_eq!(ctx.channel_capacity(), 10_000);
        assert_eq!(ctx.max_peers(), 1_000);
    }

    #[test]
    fn test_with_defaults() {
        let ctx = GossipContext::with_defaults();
        assert_eq!(ctx.channel_capacity(), 10_000);
        assert_eq!(ctx.max_peers(), 1_000);
    }

    #[test]
    fn test_builder_chain() {
        let addr: SocketAddr = "127.0.0.1:10000".parse().expect("valid addr");
        let peer_addr: SocketAddr = "127.0.0.1:10001".parse().expect("valid addr");

        let ctx = GossipContext::new()
            .with_bind_addr(addr)
            .with_known_peers(vec![peer_addr])
            .with_channel_capacity(5_000)
            .with_max_peers(500);

        assert_eq!(ctx.bind_addr(), &addr);
        assert_eq!(ctx.known_peers(), &[peer_addr]);
        assert_eq!(ctx.channel_capacity(), 5_000);
        assert_eq!(ctx.max_peers(), 500);
    }

    #[test]
    fn test_add_known_peer() {
        let peer1: SocketAddr = "127.0.0.1:10001".parse().expect("valid addr");
        let peer2: SocketAddr = "127.0.0.1:10002".parse().expect("valid addr");

        let ctx = GossipContext::new()
            .add_known_peer(peer1)
            .add_known_peer(peer2);

        assert_eq!(ctx.known_peers(), &[peer1, peer2]);
    }

    #[test]
    fn test_with_stream_read_limit() {
        let ctx = GossipContext::new().with_stream_read_limit(50 * 1024 * 1024);
        assert_eq!(ctx.stream_read_limit, 50 * 1024 * 1024);
    }

    // Integration tests for build() - require async runtime
    #[tokio::test]
    async fn test_build_creates_runtime_with_defaults() {
        // Use port 0 to let the OS assign an available port
        let runtime = GossipContext::with_defaults().build().await;
        assert!(runtime.is_ok(), "build() should succeed with defaults");

        let runtime = runtime.unwrap();
        // Verify we have a valid peer ID (non-empty)
        assert_ne!(runtime.peer_id().as_bytes(), &[0u8; 32]);
    }

    #[tokio::test]
    async fn test_build_with_custom_bind_addr() {
        // Use port 0 to get a random available port
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid addr");

        let runtime = GossipContext::new().with_bind_addr(addr).build().await;

        assert!(
            runtime.is_ok(),
            "build() should succeed with custom bind addr"
        );
    }

    #[tokio::test]
    async fn test_build_with_identity() {
        let identity = MlDsaKeyPair::generate().expect("identity generation should succeed");
        let expected_peer_id = identity.peer_id();

        let runtime = GossipContext::new().with_identity(identity).build().await;

        assert!(
            runtime.is_ok(),
            "build() should succeed with custom identity"
        );
        let runtime = runtime.unwrap();
        assert_eq!(runtime.peer_id(), expected_peer_id);
    }

    #[tokio::test]
    async fn test_build_with_custom_config() {
        let runtime = GossipContext::new()
            .with_channel_capacity(5_000)
            .with_max_peers(500)
            .with_stream_read_limit(50 * 1024 * 1024)
            .build()
            .await;

        assert!(runtime.is_ok(), "build() should succeed with custom config");
    }

    #[tokio::test]
    async fn test_build_runtime_has_transport() {
        let runtime = GossipContext::with_defaults().build().await.unwrap();

        // Verify the transport is accessible and returns valid peer IDs
        // Note: The transport has its own QUIC identity, separate from the gossip identity
        assert_ne!(runtime.transport.local_peer_id().as_bytes(), &[0u8; 32]);
        assert_ne!(runtime.peer_id().as_bytes(), &[0u8; 32]);
    }
}
