//! Wave 2 (#137): Unsigned coalesced 1-hop control frames over mutually
//! authenticated ML-DSA-65 + ML-KEM-768 QUIC sessions.
//!
//! # Security model (100% Pure PQC)
//!
//! - Every op in an [`UnsignedControlBatch`] is strictly **1-hop** and
//!   authenticated by the live [`saorsa_gossip_transport::AuthenticatedSession`]
//!   (mutually verified FIPS 203 ML-KEM-768 + FIPS 204 ML-DSA-65 QUIC link)
//!   that has completed mutual `Control::Hello` + `Control::Capabilities`
//!   negotiation for `CAP_UNSIGNED_CONTROL_V1`.
//! - Control ops carry **no** outer ML-DSA-65 signature (`3,309 B`) or public
//!   key (`1,952 B`) and are **never** stored in the message cache, delivered
//!   to subscribers, forwarded to a third peer, or treated as portable
//!   third-party evidence.
//! - Framing follows strict exact-body-consumption (`ADR 0085`): an
//!   allocation-free preflight validates byte caps, op count caps, cumulative
//!   ID caps, discriminant tags, non-empty ID lists, reserved-topic exclusion,
//!   and exact trailing-byte exhaustion before postcard allocates any `Vec`.

use crate::key_cache;
use anyhow::{anyhow, ensure, Result};
use bytes::Bytes;
use saorsa_gossip_types::{MessageKind, TopicId};
use serde::{Deserialize, Serialize};

/// Distinct 6-byte wire prefix for Wave 2 unsigned 1-hop control batches.
/// `0xff` cannot collide with valid `MessageHeader` versions (`1..=3`) and
/// differs from `key_cache::WIRE_MARKER` (`b"\xffSGKC\x03"`).
pub(crate) const CONTROL_BATCH_MARKER: &[u8; 6] = b"\xffSGCB\x01";

/// Maximum serialized wire size of an unsigned 1-hop control batch (including marker).
pub(crate) const MAX_CONTROL_BATCH_BYTES: usize = 65_536;

/// Maximum number of operations in a single unsigned 1-hop control batch.
pub(crate) const MAX_CONTROL_BATCH_OPS: usize = 64;

/// Maximum number of 32-byte message IDs in a single control operation.
pub(crate) const MAX_IDS_PER_CONTROL_OP: usize = 1_024;

/// Maximum cumulative 32-byte message IDs across all ops in a single batch.
pub(crate) const MAX_TOTAL_IDS_PER_BATCH: usize = 2_048;

/// One 1-hop Plumtree / GossipSub control operation keyed by 32-byte `TopicId`
/// (never plaintext group names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HopControlOp {
    /// Lazy digest advertisement for `topic`.
    IHave {
        topic: TopicId,
        msg_ids: Vec<[u8; 32]>,
    },
    /// Demand pull request for missing `msg_ids` on `topic`.
    IWant {
        topic: TopicId,
        msg_ids: Vec<[u8; 32]>,
    },
    /// Demote the sender from eager to lazy on `topic`.
    Prune { topic: TopicId },
    /// Promote the sender from lazy to eager on `topic` with optional repair ID.
    Graft {
        topic: TopicId,
        msg_id: Option<[u8; 32]>,
    },
    /// Suppress in-flight duplicate EAGER pushes (`>= 1 KiB`) for `msg_ids`.
    IDontWant {
        topic: TopicId,
        msg_ids: Vec<[u8; 32]>,
    },
    /// Partition-recovery anti-entropy digest (cached IDs).
    AntiEntropyDigest {
        topic: TopicId,
        msg_ids: Vec<[u8; 32]>,
    },
    /// Partition-recovery anti-entropy response (missing IDs).
    AntiEntropyResponse {
        topic: TopicId,
        missing_ids: Vec<[u8; 32]>,
    },
}

