//! Transport multiplexer for multi-transport gossip routing.
//!
//! This module provides the infrastructure for routing gossip messages across
//! multiple transport types (UDP/QUIC, BLE, LoRa) based on capability requirements.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │            TransportMultiplexer                  │
//! │  ┌─────────────────────────────────────────┐    │
//! │  │         TransportRegistry               │    │
//! │  │  ┌───────────┐  ┌───────────┐          │    │
//! │  │  │    UDP    │  │    BLE    │  ...     │    │
//! │  │  └───────────┘  └───────────┘          │    │
//! │  └─────────────────────────────────────────┘    │
//! │                     │                           │
//! │           select_transport(request)             │
//! │                     ↓                           │
//! │         Arc<dyn TransportAdapter>               │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use saorsa_gossip_transport::{
//!     TransportMultiplexer, TransportCapability, TransportRequest,
//! };
//!
//! let mux = TransportMultiplexer::new(peer_id);
//! mux.register_transport(TransportDescriptor::Udp, udp_adapter).await?;
//!
//! // Route based on capability
//! let request = TransportRequest::new()
//!     .require(TransportCapability::LowLatencyControl);
//! let transport = mux.select_transport(&request).await?;
//! ```

use crate::error::{TransportError, TransportResult};
use crate::TransportAdapter;
use saorsa_gossip_types::PeerId;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;

// ============================================================================
// TransportCapability - Describes transport characteristics
// ============================================================================

/// Capabilities that a transport may provide.
///
/// These capabilities are used by the [`TransportMultiplexer`] to select
/// the appropriate transport for a given message type or routing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportCapability {
    /// Low-latency control plane operations.
    ///
    /// Required for:
    /// - Membership protocol messages (HyParView JOIN, NEIGHBOR)
    /// - SWIM failure detection probes
    /// - Real-time presence updates
    ///
    /// Typical requirement: < 100ms round-trip latency.
    LowLatencyControl,

    /// Bulk data transfer capability.
    ///
    /// Required for:
    /// - CRDT delta synchronization
    /// - Large payload gossip messages
    /// - File/blob transfers
    ///
    /// Typical requirement: > 1MB message support, reliable delivery.
    BulkTransfer,

    /// Broadcast/multicast capability.
    ///
    /// Required for:
    /// - Local network discovery
    /// - Multicast presence beacons
    ///
    /// Not all transports support this (e.g., QUIC is point-to-point).
    Broadcast,

    /// Offline-ready / store-and-forward capability.
    ///
    /// Required for:
    /// - Delay-tolerant networking
    /// - LoRa mesh with intermittent connectivity
    /// - Local persistence before relay
    OfflineReady,
}

impl fmt::Display for TransportCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportCapability::LowLatencyControl => write!(f, "LowLatencyControl"),
            TransportCapability::BulkTransfer => write!(f, "BulkTransfer"),
            TransportCapability::Broadcast => write!(f, "Broadcast"),
            TransportCapability::OfflineReady => write!(f, "OfflineReady"),
        }
    }
}

// ============================================================================
// TransportDescriptor - Identifies transport types
// ============================================================================

/// Descriptor identifying a specific transport type.
///
/// Each variant represents a distinct transport mechanism with its own
/// characteristics and capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TransportDescriptor {
    /// UDP/QUIC transport (default, broadband).
    ///
    /// Capabilities: [`LowLatencyControl`], [`BulkTransfer`]
    ///
    /// This is the primary transport for most gossip operations,
    /// providing reliable, low-latency communication over UDP with QUIC.
    ///
    /// [`LowLatencyControl`]: TransportCapability::LowLatencyControl
    /// [`BulkTransfer`]: TransportCapability::BulkTransfer
    Udp,

    /// Bluetooth Low Energy transport (constrained, short-range).
    ///
    /// Capabilities: [`LowLatencyControl`]
    ///
    /// Suitable for local device-to-device communication with limited
    /// bandwidth. Does not support bulk transfers due to MTU constraints.
    ///
    /// [`LowLatencyControl`]: TransportCapability::LowLatencyControl
    Ble,

    /// LoRa radio transport (very constrained, long-range).
    ///
    /// Capabilities: [`OfflineReady`]
    ///
    /// Suitable for delay-tolerant, long-range mesh networking.
    /// Very limited bandwidth, store-and-forward model.
    ///
    /// [`OfflineReady`]: TransportCapability::OfflineReady
    Lora,

    /// Custom transport with user-defined identifier.
    ///
    /// Capabilities: None by default (user must configure).
    ///
    /// Allows extension with application-specific transports.
    Custom(String),
}

