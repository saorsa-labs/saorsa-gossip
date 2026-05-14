use anyhow::Result;
use bytes::Bytes;
use saorsa_gossip_pubsub::PubSub;
use saorsa_gossip_rendezvous::{calculate_shard, ProviderSummary};
use saorsa_gossip_transport::GossipTransport;
use saorsa_gossip_types::{PeerId, TopicId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// Rendezvous client for shard-based user discovery.
pub struct RendezvousClient {
    peer_id: PeerId,
    transport: Arc<RwLock<Box<dyn GossipTransport>>>,
    pubsub: Arc<RwLock<Box<dyn PubSub>>>,
    cached_summaries: Arc<RwLock<HashMap<[u8; 32], Vec<ProviderSummary>>>>,
    subscriptions: Arc<RwLock<HashMap<u16, TopicId>>>,
    active_collectors: Arc<RwLock<HashMap<[u8; 32], JoinHandle<()>>>>,
}

impl RendezvousClient {
    /// Create a new rendezvous client.
    pub fn new(
        peer_id: PeerId,
        transport: Arc<RwLock<Box<dyn GossipTransport>>>,
        pubsub: Arc<RwLock<Box<dyn PubSub>>>,
    ) -> Self {
        Self {
            peer_id,
            transport,
            pubsub,
            cached_summaries: Arc::new(RwLock::new(HashMap::new())),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            active_collectors: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Peer ID accessor.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Cached summaries accessor.
    pub async fn get_cached_summaries(&self, target_id: &[u8; 32]) -> Vec<ProviderSummary> {
        self.cached_summaries
            .read()
            .await
            .get(target_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Subscribe to a rendezvous shard for a target.
    pub async fn subscribe_to_shard(&self, target_id: &[u8; 32]) -> Result<u16> {
        let shard = calculate_shard(target_id);

        {
            let subscriptions = self.subscriptions.read().await;
            if subscriptions.contains_key(&shard) {
                return Ok(shard);
            }
        }

        let shard_name = format!("rendezvous_shard_{}", shard);
        let hash = blake3::hash(shard_name.as_bytes());
        let topic_id = TopicId::new(*hash.as_bytes());

        let pubsub = self.pubsub.read().await;
        let _rx = pubsub.subscribe(topic_id);

        self.subscriptions.write().await.insert(shard, topic_id);

        Ok(shard)
    }

    /// Publish a provider summary to a target shard.
    pub async fn publish_provider_summary(&self, summary: ProviderSummary) -> Result<()> {
        let target_id = &summary.target;
        let shard = calculate_shard(target_id);
        let shard_name = format!("rendezvous_shard_{}", shard);
        let hash = blake3::hash(shard_name.as_bytes());
        let topic_id = TopicId::new(*hash.as_bytes());

        let mut summary_bytes = Vec::new();
        ciborium::ser::into_writer(&summary, &mut summary_bytes)
            .map_err(|e| anyhow::anyhow!("CBOR encoding failed: {:?}", e))?;

        let pubsub = self.pubsub.read().await;
        pubsub.publish(topic_id, Bytes::from(summary_bytes)).await?;

        Ok(())
    }

    /// Retrieve cached providers sorted by validity.
    pub async fn get_providers_for_target(&self, target_id: &[u8; 32]) -> Vec<ProviderSummary> {
        let cache = self.cached_summaries.read().await;
        let summaries = cache.get(target_id).cloned().unwrap_or_default();
        drop(cache);

        if summaries.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(ProviderSummary, u64)> = summaries
            .into_iter()
            .filter_map(|summary| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as u64;

                if summary.exp <= now {
                    return None;
                }

                let remaining = summary.exp - now;
                Some((summary, remaining))
            })
            .collect();

        scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        scored.into_iter().map(|(summary, _)| summary).collect()
    }

    /// Process provider summaries received via PubSub.
    pub async fn process_incoming_summary(
        &self,
        target_id: &[u8; 32],
        message_bytes: Bytes,
    ) -> Result<()> {
        let summary: ProviderSummary = ciborium::de::from_reader(&message_bytes[..])
            .map_err(|e| anyhow::anyhow!("Invalid ProviderSummary payload: {}", e))?;

        if &summary.target != target_id {
            return Err(anyhow::anyhow!(
                "Summary target mismatch: expected {:?}, got {:?}",
                target_id,
                summary.target
            ));
        }

        let mut cache = self.cached_summaries.write().await;
        let summaries = cache.entry(*target_id).or_insert_with(Vec::new);
        if let Some(existing) = summaries
            .iter_mut()
            .find(|s| s.provider == summary.provider)
        {
            *existing = summary;
        } else {
            summaries.push(summary);
        }

        Ok(())
    }

    /// Start background collection for a target rendezvous shard.
    pub async fn start_collecting_for_target(&self, target_id: [u8; 32]) -> Result<()> {
        if self.active_collectors.read().await.contains_key(&target_id) {
            return Ok(());
        }

        let shard = calculate_shard(&target_id);
        let topic_id = {
            let subscriptions = self.subscriptions.read().await;
            subscriptions
                .get(&shard)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Not subscribed to shard for target"))?
        };

        let pubsub = self.pubsub.read().await;
        let mut rx = pubsub.subscribe(topic_id);
        drop(pubsub);

        let client = Arc::new(self.clone_for_background());
        let handle = tokio::spawn(async move {
            while let Some((_peer_id, message_bytes)) = rx.recv().await {
                if let Err(e) = client
                    .process_incoming_summary(&target_id, message_bytes)
                    .await
                {
                    tracing::error!("Error processing rendezvous summary: {}", e);
                }
            }
        });

        self.active_collectors
            .write()
            .await
            .insert(target_id, handle);
        Ok(())
    }

    fn clone_for_background(&self) -> Self {
        Self {
            peer_id: self.peer_id,
            transport: self.transport.clone(),
            pubsub: self.pubsub.clone(),
            cached_summaries: self.cached_summaries.clone(),
            subscriptions: self.subscriptions.clone(),
            active_collectors: self.active_collectors.clone(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use saorsa_gossip_rendezvous::Capability;
    use saorsa_gossip_transport::GossipStreamType;
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use tokio::sync::{mpsc, Mutex};

    #[derive(Clone, Default)]
    struct MockPubSub {
        state: Arc<MockPubSubState>,
    }

    #[derive(Default)]
    struct MockPubSubState {
        published: Mutex<Vec<(TopicId, Bytes)>>,
        subscribed: std::sync::Mutex<Vec<TopicId>>,
    }

    #[async_trait::async_trait]
    impl PubSub for MockPubSub {
        async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()> {
            self.state.published.lock().await.push((topic, data));
            Ok(())
        }

        fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
            let (tx, rx) = mpsc::unbounded_channel();
            drop(tx);
            self.state
                .subscribed
                .lock()
                .expect("subscription mutex is not poisoned")
                .push(topic);
            rx
        }

        async fn unsubscribe(&self, _topic: TopicId) -> Result<()> {
            Ok(())
        }

        async fn initialize_topic_peers(&self, _topic: TopicId, _peers: Vec<PeerId>) {}

        async fn handle_message(&self, _from: PeerId, _data: Bytes) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockTransport {
        incoming: Mutex<VecDeque<(PeerId, GossipStreamType, Bytes)>>,
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
            _peer: PeerId,
            _stream_type: GossipStreamType,
            _data: Bytes,
        ) -> Result<()> {
            Ok(())
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

    fn target(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn summary_for(target_id: [u8; 32], provider: PeerId, validity_ms: u64) -> ProviderSummary {
        ProviderSummary::new(target_id, provider, vec![Capability::Site], validity_ms)
    }

    fn encode_summary(summary: &ProviderSummary) -> Bytes {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(summary, &mut bytes).expect("summary encodes");
        Bytes::from(bytes)
    }

    fn client() -> (RendezvousClient, MockPubSub) {
        let pubsub = MockPubSub::default();
        let transport: Arc<RwLock<Box<dyn GossipTransport>>> =
            Arc::new(RwLock::new(Box::new(MockTransport::default())));
        let pubsub_trait: Arc<RwLock<Box<dyn PubSub>>> =
            Arc::new(RwLock::new(Box::new(pubsub.clone())));
        (
            RendezvousClient::new(peer(1), transport, pubsub_trait),
            pubsub,
        )
    }

    #[tokio::test]
    async fn peer_id_and_empty_cache_are_reported() {
        let (client, _) = client();
        assert_eq!(client.peer_id(), peer(1));
        assert!(client.get_cached_summaries(&target(7)).await.is_empty());
    }

    #[tokio::test]
    async fn subscribe_to_shard_is_idempotent() {
        let (client, pubsub) = client();
        let target_id = target(3);

        let first = client.subscribe_to_shard(&target_id).await.unwrap();
        let second = client.subscribe_to_shard(&target_id).await.unwrap();

        assert_eq!(first, calculate_shard(&target_id));
        assert_eq!(first, second);
        assert_eq!(pubsub.state.subscribed.lock().expect("mutex").len(), 1);
    }

    #[tokio::test]
    async fn publish_provider_summary_uses_target_shard_topic() {
        let (client, pubsub) = client();
        let target_id = target(4);
        let summary = summary_for(target_id, peer(2), 1_000);

        client
            .publish_provider_summary(summary.clone())
            .await
            .unwrap();

        let published = pubsub.state.published.lock().await;
        assert_eq!(published.len(), 1);
        let shard_name = format!("rendezvous_shard_{}", calculate_shard(&target_id));
        let expected_topic = TopicId::new(*blake3::hash(shard_name.as_bytes()).as_bytes());
        assert_eq!(published[0].0, expected_topic);
        let decoded: ProviderSummary = ciborium::de::from_reader(&published[0].1[..]).unwrap();
        assert_eq!(decoded.target, summary.target);
        assert_eq!(decoded.provider, summary.provider);
    }

    #[tokio::test]
    async fn process_incoming_summary_rejects_invalid_payload_and_wrong_target() {
        let (client, _) = client();
        let target_id = target(5);

        let invalid = client
            .process_incoming_summary(&target_id, Bytes::from_static(b"not-cbor"))
            .await;
        assert!(invalid.is_err());

        let wrong_target = summary_for(target(6), peer(2), 1_000);
        let mismatch = client
            .process_incoming_summary(&target_id, encode_summary(&wrong_target))
            .await;
        assert!(mismatch.is_err());
        assert!(client.get_cached_summaries(&target_id).await.is_empty());
    }

    #[tokio::test]
    async fn process_incoming_summary_inserts_and_replaces_by_provider() {
        let (client, _) = client();
        let target_id = target(8);
        let mut first = summary_for(target_id, peer(2), 1_000);
        let mut replacement = summary_for(target_id, peer(2), 2_000);
        replacement.have_root = true;
        first.have_root = false;

        client
            .process_incoming_summary(&target_id, encode_summary(&first))
            .await
            .unwrap();
        client
            .process_incoming_summary(&target_id, encode_summary(&replacement))
            .await
            .unwrap();
        client
            .process_incoming_summary(
                &target_id,
                encode_summary(&summary_for(target_id, peer(3), 2_000)),
            )
            .await
            .unwrap();

        let cached = client.get_cached_summaries(&target_id).await;
        assert_eq!(cached.len(), 2);
        assert!(cached.iter().any(|s| s.provider == peer(2) && s.have_root));
        assert!(cached.iter().any(|s| s.provider == peer(3)));
    }

    #[tokio::test]
    async fn providers_for_target_filters_expired_and_orders_by_remaining_validity() {
        let (client, _) = client();
        let target_id = target(9);
        let expired = ProviderSummary {
            exp: 1,
            ..summary_for(target_id, peer(2), 1_000)
        };
        let short = summary_for(target_id, peer(3), 60_000);
        let long = summary_for(target_id, peer(4), 120_000);

        for summary in [&expired, &short, &long] {
            client
                .process_incoming_summary(&target_id, encode_summary(summary))
                .await
                .unwrap();
        }

        let providers = client.get_providers_for_target(&target_id).await;
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider, peer(4));
        assert_eq!(providers[1].provider, peer(3));
    }

    #[tokio::test]
    async fn providers_for_unknown_target_are_empty() {
        let (client, _) = client();
        assert!(client
            .get_providers_for_target(&target(42))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn start_collecting_requires_subscription_then_records_collector() {
        let (client, _) = client();
        let target_id = target(10);

        assert!(client.start_collecting_for_target(target_id).await.is_err());
        client.subscribe_to_shard(&target_id).await.unwrap();
        client.start_collecting_for_target(target_id).await.unwrap();
        client.start_collecting_for_target(target_id).await.unwrap();

        assert!(client
            .active_collectors
            .read()
            .await
            .contains_key(&target_id));
    }
}
