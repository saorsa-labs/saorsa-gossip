// Allow deprecated transport types during migration period.
// Phase 3.3 will replace these with direct ant-quic transport integration.
#![allow(deprecated)]

use crate::{CoordinatorClient, RendezvousClient};
use anyhow::Result;
use saorsa_gossip_groups::GroupContext;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_membership::{
    HyParViewMembership, Membership, DEFAULT_ACTIVE_DEGREE, DEFAULT_PASSIVE_DEGREE,
};
use saorsa_gossip_presence::PresenceManager;
use saorsa_gossip_pubsub::{PlumtreePubSub, PubSub};
use saorsa_gossip_transport::{
    GossipTransport, MultiplexedGossipTransport, TransportDescriptor, TransportMultiplexer,
    UdpTransportAdapter, UdpTransportAdapterConfig,
};
use saorsa_gossip_types::{PeerId, TopicId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

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
            bind_addr: "0.0.0.0:0".parse().expect("valid addr"),
            known_peers: Vec::new(),
        }
    }
}

/// Builder for [`GossipRuntime`].
#[derive(Default)]
pub struct GossipRuntimeBuilder {
    config: GossipRuntimeConfig,
    identity: Option<MlDsaKeyPair>,
    /// Optional pre-configured multiplexer for multi-transport support.
    multiplexer: Option<Arc<TransportMultiplexer>>,
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

    /// Provide a pre-configured transport multiplexer.
    ///
    /// When set, the runtime will use the multiplexer for routing messages
    /// to different transports based on capability requirements. If not set,
    /// a default single-transport (UDP) configuration is used.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use saorsa_gossip_transport::{TransportMultiplexer, TransportDescriptor};
    ///
    /// let multiplexer = TransportMultiplexer::new(peer_id);
    /// multiplexer.register_transport(TransportDescriptor::Udp, udp_transport).await?;
    /// multiplexer.set_default_transport(TransportDescriptor::Udp).await?;
    ///
    /// let runtime = GossipRuntimeBuilder::new()
    ///     .with_multiplexer(Arc::new(multiplexer))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_multiplexer(mut self, multiplexer: Arc<TransportMultiplexer>) -> Self {
        self.multiplexer = Some(multiplexer);
        self
    }

    /// Build the runtime.
    pub async fn build(self) -> Result<GossipRuntime> {
        let identity = match self.identity {
            Some(id) => id,
            None => MlDsaKeyPair::generate()?,
        };

        let peer_id = identity.peer_id();

        // Create the transport - either from provided multiplexer or default single-transport
        let transport: Arc<MultiplexedGossipTransport> = match self.multiplexer {
            Some(multiplexer) => {
                // Use the provided multiplexer wrapped in MultiplexedGossipTransport
                Arc::new(MultiplexedGossipTransport::new(multiplexer, peer_id))
            }
            None => {
                // Create default single UDP transport with multiplexer wrapper
                let udp_adapter = Arc::new(
                    UdpTransportAdapter::with_config(
                        UdpTransportAdapterConfig::new(
                            self.config.bind_addr,
                            self.config.known_peers,
                        ),
                        None,
                    )
                    .await?,
                );

                // Wrap in multiplexer for consistency
                let multiplexer = TransportMultiplexer::new(peer_id);
                multiplexer
                    .register_transport(TransportDescriptor::Udp, udp_adapter)
                    .await?;
                multiplexer
                    .set_default_transport(TransportDescriptor::Udp)
                    .await?;

                Arc::new(MultiplexedGossipTransport::new(
                    Arc::new(multiplexer),
                    peer_id,
                ))
            }
        };

        let membership_impl = HyParViewMembership::new(
            peer_id,
            DEFAULT_ACTIVE_DEGREE,
            DEFAULT_PASSIVE_DEGREE,
            transport.clone(),
        );
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
    /// Shared multiplexed transport for multi-transport support.
    pub transport: Arc<MultiplexedGossipTransport>,
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