impl TransportDescriptor {
    /// Returns the capabilities provided by this transport type.
    ///
    /// # Returns
    ///
    /// A set of [`TransportCapability`] values that this transport supports.
    ///
    /// # Examples
    ///
    /// ```
    /// use saorsa_gossip_transport::{TransportDescriptor, TransportCapability};
    ///
    /// let udp_caps = TransportDescriptor::Udp.capabilities();
    /// assert!(udp_caps.contains(&TransportCapability::LowLatencyControl));
    /// assert!(udp_caps.contains(&TransportCapability::BulkTransfer));
    ///
    /// let custom_caps = TransportDescriptor::Custom("my-transport".into()).capabilities();
    /// assert!(custom_caps.is_empty());
    /// ```
    #[must_use]
    pub fn capabilities(&self) -> HashSet<TransportCapability> {
        match self {
            TransportDescriptor::Udp => {
                let mut caps = HashSet::new();
                caps.insert(TransportCapability::LowLatencyControl);
                caps.insert(TransportCapability::BulkTransfer);
                caps
            }
            TransportDescriptor::Ble => {
                let mut caps = HashSet::new();
                caps.insert(TransportCapability::LowLatencyControl);
                caps
            }
            TransportDescriptor::Lora => {
                let mut caps = HashSet::new();
                caps.insert(TransportCapability::OfflineReady);
                caps
            }
            TransportDescriptor::Custom(_) => HashSet::new(),
        }
    }
}

impl fmt::Display for TransportDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportDescriptor::Udp => write!(f, "UDP/QUIC"),
            TransportDescriptor::Ble => write!(f, "BLE"),
            TransportDescriptor::Lora => write!(f, "LoRa"),
            TransportDescriptor::Custom(name) => write!(f, "Custom({})", name),
        }
    }
}

// ============================================================================
// TransportRegistry - Manages registered transports
// ============================================================================

/// Registry of available transports.
///
/// The registry maintains a collection of transport adapters indexed by their
/// descriptor type. It supports registration, deregistration, and capability-based
/// queries.
///
/// # Example
///
/// ```ignore
/// use saorsa_gossip_transport::{TransportRegistry, TransportDescriptor};
///
/// let mut registry = TransportRegistry::new();
/// registry.register(TransportDescriptor::Udp, udp_adapter)?;
/// registry.set_default(TransportDescriptor::Udp)?;
///
/// let transports = registry.find_by_capability(TransportCapability::BulkTransfer);
/// ```
#[derive(Default)]
pub struct TransportRegistry {
    /// Registered transports indexed by descriptor.
    transports: HashMap<TransportDescriptor, Arc<dyn TransportAdapter>>,
    /// The default transport to use when no specific preference is given.
    default_transport: Option<TransportDescriptor>,
}

