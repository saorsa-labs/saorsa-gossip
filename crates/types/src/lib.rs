#![warn(missing_docs)]

//! Core types for Saorsa Gossip overlay network
//!
//! This crate provides the fundamental types used throughout the gossip overlay:
//! - `TopicId`: 32-byte topic identifier for MLS groups
//! - `PeerId`: 32-byte peer identifier derived from ML-DSA public key
//! - Wire format types for network messages

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// Domain separator for PeerId derivation (must match ant-quic)
const PEER_ID_DOMAIN_SEPARATOR: &[u8] = b"AUTONOMI_PEER_ID_V2:";

/// 32-byte topic identifier, one per MLS group
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TopicId([u8; 32]);

impl TopicId {
    /// Create a new TopicId from a 32-byte array
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create a TopicId from an entity identifier string
    ///
    /// Uses BLAKE3 to hash the entity_id string into a 32-byte topic identifier.
    /// This ensures deterministic topic IDs for the same entity across different nodes.
    ///
    /// # Arguments
    /// * `entity_id` - String identifier for the entity (channel, project, org, etc.)
    ///
    /// # Returns
    /// * `Self` - TopicId derived from the entity_id
    pub fn from_entity(entity_id: &str) -> Self {
        let hash = blake3::hash(entity_id.as_bytes());
        Self(*hash.as_bytes())
    }

    /// Get the underlying bytes
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to byte array
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for TopicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TopicId({})", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for TopicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(&self.0[..8]))
    }
}

/// 32-byte peer identifier: SHA-256(domain_separator || ML-DSA pubkey)
///
/// The PeerId derivation MUST match ant-quic's derivation to ensure
/// consistent peer identification across transport and application layers.
/// See: ant-quic/src/crypto/raw_public_keys/pqc.rs:derive_peer_id_from_public_key
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId([u8; 32]);

impl PeerId {
    /// Create a new PeerId from a 32-byte array
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create a PeerId from a public key using SHA-256 with domain separator
    ///
    /// This derivation matches ant-quic's `derive_peer_id_from_public_key` exactly:
    /// - Domain separator: `b"AUTONOMI_PEER_ID_V2:"`
    /// - Hash function: SHA-256
    /// - Input: domain_separator || public_key_bytes
    ///
    /// This ensures the PeerId is consistent between the transport layer (ant-quic)
    /// and the application layer (saorsa-gossip).
    pub fn from_pubkey(pubkey: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PEER_ID_DOMAIN_SEPARATOR);
        hasher.update(pubkey);
        let hash = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hash);
        Self(bytes)
    }

    /// Get the underlying bytes
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to byte array
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({})", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(&self.0[..8]))
    }
}

/// Message kind enumeration for wire protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MessageKind {
    /// Eager push along spanning tree
    Eager = 0,
    /// IHAVE digest to non-tree links
    IHave = 1,
    /// IWANT request for payload
    IWant = 2,
    /// SWIM ping probe
    Ping = 3,
    /// SWIM acknowledgment
    Ack = 4,
    /// FOAF find user query
    Find = 5,
    /// Presence beacon
    Presence = 6,
    /// Anti-entropy reconciliation
    AntiEntropy = 7,
    /// HyParView shuffle
    Shuffle = 8,
}

impl MessageKind {
    /// Convert u8 to MessageKind
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Eager),
            1 => Some(Self::IHave),
            2 => Some(Self::IWant),
            3 => Some(Self::Ping),
            4 => Some(Self::Ack),
            5 => Some(Self::Find),
            6 => Some(Self::Presence),
            7 => Some(Self::AntiEntropy),
            8 => Some(Self::Shuffle),
            _ => None,
        }
    }

    /// Convert to u8
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Wire format header for control frames (ML-DSA signed)
/// Format: ver:u8, topic:\[u8;32\], msg_id:\[u8;32\], kind:u8, hop:u8, ttl:u8
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageHeader {
    /// Protocol version
    pub version: u8,
    /// Topic identifier
    pub topic: TopicId,
    /// Message ID: BLAKE3(topic || epoch || signer || payload_hash)\[:32\]
    pub msg_id: [u8; 32],
    /// Message kind
    pub kind: MessageKind,
    /// Current hop count
    pub hop: u8,
    /// Time-to-live
    pub ttl: u8,
}

impl MessageHeader {
    /// Create a new message header
    pub fn new(topic: TopicId, kind: MessageKind, ttl: u8) -> Self {
        Self {
            version: 1,
            topic,
            msg_id: [0u8; 32], // To be filled with proper hash
            kind,
            hop: 0,
            ttl,
        }
    }

