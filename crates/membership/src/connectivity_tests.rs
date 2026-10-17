//! Regression coverage for #107, using live connection changes and paused time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use bytes::Bytes;
use std::net::SocketAddr;

#[derive(Default)]
struct ConnectionTransport {
    connected: RwLock<HashSet<PeerId>>,
    attempts: RwLock<Vec<(PeerId, MembershipProtocolMessage)>>,
}

#[async_trait::async_trait]
impl GossipTransport for ConnectionTransport {
    async fn dial(&self, peer: PeerId, _addr: SocketAddr) -> Result<()> {
        self.connected.write().await.insert(peer);
        Ok(())
    }

    async fn dial_bootstrap(&self, _addr: SocketAddr) -> Result<PeerId> {
        Err(anyhow!("no bootstrap configured"))
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.connected.write().await.clear();
        Ok(())
    }

    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream: GossipStreamType,
        data: Bytes,
    ) -> Result<()> {
        assert_eq!(stream, GossipStreamType::Membership);
        self.attempts
            .write()
            .await
            .push((peer, postcard::from_bytes(&data)?));
        if !self.connected.read().await.contains(&peer) {
            return Err(anyhow!("Peer not found"));
        }
        Ok(())
    }

    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        Err(anyhow!("no receive queue"))
    }

    async fn connected_peer_ids(&self) -> Vec<PeerId> {
        self.connected.read().await.iter().copied().collect()
    }

    fn local_peer_id(&self) -> PeerId {
        peer(0)
    }
}

fn peer(id: u8) -> PeerId {
    PeerId::new([id; 32])
}

async fn fixture() -> (
    Arc<ConnectionTransport>,
    HyParViewMembership<ConnectionTransport>,
) {
    let transport = Arc::new(ConnectionTransport::default());
    let config = MembershipConfig {
        active_degree: 1,
        max_active_degree: 1,
        max_passive_degree: 8,
        adaptive_enabled: false,
        shuffle_period: Duration::from_secs(1),
        ..MembershipConfig::default()
    };
    let membership = HyParViewMembership::new(peer(0), config, transport.clone());
    // Start the background timers before changing the views.
    tokio::task::yield_now().await;
    (transport, membership)
}