impl HopControlOp {
    pub(crate) fn topic(&self) -> TopicId {
        match self {
            Self::IHave { topic, .. }
            | Self::IWant { topic, .. }
            | Self::Prune { topic }
            | Self::Graft { topic, .. }
            | Self::IDontWant { topic, .. }
            | Self::AntiEntropyDigest { topic, .. }
            | Self::AntiEntropyResponse { topic, .. } => *topic,
        }
    }

    pub(crate) fn message_kind(&self) -> MessageKind {
        match self {
            Self::IHave { .. } => MessageKind::IHave,
            Self::IWant { .. } => MessageKind::IWant,
            Self::Prune { .. } => MessageKind::Prune,
            Self::Graft { .. } => MessageKind::Graft,
            Self::IDontWant { .. } => MessageKind::IDontWant,
            Self::AntiEntropyDigest { .. } | Self::AntiEntropyResponse { .. } => {
                MessageKind::AntiEntropy
            }
        }
    }

    pub(crate) fn op_label(&self) -> &'static str {
        match self {
            Self::IHave { .. } => "IHAVE",
            Self::IWant { .. } => "IWANT",
            Self::Prune { .. } => "PRUNE",
            Self::Graft { .. } => "GRAFT",
            Self::IDontWant { .. } => "IDONTWANT",
            Self::AntiEntropyDigest { .. } | Self::AntiEntropyResponse { .. } => "ANTI_ENTROPY",
        }
    }

    /// Estimated wire weight of this op for proportional byte attribution when
    /// multiple ops are coalesced into one batch frame.
    pub(crate) fn weight_bytes(&self) -> usize {
        match self {
            Self::IHave { msg_ids, .. }
            | Self::IWant { msg_ids, .. }
            | Self::IDontWant { msg_ids, .. }
            | Self::AntiEntropyDigest { msg_ids, .. } => 34 + msg_ids.len() * 32,
            Self::AntiEntropyResponse { missing_ids, .. } => 34 + missing_ids.len() * 32,
            Self::Prune { .. } => 33,
            Self::Graft { msg_id, .. } => 34 + if msg_id.is_some() { 32 } else { 0 },
        }
    }
}

/// Coalesced cross-topic 1-hop control batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnsignedControlBatch {
    pub(crate) ops: Vec<HopControlOp>,
}

pub(crate) fn is_control_batch(bytes: &[u8]) -> bool {
    bytes.starts_with(CONTROL_BATCH_MARKER)
}

fn take_varint(input: &mut &[u8]) -> Result<usize> {
    let mut value = 0u128;
    for index in 0..10 {
        let (&byte, rest) = input
            .split_first()
            .ok_or_else(|| anyhow!("truncated control batch varint"))?;
        *input = rest;
        value |= u128::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(usize::try_from(value)?);
        }
    }
    Err(anyhow!("oversized control batch varint"))
}

fn take_bytes<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let head = input
        .get(..len)
        .ok_or_else(|| anyhow!("truncated control batch field"))?;
    *input = &input[len..];
    Ok(head)
}

