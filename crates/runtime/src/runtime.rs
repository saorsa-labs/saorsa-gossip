use crate::{CoordinatorClient, RendezvousClient};
use anyhow::Result;
use saorsa_gossip_groups::GroupContext;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_membership::{HyParViewMembership, Membership, MembershipConfig};
use saorsa_gossip_presence::PresenceManager;
use saorsa_gossip_pubsub::{PlumtreePubSub, PubSub, PubSubCacheConfig};
use saorsa_gossip_transport::{GossipTransport, UdpTransportAdapter, UdpTransportAdapterConfig};
use saorsa_gossip_types::PeerHealthOracle;
use saorsa_gossip_types::{PeerId, TopicId};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default bind address for the runtime (0.0.0.0:0 - bind to all interfaces, OS-assigned port)
const DEFAULT_BIND_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));

/// Configuration for the [`GossipRuntimeBuilder`].
#[derive(Clone, Debug)]
pub struct GossipRuntimeConfig {
    /// Local bind address for the QUIC transport.
    pub bind_addr: SocketAddr,
    /// Optional known peers to dial on startup.
    pub known_peers: Vec<SocketAddr>,
    /// PubSub per-topic message-cache bounds.
    pub pubsub_cache: PubSubCacheConfig,
}

impl Default for GossipRuntimeConfig {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR,
            known_peers: Vec::new(),
            pubsub_cache: PubSubCacheConfig::default(),
        }
    }
}

/// Builder for [`GossipRuntime`].
#[derive(Default)]
pub struct GossipRuntimeBuilder {
    config: GossipRuntimeConfig,
    identity: Option<MlDsaKeyPair>,
    /// Optional pre-configured transport.
    transport: Option<Arc<UdpTransportAdapter>>,
    /// X0X-0069: optional SWIM-derived peer-health oracle. When set, the
    /// runtime wires it into `PlumtreePubSub::with_health_oracle` so per-
    /// topic cooling decisions can consult global membership state.
    peer_health_oracle: Option<Arc<dyn PeerHealthOracle>>,
}

impl GossipRuntimeBuilder {
    /// Create a new builder with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the bind address for the transport.
    pub fn bind_addr(mut self, addr: SocketAddr) -> Self {
        self.config.bind_addr = addr;
        self
    }

    /// Seed the runtime with known peers to connect to on startup.
    pub fn known_peers(mut self, peers: Vec<SocketAddr>) -> Self {
        self.config.known_peers = peers;
        self
    }

    /// Configure PubSub per-topic message-cache bounds.
    pub fn pubsub_cache(mut self, cache: PubSubCacheConfig) -> Self {
        self.config.pubsub_cache = cache;
        self
    }

    /// X0X-0069: install a SWIM-derived peer-health oracle. The runtime
    /// wires this into `PlumtreePubSub::with_health_oracle` at build
    /// time so per-topic cooling decisions can consult global membership
    /// state. Typically callers pass the runtime's own SWIM detector
    /// here (the membership crate's `SwimDetector` implements
    /// `PeerHealthOracle` via the bridge in 0.5.42+).
    pub fn peer_health_oracle(mut self, oracle: Arc<dyn PeerHealthOracle>) -> Self {
        self.peer_health_oracle = Some(oracle);
        self
    }