impl TransportRegistry {
    /// Creates a new empty transport registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transports: HashMap::new(),
            default_transport: None,
        }
    }

    /// Registers a transport adapter with the given descriptor.
    ///
    /// If a transport with the same descriptor is already registered, it will
    /// be replaced.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type identifier
    /// * `transport` - The transport adapter implementation
    ///
    /// # Returns
    ///
    /// `Ok(())` on success.
    pub fn register(
        &mut self,
        descriptor: TransportDescriptor,
        transport: Arc<dyn TransportAdapter>,
    ) -> TransportResult<()> {
        self.transports.insert(descriptor, transport);
        Ok(())
    }

    /// Removes a transport from the registry.
    ///
    /// If the removed transport was the default, the default is cleared.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to remove
    ///
    /// # Returns
    ///
    /// The removed transport adapter, or `None` if not found.
    pub fn deregister(
        &mut self,
        descriptor: &TransportDescriptor,
    ) -> Option<Arc<dyn TransportAdapter>> {
        // Clear default if we're removing the default transport
        if self.default_transport.as_ref() == Some(descriptor) {
            self.default_transport = None;
        }
        self.transports.remove(descriptor)
    }

    /// Gets a transport by its descriptor.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to retrieve
    ///
    /// # Returns
    ///
    /// The transport adapter, or `None` if not registered.
    #[must_use]
    pub fn get(&self, descriptor: &TransportDescriptor) -> Option<Arc<dyn TransportAdapter>> {
        self.transports.get(descriptor).cloned()
    }

    /// Sets the default transport.
    ///
    /// The default transport is used when no specific transport is requested.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to set as default
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidConfig`] if the descriptor is not registered.
    pub fn set_default(&mut self, descriptor: TransportDescriptor) -> TransportResult<()> {
        if !self.transports.contains_key(&descriptor) {
            return Err(TransportError::InvalidConfig {
                reason: format!(
                    "Cannot set default to unregistered transport: {}",
                    descriptor
                ),
            });
        }
        self.default_transport = Some(descriptor);
        Ok(())
    }

    /// Gets the default transport.
    ///
    /// # Returns
    ///
    /// The default transport adapter, or `None` if no default is set.
    #[must_use]
    pub fn get_default(&self) -> Option<Arc<dyn TransportAdapter>> {
        self.default_transport
            .as_ref()
            .and_then(|d| self.transports.get(d).cloned())
    }

    /// Gets the default transport descriptor.
    ///
    /// # Returns
    ///
    /// The default transport descriptor, or `None` if no default is set.
    #[must_use]
    pub fn default_descriptor(&self) -> Option<&TransportDescriptor> {
        self.default_transport.as_ref()
    }

    /// Finds all transports that support a given capability.
    ///
    /// # Arguments
    ///
    /// * `capability` - The required capability
    ///
    /// # Returns
    ///
    /// A vector of (descriptor, transport) pairs for all matching transports.
    #[must_use]
    pub fn find_by_capability(
        &self,
        capability: TransportCapability,
    ) -> Vec<(TransportDescriptor, Arc<dyn TransportAdapter>)> {
        self.transports
            .iter()
            .filter(|(desc, _)| desc.capabilities().contains(&capability))
            .map(|(desc, transport)| (desc.clone(), Arc::clone(transport)))
            .collect()
    }

    /// Returns all registered transports.
    ///
    /// # Returns
    ///
    /// A vector of (descriptor, transport) pairs for all registered transports.
    #[must_use]
    pub fn all_transports(&self) -> Vec<(TransportDescriptor, Arc<dyn TransportAdapter>)> {
        self.transports
            .iter()
            .map(|(desc, transport)| (desc.clone(), Arc::clone(transport)))
            .collect()
    }

    /// Returns the number of registered transports.
    #[must_use]
    pub fn len(&self) -> usize {
        self.transports.len()
    }

    /// Returns true if no transports are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.transports.is_empty()
    }
}

// ============================================================================
// TransportMultiplexer - Routes messages to appropriate transports
// ============================================================================

/// Transport multiplexer for routing messages across multiple transports.
///
/// The multiplexer wraps a [`TransportRegistry`] and provides async-safe access
/// for registering, querying, and routing messages to the appropriate transport
/// based on capability requirements.
///
/// # Thread Safety
///
/// The multiplexer uses [`tokio::sync::RwLock`] internally, making it safe to
/// share across async tasks.
///
/// # Example
///
/// ```ignore
/// use saorsa_gossip_transport::{TransportMultiplexer, TransportDescriptor};
///
/// let mux = TransportMultiplexer::new(peer_id);
/// mux.register_transport(TransportDescriptor::Udp, udp_adapter).await?;
/// mux.set_default_transport(TransportDescriptor::Udp).await?;
///
/// let descriptors = mux.available_transports().await;
/// ```
pub struct TransportMultiplexer {
    /// The transport registry (async-safe access).
    registry: RwLock<TransportRegistry>,
    /// Local peer ID for this node.
    local_peer_id: PeerId,
}

impl TransportMultiplexer {
    /// Creates a new transport multiplexer with an empty registry.
    ///
    /// # Arguments
    ///
    /// * `local_peer_id` - The peer ID of this node
    #[must_use]
    pub fn new(local_peer_id: PeerId) -> Self {
        Self {
            registry: RwLock::new(TransportRegistry::new()),
            local_peer_id,
        }
    }

    /// Creates a new transport multiplexer with a pre-configured registry.
    ///
    /// # Arguments
    ///
    /// * `local_peer_id` - The peer ID of this node
    /// * `registry` - A pre-configured transport registry
    #[must_use]
    pub fn with_registry(local_peer_id: PeerId, registry: TransportRegistry) -> Self {
        Self {
            registry: RwLock::new(registry),
            local_peer_id,
        }
    }

    /// Returns the local peer ID.
    #[must_use]
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Registers a transport adapter.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type identifier
    /// * `transport` - The transport adapter implementation
    ///
    /// # Errors
    ///
    /// Returns an error if registration fails.
    pub async fn register_transport(
        &self,
        descriptor: TransportDescriptor,
        transport: Arc<dyn TransportAdapter>,
    ) -> TransportResult<()> {
        let mut registry = self.registry.write().await;
        registry.register(descriptor, transport)
    }

