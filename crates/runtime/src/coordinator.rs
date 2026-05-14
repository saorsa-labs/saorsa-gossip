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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Mutex;

    /// Transport that either queues canned responses or fails sends.
    #[derive(Clone)]
    struct MockCoordTransport {
        sent: Arc<tokio::sync::Mutex<Vec<(PeerId, GossipStreamType)>>>,
        queued_responses: Arc<tokio::sync::Mutex<Vec<(PeerId, GossipStreamType, Bytes)>>>,
        /// Set of peers where send should fail (simulates network errors).
        send_fail_for: Arc<tokio::sync::Mutex<Vec<PeerId>>>,
    }

    #[async_trait::async_trait]
    impl GossipTransport for MockCoordTransport {
        async fn dial(&self, _peer: PeerId, _addr: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn dial_bootstrap(&self, _addr: std::net::SocketAddr) -> Result<PeerId> {
            Ok(PeerId::new([9; 32]))
        }
        async fn listen(&self, _bind: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }

        async fn send_to_peer(
            &self,
            peer: PeerId,
            stream_type: GossipStreamType,
            _data: Bytes,
        ) -> Result<()> {
            if self.send_fail_for.lock().await.contains(&peer) {
                return Err(anyhow::anyhow!("simulated send failure"));
            }
            self.sent.lock().await.push((peer, stream_type));
            Ok(())
        }

        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            let item = {
                let mut q = self.queued_responses.lock().await;
                q.pop()
            };
            if let Some(item) = item {
                return Ok(item);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Err(anyhow::anyhow!("no messages"))
        }

        fn local_peer_id(&self) -> PeerId {
            PeerId::new([7; 32])
        }
    }

    /// Transport that immediately returns errors.
    struct ImmediateErrorTransport;

    #[async_trait::async_trait]
    impl GossipTransport for ImmediateErrorTransport {
        async fn dial(&self, _peer: PeerId, _addr: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn dial_bootstrap(&self, _addr: std::net::SocketAddr) -> Result<PeerId> {
            Ok(PeerId::new([9; 32]))
        }
        async fn listen(&self, _bind: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        async fn send_to_peer(
            &self,
            _peer: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> Result<()> {
            Ok(())
        }
        // Returns error immediately without sleep — used to cover receive_message
        // error branches in collect_coordinator_responses.
        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            Err(anyhow::anyhow!("immediate error"))
        }
        fn local_peer_id(&self) -> PeerId {
            PeerId::new([77; 32])
        }
    }

    /// Transport that always fails sends.
    struct FailingTransport;

    #[async_trait::async_trait]
    impl GossipTransport for FailingTransport {
        async fn dial(&self, _peer: PeerId, _addr: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn dial_bootstrap(&self, _addr: std::net::SocketAddr) -> Result<PeerId> {
            Ok(PeerId::new([9; 32]))
        }
        async fn listen(&self, _bind: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        async fn send_to_peer(
            &self,
            _peer: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> Result<()> {
            Err(anyhow::anyhow!("transport send failed"))
        }
        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Err(anyhow::anyhow!("no messages"))
        }
        fn local_peer_id(&self) -> PeerId {
            PeerId::new([77; 32])
        }
    }

    #[derive(Default)]
    struct MockMembership {
        active_peers: Mutex<Vec<PeerId>>,
    }

    #[async_trait::async_trait]
    impl Membership for MockMembership {
        async fn join(&self, _seeds: Vec<String>) -> Result<()> {
            Ok(())
        }
        fn active_view(&self) -> Vec<PeerId> {
            self.active_peers.lock().unwrap().clone()
        }
        fn passive_view(&self) -> Vec<PeerId> {
            Vec::new()
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

    fn make_roles() -> CoordinatorRoles {
        CoordinatorRoles {
            coordinator: true,
            reflector: true,
            rendezvous: false,
            relay: false,
        }
    }

    fn make_addr() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9999))
    }

    #[tokio::test]
    async fn constructor_and_empty_cache() {
        let transport = Arc::new(RwLock::new(
            Box::new(FailingTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        assert!(client.get_cached_adverts().await.is_empty());
    }

    #[tokio::test]
    async fn publish_without_active_peers_caches_locally() {
        let transport = Arc::new(RwLock::new(
            Box::new(FailingTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        client
            .publish_coordinator_advert(make_roles(), vec![make_addr()], NatClass::Eim, 60_000)
            .await
            .unwrap();
        assert_eq!(client.get_cached_adverts().await.len(), 1);
    }

    #[tokio::test]
    async fn publish_with_active_peers_broadcasts_and_caches() {
        let memb = MockMembership::default();
        *memb.active_peers.lock().unwrap() = vec![PeerId::new([10; 32]), PeerId::new([11; 32])];
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sent_clone = sent.clone();
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: sent_clone,
            queued_responses: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(Box::new(memb) as Box<dyn Membership>));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        client
            .publish_coordinator_advert(make_roles(), vec![make_addr()], NatClass::Eim, 60_000)
            .await
            .unwrap();
        assert_eq!(client.get_cached_adverts().await.len(), 1);
        let recorded = sent.lock().await;
        assert_eq!(recorded.len(), 2);
        assert!(recorded
            .iter()
            .all(|(_, ty)| *ty == GossipStreamType::PubSub));
    }

    #[tokio::test]
    async fn publish_with_some_send_failures_counts_both() {
        let memb = MockMembership::default();
        *memb.active_peers.lock().unwrap() = vec![
            PeerId::new([10; 32]),
            PeerId::new([11; 32]),
            PeerId::new([12; 32]),
        ];
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sent_clone = sent.clone();
        let fail_for = Arc::new(tokio::sync::Mutex::new(vec![PeerId::new([11; 32])]));
        let fail_for_clone = fail_for.clone();
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: sent_clone,
            queued_responses: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            send_fail_for: fail_for_clone,
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(Box::new(memb) as Box<dyn Membership>));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        client
            .publish_coordinator_advert(make_roles(), vec![make_addr()], NatClass::Eim, 60_000)
            .await
            .unwrap();
        let recorded = sent.lock().await;
        // Peer 10 and 12 succeed, 11 fails
        assert_eq!(recorded.len(), 2);
        assert!(recorded.iter().any(|(p, _)| *p == PeerId::new([10; 32])));
        assert!(recorded.iter().any(|(p, _)| *p == PeerId::new([12; 32])));
        // 11 was in fail_for, not sent
        assert!(!recorded.iter().any(|(p, _)| *p == PeerId::new([11; 32])));
    }

    #[tokio::test]
    async fn foaf_with_cached_adverts_returns_cache() {
        let transport = Arc::new(RwLock::new(
            Box::new(FailingTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        client
            .publish_coordinator_advert(make_roles(), vec![make_addr()], NatClass::Eim, 60_000)
            .await
            .unwrap();
        let adverts = client.find_coordinators_via_foaf(2, 3).await.unwrap();
        assert_eq!(adverts.len(), 1);
    }

    #[tokio::test]
    async fn foaf_with_no_active_peers_returns_empty() {
        let transport = Arc::new(RwLock::new(
            Box::new(FailingTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let adverts = client.find_coordinators_via_foaf(2, 3).await.unwrap();
        assert!(adverts.is_empty());
    }

    #[tokio::test]
    async fn foaf_with_partial_send_failures_still_attempts_fanout() {
        // Simulate: peer 10 fails, others succeed. No responses → empty result.
        // This covers failed_count branch in FOAF send loop.
        let memb = MockMembership::default();
        *memb.active_peers.lock().unwrap() = vec![
            PeerId::new([10; 32]),
            PeerId::new([11; 32]),
            PeerId::new([12; 32]),
        ];
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sent_clone = sent.clone();
        let fail_for = Arc::new(tokio::sync::Mutex::new(vec![PeerId::new([10; 32])]));
        let fail_for_clone = fail_for.clone();
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: sent_clone,
            queued_responses: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            send_fail_for: fail_for_clone,
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(Box::new(memb) as Box<dyn Membership>));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let adverts = client.find_coordinators_via_foaf(1, 2).await.unwrap();
        // No responses → empty result
        assert!(adverts.is_empty());
        // At least one send succeeded (covering query_count branch)
        assert!(sent.lock().await.len() >= 1);
    }

    #[tokio::test]
    async fn foaf_populates_cache_when_adverts_received() {
        // Transport that yields a valid CoordinatorAdvert response.
        // The collect loop receives it, deserializes, caches it.
        let advert = CoordinatorAdvert::new(
            PeerId::new([88; 32]),
            make_roles(),
            vec![],
            NatClass::Eim,
            60_000,
        );
        let mut advert_bytes = Vec::new();
        ciborium::ser::into_writer(&advert, &mut advert_bytes).unwrap();

        let queued = Arc::new(tokio::sync::Mutex::new(vec![(
            PeerId::new([10; 32]), // from peer 10
            GossipStreamType::Membership,
            Bytes::from(advert_bytes),
        )]));
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            queued_responses: queued,
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let memb = MockMembership::default();
        *memb.active_peers.lock().unwrap() = vec![PeerId::new([10; 32])];
        let membership = Arc::new(RwLock::new(Box::new(memb) as Box<dyn Membership>));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let adverts = client.find_coordinators_via_foaf(1, 1).await.unwrap();
        assert_eq!(adverts.len(), 1);
        assert_eq!(adverts[0].peer, PeerId::new([88; 32]));
        // Adverts should now be cached
        let cached = client.get_cached_adverts().await;
        assert_eq!(cached.len(), 1);
    }

    #[tokio::test]
    async fn collect_responses_deserializes_valid_adverts() {
        // Transport yields a valid advert, collect returns it.
        let advert = CoordinatorAdvert::new(
            PeerId::new([77; 32]),
            make_roles(),
            vec![],
            NatClass::Edm,
            30_000,
        );
        let mut advert_bytes = Vec::new();
        ciborium::ser::into_writer(&advert, &mut advert_bytes).unwrap();

        let queued = Arc::new(tokio::sync::Mutex::new(vec![(
            PeerId::new([10; 32]),
            GossipStreamType::Membership,
            Bytes::from(advert_bytes),
        )]));
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            queued_responses: queued,
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .collect_coordinator_responses(&[PeerId::new([10; 32])], 5)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].peer, PeerId::new([77; 32]));
    }

    #[tokio::test]
    async fn collect_responses_ignores_wrong_stream_type() {
        // Transport yields a PubSub message, collect should skip it.
        let advert = CoordinatorAdvert::new(
            PeerId::new([66; 32]),
            make_roles(),
            vec![],
            NatClass::Symmetric,
            20_000,
        );
        let mut advert_bytes = Vec::new();
        ciborium::ser::into_writer(&advert, &mut advert_bytes).unwrap();

        // First: PubSub message (should be skipped), then valid Membership message.
        let queued = Arc::new(tokio::sync::Mutex::new(vec![
            (
                PeerId::new([10; 32]),
                GossipStreamType::PubSub, // wrong stream type
                Bytes::from(advert_bytes.clone()),
            ),
            (
                PeerId::new([10; 32]),
                GossipStreamType::Membership,
                Bytes::from(advert_bytes),
            ),
        ]));
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            queued_responses: queued,
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .collect_coordinator_responses(&[PeerId::new([10; 32])], 5)
            .await
            .unwrap();
        // Skipped PubSub, accepted Membership → still 1 advert
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].peer, PeerId::new([66; 32]));
    }

    #[tokio::test]
    async fn collect_responses_ignores_unexpected_peer() {
        // Transport yields advert from wrong peer, should be ignored.
        let advert = CoordinatorAdvert::new(
            PeerId::new([55; 32]),
            make_roles(),
            vec![],
            NatClass::Unknown,
            15_000,
        );
        let mut advert_bytes = Vec::new();
        ciborium::ser::into_writer(&advert, &mut advert_bytes).unwrap();

        // Wrong peer (not in expected_peers)
        let queued = Arc::new(tokio::sync::Mutex::new(vec![(
            PeerId::new([99; 32]), // unexpected peer
            GossipStreamType::Membership,
            Bytes::from(advert_bytes),
        )]));
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            queued_responses: queued,
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .collect_coordinator_responses(&[PeerId::new([10; 32])], 5)
            .await
            .unwrap();
        // No expected peers responded → empty
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn collect_responses_handles_invalid_cbor_gracefully() {
        // Transport yields non-CBOR bytes, should be skipped (debug log).
        let queued = Arc::new(tokio::sync::Mutex::new(vec![
            (
                PeerId::new([10; 32]),
                GossipStreamType::Membership,
                Bytes::from_static(b"not valid cbor at all"),
            ),
            // Then a valid advert so collect can return
            {
                let advert = CoordinatorAdvert::new(
                    PeerId::new([44; 32]),
                    make_roles(),
                    vec![],
                    NatClass::Eim,
                    10_000,
                );
                let mut b = Vec::new();
                ciborium::ser::into_writer(&advert, &mut b).unwrap();
                (
                    PeerId::new([10; 32]),
                    GossipStreamType::Membership,
                    Bytes::from(b),
                )
            },
        ]));
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            queued_responses: queued,
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .collect_coordinator_responses(&[PeerId::new([10; 32])], 5)
            .await
            .unwrap();
        // Invalid CBOR skipped, valid advert processed
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].peer, PeerId::new([44; 32]));
    }

    #[tokio::test]
    async fn request_address_reflection_with_failing_transport_returns_error() {
        let transport = Arc::new(RwLock::new(
            Box::new(FailingTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .request_address_reflection(PeerId::new([10; 32]))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn request_address_reflection_parses_valid_response() {
        // Transport that sends back a valid ADDR_REFLECT_RESPONSE on receive
        let queued = Arc::new(tokio::sync::Mutex::new(vec![(
            PeerId::new([99; 32]),
            GossipStreamType::Membership,
            Bytes::from(b"ADDR_REFLECT_RESPONSE:127.0.0.1:12345" as &[u8]),
        )]));
        let transport = Arc::new(RwLock::new(Box::new(MockCoordTransport {
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            queued_responses: queued,
            send_fail_for: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }) as Box<dyn GossipTransport>));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let addr = client
            .request_address_reflection(PeerId::new([99; 32]))
            .await
            .unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:12345");
    }

    #[tokio::test]
    async fn collect_responses_with_zero_timeout_returns_empty() {
        // Zero timeout → immediate timeout: response_future never runs,
        // collect returns Ok(Vec::new()) via the timeout branch.
        let transport = Arc::new(RwLock::new(
            Box::new(FailingTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .collect_coordinator_responses(&[PeerId::new([10; 32])], 0)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn collect_responses_handles_receive_error_breaks_loop() {
        // Transport whose receive_message returns error immediately.
        // Covers the Err(e) → warn → break path in the collect loop.
        let transport = Arc::new(RwLock::new(
            Box::new(ImmediateErrorTransport) as Box<dyn GossipTransport>
        ));
        let membership = Arc::new(RwLock::new(
            Box::new(MockMembership::default()) as Box<dyn Membership>
        ));
        let client = CoordinatorClient::new(PeerId::new([1; 32]), transport, membership);

        let result = client
            .collect_coordinator_responses(&[PeerId::new([10; 32])], 5)
            .await
            .unwrap();
        // Receive error causes loop to break, returns whatever was accumulated (none)
        assert!(result.is_empty());
    }
}