/// Allocation-free structural validation of an unsigned control batch body
/// (bytes immediately following [`CONTROL_BATCH_MARKER`]). Enforces strict
/// exact-body consumption (`rest.is_empty()`), hard op/ID/byte caps, and
/// rejects ops targeting the reserved signed key-cache control topic.
pub(crate) fn preflight_control_batch(body: &[u8]) -> Result<()> {
    ensure!(
        CONTROL_BATCH_MARKER.len().saturating_add(body.len()) <= MAX_CONTROL_BATCH_BYTES,
        "unsigned control batch exceeds byte cap"
    );
    let reserved = key_cache::control_topic();
    let mut rest = body;
    let op_count = take_varint(&mut rest)?;
    ensure!(
        (1..=MAX_CONTROL_BATCH_OPS).contains(&op_count),
        "invalid unsigned control batch op count"
    );
    let mut total_ids: usize = 0;
    for _ in 0..op_count {
        let variant = take_varint(&mut rest)?;
        let topic_slice = take_bytes(&mut rest, 32)?;
        ensure!(
            topic_slice != reserved.as_bytes(),
            "key-cache control topic must remain ML-DSA-65 signed"
        );
        match variant {
            // IHave (0), IWant (1), IDontWant (4), AntiEntropyDigest (5), AntiEntropyResponse (6)
            0 | 1 | 4 | 5 | 6 => {
                let id_count = take_varint(&mut rest)?;
                ensure!(
                    (1..=MAX_IDS_PER_CONTROL_OP).contains(&id_count),
                    "invalid control op message-ID count"
                );
                total_ids = total_ids.saturating_add(id_count);
                ensure!(
                    total_ids <= MAX_TOTAL_IDS_PER_BATCH,
                    "unsigned control batch exceeds cumulative message-ID cap"
                );
                let byte_len = id_count
                    .checked_mul(32)
                    .ok_or_else(|| anyhow!("control op ID length overflow"))?;
                let _ = take_bytes(&mut rest, byte_len)?;
            }
            // Prune (2)
            2 => {}
            // Graft (3): Option<[u8; 32]>
            3 => {
                let (&tag, after_tag) = rest
                    .split_first()
                    .ok_or_else(|| anyhow!("truncated GRAFT msg_id option tag"))?;
                rest = after_tag;
                match tag {
                    0 => {}
                    1 => {
                        total_ids = total_ids.saturating_add(1);
                        ensure!(
                            total_ids <= MAX_TOTAL_IDS_PER_BATCH,
                            "unsigned control batch exceeds cumulative message-ID cap"
                        );
                        let _ = take_bytes(&mut rest, 32)?;
                    }
                    _ => return Err(anyhow!("malformed GRAFT msg_id option tag")),
                }
            }
            _ => return Err(anyhow!("unknown unsigned control op variant")),
        }
    }
    ensure!(rest.is_empty(), "trailing unsigned control batch bytes");
    Ok(())
}

pub(crate) fn encode_control_batch(ops: Vec<HopControlOp>) -> Result<Bytes> {
    ensure!(
        !ops.is_empty() && ops.len() <= MAX_CONTROL_BATCH_OPS,
        "invalid unsigned control batch op count"
    );
    let reserved = key_cache::control_topic();
    let mut total_ids: usize = 0;
    for op in &ops {
        ensure!(
            op.topic() != reserved,
            "key-cache control topic must remain ML-DSA-65 signed"
        );
        match op {
            HopControlOp::IHave { msg_ids, .. }
            | HopControlOp::IWant { msg_ids, .. }
            | HopControlOp::IDontWant { msg_ids, .. }
            | HopControlOp::AntiEntropyDigest { msg_ids, .. } => {
                ensure!(
                    !msg_ids.is_empty() && msg_ids.len() <= MAX_IDS_PER_CONTROL_OP,
                    "invalid control op message-ID count"
                );
                total_ids = total_ids.saturating_add(msg_ids.len());
            }
            HopControlOp::AntiEntropyResponse { missing_ids, .. } => {
                ensure!(
                    !missing_ids.is_empty() && missing_ids.len() <= MAX_IDS_PER_CONTROL_OP,
                    "invalid control op message-ID count"
                );
                total_ids = total_ids.saturating_add(missing_ids.len());
            }
            HopControlOp::Graft { msg_id, .. } => {
                if msg_id.is_some() {
                    total_ids = total_ids.saturating_add(1);
                }
            }
            HopControlOp::Prune { .. } => {}
        }
        ensure!(
            total_ids <= MAX_TOTAL_IDS_PER_BATCH,
            "unsigned control batch exceeds cumulative message-ID cap"
        );
    }
    let batch = UnsignedControlBatch { ops };
    let body = postcard::to_stdvec(&batch)?;
    let total_len = CONTROL_BATCH_MARKER.len().saturating_add(body.len());
    ensure!(
        total_len <= MAX_CONTROL_BATCH_BYTES,
        "unsigned control batch exceeds byte cap"
    );
    let mut wire = Vec::with_capacity(total_len);
    wire.extend_from_slice(CONTROL_BATCH_MARKER);
    wire.extend_from_slice(&body);
    Ok(wire.into())
}