    /// Calculate message ID from components
    pub fn calculate_msg_id(
        topic: &TopicId,
        epoch: u64,
        signer: &PeerId,
        payload_hash: &[u8; 32],
    ) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(topic.as_bytes());
        hasher.update(&epoch.to_le_bytes());
        hasher.update(signer.as_bytes());
        hasher.update(payload_hash);
        let hash = hasher.finalize();
        let mut msg_id = [0u8; 32];
        msg_id.copy_from_slice(&hash.as_bytes()[..32]);
        msg_id
    }

    /// Increment hop count
    pub fn increment_hop(&mut self) -> Result<(), anyhow::Error> {
        if self.hop == 255 {
            return Err(anyhow::anyhow!("Hop count overflow"));
        }
        self.hop += 1;
        Ok(())
    }

    /// Decrement TTL
    pub fn decrement_ttl(&mut self) -> Result<(), anyhow::Error> {
        if self.ttl == 0 {
            return Err(anyhow::anyhow!("TTL expired"));
        }
        self.ttl -= 1;
        Ok(())
    }
}

/// Presence record (MLS-encrypted)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceRecord {
    /// Presence tag: KDF(exporter_secret, user_id || time_slice)
    pub presence_tag: [u8; 32],
    /// Address hints for connectivity
    pub addr_hints: Vec<String>,
    /// Timestamp when presence started
    pub since: u64,
    /// Expiration timestamp
    pub expires: u64,
    /// Sequence number for updates
    pub seq: u64,
    /// Optional four-word identity for FOAF discovery
    pub four_words: Option<String>,
}

impl PresenceRecord {
    /// Create a new presence record
    pub fn new(presence_tag: [u8; 32], addr_hints: Vec<String>, ttl_seconds: u64) -> Self {
        let now = unix_secs();
        Self {
            presence_tag,
            addr_hints,
            since: now,
            expires: now + ttl_seconds,
            seq: 0,
            four_words: None,
        }
    }

    /// Create a new presence record with four-word identity
    pub fn with_four_words(
        presence_tag: [u8; 32],
        addr_hints: Vec<String>,
        ttl_seconds: u64,
        four_words: String,
    ) -> Self {
        let now = unix_secs();
        Self {
            presence_tag,
            addr_hints,
            since: now,
            expires: now + ttl_seconds,
            seq: 0,
            four_words: Some(four_words),
        }
    }

    /// Check if the presence record is expired
    pub fn is_expired(&self) -> bool {
        unix_secs() >= self.expires
    }
}

/// FOAF (Friend-of-a-Friend) query message
///
/// Used to discover contacts through the social graph without DHT.
/// Query propagates up to max_hops (typically 2) with cycle detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoafQuery {
    /// Unique query ID for deduplication
    pub query_id: [u8; 16],
    /// Four-word address being searched for
    pub target_four_words: String,
    /// Hop count (starts at 0, incremented at each forward)
    pub hop: u8,
    /// Maximum hops allowed (typically 2)
    pub max_hops: u8,
    /// Path of peer IDs visited (for cycle detection)
    pub visited: Vec<PeerId>,
    /// Query originator (for response routing)
    pub originator: PeerId,
}

/// FOAF query response
///
/// Sent back to query originator when target is found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoafResponse {
    /// Query ID this responds to
    pub query_id: [u8; 16],
    /// Found peer ID
    pub peer_id: PeerId,
    /// Address hints for connectivity
    pub addr_hints: Vec<String>,
    /// Number of hops traversed to find
    pub hops: u8,
}

