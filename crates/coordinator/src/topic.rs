//! Coordinator topic for advertisement gossip

use saorsa_gossip_types::TopicId;

/// Well-known topic for coordinator advertisements
///
/// All coordinator adverts are gossiped on this topic per SPEC2 §8.
/// The topic ID is derived deterministically from a fixed string.
pub fn coordinator_topic() -> TopicId {
    // BLAKE3("saorsa-coordinator-topic")
    let hash = blake3::hash(b"saorsa-coordinator-topic");
    TopicId::new(*hash.as_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_topic_deterministic() {
        let topic1 = coordinator_topic();
        let topic2 = coordinator_topic();
        assert_eq!(topic1, topic2, "Topic should be deterministic");
    }

    #[test]
    fn test_coordinator_topic_unique() {
        let coordinator = coordinator_topic();

        // Compare with a different topic
        let other_hash = blake3::hash(b"different-topic");
        let other_topic = TopicId::new(*other_hash.as_bytes());

        assert_ne!(
            coordinator, other_topic,
            "Coordinator topic should be unique"
        );
    }
}