pub(crate) fn decode_control_batch(bytes: &[u8]) -> Result<UnsignedControlBatch> {
    let body = bytes
        .strip_prefix(CONTROL_BATCH_MARKER)
        .ok_or_else(|| anyhow!("missing unsigned control batch marker"))?;
    preflight_control_batch(body)?;
    let (batch, trailing): (UnsignedControlBatch, _) = postcard::take_from_bytes(body)?;
    ensure!(trailing.is_empty(), "trailing unsigned control batch bytes");
    Ok(batch)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_control_batch_roundtrips_all_ops_and_stays_compact() {
        let t1 = TopicId::new([1u8; 32]);
        let t2 = TopicId::new([2u8; 32]);
        let ops = vec![
            HopControlOp::IHave {
                topic: t1,
                msg_ids: vec![[0x11; 32]],
            },
            HopControlOp::IWant {
                topic: t2,
                msg_ids: vec![[0x22; 32], [0x23; 32]],
            },
            HopControlOp::Prune { topic: t1 },
            HopControlOp::Graft {
                topic: t2,
                msg_id: Some([0x33; 32]),
            },
            HopControlOp::Graft {
                topic: t1,
                msg_id: None,
            },
            HopControlOp::IDontWant {
                topic: t2,
                msg_ids: vec![[0x44; 32]],
            },
            HopControlOp::AntiEntropyDigest {
                topic: t1,
                msg_ids: vec![[0x55; 32]],
            },
            HopControlOp::AntiEntropyResponse {
                topic: t2,
                missing_ids: vec![[0x66; 32]],
            },
        ];

        let wire = encode_control_batch(ops.clone()).expect("encode batch");
        assert!(is_control_batch(&wire));
        assert!(!key_cache::is_v3(&wire));
        // Single-ID IHAVE must be dramatically smaller than a signed ~5.4 KiB IHAVE.
        let single_ihave = encode_control_batch(vec![HopControlOp::IHave {
            topic: t1,
            msg_ids: vec![[0xaa; 32]],
        }])
        .expect("single ihave");
        assert!(
            single_ihave.len() < 80,
            "single unsigned IHAVE should be ~73 bytes, got {}",
            single_ihave.len()
        );

        let decoded = decode_control_batch(&wire).expect("decode batch");
        assert_eq!(decoded.ops, ops);
    }

    #[test]
    fn unsigned_control_batch_rejects_trailing_empty_forged_and_reserved_topic() {
        let t1 = TopicId::new([7u8; 32]);
        let valid = encode_control_batch(vec![HopControlOp::Prune { topic: t1 }]).expect("valid");

        // ADR 0085: trailing byte rejected.
        let mut with_trailing = valid.to_vec();
        with_trailing.push(0x00);
        assert!(decode_control_batch(&with_trailing).is_err());

        // Truncated rejected.
        assert!(decode_control_batch(&valid[..valid.len() - 1]).is_err());

        // Empty batch rejected on both encode and decode.
        assert!(encode_control_batch(Vec::new()).is_err());
        let mut raw_empty = CONTROL_BATCH_MARKER.to_vec();
        raw_empty.push(0); // 0 ops
        assert!(decode_control_batch(&raw_empty).is_err());

        // Empty ID list rejected.
        assert!(encode_control_batch(vec![HopControlOp::IHave {
            topic: t1,
            msg_ids: Vec::new(),
        }])
        .is_err());

        // Key-cache control topic forbidden in unsigned batch.
        assert!(encode_control_batch(vec![HopControlOp::Prune {
            topic: key_cache::control_topic(),
        }])
        .is_err());

        // Forged huge Vec length rejected in preflight without allocation.
        let mut forged = CONTROL_BATCH_MARKER.to_vec();
        forged.push(1); // 1 op
        forged.push(0); // IHave
        forged.extend_from_slice(&[9u8; 32]); // topic
        forged.extend_from_slice(&[0xff, 0xff, 0xff, 0x0f]); // huge count
        assert!(decode_control_batch(&forged).is_err());
    }
}
