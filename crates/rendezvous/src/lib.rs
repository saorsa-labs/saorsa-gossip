#![warn(missing_docs)]

//! Rendezvous Shards for global findability without DNS/DHT
//!
//! Implements SPEC2 §9 Rendezvous Shards for publisher discovery.
//!
//! ## Overview
//!
//! Rendezvous shards provide global findability without requiring:
//! - DNS infrastructure
//! - DHT (Distributed Hash Table)
//! - Centralized directory services
//!
//! ## How it works
//!
//! 1. **Shard Space**: k=16 → 65,536 shards
//! 2. **Shard Calculation**: `shard = BLAKE3("saorsa-rendezvous" || target_id) & 0xFFFF`
//! 3. **Publishers**: Gossip Provider Summaries to target's shard
//! 4. **Seekers**: Subscribe to relevant shards, fetch from top providers
//!
//! ## Example
//!
//! ```rust
//! use saorsa_gossip_rendezvous::{calculate_shard, ProviderSummary, Capability};
//! use saorsa_gossip_types::PeerId;
//!
//! # fn main() -> anyhow::Result<()> {
//! // Calculate shard for a target
//! let target_id = [1u8; 32];
//! let shard = calculate_shard(&target_id);
//! assert!(shard <= u16::MAX);
//!
//! // Create a provider summary
//! let provider = PeerId::new([2u8; 32]);
//! let summary = ProviderSummary::new(
//!     target_id,
//!     provider,
//!     vec![Capability::Site],
//!     3600_000, // 1 hour validity
//! );
//!
//! assert!(summary.is_valid());
//! # Ok(())
//! # }
//! ```

use saorsa_gossip_types::PeerId;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

/// Shard space size: k=16 → 2^16 = 65,536 shards per SPEC2 §9
pub const SHARD_BITS: u32 = 16;
/// Total number of shards: 2^16 = 65,536
pub const SHARD_COUNT: u32 = 1 << SHARD_BITS; // 65,536
/// Bitmask for shard calculation: 0xFFFF
pub const SHARD_MASK: u32 = SHARD_COUNT - 1; // 0xFFFF

/// Rendezvous prefix for shard calculation per SPEC2 §9
const RENDEZVOUS_PREFIX: &[u8] = b"saorsa-rendezvous";

/// Shard ID (0..65,535)
pub type ShardId = u16;

fn current_time_millis() -> u64 {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as u64,
        Err(_) => Duration::ZERO.as_millis() as u64,
    }
}

/// Calculate the rendezvous shard for a target ID per SPEC2 §9
///
/// Formula: `shard = BLAKE3("saorsa-rendezvous" || target_id) & 0xFFFF`
///
/// # Arguments
/// * `target_id` - The target identifier (32 bytes)
///
/// # Returns
/// Shard ID in range [0, 65535]
///
/// # Example
/// ```
/// use saorsa_gossip_rendezvous::calculate_shard;
///
/// let target = [42u8; 32];
/// let shard = calculate_shard(&target);
/// // shard is u16, always in range [0, 65535]
/// assert!(shard <= u16::MAX);
/// ```
pub fn calculate_shard(target_id: &[u8; 32]) -> ShardId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RENDEZVOUS_PREFIX);
    hasher.update(target_id);
    let hash = hasher.finalize();

    // Take first 4 bytes and mask to 16 bits
    let bytes = hash.as_bytes();
    let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    (value & SHARD_MASK) as u16
}

/// Capability that a provider can serve per SPEC2 §9
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Capability {
    /// Serves a Saorsa Site
    Site,
    /// Serves identity/presence information
    Identity,
}

