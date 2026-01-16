#![warn(missing_docs)]

//! MLS group management
//!
//! Manages MLS groups for secure group communication

use saorsa_gossip_types::TopicId;
use serde::{Deserialize, Serialize};

/// MLS cipher suite (placeholder for saorsa-mls integration)
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CipherSuite {
    /// ML-KEM-768 + ML-DSA-65 (default PQC suite)
    MlKem768MlDsa65,
}

/// MLS group context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupContext {
    /// Group/Topic identifier
    pub topic_id: TopicId,
    /// Cipher suite
    pub cipher_suite: CipherSuite,
    /// Current epoch
    pub epoch: u64,
    /// Optional MLS exporter secret used for presence beacons
    presence_exporter: Option<[u8; 32]>,
}

impl GroupContext {
    /// Create a new group context
    pub fn new(topic_id: TopicId) -> Self {
        Self {
            topic_id,
            cipher_suite: CipherSuite::MlKem768MlDsa65,
            epoch: 0,
            presence_exporter: None,
        }
    }

    /// Create a group context with a known MLS exporter secret
    pub fn with_presence_exporter(topic_id: TopicId, exporter_secret: [u8; 32]) -> Self {
        Self {
            presence_exporter: Some(exporter_secret),
            ..Self::new(topic_id)
        }
    }

    /// Create a new group context from an entity identifier.
    ///
    /// This is a convenience constructor that derives the TopicId from the entity_id.
    /// Equivalent to `GroupContext::new(TopicId::from_entity(entity_id))`
    ///
    /// Accepts any type that can be converted to a byte slice (see `TopicId::from_entity`).
    ///
    /// # Arguments
    /// * `entity_id` - Identifier for the entity (channel, project, org, binary ID, etc.)
    ///
    /// # Returns
    /// * `Self` - GroupContext with topic_id derived from entity_id
    pub fn from_entity(entity_id: impl AsRef<[u8]>) -> Self {
        let topic_id = TopicId::from_entity(entity_id);
        Self::new(topic_id)
    }

    /// Set the MLS exporter secret used for presence beacons.
    pub fn set_presence_exporter(&mut self, exporter_secret: [u8; 32]) {
        self.presence_exporter = Some(exporter_secret);
    }

    /// Get the configured MLS exporter secret, if any.
    pub fn presence_exporter(&self) -> Option<[u8; 32]> {
        self.presence_exporter
    }

    /// Advance to next epoch
    pub fn next_epoch(&mut self) {
        self.epoch += 1;
    }

