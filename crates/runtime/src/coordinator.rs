use anyhow::Result;
use bytes::Bytes;
use saorsa_gossip_coordinator::{
    AddrHint, CoordinatorAdvert, CoordinatorRoles, FindCoordinatorQuery, NatClass,
};
use saorsa_gossip_membership::Membership;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use saorsa_gossip_types::PeerId;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Coordinator client wrapper that can be shared across applications.
pub struct CoordinatorClient {
    peer_id: PeerId,
    transport: Arc<RwLock<Box<dyn GossipTransport>>>,
    membership: Arc<RwLock<Box<dyn Membership>>>,
    cached_adverts: Arc<RwLock<Vec<CoordinatorAdvert>>>,
}

impl CoordinatorClient {
    /// Create a new coordinator client
    pub fn new(
        peer_id: PeerId,
        transport: Arc<RwLock<Box<dyn GossipTransport>>>,
        membership: Arc<RwLock<Box<dyn Membership>>>,
    ) -> Self {
        Self {
            peer_id,
            transport,
            membership,
            cached_adverts: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Publish our coordinator advert
    pub async fn publish_coordinator_advert(
        &self,
        roles: CoordinatorRoles,
        endpoints: Vec<SocketAddr>,
        nat_class: NatClass,
        validity_ms: u64,
    ) -> Result<()> {
        info!("Publishing coordinator advert for peer {:?}", self.peer_id);

        let addr_hints: Vec<AddrHint> = endpoints.into_iter().map(AddrHint::new).collect();
        let advert =
            CoordinatorAdvert::new(self.peer_id, roles, addr_hints, nat_class, validity_ms);

        self.cached_adverts.write().await.push(advert.clone());

        let mut advert_bytes = Vec::new();
        ciborium::ser::into_writer(&advert, &mut advert_bytes)
            .map_err(|e| anyhow::anyhow!("CBOR encoding failed: {:?}", e))?;

        let membership = self.membership.read().await;
        let active_peers = membership.active_view();

        if active_peers.is_empty() {
            debug!("No active peers to broadcast advert to, cached locally only");
            return Ok(());
        }

        let transport = self.transport.read().await;
        let mut broadcast_count = 0;
        let mut failed_count = 0;

        for peer_id in active_peers {
            match transport
                .send_to_peer(
                    peer_id,
                    GossipStreamType::PubSub,
                    Bytes::from(advert_bytes.clone()),
                )
                .await
            {
                Ok(_) => {
                    broadcast_count += 1;
                    debug!("Broadcasted coordinator advert to peer {:?}", peer_id);
                }
                Err(e) => {
                    failed_count += 1;
                    warn!("Failed to broadcast advert to peer {:?}: {}", peer_id, e);
                }
            }
        }

        info!(
            "Coordinator advert broadcast complete: {} succeeded, {} failed out of {} peers",
            broadcast_count,
            failed_count,
            broadcast_count + failed_count
        );

        Ok(())
    }

    /// Find coordinators via FOAF discovery
    pub async fn find_coordinators_via_foaf(
        &self,
        ttl: u8,
        fanout: u8,
    ) -> Result<Vec<CoordinatorAdvert>> {
        debug!(
            "Finding coordinators via FOAF (TTL={}, fanout={})",
            ttl, fanout
        );

        let cached = self.cached_adverts.read().await.clone();
        if !cached.is_empty() {
            info!("Returning {} cached coordinator adverts", cached.len());
            return Ok(cached);
        }

        let membership = self.membership.read().await;
        let active_peers = membership.active_view();

        if active_peers.is_empty() {
            debug!("No active peers available for FOAF query, returning empty result");
            return Ok(Vec::new());
        }

        use rand::seq::SliceRandom;
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::from_entropy();
        let selected_peers: Vec<_> = active_peers
            .choose_multiple(&mut rng, fanout as usize)
            .cloned()
            .collect();

        info!(
            "Sending FOAF query to {} peers (fanout={})",
            selected_peers.len(),
            fanout
        );

        let query = FindCoordinatorQuery::new(self.peer_id);
        let mut query_bytes = Vec::new();
        ciborium::ser::into_writer(&query, &mut query_bytes)
            .map_err(|e| anyhow::anyhow!("CBOR encoding failed: {:?}", e))?;

        let transport = self.transport.read().await;
        let mut query_count = 0;
        let mut failed_count = 0;

        for peer_id in &selected_peers {
            match transport
                .send_to_peer(
                    *peer_id,
                    GossipStreamType::Membership,
                    Bytes::from(query_bytes.clone()),
                )
                .await
            {
                Ok(_) => {
                    query_count += 1;
                    debug!("Sent FOAF query to peer {:?}", peer_id);
                }
                Err(e) => {
                    failed_count += 1;
                    warn!("Failed to send FOAF query to peer {:?}: {}", peer_id, e);
                }
            }
        }

        info!(
            "FOAF queries sent: {} succeeded, {} failed",
            query_count, failed_count
        );

        if query_count == 0 {
            debug!("No queries succeeded, returning empty result");
            return Ok(Vec::new());
        }

        let adverts = self
            .collect_coordinator_responses(&selected_peers, ttl as u64 * 3)
            .await?;

        if !adverts.is_empty() {
            let mut cache = self.cached_adverts.write().await;
            for advert in &adverts {
                if !cache.iter().any(|a| a.peer == advert.peer) {
                    cache.push(advert.clone());
                }
            }
        }

        Ok(adverts)
    }

    /// Request address reflection from a coordinator
    pub async fn request_address_reflection(
        &self,
        coordinator_peer_id: PeerId,
    ) -> Result<SocketAddr> {
        debug!(
            "Requesting address reflection from coordinator {:?}",
            coordinator_peer_id
        );

        let request = Bytes::from_static(b"ADDR_REFLECT_REQUEST");
        let transport = self.transport.read().await;
        transport
            .send_to_peer(coordinator_peer_id, GossipStreamType::Membership, request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send reflection request: {}", e))?;

        let response_future = async {
            loop {
                match transport.receive_message().await {
                    Ok((peer, stream_type, data)) => {
                        if peer == coordinator_peer_id
                            && stream_type == GossipStreamType::Membership
                        {
                            if let Ok(response_str) = String::from_utf8(data.to_vec()) {
                                if let Some(addr_str) =
                                    response_str.strip_prefix("ADDR_REFLECT_RESPONSE:")
                                {
                                    if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                        return Ok(addr);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Error receiving reflection response: {}", e);
                        break;
                    }
                }
            }
            Err(anyhow::anyhow!("No valid reflection response received"))
        };

        match timeout(Duration::from_secs(5), response_future).await {
            Ok(Ok(addr)) => {
                info!("Received reflected address: {}", addr);
                Ok(addr)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow::anyhow!("Address reflection request timed out")),
        }
    }

    /// Cached adverts accessor.
    pub async fn get_cached_adverts(&self) -> Vec<CoordinatorAdvert> {
        self.cached_adverts.read().await.clone()
    }

    async fn collect_coordinator_responses(
        &self,
        expected_peers: &[PeerId],
        timeout_secs: u64,
    ) -> Result<Vec<CoordinatorAdvert>> {
        use std::collections::HashMap;

        debug!(
            "Collecting coordinator responses from {} peers (timeout: {}s)",
            expected_peers.len(),
            timeout_secs
        );

        let transport = self.transport.read().await;

        let response_future = async {
            let mut adverts_map: HashMap<PeerId, CoordinatorAdvert> = HashMap::new();
            loop {
                match transport.receive_message().await {
                    Ok((peer_id, stream_type, data)) => {
                        if stream_type != GossipStreamType::Membership {
                            continue;
                        }
                        if !expected_peers.contains(&peer_id) {
                            continue;
                        }

                        match ciborium::de::from_reader::<CoordinatorAdvert, _>(&data[..]) {
                            Ok(advert) => {
                                let coord_peer = advert.peer;
                                adverts_map.entry(coord_peer).or_insert_with(|| {
                                    debug!(
                                        "Received coordinator advert from {:?} via {:?}",
                                        coord_peer, peer_id
                                    );
                                    advert
                                });
                            }
                            Err(e) => {
                                debug!(
                                    "Failed to deserialize coordinator advert from {:?}: {}",
                                    peer_id, e
                                );
                            }
                        }

                        if adverts_map.len() >= expected_peers.len() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Error receiving coordinator response: {}", e);
                        break;
                    }
                }
            }
            adverts_map.into_values().collect::<Vec<_>>()
        };

        match timeout(Duration::from_secs(timeout_secs), response_future).await {
            Ok(adverts) => {
                info!("Collected {} coordinator adverts", adverts.len());
                Ok(adverts)
            }
            Err(_) => {
                info!("Response collection timed out, returning empty result");
                Ok(Vec::new())
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct MockMembership {
        active: Vec<PeerId>,
        passive: Vec<PeerId>,
    }

    #[async_trait::async_trait]
    impl Membership for MockMembership {
        async fn join(&self, _seeds: Vec<String>) -> Result<()> {
            Ok(())
        }

        fn active_view(&self) -> Vec<PeerId> {
            self.active.clone()
        }

        fn passive_view(&self) -> Vec<PeerId> {
            self.passive.clone()
        }

        async fn add_active(&self, _peer: PeerId) -> Result<()> {
            Ok(())
        }

        async fn remove_active(&self, _peer: PeerId) -> Result<()> {
            Ok(())
        }

        async fn promote(&self, _peer: PeerId) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockTransport {
        sent: Mutex<Vec<(PeerId, GossipStreamType, Bytes)>>,
        incoming: Mutex<VecDeque<(PeerId, GossipStreamType, Bytes)>>,
        fail_peers: HashSet<PeerId>,
    }

    #[async_trait::async_trait]
    impl GossipTransport for MockTransport {
        async fn dial(&self, _peer: PeerId, _addr: SocketAddr) -> Result<()> {
            Ok(())
        }

        async fn dial_bootstrap(&self, _addr: SocketAddr) -> Result<PeerId> {
            Ok(peer(9))
        }

        async fn listen(&self, _bind: SocketAddr) -> Result<()> {
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }

        async fn send_to_peer(
            &self,
            peer: PeerId,
            stream_type: GossipStreamType,
            data: Bytes,
        ) -> Result<()> {
            self.sent.lock().await.push((peer, stream_type, data));
            if self.fail_peers.contains(&peer) {
                Err(anyhow::anyhow!("send failed"))
            } else {
                Ok(())
            }
        }

        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            self.incoming
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no message"))
        }

        fn local_peer_id(&self) -> PeerId {
            peer(1)
        }
    }

    fn peer(byte: u8) -> PeerId {
        PeerId::new([byte; 32])
    }

    fn endpoint(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn encode_advert(advert: &CoordinatorAdvert) -> Bytes {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(advert, &mut bytes).expect("advert encodes");
        Bytes::from(bytes)
    }

    fn client_with(
        active: Vec<PeerId>,
        incoming: Vec<(PeerId, GossipStreamType, Bytes)>,
        fail_peers: HashSet<PeerId>,
    ) -> (CoordinatorClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport {
            sent: Mutex::new(Vec::new()),
            incoming: Mutex::new(incoming.into()),
            fail_peers,
        });
        let transport_trait: Arc<RwLock<Box<dyn GossipTransport>>> =
            Arc::new(RwLock::new(Box::new(transport.clone())));
        let membership: Arc<RwLock<Box<dyn Membership>>> =
            Arc::new(RwLock::new(Box::new(MockMembership {
                active,
                passive: vec![peer(8)],
            })));
        (
            CoordinatorClient::new(peer(1), transport_trait, membership),
            transport,
        )
    }

    #[tokio::test]
    async fn cached_adverts_are_initially_empty() {
        let (client, _) = client_with(Vec::new(), Vec::new(), HashSet::new());
        assert!(client.get_cached_adverts().await.is_empty());
    }

    #[tokio::test]
    async fn publish_advert_without_active_peers_caches_only() {
        let (client, transport) = client_with(Vec::new(), Vec::new(), HashSet::new());

        client
            .publish_coordinator_advert(
                CoordinatorRoles::default(),
                vec![endpoint(10_001)],
                NatClass::Eim,
                1_000,
            )
            .await
            .unwrap();

        let cached = client.get_cached_adverts().await;
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].peer, peer(1));
        assert_eq!(cached[0].addr_hints[0].addr, endpoint(10_001));
        assert!(transport.sent.lock().await.is_empty());
    }

    #[tokio::test]
    async fn publish_advert_broadcasts_to_all_active_peers_and_tolerates_failures() {
        let mut fail_peers = HashSet::new();
        fail_peers.insert(peer(3));
        let (client, transport) = client_with(vec![peer(2), peer(3)], Vec::new(), fail_peers);

        client
            .publish_coordinator_advert(
                CoordinatorRoles::default(),
                vec![endpoint(10_002)],
                NatClass::Edm,
                1_000,
            )
            .await
            .unwrap();

        let sent = transport.sent.lock().await;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].1, GossipStreamType::PubSub);
        assert_eq!(sent[1].1, GossipStreamType::PubSub);
        let decoded: CoordinatorAdvert = ciborium::de::from_reader(&sent[0].2[..]).unwrap();
        assert_eq!(decoded.peer, peer(1));
        assert_eq!(client.get_cached_adverts().await.len(), 1);
    }

    #[tokio::test]
    async fn find_coordinators_returns_cached_or_empty_without_peers() {
        let (client, _) = client_with(Vec::new(), Vec::new(), HashSet::new());
        assert!(client
            .find_coordinators_via_foaf(2, 2)
            .await
            .unwrap()
            .is_empty());

        client
            .publish_coordinator_advert(
                CoordinatorRoles::default(),
                vec![endpoint(10_003)],
                NatClass::Unknown,
                1_000,
            )
            .await
            .unwrap();
        let found = client.find_coordinators_via_foaf(2, 2).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].peer, peer(1));
    }

    #[tokio::test]
    async fn find_coordinators_queries_active_peers_and_collects_valid_responses() {
        let advert = CoordinatorAdvert::new(
            peer(7),
            CoordinatorRoles::default(),
            vec![AddrHint::new(endpoint(10_004))],
            NatClass::Symmetric,
            1_000,
        );
        let incoming = vec![
            (
                peer(4),
                GossipStreamType::PubSub,
                Bytes::from_static(b"ignored"),
            ),
            (
                peer(6),
                GossipStreamType::Membership,
                encode_advert(&advert),
            ),
        ];
        let (client, transport) = client_with(vec![peer(6)], incoming, HashSet::new());

        let found = client.find_coordinators_via_foaf(1, 3).await.unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].peer, peer(7));
        let sent = transport.sent.lock().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, peer(6));
        assert_eq!(sent[0].1, GossipStreamType::Membership);
        assert_eq!(client.get_cached_adverts().await.len(), 1);
    }

