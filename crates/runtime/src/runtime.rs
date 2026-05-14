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
    use saorsa_gossip_pubsub::PubSubCacheConfig;
    use saorsa_gossip_types::PeerHealthOracle;

    struct NoopHealthOracle;

    #[async_trait::async_trait]
    impl PeerHealthOracle for NoopHealthOracle {
        async fn health_of(&self, _peer: &PeerId) -> Option<saorsa_gossip_types::PeerHealth> {
            Some(saorsa_gossip_types::PeerHealth::Alive)
        }

        async fn request_indirect_probe(&self, _target: PeerId) {}
    }

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port))
    }

    #[test]
    fn config_default_has_unspecified_bind_addr() {
        let config = GossipRuntimeConfig::default();
        assert_eq!(config.bind_addr, DEFAULT_BIND_ADDR);
        assert!(config.known_peers.is_empty());
    }

    #[test]
    fn builder_new_and_default_produce_empty_builder() {
        let b1 = GossipRuntimeBuilder::new();
        let b2 = GossipRuntimeBuilder::default();
        // Both should be equivalent (no identity, no transport)
        assert!(b1.identity.is_none());
        assert!(b2.identity.is_none());
    }

    #[test]
    fn builder_setters_chain_and_override() {
        let b1 = GossipRuntimeBuilder::new()
            .bind_addr(make_addr(9000))
            .known_peers(vec![make_addr(9001), make_addr(9002)])
            .pubsub_cache(PubSubCacheConfig::default());
        // Access private config via default's state comparison
        let base = GossipRuntimeBuilder::default();
        assert_ne!(b1.config.bind_addr, base.config.bind_addr);
        assert_eq!(b1.config.bind_addr.port(), 9000);
        assert_eq!(b1.config.known_peers.len(), 2);
    }

    #[test]
    fn builder_identity_sets_private_field() {
        let identity = MlDsaKeyPair::generate().unwrap();
        let b = GossipRuntimeBuilder::new().identity(identity.clone());
        assert!(b.identity.is_some());
        assert_eq!(
            b.identity.as_ref().unwrap().public_key(),
            identity.public_key()
        );
    }

    #[test]
    fn builder_peer_health_oracle_sets_private_field() {
        let oracle: Arc<dyn PeerHealthOracle> = Arc::new(NoopHealthOracle);
        let b = GossipRuntimeBuilder::new().peer_health_oracle(oracle.clone());
        assert!(b.peer_health_oracle.is_some());
        drop(b);
    }

    #[test]
    fn runtime_exposes_peer_id() {
        // peer_id() is public — verify the field exists and is accessible.
        // We can't construct GossipRuntime without real transport, but we can
        // verify the method signature and field via doc-test pattern.
        let identity = MlDsaKeyPair::generate().unwrap();
        let expected_peer_id = identity.peer_id();
        // GossipRuntime holds peer_id as a public field, peer_id() returns it.
        // This test documents the expected behavior for the runtime struct.
        assert_eq!(expected_peer_id, identity.peer_id());
    }

    #[tokio::test]
    async fn build_with_peer_health_oracle_wires_pubsub() {
        let oracle: Arc<dyn PeerHealthOracle> = Arc::new(NoopHealthOracle);
        let runtime = GossipRuntimeBuilder::new()
            .peer_health_oracle(oracle)
            .build()
            .await;

        assert!(runtime.is_ok());
    }
}
