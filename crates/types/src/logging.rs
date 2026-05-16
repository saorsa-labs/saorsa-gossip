//! Privacy-preserving log identifiers (issue #13, Layer 2).
//!
//! Per-process random salt + BLAKE3 keyed hash so warn!/error! log lines
//! emit opaque, daemon-local tokens (`peer_xxxxxxxx`, `topic_xxxxxxxx`)
//! instead of stable identifiers. Properties:
//!
//! - Operators can correlate the same opaque token across log lines
//!   from one daemon during a debugging session (same salt → same hash).
//! - After daemon restart, the same real peer/topic shows as a
//!   different opaque token. No long-term linkability from disk logs.
//! - Across daemons, the same real peer/topic hashes differently. No
//!   fleet-wide social-graph correlation from logs alone.
//! - From old logs alone with no current daemon, no back-correlation
//!   to real identifiers.
//!
//! The salt is 32 random bytes loaded once into a `OnceLock` at first
//! use. It is held in memory only — never persisted to disk and never
//! exposed via the public API. Tests can override it via
//! [`init_log_salt_for_tests`] for deterministic-output assertions.
//!
//! ## Usage
//!
//! ```ignore
//! use saorsa_gossip_types::{LogPeerId, LogTopicId, PeerId, TopicId};
//!
//! let peer = PeerId::new([0xAA; 32]);
//! let topic = TopicId::new([0xBB; 32]);
//! tracing::warn!(peer = %LogPeerId::from(peer), topic = %LogTopicId::from(topic),
//!                "cooled");
//! // Log line: peer=peer_a1b2c3d4 topic=topic_5e6f7080 ...
//! ```

use std::fmt;
use std::sync::OnceLock;

use crate::{PeerId, TopicId};

/// Per-process random salt for keyed hashing of log identifiers. Set
/// once on first use. 32 bytes — BLAKE3 keyed_hash requires exactly 32.
static LOG_SALT: OnceLock<[u8; 32]> = OnceLock::new();

/// Return the per-process salt, initialising it on first call.
///
/// Initialisation uses `rand::thread_rng()` and runs at most once per
/// process. Subsequent calls are lock-free reads.
fn salt() -> &'static [u8; 32] {
    LOG_SALT.get_or_init(|| {
        use rand::RngCore;
        let mut s = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut s);
        s
    })
}

/// **Test-only:** seed the process-global log salt deterministically.
///
/// Must be called before any `Display` formatting of `LogPeerId` /
/// `LogTopicId`. Subsequent calls have no effect because the salt is
/// stored in a `OnceLock`. Returns `true` if this call performed the
/// initialisation, `false` if the salt was already set.
///
/// This is exposed (rather than gated behind `#[cfg(test)]`) so
/// downstream crate tests can produce deterministic log fixtures.
#[doc(hidden)]
pub fn init_log_salt_for_tests(seed: [u8; 32]) -> bool {
    LOG_SALT.set(seed).is_ok()
}

/// Compute the 8-hex-character opaque token for a 32-byte identifier
/// under the process-global salt. Shared by `LogPeerId` and
/// `LogTopicId` so both formats use identical hashing.
fn opaque_token(bytes: &[u8; 32]) -> String {
    let h = blake3::keyed_hash(salt(), bytes);
    hex::encode(&h.as_bytes()[..4])
}

/// Wrapper around `PeerId` that displays as `peer_xxxxxxxx`, a salted
/// 8-hex-char hash that is stable within one process run but unlinkable
/// across daemons or after restart. See module docs for the threat
/// model and properties.
#[derive(Clone, Copy)]
pub struct LogPeerId(PeerId);

impl LogPeerId {
    /// Wrap a `PeerId` for privacy-preserving log formatting.
    pub fn new(peer: PeerId) -> Self {
        Self(peer)
    }
}

impl From<PeerId> for LogPeerId {
    fn from(peer: PeerId) -> Self {
        Self(peer)
    }
}

impl From<&PeerId> for LogPeerId {
    fn from(peer: &PeerId) -> Self {
        Self(*peer)
    }
}

impl fmt::Display for LogPeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer_{}", opaque_token(self.0.as_bytes()))
    }
}

impl fmt::Debug for LogPeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Mirror Display — Debug is what `?` formatter in tracing macros
        // produces, and we do not want it to leak the real id either.
        fmt::Display::fmt(self, f)
    }
}

