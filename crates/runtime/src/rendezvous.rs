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
    use crate::rendezvous::RendezvousClient;
    use anyhow::Result;
    use async_trait::async_trait;
    use saorsa_gossip_pubsub::PubSub as PubSubTrait;
    use saorsa_gossip_rendezvous::{calculate_shard, ProviderSummary};
    use saorsa_gossip_transport::{GossipStreamType, GossipTransport as GossipTransportTrait};
    use saorsa_gossip_types::{PeerId, TopicId};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::{mpsc, RwLock as TokioRwLock};

    // ─── Mock implementations ──────────────────────────────────────────

    struct MockTransport {
        send_count: Arc<AtomicUsize>,
        fail_send: Arc<AtomicBool>,
        local_id: PeerId,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                send_count: Arc::new(AtomicUsize::new(0)),
                fail_send: Arc::new(AtomicBool::new(false)),
                local_id: PeerId::new([1u8; 32]),
            }
        }
    }

    #[async_trait]
    impl GossipTransportTrait for MockTransport {
        async fn dial(&self, _peer: PeerId, _addr: std::net::SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn dial_bootstrap(&self, _addr: std::net::SocketAddr) -> Result<PeerId> {
            Ok(PeerId::new([2u8; 32]))
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
            if self.fail_send.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("mock send failure"));
            }
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
            Err(anyhow::anyhow!("not implemented"))
        }
        fn local_peer_id(&self) -> PeerId {
            self.local_id
        }
    }

    struct MockPubSub {
        published: Arc<TokioRwLock<Vec<(TopicId, Bytes)>>>,
    }

    impl MockPubSub {
        fn new() -> Self {
            Self {
                published: Arc::new(TokioRwLock::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl PubSubTrait for MockPubSub {
        async fn publish(&self, topic: TopicId, data: Bytes) -> Result<()> {
            self.published.write().await.push((topic, data));
            Ok(())
        }
        fn subscribe(&self, _topic: TopicId) -> mpsc::UnboundedReceiver<(PeerId, Bytes)> {
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

    // ─── Helpers ────────────────────────────────────────────────────────

    fn make_rendezvous_client(
        peer_id: PeerId,
        transport: MockTransport,
        pubsub: MockPubSub,
    ) -> RendezvousClient {
        let t: Arc<TokioRwLock<Box<dyn GossipTransportTrait>>> =
            Arc::new(TokioRwLock::new(Box::new(transport)));
        let p: Arc<TokioRwLock<Box<dyn PubSubTrait>>> =
            Arc::new(TokioRwLock::new(Box::new(pubsub)));
        RendezvousClient::new(peer_id, t, p)
    }

    fn make_provider_summary(target: [u8; 32], provider: PeerId) -> ProviderSummary {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        ProviderSummary {
            v: 1,
            target,
            provider,
            cap: Vec::new(),
            have_root: true,
            manifest_ver: None,
            summary: None,
            exp: now + 60_000, // valid for 60s
            extensions: None,
            sig: Vec::new(),
        }
    }

    fn make_expired_summary(target: [u8; 32], provider: PeerId) -> ProviderSummary {
        ProviderSummary {
            v: 1,
            target,
            provider,
            cap: Vec::new(),
            have_root: true,
            manifest_ver: None,
            summary: None,
            exp: 0, // already expired
            extensions: None,
            sig: Vec::new(),
        }
    }

    // ─── Tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_rendezvous_client_new() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);
        assert_eq!(client.peer_id(), peer);
    }

    #[tokio::test]
    async fn test_get_cached_summaries_empty() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let summaries = client.get_cached_summaries(&target).await;
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_to_shard() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let expected_shard = calculate_shard(&target);

        let result = client.subscribe_to_shard(&target).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_shard);
    }

    #[tokio::test]
    async fn test_subscribe_to_shard_duplicate() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let expected_shard = calculate_shard(&target);

        // First subscription
        let shard1 = client.subscribe_to_shard(&target).await.unwrap();
        // Second subscription should return same shard without re-subscribing
        let shard2 = client.subscribe_to_shard(&target).await.unwrap();
        assert_eq!(shard1, expected_shard);
        assert_eq!(shard2, expected_shard);
    }

    #[tokio::test]
    async fn test_subscribe_different_targets() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target1: [u8; 32] = [1u8; 32];
        let target2: [u8; 32] = [2u8; 32];

        let shard1 = client.subscribe_to_shard(&target1).await.unwrap();
        let shard2 = client.subscribe_to_shard(&target2).await.unwrap();

        // Different targets should map to different shards (very likely)
        assert_eq!(shard1, calculate_shard(&target1));
        assert_eq!(shard2, calculate_shard(&target2));
    }

    #[tokio::test]
    async fn test_publish_provider_summary() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let published = pubsub.published.clone();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let summary = make_provider_summary(target, peer);

        let result = client.publish_provider_summary(summary).await;
        assert!(result.is_ok());

        // Should have published to pubsub
        let pub_list = published.read().await;
        assert_eq!(pub_list.len(), 1);
    }

    #[tokio::test]
    async fn test_get_providers_for_target_empty() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let providers = client.get_providers_for_target(&target).await;
        assert!(providers.is_empty());
    }

    #[tokio::test]
    async fn test_process_incoming_summary() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let provider_id = PeerId::new([7u8; 32]);
        let summary = make_provider_summary(target, provider_id);

        // Serialize to bytes
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&summary, &mut buf).unwrap();
        let msg_bytes = Bytes::from(buf);

        let result = client.process_incoming_summary(&target, msg_bytes).await;
        assert!(result.is_ok());

        // Should be cached now
        let cached = client.get_cached_summaries(&target).await;
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].provider, provider_id);
    }

    #[tokio::test]
    async fn test_process_incoming_summary_target_mismatch() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let wrong_target: [u8; 32] = [99u8; 32];
        let summary = make_provider_summary(wrong_target, PeerId::new([7u8; 32]));

        let mut buf = Vec::new();
        ciborium::ser::into_writer(&summary, &mut buf).unwrap();
        let msg_bytes = Bytes::from(buf);

        let result = client.process_incoming_summary(&target, msg_bytes).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_process_incoming_summary_invalid_cbor() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let msg_bytes = Bytes::from(b"not valid cbor".to_vec());

        let result = client.process_incoming_summary(&target, msg_bytes).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_process_incoming_summary_update_existing() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let provider_id = PeerId::new([7u8; 32]);

        // Insert first summary
        let summary1 = make_provider_summary(target, provider_id);
        let mut buf1 = Vec::new();
        ciborium::ser::into_writer(&summary1, &mut buf1).unwrap();
        client
            .process_incoming_summary(&target, Bytes::from(buf1))
            .await
            .unwrap();

        // Insert updated summary from same provider
        let summary2 = make_provider_summary(target, provider_id);
        let mut buf2 = Vec::new();
        ciborium::ser::into_writer(&summary2, &mut buf2).unwrap();
        client
            .process_incoming_summary(&target, Bytes::from(buf2))
            .await
            .unwrap();

        // Should still be only 1 entry (updated in place)
        let cached = client.get_cached_summaries(&target).await;
        assert_eq!(cached.len(), 1);
    }

    #[tokio::test]
    async fn test_get_providers_for_target_valid() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let provider1 = PeerId::new([7u8; 32]);
        let provider2 = PeerId::new([8u8; 32]);

        // Insert two valid summaries
        let s1 = make_provider_summary(target, provider1);
        let mut buf1 = Vec::new();
        ciborium::ser::into_writer(&s1, &mut buf1).unwrap();
        client
            .process_incoming_summary(&target, Bytes::from(buf1))
            .await
            .unwrap();

        let s2 = make_provider_summary(target, provider2);
        let mut buf2 = Vec::new();
        ciborium::ser::into_writer(&s2, &mut buf2).unwrap();
        client
            .process_incoming_summary(&target, Bytes::from(buf2))
            .await
            .unwrap();

        let providers = client.get_providers_for_target(&target).await;
        assert_eq!(providers.len(), 2);
    }

    #[tokio::test]
    async fn test_get_providers_for_target_filters_expired() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        let provider1 = PeerId::new([7u8; 32]);
        let provider2 = PeerId::new([8u8; 32]);

        // Insert one valid, one expired
        let s1 = make_provider_summary(target, provider1);
        let mut buf1 = Vec::new();
        ciborium::ser::into_writer(&s1, &mut buf1).unwrap();
        client
            .process_incoming_summary(&target, Bytes::from(buf1))
            .await
            .unwrap();

        let s2 = make_expired_summary(target, provider2);
        let mut buf2 = Vec::new();
        ciborium::ser::into_writer(&s2, &mut buf2).unwrap();
        client
            .process_incoming_summary(&target, Bytes::from(buf2))
            .await
            .unwrap();

        // Only the valid one should be returned
        let providers = client.get_providers_for_target(&target).await;
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].provider, provider1);
    }

    #[tokio::test]
    async fn test_start_collecting_for_target_not_subscribed() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        // Not subscribed to any shard → error
        let result = client.start_collecting_for_target(target).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_start_collecting_for_target_success() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        // Subscribe first
        client.subscribe_to_shard(&target).await.unwrap();

        // Now start collecting should succeed
        let result = client.start_collecting_for_target(target).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_start_collecting_for_target_duplicate() {
        let peer = PeerId::new([1u8; 32]);
        let transport = MockTransport::new();
        let pubsub = MockPubSub::new();
        let client = make_rendezvous_client(peer, transport, pubsub);

        let target: [u8; 32] = [42u8; 32];
        client.subscribe_to_shard(&target).await.unwrap();

        // Start collecting twice — second call should be a no-op (OK)
        let result1 = client.start_collecting_for_target(target).await;
        let result2 = client.start_collecting_for_target(target).await;
        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }
}
