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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use saorsa_gossip_rendezvous::Capability;
    use saorsa_gossip_transport::GossipStreamType;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    #[derive(Clone, Default)]
    struct MockPubSub {
        published: Arc<Mutex<Vec<(TopicId, Bytes)>>>,
        subscribed: Arc<Mutex<Vec<TopicId>>>,
    }

    #[async_trait::async_trait]
    impl PubSub for MockPubSub {
        async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()> {
            self.published.lock().unwrap().push((topic, data));
            Ok(())
        }

        fn subscribe(&self, topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
            self.subscribed.lock().unwrap().push(topic);
            let (_tx, rx) = mpsc::unbounded_channel();
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

    struct MockTransport;

    #[async_trait::async_trait]
    impl GossipTransport for MockTransport {
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

        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            Err(anyhow::anyhow!("no messages"))
        }

        fn local_peer_id(&self) -> PeerId {
            PeerId::new([7; 32])
        }
    }

    fn client() -> RendezvousClient {
        RendezvousClient::new(
            PeerId::new([1; 32]),
            Arc::new(RwLock::new(Box::new(MockTransport))),
            Arc::new(RwLock::new(Box::new(MockPubSub::default()))),
        )
    }

    fn summary(target: [u8; 32], provider_byte: u8, validity_ms: u64) -> ProviderSummary {
        ProviderSummary::new(
            target,
            PeerId::new([provider_byte; 32]),
            vec![Capability::Site],
            validity_ms,
        )
    }

    fn encode(summary: &ProviderSummary) -> Bytes {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(summary, &mut bytes).unwrap();
        Bytes::from(bytes)
    }

    #[tokio::test]
    async fn constructor_and_empty_cache_report_peer_id() {
        let client = client();
        let target = [42; 32];

        assert_eq!(client.peer_id(), PeerId::new([1; 32]));
        assert!(client.get_cached_summaries(&target).await.is_empty());
    }

    #[tokio::test]
    async fn subscribe_to_shard_is_idempotent() {
        let client = client();
        let target = [3; 32];

        let first = client.subscribe_to_shard(&target).await.unwrap();
        let second = client.subscribe_to_shard(&target).await.unwrap();

        assert_eq!(first, calculate_shard(&target));
        assert_eq!(first, second);
        assert_eq!(client.subscriptions.read().await.len(), 1);
    }

    #[tokio::test]
    async fn publish_provider_summary_serializes_to_shard_topic() {
        let mock = MockPubSub::default();
        let published = mock.published.clone();
        let client = RendezvousClient::new(
            PeerId::new([1; 32]),
            Arc::new(RwLock::new(Box::new(MockTransport))),
            Arc::new(RwLock::new(Box::new(mock))),
        );
        let target = [4; 32];
        let published_summary = summary(target, 8, 60_000);

        client
            .publish_provider_summary(published_summary.clone())
            .await
            .unwrap();

        let records = published.lock().unwrap();
        assert_eq!(records.len(), 1);
        let shard = calculate_shard(&target);
        let shard_name = format!("rendezvous_shard_{}", shard);
        let hash = blake3::hash(shard_name.as_bytes());
        assert_eq!(records[0].0, TopicId::new(*hash.as_bytes()));
        let decoded: ProviderSummary = ciborium::de::from_reader(&records[0].1[..]).unwrap();
        assert_eq!(decoded.target, target);
        assert_eq!(decoded.provider, published_summary.provider);
    }

    #[tokio::test]
    async fn process_incoming_summary_caches_and_replaces_by_provider() {
        let client = client();
        let target = [5; 32];
        let first = summary(target, 11, 60_000).with_manifest_version(1);
        let replacement = summary(target, 11, 60_000).with_manifest_version(2);
        let other = summary(target, 12, 60_000);

        client
            .process_incoming_summary(&target, encode(&first))
            .await
            .unwrap();
        client
            .process_incoming_summary(&target, encode(&replacement))
            .await
            .unwrap();
        client
            .process_incoming_summary(&target, encode(&other))
            .await
            .unwrap();

        let cached = client.get_cached_summaries(&target).await;
        assert_eq!(cached.len(), 2);
        assert!(cached.iter().any(|s| s.manifest_ver == Some(2)));
        assert!(!cached.iter().any(|s| s.manifest_ver == Some(1)));
    }

    #[tokio::test]
    async fn process_incoming_summary_rejects_invalid_and_mismatched_payloads() {
        let client = client();
        let target = [6; 32];
        let wrong_target = [7; 32];

        assert!(client
            .process_incoming_summary(&target, Bytes::from_static(b"not cbor"))
            .await
            .is_err());
        assert!(client
            .process_incoming_summary(&target, encode(&summary(wrong_target, 9, 60_000)))
            .await
            .is_err());
        assert!(client.get_cached_summaries(&target).await.is_empty());
    }

    #[tokio::test]
    async fn get_providers_filters_expired_and_sorts_by_remaining_validity() {
        let client = client();
        let target = [8; 32];
        let mut expired = summary(target, 20, 1);
        expired.exp = 1;
        let short = summary(target, 21, 10_000);
        let long = summary(target, 22, 30_000);
        client
            .cached_summaries
            .write()
            .await
            .insert(target, vec![short, expired, long]);

        let providers = client.get_providers_for_target(&target).await;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider, PeerId::new([22; 32]));
        assert_eq!(providers[1].provider, PeerId::new([21; 32]));
    }

    #[tokio::test]
    async fn collector_requires_subscription_and_duplicate_start_is_noop() {
        let client = client();
        let target = [9; 32];

        assert!(client.start_collecting_for_target(target).await.is_err());
        client.subscribe_to_shard(&target).await.unwrap();
        client.start_collecting_for_target(target).await.unwrap();
        client.start_collecting_for_target(target).await.unwrap();

        let collectors = client.active_collectors.read().await;
        assert_eq!(collectors.len(), 1);
        for handle in collectors.values() {
            handle.abort();
        }
    }

    #[test]
    fn mock_transport_has_stable_local_peer_id() {
        assert_eq!(MockTransport.local_peer_id(), PeerId::new([7; 32]));
        let _addr = std::net::SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    }
}