/// Get current time as Unix timestamp in seconds
///
/// Returns 0 if system time is before Unix epoch (shouldn't happen in practice).
pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Get current time as Unix timestamp in milliseconds
///
/// Returns 0 if system time is before Unix epoch (shouldn't happen in practice).
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_id_from_pubkey() {
        let pubkey = b"test_public_key_data";
        let peer_id = PeerId::from_pubkey(pubkey);
        assert_eq!(peer_id.as_bytes().len(), 32);
    }

    #[test]
    fn test_topic_id_creation() {
        let bytes = [42u8; 32];
        let topic = TopicId::new(bytes);
        assert_eq!(topic.to_bytes(), bytes);
    }

    #[test]
    fn test_message_kind_conversion() {
        assert_eq!(MessageKind::from_u8(0), Some(MessageKind::Eager));
        assert_eq!(MessageKind::from_u8(255), None);
        assert_eq!(MessageKind::Eager.to_u8(), 0);
    }

    #[test]
    fn test_message_header_hop_increment() {
        let mut header = MessageHeader::new(TopicId::new([0u8; 32]), MessageKind::Eager, 10);
        assert_eq!(header.hop, 0);
        header.increment_hop().ok();
        assert_eq!(header.hop, 1);
    }

    #[test]
    fn test_message_header_ttl_decrement() {
        let mut header = MessageHeader::new(TopicId::new([0u8; 32]), MessageKind::Eager, 10);
        assert_eq!(header.ttl, 10);
        header.decrement_ttl().ok();
        assert_eq!(header.ttl, 9);
    }

    #[test]
    fn test_presence_record_expiry() {
        let record = PresenceRecord::new([0u8; 32], vec![], 0);
        // Should be expired immediately with 0 TTL
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(record.is_expired());
    }

    #[test]
    fn test_calculate_msg_id_deterministic() {
        let topic = TopicId::new([1u8; 32]);
        let epoch = 12345u64;
        let signer = PeerId::new([2u8; 32]);
        let payload_hash = [3u8; 32];

        // Calculate message ID twice - should be identical
        let msg_id1 = MessageHeader::calculate_msg_id(&topic, epoch, &signer, &payload_hash);
        let msg_id2 = MessageHeader::calculate_msg_id(&topic, epoch, &signer, &payload_hash);

        assert_eq!(
            msg_id1, msg_id2,
            "Message ID calculation should be deterministic"
        );
    }

    #[test]
    fn test_calculate_msg_id_unique() {
        let topic = TopicId::new([1u8; 32]);
        let epoch = 12345u64;
        let signer = PeerId::new([2u8; 32]);
        let payload_hash1 = [3u8; 32];
        let payload_hash2 = [4u8; 32];

        // Different payload hashes should produce different message IDs
        let msg_id1 = MessageHeader::calculate_msg_id(&topic, epoch, &signer, &payload_hash1);
        let msg_id2 = MessageHeader::calculate_msg_id(&topic, epoch, &signer, &payload_hash2);

        assert_ne!(
            msg_id1, msg_id2,
            "Different payloads should produce different message IDs"
        );
    }

    #[test]
    fn test_calculate_msg_id_epoch_sensitivity() {
        let topic = TopicId::new([1u8; 32]);
        let epoch1 = 12345u64;
        let epoch2 = 12346u64;
        let signer = PeerId::new([2u8; 32]);
        let payload_hash = [3u8; 32];

        // Different epochs should produce different message IDs
        let msg_id1 = MessageHeader::calculate_msg_id(&topic, epoch1, &signer, &payload_hash);
        let msg_id2 = MessageHeader::calculate_msg_id(&topic, epoch2, &signer, &payload_hash);

        assert_ne!(
            msg_id1, msg_id2,
            "Different epochs should produce different message IDs"
        );
    }

    #[test]
    fn test_calculate_msg_id_signer_sensitivity() {
        let topic = TopicId::new([1u8; 32]);
        let epoch = 12345u64;
        let signer1 = PeerId::new([2u8; 32]);
        let signer2 = PeerId::new([3u8; 32]);
        let payload_hash = [4u8; 32];

        // Different signers should produce different message IDs
        let msg_id1 = MessageHeader::calculate_msg_id(&topic, epoch, &signer1, &payload_hash);
        let msg_id2 = MessageHeader::calculate_msg_id(&topic, epoch, &signer2, &payload_hash);

        assert_ne!(
            msg_id1, msg_id2,
            "Different signers should produce different message IDs"
        );
    }

    #[test]
    fn test_calculate_msg_id_topic_sensitivity() {
        let topic1 = TopicId::new([1u8; 32]);
        let topic2 = TopicId::new([2u8; 32]);
        let epoch = 12345u64;
        let signer = PeerId::new([3u8; 32]);
        let payload_hash = [4u8; 32];

        // Different topics should produce different message IDs
        let msg_id1 = MessageHeader::calculate_msg_id(&topic1, epoch, &signer, &payload_hash);
        let msg_id2 = MessageHeader::calculate_msg_id(&topic2, epoch, &signer, &payload_hash);

        assert_ne!(
            msg_id1, msg_id2,
            "Different topics should produce different message IDs"
        );
    }

    #[test]
    fn test_message_header_with_calculated_id() {
        let topic = TopicId::new([1u8; 32]);
        let epoch = 12345u64;
        let signer = PeerId::new([2u8; 32]);
        let payload_hash = [3u8; 32];

        let mut header = MessageHeader::new(topic, MessageKind::Eager, 10);
        header.msg_id = MessageHeader::calculate_msg_id(&topic, epoch, &signer, &payload_hash);

        // Verify msg_id is not all zeros
        assert_ne!(header.msg_id, [0u8; 32], "Message ID should be calculated");
        assert_eq!(header.msg_id.len(), 32, "Message ID should be 32 bytes");
    }

    #[test]
    fn test_topic_id_from_entity() {
        let entity_id = "channel-123";
        let topic = TopicId::from_entity(entity_id);

        // Should produce a valid 32-byte topic ID
        assert_eq!(topic.as_bytes().len(), 32);
    }

    #[test]
    fn test_topic_id_from_entity_deterministic() {
        // Same entity ID should always produce the same topic
        let entity_id = "project-xyz";
        let topic1 = TopicId::from_entity(entity_id);
        let topic2 = TopicId::from_entity(entity_id);

        assert_eq!(topic1, topic2, "Same entity should produce same topic");
    }

    #[test]
    fn test_topic_id_from_entity_unique() {
        // Different entity IDs should produce different topics
        let topic1 = TopicId::from_entity("channel-A");
        let topic2 = TopicId::from_entity("channel-B");

        assert_ne!(
            topic1, topic2,
            "Different entities should produce different topics"
        );
    }

    #[test]
    fn test_topic_id_from_entity_hex() {
        // Should accept hex-encoded entity IDs
        let hex_entity = "deadbeef12345678";
        let topic = TopicId::from_entity(hex_entity);

        assert_eq!(topic.as_bytes().len(), 32);
    }
}
