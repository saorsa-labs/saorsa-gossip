//! x0x #1964 — fan-out cause and per-topic ingress counters.
//!
//! Instrumentation only: nothing here changes what is sent, verified,
//! delivered or forwarded. Two questions from the live fleet drive it:
//!
//! 1. Who re-publishes? Signature-valid EAGER frames whose msg_id is new but
//!    whose payload hash is already in the topic's payload replay cache are
//!    dropped without a counter today. [`TopicIngressMeters`] counts them per
//!    topic and keeps a bounded heavy-hitter table of gossip signers.
//! 2. Why do more EAGER copies leave than the eager degree predicts?
//!    [`EagerSendCauseMeters`] splits every metered outbound EAGER send by the
//!    code path that caused it. The cause counters sum to
//!    `outbound_by_kind["eager"].msgs` for production paths.
//!
//! Memory is bounded whatever peers send: at most [`TOPIC_INGRESS_METER_CAP`]
//! topics, each with a fixed-size distinct-signer sketch
//! ([`SIGNER_SKETCH_REGISTERS`] bytes) and at most [`SIGNER_TRACKED_SLOTS`]
//! signer slots (Space-Saving, Metwally et al. 2005).

use saorsa_gossip_types::{PeerId, TopicId};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Cap on distinct topics tracked by [`TopicIngressMeters`]. Topics beyond it
/// are counted in `ingress_topics_uncounted` and not inserted.
pub(crate) const TOPIC_INGRESS_METER_CAP: usize = 256;

/// Signers reported per topic in a snapshot.
pub(crate) const SIGNER_TOP_N: usize = 8;

/// Space-Saving slots kept per topic. Any signer with more than
/// `total / SIGNER_TRACKED_SLOTS` messages on a topic is guaranteed a slot.
pub(crate) const SIGNER_TRACKED_SLOTS: usize = 32;

/// HyperLogLog registers per topic (one byte each). Standard error is about
/// `1.04 / sqrt(256)` = 6.5%.
pub(crate) const SIGNER_SKETCH_REGISTERS: usize = 256;

/// Code path that caused one outbound EAGER send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EagerSendCause {
    /// `publish_local*`: the ordinary fan-out of a local publish to the
    /// topic's eager set.
    PublishFanout,
    /// `publish_local_with_target`: the extra targeted leg of a local publish.
    PublishTargeted,
    /// `handle_eager`: relay forward of a newly accepted message to the eager
    /// peers (minus the sender).
    RelayForward,
    /// `send_claimed_iwant_reply`: a cached message served in reply to IWANT.
    IwantReply,
    /// `flush_deferred_eager_replies`: a deferred IWANT reply sent on a later
    /// IHAVE flush tick.
    IwantReplyDeferredRetry,
    /// `handle_anti_entropy` digest: cached messages the peer's digest lacked,
    /// pushed as EAGER.
    AntiEntropyPush,
    /// `retry_stranded_publish`: the bounded retry of a stranded local publish.
    StrandedPublishRetry,
}

impl EagerSendCause {
    const COUNT: usize = 7;

    fn index(self) -> usize {
        match self {
            Self::PublishFanout => 0,
            Self::PublishTargeted => 1,
            Self::RelayForward => 2,
            Self::IwantReply => 3,
            Self::IwantReplyDeferredRetry => 4,
            Self::AntiEntropyPush => 5,
            Self::StrandedPublishRetry => 6,
        }
    }
}

/// Lock-free per-cause EAGER send counters.
#[derive(Debug, Default)]
pub(crate) struct EagerSendCauseMeters {
    sends: [AtomicU64; EagerSendCause::COUNT],
}

