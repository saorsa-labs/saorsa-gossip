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
    /// Arbitrary caller-defined extension data.
    ///
    /// This field is included in the signed payload when present.
    /// When `None`, it is omitted from the wire format via `skip_serializing_if`,
    /// so existing signatures over records without `extensions` remain valid.
    ///
    /// x0x uses this field to embed serialized `Vec<SocketAddr>` (bincode format)
    /// so that agent reachability addresses are available from the rendezvous record.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_serde_bytes"
    )]
    pub extensions: Option<Vec<u8>>,
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
            extensions: None,
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

    /// Set caller-defined extension data.
    ///
    /// The extension bytes are included in the signed payload.
    /// x0x uses this field to encode agent socket addresses.
    pub fn with_extensions(mut self, data: Vec<u8>) -> Self {
        self.extensions = Some(data);
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
                extensions: &self.extensions,
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
                extensions: &self.extensions,
            },
            &mut to_verify,
        )?;

        // Verify signature
        let verifier = MlDsa65::new();
        let sig = MlDsaSignature::from_bytes(&self.sig)?;

        Ok(verifier.verify(public_key, &to_verify, &sig)?)
    }

    /// Sign the summary using raw ML-DSA-65 secret key bytes.
    ///
    /// Equivalent to [`Self::sign`] but accepts raw bytes instead of a typed key.
    /// Allows callers using different key wrappers (e.g., `ant-quic`) to sign
    /// without depending on `saorsa-pqc` directly.
    ///
    /// # Errors
    ///
    /// Returns an error if the key bytes are not a valid ML-DSA-65 secret key
    /// or if signing fails.
    pub fn sign_raw(&mut self, secret_key_bytes: &[u8]) -> anyhow::Result<()> {
        let key = saorsa_pqc::MlDsaSecretKey::from_bytes(secret_key_bytes)
            .map_err(|e| anyhow::anyhow!("invalid secret key bytes: {:?}", e))?;
        self.sign(&key)
    }

    /// Verify the summary signature using raw ML-DSA-65 public key bytes.
    ///
    /// Equivalent to [`Self::verify`] but accepts raw bytes instead of a typed key.
    ///
    /// # Errors
    ///
    /// Returns an error if the key bytes are invalid or signature verification fails.
    pub fn verify_raw(&self, public_key_bytes: &[u8]) -> anyhow::Result<bool> {
        let key = saorsa_pqc::MlDsaPublicKey::from_bytes(public_key_bytes)
            .map_err(|e| anyhow::anyhow!("invalid public key bytes: {:?}", e))?;
        self.verify(&key)
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
    #[serde(skip_serializing_if = "Option::is_none", with = "optional_serde_bytes")]
    extensions: &'a Option<Vec<u8>>,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

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

    // ─── with_summary builder ────────────────────────────────────────

    #[test]
    fn test_with_summary_builder() {
        let target = [11u8; 32];
        let provider = PeerId::new([12u8; 32]);

        let summary_data = SummaryData {
            bloom: Some(vec![0xAA, 0xBB]),
            iblt: Some(vec![0xCC, 0xDD]),
        };

        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_summary(summary_data.clone());

        assert!(summary.summary.is_some());
        let inner = summary.summary.unwrap();
        assert_eq!(inner.bloom, summary_data.bloom);
        assert_eq!(inner.iblt, summary_data.iblt);
    }

    #[test]
    fn test_with_summary_round_trip() {
        let target = [13u8; 32];
        let provider = PeerId::new([14u8; 32]);

        let summary_data = SummaryData {
            bloom: Some(vec![1, 2, 3]),
            iblt: Some(vec![4, 5, 6]),
        };

        let original = ProviderSummary::new(target, provider, vec![Capability::Identity], 60_000)
            .with_summary(summary_data.clone());

        let cbor = original.to_cbor().expect("serialize");
        let decoded = ProviderSummary::from_cbor(&cbor).expect("deserialize");

        assert!(decoded.summary.is_some());
        let inner = decoded.summary.unwrap();
        assert_eq!(inner.bloom, summary_data.bloom);
        assert_eq!(inner.iblt, summary_data.iblt);
    }

    // ─── with_extensions builder ─────────────────────────────────────

    #[test]
    fn test_with_extensions_builder() {
        let target = [15u8; 32];
        let provider = PeerId::new([16u8; 32]);

        let ext_data = vec![0x01, 0x02, 0x03, 0x04];
        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_extensions(ext_data.clone());

        assert_eq!(summary.extensions, Some(ext_data));
    }

    #[test]
    fn test_with_extensions_round_trip() {
        let target = [17u8; 32];
        let provider = PeerId::new([18u8; 32]);

        let ext_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let original = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_extensions(ext_data.clone());

        let cbor = original.to_cbor().expect("serialize");
        let decoded = ProviderSummary::from_cbor(&cbor).expect("deserialize");

        assert_eq!(decoded.extensions, Some(ext_data));
    }

    #[test]
    fn test_extensions_none_serializes_and_deserializes() {
        // Serialize a summary with extensions: None (should omit field via skip_serializing_if)
        let target = [19u8; 32];
        let provider = PeerId::new([20u8; 32]);

        let original = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000);
        assert!(original.extensions.is_none());

        let cbor = original.to_cbor().expect("serialize");
        let decoded = ProviderSummary::from_cbor(&cbor).expect("deserialize");

        // After round-trip, extensions should still be None (default)
        assert!(decoded.extensions.is_none());
    }

    #[test]
    fn test_optional_byte_adapter_round_trips_explicit_none() {
        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        struct OptionalBytesHarness {
            #[serde(with = "super::optional_serde_bytes")]
            field: Option<Vec<u8>>,
        }

        let original = OptionalBytesHarness { field: None };
        let mut encoded = Vec::new();
        ciborium::into_writer(&original, &mut encoded).expect("serialize none field");

        let decoded: OptionalBytesHarness =
            ciborium::from_reader(&encoded[..]).expect("deserialize none field");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_optional_byte_adapter_rejects_non_bytes_value() {
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(transparent)]
        struct OptionalBytesHarness {
            #[serde(with = "super::optional_serde_bytes")]
            field: Option<Vec<u8>>,
        }

        // CBOR text string "hello" is not valid for the optional byte adapter's
        // Some(bytes) path, so malformed wire payloads are rejected.
        let cbor_text_hello = [0x65, b'h', b'e', b'l', b'l', b'o'];
        let decoded = ciborium::from_reader::<OptionalBytesHarness, _>(&cbor_text_hello[..]);
        assert!(decoded.is_err(), "text is not accepted as optional bytes");
    }

    #[test]
    fn test_required_byte_adapter_rejects_non_bytes_value() {
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(transparent)]
        struct RequiredBytesHarness {
            #[serde(with = "super::serde_bytes")]
            field: Vec<u8>,
        }

        // CBOR text string "hello" is not valid for required byte fields such
        // as ProviderSummary::sig, so decoding must fail instead of coercing.
        let cbor_text_hello = [0x65, b'h', b'e', b'l', b'l', b'o'];
        let decoded = ciborium::from_reader::<RequiredBytesHarness, _>(&cbor_text_hello[..]);
        assert!(decoded.is_err(), "text is not accepted as required bytes");
    }

    #[test]
    fn test_full_builder_chain() {
        let target = [21u8; 32];
        let provider = PeerId::new([22u8; 32]);

        let summary_data = SummaryData {
            bloom: Some(vec![0x01]),
            iblt: Some(vec![0x02]),
        };

        let summary = ProviderSummary::new(
            target,
            provider,
            vec![Capability::Site, Capability::Identity],
            3_600_000,
        )
        .with_root(true)
        .with_manifest_version(100)
        .with_summary(summary_data)
        .with_extensions(vec![0xFF]);

        assert!(summary.have_root);
        assert_eq!(summary.manifest_ver, Some(100));
        assert!(summary.summary.is_some());
        assert!(summary.extensions.is_some());
        assert_eq!(summary.cap.len(), 2);
    }

    // ─── sign_raw / verify_raw ──────────────────────────────────────

    #[test]
    fn test_sign_raw_and_verify_raw() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let target = [23u8; 32];
        let provider = PeerId::new([24u8; 32]);

        let mut summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000);

        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");

        // Sign with raw secret key bytes
        summary.sign_raw(sk.as_bytes()).expect("sign_raw");
        assert!(!summary.sig.is_empty());

        // Verify with raw public key bytes
        let valid = summary.verify_raw(pk.as_bytes()).expect("verify_raw");
        assert!(valid, "Raw signature should verify");
    }

    #[test]
    fn test_sign_raw_invalid_key_bytes() {
        let target = [25u8; 32];
        let provider = PeerId::new([26u8; 32]);

        let mut summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000);

        // Pass wrong-length bytes — should fail
        let result = summary.sign_raw(&[0x00, 0x01, 0x02]);
        assert!(
            result.is_err(),
            "sign_raw with invalid key bytes should error"
        );
    }

    #[test]
    fn test_verify_raw_invalid_key_bytes() {
        let target = [27u8; 32];
        let provider = PeerId::new([28u8; 32]);

        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000);

        // Pass wrong-length bytes — should fail
        let result = summary.verify_raw(&[0x00, 0x01, 0x02]);
        assert!(
            result.is_err(),
            "verify_raw with invalid key bytes should error"
        );
    }

    #[test]
    fn test_sign_raw_then_verify_tampered() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let target = [29u8; 32];
        let provider = PeerId::new([30u8; 32]);

        let mut summary =
            ProviderSummary::new(target, provider, vec![Capability::Identity], 60_000);

        let signer = MlDsa65::new();
        let (pk, _sk) = signer.generate_keypair().expect("keypair");

        // Sign with raw bytes, then tamper, then verify with raw bytes
        summary
            .sign_raw(signer.generate_keypair().unwrap().1.as_bytes())
            .expect("sign_raw");
        summary.have_root = true; // tamper

        let valid = summary.verify_raw(pk.as_bytes()).expect("verify_raw");
        assert!(!valid, "Tampered summary should fail verification");
    }

    // ─── to_bytes / from_bytes ──────────────────────────────────────

    #[test]
    fn test_to_bytes_and_from_bytes() {
        let target = [31u8; 32];
        let provider = PeerId::new([0u8; 32]);

        let original = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_root(true)
            .with_manifest_version(7);

        // to_bytes -> from_bytes round trip
        let bytes = original.to_bytes().expect("to_bytes");
        let decoded = ProviderSummary::from_bytes(&bytes).expect("from_bytes");

        assert_eq!(decoded.v, original.v);
        assert_eq!(decoded.target, original.target);
        assert_eq!(decoded.provider, original.provider);
        assert_eq!(decoded.have_root, original.have_root);
        assert_eq!(decoded.manifest_ver, original.manifest_ver);
    }

    #[test]
    fn test_from_bytes_invalid_cbor() {
        // Invalid CBOR data should return an error
        let result = ProviderSummary::from_bytes(&[0xFF, 0xFE, 0xFD]);
        assert!(result.is_err(), "Invalid CBOR should fail deserialization");
    }

    #[test]
    fn test_from_cbor_rejects_malformed_wire_byte_fields() {
        fn replace_field(value: &mut ciborium::Value, field: &str, replacement: ciborium::Value) {
            let ciborium::Value::Map(entries) = value else {
                panic!("ProviderSummary encodes as a CBOR map");
            };
            let (_, slot) = entries
                .iter_mut()
                .find(|(key, _)| matches!(key, ciborium::Value::Text(name) if name == field))
                .expect("field is present in encoded summary");
            *slot = replacement;
        }

        let target = [39u8; 32];
        let provider = PeerId::new([40u8; 32]);
        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_extensions(vec![1, 2, 3]);

        let mut value: ciborium::Value =
            ciborium::from_reader(&summary.to_cbor().expect("summary cbor")[..])
                .expect("summary decodes to dynamic value");
        replace_field(&mut value, "sig", ciborium::Value::Text("not bytes".into()));
        let mut malformed_sig = Vec::new();
        ciborium::into_writer(&value, &mut malformed_sig).expect("malformed sig encodes");
        assert!(ProviderSummary::from_cbor(&malformed_sig).is_err());

        let mut value: ciborium::Value =
            ciborium::from_reader(&summary.to_cbor().expect("summary cbor")[..])
                .expect("summary decodes to dynamic value");
        replace_field(
            &mut value,
            "extensions",
            ciborium::Value::Text("not bytes".into()),
        );
        let mut malformed_extensions = Vec::new();
        ciborium::into_writer(&value, &mut malformed_extensions)
            .expect("malformed extensions encodes");
        assert!(ProviderSummary::from_cbor(&malformed_extensions).is_err());
    }

    #[test]
    fn test_to_bytes_returns_non_empty() {
        let target = [42u8; 32];
        let provider = PeerId::new([0u8; 32]);
        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000);

        let bytes = summary.to_bytes().expect("to_bytes");
        assert!(!bytes.is_empty(), "Serialized bytes should not be empty");
    }

    // ─── sign/verify with extensions ────────────────────────────────

    #[test]
    fn test_sign_and_verify_with_extensions() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let target = [33u8; 32];
        let provider = PeerId::new([34u8; 32]);

        let ext_data = vec![0x01, 0x02, 0x03];
        let mut summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_extensions(ext_data);

        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");

        summary.sign(&sk).expect("signing with extensions");
        let valid = summary.verify(&pk).expect("verification");
        assert!(valid, "Signature with extensions should verify");
    }

    #[test]
    fn test_sign_and_verify_with_summary_data() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let target = [35u8; 32];
        let provider = PeerId::new([36u8; 32]);

        let summary_data = SummaryData {
            bloom: Some(vec![0xAA; 16]),
            iblt: Some(vec![0xBB; 32]),
        };

        let mut summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000)
            .with_summary(summary_data);

        let signer = MlDsa65::new();
        let (pk, sk) = signer.generate_keypair().expect("keypair");

        summary.sign(&sk).expect("signing with summary data");
        let valid = summary.verify(&pk).expect("verification");
        assert!(valid, "Signature with summary data should verify");
    }

    // ─── SummaryData edge cases ─────────────────────────────────────

    #[test]
    fn test_summary_data_partial_fields() {
        // SummaryData with only bloom set (iblt is Some but empty)
        let data = SummaryData {
            bloom: Some(vec![0xAA]),
            iblt: Some(vec![]),
        };

        let mut buffer = Vec::new();
        ciborium::into_writer(&data, &mut buffer).expect("serialize");

        let decoded: SummaryData = ciborium::from_reader(&buffer[..]).expect("deserialize");

        assert_eq!(decoded.bloom, Some(vec![0xAA]));
        assert_eq!(decoded.iblt, Some(vec![]));
    }

    // ─── verify with empty signature should fail gracefully ─────────

    #[test]
    fn test_verify_empty_signature() {
        use saorsa_pqc::{MlDsa65, MlDsaOperations};

        let target = [37u8; 32];
        let provider = PeerId::new([38u8; 32]);

        let summary = ProviderSummary::new(target, provider, vec![Capability::Site], 60_000);
        // sig is empty Vec — should return error (invalid signature size)
        let signer = MlDsa65::new();
        let (pk, _sk) = signer.generate_keypair().expect("keypair");

        let result = summary.verify(&pk);
        assert!(result.is_err(), "Empty signature should fail verification");
    }

    // ─── constants and shard spread ──────────────────────────────────

    #[test]
    fn test_shard_constants() {
        assert_eq!(SHARD_BITS, 16);
        assert_eq!(SHARD_COUNT, 65_536);
        assert_eq!(SHARD_MASK, 0xFFFF);
    }

    #[test]
    fn test_shard_distribution_spread() {
        // Verify that shards are well-distributed across the space
        let mut seen = std::collections::HashSet::new();
        for i in 0u8..=255 {
            let target = [i; 32];
            seen.insert(calculate_shard(&target));
        }
        // With 256 inputs and 65k shards, we should see many unique values
        assert!(seen.len() > 200, "Shards should be well-distributed");
    }
}