    /// Derive exporter secret for presence tags
    ///
    /// Uses BLAKE3 keyed hash to derive presence tags from MLS exporter secret.
    /// Per SPEC2 §10, presence tags rotate based on time_slice for privacy.
    ///
    /// # Arguments
    /// * `exporter_context` - MLS exporter secret (32 bytes)
    /// * `user_id` - User's PeerId bytes
    /// * `time_slice` - Time-based rotation parameter (e.g., hour since epoch)
    ///
    /// # Returns
    /// Derived presence tag (32 bytes)
    pub fn derive_presence_secret(
        exporter_context: &[u8; 32],
        user_id: &[u8],
        time_slice: u64,
    ) -> [u8; 32] {
        // KDF(exporter_secret, user_id || time_slice) using BLAKE3 keyed hash
        let mut hasher = blake3::Hasher::new_keyed(exporter_context);
        hasher.update(user_id);
        hasher.update(&time_slice.to_le_bytes());
        let hash = hasher.finalize();
        let mut tag = [0u8; 32];
        tag.copy_from_slice(&hash.as_bytes()[..32]);
        tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_context() {
        let topic = TopicId::new([1u8; 32]);
        let mut ctx = GroupContext::new(topic);

        assert_eq!(ctx.epoch, 0);
        assert!(ctx.presence_exporter().is_none());
        ctx.next_epoch();
        assert_eq!(ctx.epoch, 1);
    }

    #[test]
    fn test_group_context_with_presence_exporter() {
        let topic = TopicId::new([9u8; 32]);
        let secret = [7u8; 32];
        let ctx = GroupContext::with_presence_exporter(topic, secret);
        assert_eq!(ctx.presence_exporter(), Some(secret));
    }

    #[test]
    fn test_group_context_from_entity() {
        let entity_id = "channel-general";
        let ctx = GroupContext::from_entity(entity_id);

        assert_eq!(ctx.epoch, 0);
        assert!(matches!(ctx.cipher_suite, CipherSuite::MlKem768MlDsa65));
    }

    #[test]
    fn test_group_context_from_entity_deterministic() {
        // Same entity ID should produce same topic ID
        let entity_id = "project-alpha";
        let ctx1 = GroupContext::from_entity(entity_id);
        let ctx2 = GroupContext::from_entity(entity_id);

        assert_eq!(
            ctx1.topic_id, ctx2.topic_id,
            "Same entity should produce same topic"
        );
    }

    #[test]
    fn test_group_context_from_entity_vs_new() {
        // from_entity should be equivalent to new(TopicId::from_entity(...))
        let entity_id = "org-acme";
        let ctx_from_entity = GroupContext::from_entity(entity_id);
        let topic = TopicId::from_entity(entity_id);
        let ctx_from_new = GroupContext::new(topic);

        assert_eq!(ctx_from_entity.topic_id, ctx_from_new.topic_id);
        assert_eq!(ctx_from_entity.epoch, ctx_from_new.epoch);
        assert_eq!(ctx_from_entity.presence_exporter(), None);
    }

    #[test]
    fn test_set_presence_exporter() {
        let topic = TopicId::new([5u8; 32]);
        let mut ctx = GroupContext::new(topic);
        assert!(ctx.presence_exporter().is_none());
        let secret = [11u8; 32];
        ctx.set_presence_exporter(secret);
        assert_eq!(ctx.presence_exporter(), Some(secret));
    }

    #[test]
    fn test_derive_presence_secret_deterministic() {
        let exporter = [1u8; 32];
        let user_id = [2u8; 32];
        let time_slice = 12345u64;

        let tag1 = GroupContext::derive_presence_secret(&exporter, &user_id, time_slice);
        let tag2 = GroupContext::derive_presence_secret(&exporter, &user_id, time_slice);

        assert_eq!(tag1, tag2, "Same inputs should produce same tag");
    }

    #[test]
    fn test_derive_presence_secret_rotation() {
        let exporter = [1u8; 32];
        let user_id = [2u8; 32];

        let tag1 = GroupContext::derive_presence_secret(&exporter, &user_id, 1000);
        let tag2 = GroupContext::derive_presence_secret(&exporter, &user_id, 1001);

        assert_ne!(
            tag1, tag2,
            "Different time slices should produce different tags"
        );
    }

    #[test]
    fn test_derive_presence_secret_user_unique() {
        let exporter = [1u8; 32];
        let user1 = [1u8; 32];
        let user2 = [2u8; 32];
        let time_slice = 12345u64;

        let tag1 = GroupContext::derive_presence_secret(&exporter, &user1, time_slice);
        let tag2 = GroupContext::derive_presence_secret(&exporter, &user2, time_slice);

        assert_ne!(tag1, tag2, "Different users should produce different tags");
    }

    #[test]
    fn test_derive_presence_secret_exporter_unique() {
        let exporter1 = [1u8; 32];
        let exporter2 = [2u8; 32];
        let user_id = [3u8; 32];
        let time_slice = 12345u64;

        let tag1 = GroupContext::derive_presence_secret(&exporter1, &user_id, time_slice);
        let tag2 = GroupContext::derive_presence_secret(&exporter2, &user_id, time_slice);

        assert_ne!(
            tag1, tag2,
            "Different exporters should produce different tags"
        );
    }
}