    /// Removes a transport from the multiplexer.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to remove
    ///
    /// # Returns
    ///
    /// The removed transport adapter, or `None` if not found.
    pub async fn deregister_transport(
        &self,
        descriptor: &TransportDescriptor,
    ) -> Option<Arc<dyn TransportAdapter>> {
        let mut registry = self.registry.write().await;
        registry.deregister(descriptor)
    }

    /// Sets the default transport.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to set as default
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidConfig`] if the descriptor is not registered.
    pub async fn set_default_transport(
        &self,
        descriptor: TransportDescriptor,
    ) -> TransportResult<()> {
        let mut registry = self.registry.write().await;
        registry.set_default(descriptor)
    }

    /// Returns the descriptors of all available transports.
    pub async fn available_transports(&self) -> Vec<TransportDescriptor> {
        let registry = self.registry.read().await;
        registry
            .all_transports()
            .into_iter()
            .map(|(desc, _)| desc)
            .collect()
    }

    /// Returns the number of registered transports.
    pub async fn transport_count(&self) -> usize {
        let registry = self.registry.read().await;
        registry.len()
    }

    /// Returns true if no transports are registered.
    pub async fn is_empty(&self) -> bool {
        let registry = self.registry.read().await;
        registry.is_empty()
    }

    /// Gets the default transport.
    ///
    /// # Returns
    ///
    /// The default transport adapter, or `None` if no default is set.
    pub async fn get_default_transport(&self) -> Option<Arc<dyn TransportAdapter>> {
        let registry = self.registry.read().await;
        registry.get_default()
    }

    /// Gets a transport by its descriptor.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to retrieve
    ///
    /// # Returns
    ///
    /// The transport adapter, or `None` if not registered.
    pub async fn get_transport(
        &self,
        descriptor: &TransportDescriptor,
    ) -> Option<Arc<dyn TransportAdapter>> {
        let registry = self.registry.read().await;
        registry.get(descriptor)
    }

    /// Selects a transport based on a routing request.
    ///
    /// The selection algorithm:
    /// 1. If a preferred descriptor is set and it's registered and has all required
    ///    capabilities, return it.
    /// 2. Otherwise, find all transports that have all required capabilities
    ///    (excluding any in the exclude set).
    /// 3. If the default transport qualifies, prefer it.
    /// 4. Otherwise, return the first qualifying transport.
    ///
    /// # Arguments
    ///
    /// * `request` - The transport routing request with capability requirements
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidConfig`] if no transport matches the request.
    pub async fn select_transport(
        &self,
        request: &TransportRequest,
    ) -> TransportResult<Arc<dyn TransportAdapter>> {
        let registry = self.registry.read().await;

        // Check preferred transport first
        if let Some(preferred) = &request.preferred_descriptor {
            if !request.exclude_descriptors.contains(preferred) {
                if let Some(transport) = registry.get(preferred) {
                    let caps = preferred.capabilities();
                    if request.required_capabilities.is_subset(&caps) {
                        tracing::debug!("Selected preferred transport: {} for request", preferred);
                        return Ok(transport);
                    }
                }
            }
        }

        // Find all transports with required capabilities
        let candidates: Vec<_> = registry
            .all_transports()
            .into_iter()
            .filter(|(desc, _)| {
                // Not excluded
                !request.exclude_descriptors.contains(desc)
                    // Has all required capabilities
                    && request.required_capabilities.is_subset(&desc.capabilities())
            })
            .collect();

        if candidates.is_empty() {
            return Err(TransportError::InvalidConfig {
                reason: format!(
                    "No transport matches request: required={:?}, excluded={:?}",
                    request.required_capabilities, request.exclude_descriptors
                ),
            });
        }

        // Prefer default transport if it qualifies
        if let Some(default_desc) = registry.default_descriptor() {
            for (desc, transport) in &candidates {
                if desc == default_desc {
                    tracing::debug!("Selected default transport: {} for request", desc);
                    return Ok(Arc::clone(transport));
                }
            }
        }

        // Return first qualifying transport
        let (desc, transport) =
            candidates
                .into_iter()
                .next()
                .ok_or_else(|| TransportError::InvalidConfig {
                    reason: "No transport found (unexpected)".to_string(),
                })?;

        tracing::debug!("Selected transport: {} for request", desc);
        Ok(transport)
    }

    /// Selects a transport appropriate for a given stream type.
    ///
    /// This is a convenience method that maps stream types to capability requirements:
    /// - `Membership` → requires [`LowLatencyControl`]
    /// - `PubSub` → requires [`LowLatencyControl`]
    /// - `Bulk` → requires [`BulkTransfer`]
    ///
    /// # Arguments
    ///
    /// * `stream_type` - The gossip stream type
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidConfig`] if no transport matches.
    ///
    /// [`LowLatencyControl`]: TransportCapability::LowLatencyControl
    /// [`BulkTransfer`]: TransportCapability::BulkTransfer
    pub async fn select_transport_for_stream(
        &self,
        stream_type: crate::GossipStreamType,
    ) -> TransportResult<Arc<dyn TransportAdapter>> {
        let request = match stream_type {
            crate::GossipStreamType::Membership => {
                TransportRequest::new().require(TransportCapability::LowLatencyControl)
            }
            crate::GossipStreamType::PubSub => {
                TransportRequest::new().require(TransportCapability::LowLatencyControl)
            }
            crate::GossipStreamType::Bulk => {
                TransportRequest::new().require(TransportCapability::BulkTransfer)
            }
        };

        self.select_transport(&request).await
    }
}

// ============================================================================
// TransportRequest - Describes routing requirements
// ============================================================================

/// A request for selecting a transport with specific requirements.
///
/// Use the builder pattern to construct a request with capability requirements,
/// preferences, and exclusions.
///
/// # Example
///
/// ```
/// use saorsa_gossip_transport::{TransportRequest, TransportCapability, TransportDescriptor};
///
/// let request = TransportRequest::new()
///     .require(TransportCapability::LowLatencyControl)
///     .prefer(TransportDescriptor::Udp)
///     .exclude(TransportDescriptor::Lora);
/// ```
#[derive(Debug, Clone, Default)]
pub struct TransportRequest {
    /// Required capabilities that the transport must support.
    pub required_capabilities: HashSet<TransportCapability>,
    /// Preferred transport descriptor (will be used if it qualifies).
    pub preferred_descriptor: Option<TransportDescriptor>,
    /// Transport descriptors to exclude from consideration.
    pub exclude_descriptors: HashSet<TransportDescriptor>,
}