/// Provider Summary per SPEC2 §9
///
/// Wire format (CBOR):
/// ```json
/// {
///   "v": 1,
///   "target": [u8; 32],
///   "provider": PeerId,
///   "cap": ["SITE", "IDENTITY"],
///   "have_root": bool,
///   "manifest_ver": u64,
///   "summary": { "bloom": bytes, "iblt": bytes },
///   "exp": u64,
///   "sig": [u8]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSummary {
    /// Protocol version (currently 1)
    pub v: u8,
    /// Target identifier (what is being provided)
    pub target: [u8; 32],
    /// Provider peer ID
    pub provider: PeerId,
    /// Capabilities this provider offers
    pub cap: Vec<Capability>,
    /// Whether provider has the root manifest
    pub have_root: bool,
    /// Manifest version (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_ver: Option<u64>,
    /// Summary data (bloom filters, IBLT)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummaryData>,
    /// Expiration timestamp (unix ms)
    pub exp: u64,
    /// ML-DSA signature over all fields except sig
    #[serde(with = "serde_bytes")]
    pub sig: Vec<u8>,
}

/// Summary data for content reconciliation per SPEC2 §9
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryData {
    /// Bloom filter bytes (optional)
    #[serde(skip_serializing_if = "Option::is_none", with = "optional_serde_bytes")]
    pub bloom: Option<Vec<u8>>,
    /// IBLT bytes (optional)
    #[serde(skip_serializing_if = "Option::is_none", with = "optional_serde_bytes")]
    pub iblt: Option<Vec<u8>>,
}

mod serde_bytes {
    use serde::{de::Visitor, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;

        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a byte array")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(v.to_vec())
            }

            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(v)
            }
        }

        deserializer.deserialize_bytes(BytesVisitor)
    }
}

mod optional_serde_bytes {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match bytes {
            Some(b) => serializer.serialize_bytes(b),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Visitor;

        struct OptionalBytesVisitor;

        impl<'de> Visitor<'de> for OptionalBytesVisitor {
            type Value = Option<Vec<u8>>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an optional byte array")
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct BytesVisitorWrapper;
                impl<'de> Visitor<'de> for BytesVisitorWrapper {
                    type Value = Vec<u8>;

                    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                        formatter.write_str("a byte array")
                    }

                    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok(v.to_vec())
                    }

                    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok(v)
                    }
                }

                deserializer
                    .deserialize_bytes(BytesVisitorWrapper)
                    .map(Some)
            }
        }

        deserializer.deserialize_option(OptionalBytesVisitor)
    }
}

impl ProviderSummary {
    /// Create a new provider summary (unsigned)
    ///
    /// # Arguments
    /// * `target` - Target identifier being provided
    /// * `provider` - Provider peer ID
    /// * `capabilities` - What this provider offers
    /// * `validity_ms` - How long this summary is valid (milliseconds)
    pub fn new(
        target: [u8; 32],
        provider: PeerId,
        capabilities: Vec<Capability>,
        validity_ms: u64,
    ) -> Self {
        let now = current_time_millis();

        Self {
            v: 1,
            target,
            provider,
            cap: capabilities,
            have_root: false,
            manifest_ver: None,
            summary: None,
            exp: now + validity_ms,
            sig: Vec::new(),
        }
    }

    /// Set whether provider has root manifest
    pub fn with_root(mut self, has_root: bool) -> Self {
        self.have_root = has_root;
        self
    }

    /// Set manifest version
    pub fn with_manifest_version(mut self, version: u64) -> Self {
        self.manifest_ver = Some(version);
        self
    }

    /// Set summary data
    pub fn with_summary(mut self, summary: SummaryData) -> Self {
        self.summary = Some(summary);
        self
    }

    /// Check if summary is currently valid (not expired)
    pub fn is_valid(&self) -> bool {
        let now = current_time_millis();

        now <= self.exp
    }