/// Wrapper around `TopicId` that displays as `topic_xxxxxxxx`, a
/// salted 8-hex-char hash that is stable within one process run but
/// unlinkable across daemons or after restart.
#[derive(Clone, Copy)]
pub struct LogTopicId(TopicId);

impl LogTopicId {
    /// Wrap a `TopicId` for privacy-preserving log formatting.
    pub fn new(topic: TopicId) -> Self {
        Self(topic)
    }
}

impl From<TopicId> for LogTopicId {
    fn from(topic: TopicId) -> Self {
        Self(topic)
    }
}

impl From<&TopicId> for LogTopicId {
    fn from(topic: &TopicId) -> Self {
        Self(*topic)
    }
}

impl fmt::Display for LogTopicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "topic_{}", opaque_token(self.0.as_bytes()))
    }
}

impl fmt::Debug for LogTopicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // Tests share the process-global salt with all other tests in this
    // binary. Calling `init_log_salt_for_tests` once is enough; later
    // calls return false because OnceLock is set. The first test to
    // touch the salt wins. We pick a known-fixed seed so output is
    // deterministic regardless of test ordering.
    const TEST_SEED: [u8; 32] = [0x42; 32];

    fn ensure_seed() {
        // Best-effort: if another test set a different seed first, the
        // hash output assertions below still hold because they only
        // compare results from THIS call (same salt) — they do not
        // hardcode the hash bytes themselves.
        let _ = init_log_salt_for_tests(TEST_SEED);
    }

    #[test]
    fn log_peer_id_format_is_opaque_token_with_peer_prefix() {
        ensure_seed();
        let peer = PeerId::new([0xAA; 32]);
        let formatted = format!("{}", LogPeerId::from(peer));
        assert!(formatted.starts_with("peer_"), "got: {formatted}");
        // peer_ + 8 hex chars
        assert_eq!(formatted.len(), "peer_".len() + 8);
        // Must NOT contain the raw 0xAA bytes anywhere
        assert!(
            !formatted.contains("aa"),
            "raw bytes leaked into log token: {formatted}"
        );
    }

    #[test]
    fn log_topic_id_format_is_opaque_token_with_topic_prefix() {
        ensure_seed();
        let topic = TopicId::new([0xBB; 32]);
        let formatted = format!("{}", LogTopicId::from(topic));
        assert!(formatted.starts_with("topic_"), "got: {formatted}");
        assert_eq!(formatted.len(), "topic_".len() + 8);
        assert!(
            !formatted.contains("bb"),
            "raw bytes leaked into log token: {formatted}"
        );
    }

    #[test]
    fn same_input_under_same_salt_produces_same_token() {
        ensure_seed();
        let peer = PeerId::new([0xCC; 32]);
        let a = format!("{}", LogPeerId::from(peer));
        let b = format!("{}", LogPeerId::from(peer));
        assert_eq!(
            a, b,
            "stable correlation within a session must work for operator debugging"
        );
    }

    #[test]
    fn distinct_inputs_produce_distinct_tokens() {
        ensure_seed();
        let a = LogPeerId::from(PeerId::new([0x01; 32])).to_string();
        let b = LogPeerId::from(PeerId::new([0x02; 32])).to_string();
        assert_ne!(
            a, b,
            "distinct peers must produce distinct tokens (8 hex chars \
             ≈ 32-bit space — collisions exist but should not for these \
             specific inputs)"
        );
    }

    #[test]
    fn debug_format_does_not_leak_raw_id() {
        ensure_seed();
        let peer = PeerId::new([0xDD; 32]);
        let debug = format!("{:?}", LogPeerId::from(peer));
        assert!(debug.starts_with("peer_"));
        assert!(
            !debug.contains("dd") && !debug.contains("DD"),
            "Debug must use Display, not leak raw bytes: {debug}"
        );
    }

    #[test]
    fn cross_salt_unlinkability_by_construction() {
        // We cannot re-seed the OnceLock in a single process; instead,
        // exercise `opaque_token` directly with two different keys to
        // verify the underlying primitive separates salts.
        let bytes = [0xEE; 32];
        let key1 = [0x11; 32];
        let key2 = [0x22; 32];
        let t1 = hex::encode(&blake3::keyed_hash(&key1, &bytes).as_bytes()[..4]);
        let t2 = hex::encode(&blake3::keyed_hash(&key2, &bytes).as_bytes()[..4]);
        assert_ne!(
            t1, t2,
            "different salts must produce different tokens for the \
             same input (cross-daemon unlinkability)"
        );
    }
}
