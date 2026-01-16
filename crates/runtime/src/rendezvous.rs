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

        scored.sort_by(|a, b| b.1.cmp(&a.1));
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