    /// Sign the summary with ML-DSA
    ///
    /// Signs all fields except `sig` using ML-DSA-65 from saorsa-pqc.
    /// Uses CBOR serialization for deterministic signing.
    pub fn sign(&mut self, signing_key: &saorsa_pqc::MlDsaSecretKey) -> anyhow::Result<()> {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        // Serialize all fields except signature using CBOR
        let mut to_sign = Vec::new();
        ciborium::into_writer(
            &SignableFields {
                v: self.v,
                target: &self.target,
                provider: &self.provider,
                cap: &self.cap,
                have_root: self.have_root,
                manifest_ver: self.manifest_ver,
                summary: &self.summary,
                exp: self.exp,
            },
            &mut to_sign,
        )?;

        // Sign with ML-DSA-65
        let signer = MlDsa65::new();
        let signature = signer.sign(signing_key, &to_sign)?;

        self.sig = signature.as_bytes().to_vec();
        Ok(())
    }

    /// Verify the summary signature
    ///
    /// Verifies the ML-DSA-65 signature over all fields except `sig`.
    /// Uses CBOR serialization for deterministic verification.
    pub fn verify(&self, public_key: &saorsa_pqc::MlDsaPublicKey) -> anyhow::Result<bool> {
        use saorsa_pqc::{MlDsa65, MlDsaOperations, MlDsaSignature};

        // Reconstruct signed data using CBOR
        let mut to_verify = Vec::new();
        ciborium::into_writer(
            &SignableFields {
                v: self.v,
                target: &self.target,
                provider: &self.provider,
                cap: &self.cap,
                have_root: self.have_root,
                manifest_ver: self.manifest_ver,
                summary: &self.summary,
                exp: self.exp,
            },
            &mut to_verify,
        )?;

        // Verify signature
        let verifier = MlDsa65::new();
        let sig = MlDsaSignature::from_bytes(&self.sig)?;

        Ok(verifier.verify(public_key, &to_verify, &sig)?)
    }

    /// Serialize to CBOR wire format (RFC 8949)
    pub fn to_cbor(&self) -> anyhow::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        ciborium::into_writer(self, &mut buffer)?;
        Ok(buffer)
    }

    /// Deserialize from CBOR wire format (RFC 8949)
    pub fn from_cbor(data: &[u8]) -> anyhow::Result<Self> {
        let summary = ciborium::from_reader(data)?;
        Ok(summary)
    }

    /// Convert to bytes for transport
    pub fn to_bytes(&self) -> anyhow::Result<bytes::Bytes> {
        Ok(bytes::Bytes::from(self.to_cbor()?))
    }

    /// Parse from bytes received over transport
    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        Self::from_cbor(data)
    }

    /// Calculate the shard this summary should be gossiped to
    pub fn shard(&self) -> ShardId {
        calculate_shard(&self.target)
    }
}