impl EagerSendCauseMeters {
    /// Record `sends` metered EAGER wire sends caused by `cause`.
    pub(crate) fn record(&self, cause: EagerSendCause, sends: usize) {
        if sends == 0 {
            return;
        }
        self.sends[cause.index()].fetch_add(sends as u64, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> EagerSendCauseSnapshot {
        let load = |cause: EagerSendCause| self.sends[cause.index()].load(Ordering::Relaxed);
        EagerSendCauseSnapshot {
            publish_fanout: load(EagerSendCause::PublishFanout),
            publish_targeted: load(EagerSendCause::PublishTargeted),
            relay_forward: load(EagerSendCause::RelayForward),
            iwant_reply: load(EagerSendCause::IwantReply),
            iwant_reply_deferred_retry: load(EagerSendCause::IwantReplyDeferredRetry),
            anti_entropy_push: load(EagerSendCause::AntiEntropyPush),
            stranded_publish_retry: load(EagerSendCause::StrandedPublishRetry),
        }
    }
}

/// x0x #1964: cumulative outbound EAGER wire sends split by cause. Each send
/// is counted where `outbound_by_kind["eager"]` counts it, so for production
/// paths `total()` equals `outbound_by_kind["eager"].msgs`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct EagerSendCauseSnapshot {
    /// Ordinary fan-out of local publishes to the eager set.
    pub publish_fanout: u64,
    /// Targeted leg of `publish_local_with_target`.
    pub publish_targeted: u64,
    /// Relay forwards of newly accepted messages to eager peers.
    pub relay_forward: u64,
    /// Cached messages served in reply to IWANT.
    pub iwant_reply: u64,
    /// Deferred IWANT replies sent on a later flush tick.
    pub iwant_reply_deferred_retry: u64,
    /// Cached messages pushed after an anti-entropy digest.
    pub anti_entropy_push: u64,
    /// Bounded retries of stranded local publishes.
    pub stranded_publish_retry: u64,
}

impl EagerSendCauseSnapshot {
    /// Sum over all causes.
    pub fn total(&self) -> u64 {
        self.publish_fanout
            + self.publish_targeted
            + self.relay_forward
            + self.iwant_reply
            + self.iwant_reply_deferred_retry
            + self.anti_entropy_push
            + self.stranded_publish_retry
    }
}

/// One Space-Saving slot.
#[derive(Debug, Clone, Copy)]
struct SignerSlot {
    signer: PeerId,
    /// Messages counted for this slot (an over-estimate by at most
    /// `overcount` when the slot was taken over from an evicted signer).
    msgs: u64,
    /// Payload replays counted since this signer took the slot.
    payload_replays: u64,
    /// The evicted slot's count inherited on takeover (Space-Saving error).
    overcount: u64,
}

/// Per-topic ingress counters. Fixed size after creation: the slot vector is
/// allocated once at `SIGNER_TRACKED_SLOTS` capacity and never grows.
#[derive(Debug)]
struct TopicIngressMeter {
    accepted_unique: u64,
    payload_replay_dropped: u64,
    sketch: [u8; SIGNER_SKETCH_REGISTERS],
    slots: Vec<SignerSlot>,
}

impl TopicIngressMeter {
    fn new() -> Self {
        Self {
            accepted_unique: 0,
            payload_replay_dropped: 0,
            sketch: [0; SIGNER_SKETCH_REGISTERS],
            slots: Vec::with_capacity(SIGNER_TRACKED_SLOTS),
        }
    }

    fn record_signer(&mut self, signer: &PeerId, payload_replay: bool) {
        // HyperLogLog update. PeerId is a SHA-256 output, so its bytes are
        // already uniform: byte 0 picks the register, bytes 1..9 the rank.
        let bytes = signer.as_bytes();
        let register = usize::from(bytes[0]);
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[1..9]);
        let rank = (u64::from_le_bytes(word).leading_zeros() + 1).min(64) as u8;
        if let Some(slot) = self.sketch.get_mut(register) {
            if rank > *slot {
                *slot = rank;
            }
        }

        let replay = u64::from(payload_replay);
        if let Some(slot) = self.slots.iter_mut().find(|s| s.signer == *signer) {
            slot.msgs = slot.msgs.saturating_add(1);
            slot.payload_replays = slot.payload_replays.saturating_add(replay);
            return;
        }
        if self.slots.len() < SIGNER_TRACKED_SLOTS {
            self.slots.push(SignerSlot {
                signer: *signer,
                msgs: 1,
                payload_replays: replay,
                overcount: 0,
            });
            return;
        }
        // Space-Saving takeover of the minimum slot.
        if let Some(min) = self.slots.iter_mut().min_by_key(|s| s.msgs) {
            let inherited = min.msgs;
            *min = SignerSlot {
                signer: *signer,
                msgs: inherited.saturating_add(1),
                payload_replays: replay,
                overcount: inherited,
            };
        }
    }