    /// Provide an explicit ML-DSA identity instead of generating one.
    pub fn identity(mut self, identity: MlDsaKeyPair) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Provide a pre-configured transport.
    ///
    /// When set, the runtime will use the provided transport directly.
    /// If not set, a default UDP transport is created.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use saorsa_gossip_transport::UdpTransportAdapter;
    ///
    /// let transport = Arc::new(UdpTransportAdapter::new(bind_addr).await?);
    ///
    /// let runtime = GossipRuntimeBuilder::new()
    ///     .with_transport(transport)
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_transport(mut self, transport: Arc<UdpTransportAdapter>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Build the runtime.
    pub async fn build(self) -> Result<GossipRuntime> {
        let identity = match self.identity {
            Some(id) => id,
            None => MlDsaKeyPair::generate()?,
        };

        let peer_id = identity.peer_id();

        // Create the transport - either from provided transport or default UDP
        let transport: Arc<UdpTransportAdapter> = match self.transport {
            Some(transport) => transport,
            None => {
                // Create default UDP transport
                Arc::new(
                    UdpTransportAdapter::with_config(
                        UdpTransportAdapterConfig::new(
                            self.config.bind_addr,
                            self.config.known_peers,
                        ),
                        None,
                    )
                    .await?,
                )
            }
        };

        let membership_impl =
            HyParViewMembership::new(peer_id, MembershipConfig::default(), transport.clone());
        let membership: Arc<RwLock<Box<dyn Membership>>> =
            Arc::new(RwLock::new(Box::new(membership_impl)));

        let pubsub_impl = {
            let base = PlumtreePubSub::new_with_cache_config(
                peer_id,
                transport.clone(),
                identity.clone(),
                self.config.pubsub_cache,
            );
            if let Some(oracle) = self.peer_health_oracle.clone() {
                base.with_health_oracle(oracle)
            } else {
                base
            }
        };
        let pubsub: Arc<RwLock<Box<dyn PubSub>>> = Arc::new(RwLock::new(Box::new(pubsub_impl)));

        let groups = Arc::new(RwLock::new(HashMap::<String, GroupContext>::new()));
        let groups_by_topic = Arc::new(RwLock::new(HashMap::<TopicId, GroupContext>::new()));

        let presence_transport: Arc<dyn GossipTransport> = transport.clone();
        let presence = Arc::new(RwLock::new(PresenceManager::new(
            peer_id,
            presence_transport,
            groups_by_topic.clone(),
        )));

        let coordinator_transport: Arc<RwLock<Box<dyn GossipTransport>>> =
            Arc::new(RwLock::new(Box::new(transport.clone())));
        let coordinator = Arc::new(CoordinatorClient::new(
            peer_id,
            coordinator_transport,
            membership.clone(),
        ));

        let rendezvous_transport: Arc<RwLock<Box<dyn GossipTransport>>> =
            Arc::new(RwLock::new(Box::new(transport.clone())));
        let rendezvous_pubsub = pubsub.clone();
        let rendezvous = Arc::new(RendezvousClient::new(
            peer_id,
            rendezvous_transport,
            rendezvous_pubsub,
        ));

        Ok(GossipRuntime {
            identity,
            peer_id,
            transport,
            membership,
            pubsub,
            presence,
            groups,
            groups_by_topic,
            coordinator,
            rendezvous,
        })
    }
}

/// High-level Saorsa gossip runtime.
pub struct GossipRuntime {
    /// Node identity.
    pub identity: MlDsaKeyPair,
    /// Local peer-id.
    pub peer_id: PeerId,
    /// Shared transport layer.
    pub transport: Arc<UdpTransportAdapter>,
    /// Membership layer.
    pub membership: Arc<RwLock<Box<dyn Membership>>>,
    /// PubSub layer.
    pub pubsub: Arc<RwLock<Box<dyn PubSub>>>,
    /// Presence manager (group-scoped beacons).
    pub presence: Arc<RwLock<PresenceManager>>,
    /// Map of entity-id to group context.
    pub groups: Arc<RwLock<HashMap<String, GroupContext>>>,
    /// Map of topic-id to group context.
    pub groups_by_topic: Arc<RwLock<HashMap<TopicId, GroupContext>>>,
    /// Coordinator client interface.
    pub coordinator: Arc<CoordinatorClient>,
    /// Rendezvous client interface.
    pub rendezvous: Arc<RendezvousClient>,
}

impl GossipRuntime {
    /// Return the local peer id.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_config_default() {
        let config = GossipRuntimeConfig::default();
        assert_eq!(config.bind_addr.port(), 0);
        assert!(config.known_peers.is_empty());
    }