async fn maintenance_interval() {
    // Exercise real background tasks, including SWIM and shuffles. Advancing
    // in one-second steps lets every periodic task run at its scheduled time.
    for _ in 0..10 {
        time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn join_never_connected_peer_is_pruned_within_ten_seconds() {
    let (_transport, membership) = fixture().await;
    membership.handle_join(peer(1), 0).await.unwrap();

    maintenance_interval().await;

    assert!(!membership.active_view().contains(&peer(1)));
    assert!(membership.passive_view().contains(&peer(1)));
    assert_eq!(membership.swim.get_state(&peer(1)).await, None);
}

#[tokio::test(start_paused = true)]
async fn forward_join_never_connected_peer_is_pruned_within_ten_seconds() {
    let (_transport, membership) = fixture().await;
    // The NEIGHBOR send fails: the random walk supplied only a peer ID.
    assert!(membership
        .handle_forward_join(peer(2), peer(1), 0)
        .await
        .is_err());

    maintenance_interval().await;

    assert!(!membership.active_view().contains(&peer(1)));
    assert!(membership.passive_view().contains(&peer(1)));
    assert_eq!(membership.swim.get_state(&peer(1)).await, None);
}

#[tokio::test(start_paused = true)]
async fn disconnected_peer_is_replaced_by_connected_passive_peer() {
    let (transport, membership) = fixture().await;
    transport.connected.write().await.extend([peer(1), peer(2)]);
    membership.add_active(peer(1)).await.unwrap();
    membership.add_to_passive(peer(2)).await;
    // An unavailable candidate must stay passive even if encountered first.
    membership.add_to_passive(peer(3)).await;
    membership.swim.record_probe(peer(1)).await;
    membership
        .swim
        .record_indirect_probe_request(peer(2), peer(1))
        .await;
    transport.connected.write().await.remove(&peer(1));

    maintenance_interval().await;

    assert_eq!(membership.active_view(), vec![peer(2)]);
    assert!(membership.passive_view().contains(&peer(1)));
    assert!(membership.passive_view().contains(&peer(3)));
    assert!(!membership.passive_view().contains(&peer(2)));
    assert_eq!(membership.swim.get_state(&peer(1)).await, None);
    assert_eq!(
        membership.swim.get_state(&peer(2)).await,
        Some(PeerState::Alive)
    );
    assert!(!membership.swim.clear_probe(&peer(1)).await);
    assert!(membership
        .swim
        .pending_indirect_probes
        .read()
        .await
        .is_empty());
}

#[tokio::test(start_paused = true)]
async fn disconnected_peer_returns_on_reconnect_without_another_join() {
    let (transport, membership) = fixture().await;
    transport.connected.write().await.insert(peer(1));
    membership.add_active(peer(1)).await.unwrap();
    transport.connected.write().await.clear();
    maintenance_interval().await;
    assert!(membership.active_view().is_empty());

    transport.connected.write().await.insert(peer(1));
    maintenance_interval().await;

    assert_eq!(membership.active_view(), vec![peer(1)]);
    assert!(!membership.passive_view().contains(&peer(1)));
    assert_eq!(
        membership.swim.get_state(&peer(1)).await,
        Some(PeerState::Alive)
    );
}

#[tokio::test(start_paused = true)]
async fn full_passive_view_keeps_replacement_and_respects_capacity() {
    let transport = Arc::new(ConnectionTransport::default());
    let config = MembershipConfig {
        active_degree: 1,
        max_active_degree: 1,
        max_passive_degree: 1,
        adaptive_enabled: false,
        ..MembershipConfig::default()
    };
    let membership = HyParViewMembership::new(peer(0), config, transport.clone());
    tokio::task::yield_now().await;
    transport.connected.write().await.extend([peer(1), peer(2)]);
    membership.add_active(peer(1)).await.unwrap();
    membership.add_to_passive(peer(2)).await;
    transport.connected.write().await.remove(&peer(1));

    maintenance_interval().await;

    assert_eq!(membership.active_view(), vec![peer(2)]);
    assert_eq!(membership.passive_view(), vec![peer(1)]);

    // Another disconnected admission must not make the passive view unbounded.
    membership.handle_join(peer(3), 0).await.unwrap();
    maintenance_interval().await;
    assert!(membership.active_view().is_empty());
    assert_eq!(membership.passive_view().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn pruned_peer_is_not_targeted_by_swim_shuffle_or_forward_join() {
    let (transport, membership) = fixture().await;
    transport.connected.write().await.extend([peer(1), peer(2)]);
    membership.add_active(peer(1)).await.unwrap();
    membership.add_to_passive(peer(2)).await;
    transport.connected.write().await.remove(&peer(1));
    maintenance_interval().await;
    assert_eq!(membership.active_view(), vec![peer(2)]);
    transport.attempts.write().await.clear();

    for _ in 0..3 {
        membership.shuffle().await.unwrap();
        membership
            .handle_forward_join(peer(3), peer(4), 2)
            .await
            .unwrap();
        membership
            .swim
            .request_indirect_probes(peer(4))
            .await
            .unwrap();
        maintenance_interval().await;
        assert!(!membership.active_view().contains(&peer(1)));
    }

    let attempts = transport.attempts.read().await;
    assert!(!attempts.is_empty());
    assert!(attempts.iter().all(|(target, _)| *target == peer(2)));
    assert!(attempts
        .iter()
        .any(|(_, message)| matches!(message, MembershipProtocolMessage::Swim(SwimMessage::Ping))));
    assert!(attempts.iter().any(|(_, message)| matches!(
        message,
        MembershipProtocolMessage::HyParView(HyParViewMessage::Shuffle { .. })
    )));
    assert!(attempts.iter().any(|(_, message)| matches!(
        message,
        MembershipProtocolMessage::HyParView(HyParViewMessage::ForwardJoin { .. })
    )));
    assert!(!attempts.iter().any(|(_, message)| matches!(
        message,
        MembershipProtocolMessage::Swim(SwimMessage::PingReq { target, .. })
            if *target == peer(1)
    )));
}