    fn distinct_signers_estimate(&self) -> u64 {
        let m = SIGNER_SKETCH_REGISTERS as f64;
        let mut zeros = 0usize;
        let mut sum = 0.0f64;
        for &register in &self.sketch {
            if register == 0 {
                zeros += 1;
            }
            sum += 2f64.powi(-i32::from(register));
        }
        if zeros == SIGNER_SKETCH_REGISTERS {
            return 0;
        }
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let raw = alpha * m * m / sum;
        let estimate = if raw <= 2.5 * m && zeros > 0 {
            m * (m / zeros as f64).ln()
        } else {
            raw
        };
        estimate.round() as u64
    }

    fn snapshot(&self) -> TopicIngressSnapshot {
        let mut slots = self.slots.clone();
        slots.sort_by(|a, b| {
            b.msgs
                .cmp(&a.msgs)
                .then_with(|| a.signer.as_bytes().cmp(b.signer.as_bytes()))
        });
        TopicIngressSnapshot {
            accepted_unique: self.accepted_unique,
            payload_replay_dropped: self.payload_replay_dropped,
            distinct_signers_estimate: self.distinct_signers_estimate(),
            top_signers: slots
                .iter()
                .take(SIGNER_TOP_N)
                .map(|slot| SignerCountSnapshot {
                    signer: hex32(slot.signer.as_bytes()),
                    msgs: slot.msgs,
                    payload_replays: slot.payload_replays,
                    max_overcount: slot.overcount,
                })
                .collect(),
        }
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// x0x #1964: one gossip signer in a topic's heavy-hitter table.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SignerCountSnapshot {
    /// Full hex PeerId of the gossip signer (SHA-256 of its ML-DSA key).
    pub signer: String,
    /// Signature-valid EAGER messages with a new msg_id from this signer
    /// (accepted + payload replays). Over-estimated by at most
    /// `max_overcount`.
    pub msgs: u64,
    /// Of those, payload replays counted since this signer took its slot.
    pub payload_replays: u64,
    /// Space-Saving error bound: `msgs - max_overcount` is a lower bound.
    pub max_overcount: u64,
}

/// x0x #1964: per-topic inbound EAGER counters.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TopicIngressSnapshot {
    /// Signature-valid EAGER messages with a new msg_id AND a new payload
    /// hash, counted before the topic validator verdict (validator drops are
    /// in `validator.by_topic`). The "unique messages" denominator.
    pub accepted_unique: u64,
    /// Signature-valid EAGER messages with a new msg_id whose payload hash
    /// was already in the topic's payload replay cache: dropped, not
    /// delivered, not forwarded.
    pub payload_replay_dropped: u64,
    /// HyperLogLog estimate (about 6.5% standard error) of distinct gossip
    /// signers over `accepted_unique + payload_replay_dropped`.
    pub distinct_signers_estimate: u64,
    /// Up to 8 signers by message count, highest first.
    pub top_signers: Vec<SignerCountSnapshot>,
}

/// Bounded per-topic ingress meters. Uses a std mutex held only for the
/// in-memory update; never held across an await.
#[derive(Debug, Default)]
pub(crate) struct TopicIngressMeters {
    topics: Mutex<HashMap<TopicId, TopicIngressMeter>>,
    topics_uncounted: AtomicU64,
    payload_replay_dropped: AtomicU64,
}

impl TopicIngressMeters {
    /// A verified EAGER message with a new msg_id and a new payload.
    pub(crate) fn record_accepted(&self, topic: TopicId, signer: &PeerId) {
        self.with_topic(topic, |meter| {
            meter.accepted_unique = meter.accepted_unique.saturating_add(1);
            meter.record_signer(signer, false);
        });
    }

    /// A verified EAGER message with a new msg_id dropped as a payload replay.
    pub(crate) fn record_payload_replay(&self, topic: TopicId, signer: &PeerId) {
        self.payload_replay_dropped.fetch_add(1, Ordering::Relaxed);
        self.with_topic(topic, |meter| {
            meter.payload_replay_dropped = meter.payload_replay_dropped.saturating_add(1);
            meter.record_signer(signer, true);
        });
    }

    /// Wave 1 (#136): an EAGER message dropped pre-verify as a live payload
    /// replay. Increments the replay counters without mixing an adjacent
    /// forwarder PeerId (or an unverified outer key) into `top_signers` or
    /// `distinct_signers_estimate`.
    pub(crate) fn record_payload_replay_pre_verify(&self, topic: TopicId) {
        self.payload_replay_dropped.fetch_add(1, Ordering::Relaxed);
        self.with_topic(topic, |meter| {
            meter.payload_replay_dropped = meter.payload_replay_dropped.saturating_add(1);
        });
    }

