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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use saorsa_gossip_types::PeerHealth;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    struct MockOracle;

    #[async_trait::async_trait]
    impl PeerHealthOracle for MockOracle {
        async fn health_of(&self, _peer: &PeerId) -> Option<PeerHealth> {
            None
        }

        async fn request_indirect_probe(&self, _target: PeerId) {}
    }

    #[test]
    fn runtime_config_default_uses_ephemeral_bind_and_empty_peers() {
        let config = GossipRuntimeConfig::default();

        assert_eq!(config.bind_addr, DEFAULT_BIND_ADDR);
        assert!(config.known_peers.is_empty());
        let default_cache = PubSubCacheConfig::default();
        assert_eq!(
            config.pubsub_cache.max_messages_per_topic,
            default_cache.max_messages_per_topic
        );
        assert_eq!(
            config.pubsub_cache.max_bytes_per_topic,
            default_cache.max_bytes_per_topic
        );
        assert_eq!(config.pubsub_cache.max_age, default_cache.max_age);
    }

    #[test]
    fn builder_methods_store_configuration() {
        let bind_addr: SocketAddr = "127.0.0.1:12000".parse().unwrap();
        let known_peer: SocketAddr = "127.0.0.1:12001".parse().unwrap();
        let cache = PubSubCacheConfig {
            max_messages_per_topic: NonZeroUsize::new(17).unwrap(),
            max_bytes_per_topic: 4_096,
            max_age: Duration::from_secs(7),
        };
        let identity = MlDsaKeyPair::generate().unwrap();
        let expected_peer = identity.peer_id();

        let builder = GossipRuntimeBuilder::new()
            .bind_addr(bind_addr)
            .known_peers(vec![known_peer])
            .pubsub_cache(cache)
            .identity(identity);

        assert_eq!(builder.config.bind_addr, bind_addr);
        assert_eq!(builder.config.known_peers, vec![known_peer]);
        assert_eq!(
            builder.config.pubsub_cache.max_messages_per_topic,
            cache.max_messages_per_topic
        );
        assert_eq!(
            builder.config.pubsub_cache.max_bytes_per_topic,
            cache.max_bytes_per_topic
        );
        assert_eq!(builder.config.pubsub_cache.max_age, cache.max_age);
        assert_eq!(builder.identity.as_ref().unwrap().peer_id(), expected_peer);
    }

    #[test]
    fn builder_accepts_peer_health_oracle() {
        let builder = GossipRuntimeBuilder::new().peer_health_oracle(Arc::new(MockOracle));
        assert!(builder.peer_health_oracle.is_some());
    }

    #[tokio::test]
    async fn build_without_explicit_identity_generates_local_identity() {
        let runtime = GossipRuntimeBuilder::new().build().await.unwrap();

        assert_eq!(runtime.peer_id(), runtime.identity.peer_id());
        assert_eq!(runtime.rendezvous.peer_id(), runtime.peer_id());
        assert!(runtime.groups.read().await.is_empty());
        assert!(runtime.groups_by_topic.read().await.is_empty());
    }

    #[tokio::test]
    async fn build_with_peer_health_oracle_wires_pubsub() {
        let runtime = GossipRuntimeBuilder::new()
            .peer_health_oracle(Arc::new(MockOracle))
            .build()
            .await
            .unwrap();

        assert_eq!(runtime.peer_id(), runtime.identity.peer_id());
        assert!(runtime
            .pubsub
            .read()
            .await
            .trigger_anti_entropy(TopicId::new([7; 32]))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn build_with_explicit_identity_wires_clients_and_empty_group_maps() {
        let identity = MlDsaKeyPair::generate().unwrap();
        let expected_peer = identity.peer_id();

        let runtime = GossipRuntimeBuilder::new()
            .identity(identity)
            .build()
            .await
            .unwrap();

        assert_eq!(runtime.peer_id(), expected_peer);
        assert_eq!(runtime.identity.peer_id(), expected_peer);
        assert_eq!(runtime.rendezvous.peer_id(), expected_peer);
        assert!(runtime.coordinator.get_cached_adverts().await.is_empty());
        assert!(runtime.groups.read().await.is_empty());
        assert!(runtime.groups_by_topic.read().await.is_empty());
    }
}