impl TransportRequest {
    /// Creates a new empty transport request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a required capability to the request.
    ///
    /// # Arguments
    ///
    /// * `capability` - The capability that the transport must support
    #[must_use]
    pub fn require(mut self, capability: TransportCapability) -> Self {
        self.required_capabilities.insert(capability);
        self
    }

    /// Sets a preferred transport descriptor.
    ///
    /// If this transport is registered and has all required capabilities,
    /// it will be selected over others.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The preferred transport type
    #[must_use]
    pub fn prefer(mut self, descriptor: TransportDescriptor) -> Self {
        self.preferred_descriptor = Some(descriptor);
        self
    }

    /// Excludes a transport descriptor from consideration.
    ///
    /// # Arguments
    ///
    /// * `descriptor` - The transport type to exclude
    #[must_use]
    pub fn exclude(mut self, descriptor: TransportDescriptor) -> Self {
        self.exclude_descriptors.insert(descriptor);
        self
    }

    /// Creates a request for low-latency control messages.
    ///
    /// Used by the membership protocol for probes, joins, heartbeats,
    /// and other latency-sensitive control plane messages.
    ///
    /// # Example
    ///
    /// ```
    /// use saorsa_gossip_transport::TransportRequest;
    ///
    /// let request = TransportRequest::low_latency_control();
    /// ```
    #[must_use]
    pub fn low_latency_control() -> Self {
        Self::new().require(TransportCapability::LowLatencyControl)
    }

    /// Creates a request for bulk data transfer.
    ///
    /// Used by pubsub for CRDT deltas, large messages, and other
    /// bandwidth-intensive operations where throughput is prioritized
    /// over latency.
    ///
    /// # Example
    ///
    /// ```
    /// use saorsa_gossip_transport::TransportRequest;
    ///
    /// let request = TransportRequest::bulk_transfer();
    /// ```
    #[must_use]
    pub fn bulk_transfer() -> Self {
        Self::new().require(TransportCapability::BulkTransfer)
    }