    fn with_topic(&self, topic: TopicId, update: impl FnOnce(&mut TopicIngressMeter)) {
        let mut guard = match self.topics.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(meter) = guard.get_mut(&topic) {
            update(meter);
            return;
        }
        if guard.len() >= TOPIC_INGRESS_METER_CAP {
            drop(guard);
            self.topics_uncounted.fetch_add(1, Ordering::Relaxed);
            return;
        }
        update(guard.entry(topic).or_insert_with(TopicIngressMeter::new));
    }

    pub(crate) fn payload_replay_dropped(&self) -> u64 {
        self.payload_replay_dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn topics_uncounted(&self) -> u64 {
        self.topics_uncounted.load(Ordering::Relaxed)
    }

    pub(crate) fn snapshot(&self) -> BTreeMap<String, TopicIngressSnapshot> {
        let guard = match self.topics.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .iter()
            .map(|(topic, meter)| (topic.to_string(), meter.snapshot()))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn tracked_slots(&self, topic: TopicId) -> usize {
        let guard = match self.topics.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(&topic).map_or(0, |meter| meter.slots.len())
    }

    #[cfg(test)]
    pub(crate) fn tracked_topics(&self) -> usize {
        let guard = match self.topics.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn signer(i: u64) -> PeerId {
        PeerId::from_pubkey(&i.to_le_bytes())
    }

    #[test]
    fn signer_table_stays_bounded_under_ten_thousand_signers() {
        let meters = TopicIngressMeters::default();
        let topic = TopicId::new([7u8; 32]);
        let heavy = signer(u64::MAX);
        for i in 0..10_000u64 {
            meters.record_payload_replay(topic, &signer(i));
            // A heavy hitter: 1 in 4 messages.
            if i % 3 == 0 {
                meters.record_payload_replay(topic, &heavy);
            }
        }
        assert!(meters.tracked_slots(topic) <= SIGNER_TRACKED_SLOTS);
        let snapshot = meters.snapshot();
        let row = snapshot.get(&topic.to_string()).expect("topic row");
        assert_eq!(row.top_signers.len(), SIGNER_TOP_N);
        assert_eq!(row.top_signers[0].signer, hex32(heavy.as_bytes()));
        // Space-Saving: the heavy hitter's lower bound is its true count.
        let true_heavy = 10_000u64.div_ceil(3);
        let top = &row.top_signers[0];
        assert!(top.msgs - top.max_overcount <= true_heavy);
        assert!(top.msgs >= true_heavy);
        assert_eq!(row.payload_replay_dropped, 10_000 + true_heavy);
        assert_eq!(meters.payload_replay_dropped(), 10_000 + true_heavy);
        let estimate = row.distinct_signers_estimate as f64;
        assert!(
            (8_000.0..=12_000.0).contains(&estimate),
            "distinct estimate {estimate} not within 20% of 10,001"
        );
    }

    #[test]
    fn topic_map_is_capped() {
        let meters = TopicIngressMeters::default();
        for i in 0..(TOPIC_INGRESS_METER_CAP + 50) {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
            meters.record_accepted(TopicId::new(bytes), &signer(1));
        }
        assert_eq!(meters.tracked_topics(), TOPIC_INGRESS_METER_CAP);
        assert_eq!(meters.topics_uncounted(), 50);
    }

    #[test]
    fn small_distinct_counts_are_close() {
        let meters = TopicIngressMeters::default();
        let topic = TopicId::new([9u8; 32]);
        for i in 0..20u64 {
            meters.record_accepted(topic, &signer(i));
            meters.record_accepted(topic, &signer(i));
        }
        let row = meters.snapshot().remove(&topic.to_string()).expect("row");
        assert_eq!(row.accepted_unique, 40);
        assert!((18..=22).contains(&row.distinct_signers_estimate));
        assert!(row.top_signers.iter().all(|s| s.msgs == 2));
    }

    #[test]
    fn cause_meters_sum() {
        let meters = EagerSendCauseMeters::default();
        meters.record(EagerSendCause::RelayForward, 5);
        meters.record(EagerSendCause::IwantReply, 1);
        meters.record(EagerSendCause::AntiEntropyPush, 0);
        let snap = meters.snapshot();
        assert_eq!(snap.relay_forward, 5);
        assert_eq!(snap.iwant_reply, 1);
        assert_eq!(snap.total(), 6);
    }
}
