use crate::{CoordinatorClient, RendezvousClient};
use anyhow::Result;
use saorsa_gossip_groups::GroupContext;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_membership::{HyParViewMembership, Membership, MembershipConfig};
use saorsa_gossip_presence::PresenceManager;
use saorsa_gossip_pubsub::{PlumtreePubSub, PubSub};
use saorsa_gossip_transport::{GossipTransport, UdpTransportAdapter, UdpTransportAdapterConfig};
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
}

impl Default for GossipRuntimeConfig {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR,
            known_peers: Vec::new(),
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

        let pubsub_impl = PlumtreePubSub::new(peer_id, transport.clone(), identity.clone());
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