    /// Creates a request for offline-ready message delivery.
    ///
    /// Used for delay-tolerant networking scenarios where messages
    /// may need to be stored and forwarded later, such as LoRa mesh
    /// with intermittent connectivity.
    ///
    /// # Example
    ///
    /// ```
    /// use saorsa_gossip_transport::TransportRequest;
    ///
    /// let request = TransportRequest::offline_ready();
    /// ```
    #[must_use]
    pub fn offline_ready() -> Self {
        Self::new().require(TransportCapability::OfflineReady)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::TransportResult;
    use crate::{GossipStreamType, TransportCapabilities};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::net::SocketAddr;

    // ========================================================================
    // Mock Transport Adapter
    // ========================================================================

    /// A mock transport adapter for testing.
    #[allow(dead_code)]
    struct MockTransportAdapter {
        peer_id: PeerId,
        capabilities: TransportCapabilities,
    }

    impl MockTransportAdapter {
        fn new_udp() -> Self {
            Self {
                peer_id: PeerId::new([1u8; 32]),
                capabilities: TransportCapabilities {
                    supports_broadcast: false,
                    max_message_size: 100 * 1024 * 1024,
                    typical_latency_ms: 50,
                    is_reliable: true,
                    name: "MockUDP",
                },
            }
        }

        fn new_ble() -> Self {
            Self {
                peer_id: PeerId::new([2u8; 32]),
                capabilities: TransportCapabilities {
                    supports_broadcast: false,
                    max_message_size: 512,
                    typical_latency_ms: 100,
                    is_reliable: true,
                    name: "MockBLE",
                },
            }
        }

        fn new_lora() -> Self {
            Self {
                peer_id: PeerId::new([3u8; 32]),
                capabilities: TransportCapabilities {
                    supports_broadcast: true,
                    max_message_size: 256,
                    typical_latency_ms: 5000,
                    is_reliable: false,
                    name: "MockLoRa",
                },
            }
        }
    }

    #[async_trait]
    impl TransportAdapter for MockTransportAdapter {
        fn local_peer_id(&self) -> PeerId {
            self.peer_id
        }

        async fn dial(&self, _addr: SocketAddr) -> TransportResult<PeerId> {
            Ok(PeerId::new([99u8; 32]))
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
            Ok((self.peer_id, GossipStreamType::Membership, Bytes::new()))
        }

        async fn close(&self) -> TransportResult<()> {
            Ok(())
        }

        async fn connected_peers(&self) -> Vec<(PeerId, SocketAddr)> {
            vec![]
        }

        fn capabilities(&self) -> TransportCapabilities {
            self.capabilities.clone()
        }
    }

    // ========================================================================
    // TransportCapability Tests
    // ========================================================================

    #[test]
    fn test_capability_display() {
        assert_eq!(
            TransportCapability::LowLatencyControl.to_string(),
            "LowLatencyControl"
        );
        assert_eq!(
            TransportCapability::BulkTransfer.to_string(),
            "BulkTransfer"
        );
        assert_eq!(TransportCapability::Broadcast.to_string(), "Broadcast");
        assert_eq!(
            TransportCapability::OfflineReady.to_string(),
            "OfflineReady"
        );
    }

    #[test]
    fn test_capability_equality() {
        assert_eq!(
            TransportCapability::LowLatencyControl,
            TransportCapability::LowLatencyControl
        );
        assert_ne!(
            TransportCapability::LowLatencyControl,
            TransportCapability::BulkTransfer
        );
    }

    // ========================================================================
    // TransportDescriptor Tests
    // ========================================================================

    #[test]
    fn test_descriptor_display() {
        assert_eq!(TransportDescriptor::Udp.to_string(), "UDP/QUIC");
        assert_eq!(TransportDescriptor::Ble.to_string(), "BLE");
        assert_eq!(TransportDescriptor::Lora.to_string(), "LoRa");
        assert_eq!(
            TransportDescriptor::Custom("test".into()).to_string(),
            "Custom(test)"
        );
    }

    #[test]
    fn test_descriptor_capabilities_udp() {
        let caps = TransportDescriptor::Udp.capabilities();
        assert!(caps.contains(&TransportCapability::LowLatencyControl));
        assert!(caps.contains(&TransportCapability::BulkTransfer));
        assert!(!caps.contains(&TransportCapability::Broadcast));
        assert!(!caps.contains(&TransportCapability::OfflineReady));
    }

    #[test]
    fn test_descriptor_capabilities_ble() {
        let caps = TransportDescriptor::Ble.capabilities();
        assert!(caps.contains(&TransportCapability::LowLatencyControl));
        assert!(!caps.contains(&TransportCapability::BulkTransfer));
    }

    #[test]
    fn test_descriptor_capabilities_lora() {
        let caps = TransportDescriptor::Lora.capabilities();
        assert!(!caps.contains(&TransportCapability::LowLatencyControl));
        assert!(caps.contains(&TransportCapability::OfflineReady));
    }

    #[test]
    fn test_descriptor_custom_empty_capabilities() {
        let caps = TransportDescriptor::Custom("my-transport".into()).capabilities();
        assert!(caps.is_empty());
    }

    // ========================================================================
    // TransportRegistry Tests
    // ========================================================================

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = TransportRegistry::new();
        let transport = Arc::new(MockTransportAdapter::new_udp());

        registry
            .register(TransportDescriptor::Udp, transport.clone())
            .unwrap();

        let retrieved = registry.get(&TransportDescriptor::Udp);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_registry_deregister() {
        let mut registry = TransportRegistry::new();
        let transport = Arc::new(MockTransportAdapter::new_udp());

        registry
            .register(TransportDescriptor::Udp, transport)
            .unwrap();
        assert_eq!(registry.len(), 1);

        let removed = registry.deregister(&TransportDescriptor::Udp);
        assert!(removed.is_some());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_registry_set_default() {
        let mut registry = TransportRegistry::new();
        let transport = Arc::new(MockTransportAdapter::new_udp());

        registry
            .register(TransportDescriptor::Udp, transport)
            .unwrap();
        registry.set_default(TransportDescriptor::Udp).unwrap();

        assert!(registry.get_default().is_some());
    }

    #[test]
    fn test_registry_set_default_unregistered_fails() {
        let mut registry = TransportRegistry::new();

        let result = registry.set_default(TransportDescriptor::Udp);
        assert!(result.is_err());
    }

    #[test]
    fn test_registry_find_by_capability() {
        let mut registry = TransportRegistry::new();
        registry
            .register(
                TransportDescriptor::Udp,
                Arc::new(MockTransportAdapter::new_udp()),
            )
            .unwrap();
        registry
            .register(
                TransportDescriptor::Ble,
                Arc::new(MockTransportAdapter::new_ble()),
            )
            .unwrap();
        registry
            .register(
                TransportDescriptor::Lora,
                Arc::new(MockTransportAdapter::new_lora()),
            )
            .unwrap();

        let low_latency = registry.find_by_capability(TransportCapability::LowLatencyControl);
        assert_eq!(low_latency.len(), 2); // UDP and BLE

        let bulk = registry.find_by_capability(TransportCapability::BulkTransfer);
        assert_eq!(bulk.len(), 1); // Only UDP

        let offline = registry.find_by_capability(TransportCapability::OfflineReady);
        assert_eq!(offline.len(), 1); // Only LoRa
    }

    #[test]
    fn test_registry_all_transports() {
        let mut registry = TransportRegistry::new();
        registry
            .register(
                TransportDescriptor::Udp,
                Arc::new(MockTransportAdapter::new_udp()),
            )
            .unwrap();
        registry
            .register(
                TransportDescriptor::Ble,
                Arc::new(MockTransportAdapter::new_ble()),
            )
            .unwrap();

        let all = registry.all_transports();
        assert_eq!(all.len(), 2);
    }

    // ========================================================================
    // TransportMultiplexer Tests
    // ========================================================================

    #[tokio::test]
    async fn test_multiplexer_creation() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        assert_eq!(mux.local_peer_id(), peer_id);
        assert!(mux.is_empty().await);
    }

    #[tokio::test]
    async fn test_multiplexer_register_transport() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();

        assert_eq!(mux.transport_count().await, 1);
        assert!(!mux.is_empty().await);
    }