/// Helper struct for serializing fields to sign
#[derive(Serialize)]
struct SignableFields<'a> {
    v: u8,
    target: &'a [u8; 32],
    provider: &'a PeerId,
    cap: &'a Vec<Capability>,
    have_root: bool,
    manifest_ver: Option<u64>,
    summary: &'a Option<SummaryData>,
    exp: u64,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_shard_calculation_deterministic() {
        let target = [42u8; 32];

        let shard1 = calculate_shard(&target);
        let shard2 = calculate_shard(&target);

        assert_eq!(shard1, shard2, "Shard calculation must be deterministic");
        // shard is u16, so it's always < 65536 by type
    }

    #[test]
    fn test_shard_calculation_different_targets() {
        let target1 = [1u8; 32];
        let target2 = [2u8; 32];

        let shard1 = calculate_shard(&target1);
        let shard2 = calculate_shard(&target2);

        // Different targets should (very likely) map to different shards
        // This is probabilistic, but with 65k shards collision is unlikely
        assert_ne!(
            shard1, shard2,
            "Different targets should map to different shards"
        );
    }

    #[test]
    fn test_shard_within_bounds() {
        // Test with various inputs
        for i in 0..100 {
            let target = [i; 32];
            let _shard = calculate_shard(&target);
            // shard is u16, so it's always in valid range [0, 65535]
        }
    }

    #[test]
    fn test_provider_summary_creation() {
        let target = [1u8; 32];
        let provider = PeerId::new([2u8; 32]);

        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 3_600_000);

        assert_eq!(summary.v, 1);
        assert_eq!(summary.target, target);
        assert_eq!(summary.provider, provider);
        assert_eq!(summary.cap.len(), 1);
        assert!(summary.is_valid());
    }

    #[test]
    fn test_provider_summary_expiry() {
        let target = [2u8; 32];
        let provider = PeerId::new([3u8; 32]);

        // Create summary with very short validity
        let summary = ProviderSummary::new(
            target,
            provider,
            vec![Capability::Identity],
            1, // 1ms validity
        );

        assert!(summary.is_valid(), "Should be valid immediately");

        // Sleep to let it expire
        std::thread::sleep(std::time::Duration::from_millis(5));

        assert!(!summary.is_valid(), "Should be expired after sleep");
    }

    #[test]
    fn test_provider_summary_builder() {
        let target = [3u8; 32];
        let provider = PeerId::new([4u8; 32]);

        let summary = ProviderSummary::new(
            target,
            provider,
            vec![Capability::Site, Capability::Identity],
            60_000,
        )
        .with_root(true)
        .with_manifest_version(42);

        assert!(summary.have_root);
        assert_eq!(summary.manifest_ver, Some(42));
    }

    #[test]
    fn test_provider_summary_shard() {
        let target = [5u8; 32];
        let provider = PeerId::new([6u8; 32]);

        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 3_600_000);

        let shard = summary.shard();
        assert_eq!(shard, calculate_shard(&target));
    }

    #[test]
    fn test_cbor_round_trip() {
        let target = [7u8; 32];
        let provider = PeerId::new([8u8; 32]);

        let summary =
            ProviderSummary::new(target, provider, vec![Capability::Site], 60_000).with_root(true);

        let cbor = summary.to_cbor().expect("CBOR serialization");
        let decoded = ProviderSummary::from_cbor(&cbor).expect("CBOR deserialization");

        assert_eq!(decoded.v, summary.v);
        assert_eq!(decoded.target, summary.target);
        assert_eq!(decoded.provider, summary.provider);
        assert_eq!(decoded.have_root, summary.have_root);
    }

    #[test]
    fn test_sign_and_verify() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let target = [9u8; 32];
        let provider = PeerId::new([10u8; 32]);

        let mut summary =
            ProviderSummary::new(target, provider, vec![Capability::Identity], 60_000);

        // Generate keypair
        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");

        // Sign
        summary.sign(&sk).expect("signing");
        assert!(!summary.sig.is_empty(), "Signature should be populated");

        // Verify
        let valid = summary.verify(&pk).expect("verification");
        assert!(valid, "Signature should be valid");

        // Tamper
        summary.have_root = true;

        // Verify should fail
        let valid = summary.verify(&pk).expect("verification");
        assert!(!valid, "Tampered signature should be invalid");
    }

    #[test]
    fn test_capability_serialization() {
        let caps = vec![Capability::Site, Capability::Identity];

        let mut buffer = Vec::new();
        ciborium::into_writer(&caps, &mut buffer).expect("serialize");

        let decoded: Vec<Capability> = ciborium::from_reader(&buffer[..]).expect("deserialize");

        assert_eq!(decoded, caps);
    }

    #[test]
    fn test_summary_data() {
        let data = SummaryData {
            bloom: Some(vec![1, 2, 3]),
            iblt: Some(vec![4, 5, 6]),
        };

        let mut buffer = Vec::new();
        ciborium::into_writer(&data, &mut buffer).expect("serialize");

        let decoded: SummaryData = ciborium::from_reader(&buffer[..]).expect("deserialize");

        assert_eq!(decoded.bloom, data.bloom);
        assert_eq!(decoded.iblt, data.iblt);
    }
}