    #[tokio::test]
    async fn find_coordinators_returns_empty_when_all_queries_fail() {
        let mut fail_peers = HashSet::new();
        fail_peers.insert(peer(2));
        let (client, transport) = client_with(vec![peer(2)], Vec::new(), fail_peers);

        let found = client.find_coordinators_via_foaf(1, 1).await.unwrap();

        assert!(found.is_empty());
        assert_eq!(transport.sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn request_address_reflection_parses_matching_membership_response() {
        let response = Bytes::from_static(b"ADDR_REFLECT_RESPONSE:127.0.0.1:12345");
        let incoming = vec![
            (peer(3), GossipStreamType::Membership, response.clone()),
            (peer(2), GossipStreamType::PubSub, response.clone()),
            (
                peer(2),
                GossipStreamType::Membership,
                Bytes::from_static(b"bad"),
            ),
            (peer(2), GossipStreamType::Membership, response),
        ];
        let (client, transport) = client_with(Vec::new(), incoming, HashSet::new());

        let reflected = client.request_address_reflection(peer(2)).await.unwrap();

        assert_eq!(reflected, endpoint(12_345));
        let sent = transport.sent.lock().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, peer(2));
        assert_eq!(sent[0].1, GossipStreamType::Membership);
        assert_eq!(&sent[0].2[..], b"ADDR_REFLECT_REQUEST");
    }

    #[tokio::test]
    async fn request_address_reflection_reports_send_or_receive_failures() {
        let mut fail_peers = HashSet::new();
        fail_peers.insert(peer(2));
        let (send_fail_client, _) = client_with(Vec::new(), Vec::new(), fail_peers);
        assert!(send_fail_client
            .request_address_reflection(peer(2))
            .await
            .is_err());

        let (receive_fail_client, _) = client_with(Vec::new(), Vec::new(), HashSet::new());
        assert!(receive_fail_client
            .request_address_reflection(peer(2))
            .await
            .is_err());
    }
}