    #[tokio::test]
    async fn test_select_transport_preferred() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();
        mux.register_transport(
            TransportDescriptor::Ble,
            Arc::new(MockTransportAdapter::new_ble()),
        )
        .await
        .unwrap();

        let request = TransportRequest::new()
            .require(TransportCapability::LowLatencyControl)
            .prefer(TransportDescriptor::Ble);

        let transport = mux.select_transport(&request).await.unwrap();
        // BLE is preferred and has LowLatencyControl
        assert_eq!(transport.capabilities().name, "MockBLE");
    }

    #[tokio::test]
    async fn test_select_transport_fallback() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();
        mux.set_default_transport(TransportDescriptor::Udp)
            .await
            .unwrap();

        // Request capability that only UDP has
        let request = TransportRequest::new().require(TransportCapability::BulkTransfer);

        let transport = mux.select_transport(&request).await.unwrap();
        assert_eq!(transport.capabilities().name, "MockUDP");
    }

    #[tokio::test]
    async fn test_select_transport_no_match() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Lora,
            Arc::new(MockTransportAdapter::new_lora()),
        )
        .await
        .unwrap();

        // LoRa doesn't have LowLatencyControl
        let request = TransportRequest::new().require(TransportCapability::LowLatencyControl);

        let result = mux.select_transport(&request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_select_transport_for_stream_membership() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();

        let transport = mux
            .select_transport_for_stream(GossipStreamType::Membership)
            .await
            .unwrap();
        assert_eq!(transport.capabilities().name, "MockUDP");
    }

    #[tokio::test]
    async fn test_select_transport_for_stream_bulk() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();

        let transport = mux
            .select_transport_for_stream(GossipStreamType::Bulk)
            .await
            .unwrap();
        assert_eq!(transport.capabilities().name, "MockUDP");
    }

    // ========================================================================
    // TransportRequest Tests
    // ========================================================================

    #[test]
    fn test_request_builder() {
        let request = TransportRequest::new()
            .require(TransportCapability::LowLatencyControl)
            .prefer(TransportDescriptor::Udp)
            .exclude(TransportDescriptor::Lora);

        assert!(request
            .required_capabilities
            .contains(&TransportCapability::LowLatencyControl));
        assert_eq!(request.preferred_descriptor, Some(TransportDescriptor::Udp));
        assert!(request
            .exclude_descriptors
            .contains(&TransportDescriptor::Lora));
    }

    #[test]
    fn test_request_require_multiple() {
        let request = TransportRequest::new()
            .require(TransportCapability::LowLatencyControl)
            .require(TransportCapability::BulkTransfer);

        assert_eq!(request.required_capabilities.len(), 2);
        assert!(request
            .required_capabilities
            .contains(&TransportCapability::LowLatencyControl));
        assert!(request
            .required_capabilities
            .contains(&TransportCapability::BulkTransfer));
    }

    #[tokio::test]
    async fn test_select_transport_excludes() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();
        mux.register_transport(
            TransportDescriptor::Ble,
            Arc::new(MockTransportAdapter::new_ble()),
        )
        .await
        .unwrap();

        // Both have LowLatencyControl, but exclude UDP
        let request = TransportRequest::new()
            .require(TransportCapability::LowLatencyControl)
            .exclude(TransportDescriptor::Udp);

        let transport = mux.select_transport(&request).await.unwrap();
        assert_eq!(transport.capabilities().name, "MockBLE");
    }

    #[tokio::test]
    async fn test_select_transport_default_preferred() {
        let peer_id = PeerId::new([1u8; 32]);
        let mux = TransportMultiplexer::new(peer_id);

        mux.register_transport(
            TransportDescriptor::Udp,
            Arc::new(MockTransportAdapter::new_udp()),
        )
        .await
        .unwrap();
        mux.register_transport(
            TransportDescriptor::Ble,
            Arc::new(MockTransportAdapter::new_ble()),
        )
        .await
        .unwrap();
        mux.set_default_transport(TransportDescriptor::Ble)
            .await
            .unwrap();

        // Both have LowLatencyControl, default is BLE
        let request = TransportRequest::new().require(TransportCapability::LowLatencyControl);

        let transport = mux.select_transport(&request).await.unwrap();
        assert_eq!(transport.capabilities().name, "MockBLE");
    }

    // ========================================================================
    // TransportRequest Convenience Constructor Tests
    // ========================================================================

    #[test]
    fn test_low_latency_control_constructor() {
        let request = TransportRequest::low_latency_control();

        assert_eq!(request.required_capabilities.len(), 1);
        assert!(request
            .required_capabilities
            .contains(&TransportCapability::LowLatencyControl));
        assert!(request.preferred_descriptor.is_none());
        assert!(request.exclude_descriptors.is_empty());
    }

    #[test]
    fn test_bulk_transfer_constructor() {
        let request = TransportRequest::bulk_transfer();

        assert_eq!(request.required_capabilities.len(), 1);
        assert!(request
            .required_capabilities
            .contains(&TransportCapability::BulkTransfer));
        assert!(request.preferred_descriptor.is_none());
        assert!(request.exclude_descriptors.is_empty());
    }

    #[test]
    fn test_offline_ready_constructor() {
        let request = TransportRequest::offline_ready();

        assert_eq!(request.required_capabilities.len(), 1);
        assert!(request
            .required_capabilities
            .contains(&TransportCapability::OfflineReady));
        assert!(request.preferred_descriptor.is_none());
        assert!(request.exclude_descriptors.is_empty());
    }

    #[test]
    fn test_convenience_constructors_are_chainable() {
        // Test that convenience constructors return a request that can be further configured
        let request = TransportRequest::low_latency_control()
            .prefer(TransportDescriptor::Udp)
            .exclude(TransportDescriptor::Lora);

        assert!(request
            .required_capabilities
            .contains(&TransportCapability::LowLatencyControl));
        assert_eq!(request.preferred_descriptor, Some(TransportDescriptor::Udp));
        assert!(request
            .exclude_descriptors
            .contains(&TransportDescriptor::Lora));
    }
}