    #[test]
    fn test_runtime_builder_new() {
        let builder = GossipRuntimeBuilder::new();
        // Builder starts with defaults
        assert_eq!(builder.config.bind_addr.port(), 0);
        assert!(builder.config.known_peers.is_empty());
    }

    #[test]
    fn test_runtime_builder_bind_addr() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let builder = GossipRuntimeBuilder::new().bind_addr(addr);
        assert_eq!(builder.config.bind_addr, addr);
    }

    #[test]
    fn test_runtime_builder_known_peers() {
        let peers: Vec<SocketAddr> = vec!["127.0.0.1:10001".parse().unwrap()];
        let builder = GossipRuntimeBuilder::new().known_peers(peers.clone());
        assert_eq!(builder.config.known_peers, peers);
    }

    #[test]
    fn test_runtime_builder_pubsub_cache() {
        use std::num::NonZeroUsize;
        let cache = PubSubCacheConfig {
            max_messages_per_topic: NonZeroUsize::new(500).unwrap(),
            max_bytes_per_topic: 1_000_000,
            max_age: std::time::Duration::from_secs(300),
        };
        let builder = GossipRuntimeBuilder::new().pubsub_cache(cache);
        assert_eq!(
            builder.config.pubsub_cache.max_messages_per_topic.get(),
            500
        );
        assert_eq!(builder.config.pubsub_cache.max_bytes_per_topic, 1_000_000);
    }

    #[test]
    fn test_runtime_builder_identity() {
        let identity = MlDsaKeyPair::generate().unwrap();
        let expected_peer_id = identity.peer_id();
        let _builder = GossipRuntimeBuilder::new().identity(identity);
        // Builder accepts the identity without error
        assert_ne!(expected_peer_id.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn test_runtime_config_clone() {
        let config = GossipRuntimeConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.bind_addr, config.bind_addr);
    }

    #[tokio::test]
    async fn test_build_with_default_transport() {
        let identity = MlDsaKeyPair::generate().unwrap();
        let expected_peer_id = identity.peer_id();

        let runtime = GossipRuntimeBuilder::new().identity(identity).build().await;

        assert!(runtime.is_ok());
        let runtime = runtime.unwrap();
        assert_eq!(runtime.peer_id(), expected_peer_id);
    }

    #[tokio::test]
    async fn test_build_with_custom_bind_addr() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let runtime = GossipRuntimeBuilder::new().bind_addr(addr).build().await;

        assert!(runtime.is_ok());
    }

    #[tokio::test]
    async fn test_build_with_known_peers() {
        let peers: Vec<SocketAddr> = vec!["127.0.0.1:10001".parse().unwrap()];
        let runtime = GossipRuntimeBuilder::new().known_peers(peers).build().await;

        assert!(runtime.is_ok());
    }

    #[tokio::test]
    async fn test_build_with_pubsub_cache() {
        use std::num::NonZeroUsize;
        let cache = PubSubCacheConfig {
            max_messages_per_topic: NonZeroUsize::new(100).unwrap(),
            max_bytes_per_topic: 500_000,
            max_age: std::time::Duration::from_secs(60),
        };
        let runtime = GossipRuntimeBuilder::new()
            .pubsub_cache(cache)
            .build()
            .await;

        assert!(runtime.is_ok());
    }

    #[tokio::test]
    async fn test_build_runtime_components_exist() {
        let runtime = GossipRuntimeBuilder::new().build().await.unwrap();

        // All components should be accessible
        assert!(!runtime.peer_id.as_bytes().is_empty());
        assert!(runtime.groups.read().await.is_empty());
        assert!(runtime.groups_by_topic.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_build_runtime_peer_id_accessor() {
        let identity = MlDsaKeyPair::generate().unwrap();
        let expected = identity.peer_id();
        let runtime = GossipRuntimeBuilder::new()
            .identity(identity)
            .build()
            .await
            .unwrap();

        assert_eq!(runtime.peer_id(), expected);
    }
}
