//! Negotiated, hop-local compression of the outer ML-DSA public key.
//!
//! The signed `MessageHeader` is never rewritten here. A distinct marker is
//! carried only on sessions which exchanged signed legacy Ping controls.

use crate::GossipMessage;
use anyhow::{anyhow, ensure, Result};
use bytes::Bytes;
use lru::LruCache;
use saorsa_gossip_transport::AuthenticatedSession;
use saorsa_gossip_types::{PeerId, TopicId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

pub(crate) const WIRE_MARKER: &[u8; 6] = b"\xffSGKC\x03";
pub(crate) const CONTROL_DOMAIN: &str = "saorsa-gossip/key-cache-control/v1";
pub(crate) const MAX_KEY_CACHE_CONTROL_BYTES: usize = 65_536;
const CONTROL_MAGIC: [u8; 8] = *b"SGKEYC01";
const KEY_BYTES: usize = 1_952;
const SIGNATURE_BYTES: usize = 3_309;
const MAX_CONTROL_ITEMS: usize = 32;
const MAX_SESSIONS: usize = 1_024;
const MAX_QUEUED_IDS_PER_SESSION: usize = 64;
const MAX_PENDING_PER_SESSION: usize = 16;
const MAX_PENDING_BYTES_PER_SESSION: usize = 256 * 1024;
const MAX_PENDING_GLOBAL: usize = 1_024;
const MAX_PENDING_BYTES_GLOBAL: usize = 8 * 1024 * 1024;
const PENDING_TTL: Duration = Duration::from_secs(5);
const CONTROL_BURST: f64 = 2.0;

pub(crate) fn control_topic() -> TopicId {
    TopicId::from_entity(CONTROL_DOMAIN)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum KeyMaterial {
    Full { key_id: PeerId, public_key: Vec<u8> },
    Ref { key_id: PeerId },
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMessage {
    header: saorsa_gossip_types::MessageHeader,
    payload: Option<Bytes>,
    signature: Vec<u8>,
    key: KeyMaterial,
}

pub(crate) fn encode(message: &GossipMessage, reference: bool) -> Result<Bytes> {
    ensure!(
        message.public_key.len() == KEY_BYTES,
        "invalid ML-DSA public key size"
    );
    ensure!(
        message.signature.len() == SIGNATURE_BYTES,
        "invalid ML-DSA signature size"
    );
    let key_id = PeerId::from_pubkey(&message.public_key);
    let key = if reference {
        KeyMaterial::Ref { key_id }
    } else {
        KeyMaterial::Full {
            key_id,
            public_key: message.public_key.clone(),
        }
    };
    let wire = WireMessage {
        header: message.header.clone(),
        payload: message.payload.clone(),
        signature: message.signature.clone(),
        key,
    };
    let mut bytes = Vec::with_capacity(
        WIRE_MARKER.len()
            + 128
            + message.payload.as_ref().map_or(0, Bytes::len)
            + SIGNATURE_BYTES
            + KEY_BYTES,
    );
    bytes.extend_from_slice(WIRE_MARKER);
    bytes.extend_from_slice(&postcard::to_stdvec(&wire)?);
    Ok(bytes.into())
}

pub(crate) fn is_v3(bytes: &[u8]) -> bool {
    bytes.starts_with(WIRE_MARKER)
}

/// Parse postcard lengths without materializing payload, signature, or key.
/// In particular, a forged `Vec` length cannot allocate before the ML-DSA
/// shape and exact trailing-byte checks have passed.
fn take_varint(input: &mut &[u8]) -> Result<usize> {
    let mut value = 0u128;
    for index in 0..10 {
        let (&byte, rest) = input
            .split_first()
            .ok_or_else(|| anyhow!("truncated postcard length"))?;
        *input = rest;
        value |= u128::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(usize::try_from(value)?);
        }
    }
    Err(anyhow!("oversized postcard length"))
}

fn skip(input: &mut &[u8], len: usize) -> Result<()> {
    *input = input
        .get(len..)
        .ok_or_else(|| anyhow!("truncated postcard field"))?;
    Ok(())
}

fn take_payload(input: &mut &[u8], cap: Option<usize>) -> Result<()> {
    let (&tag, rest) = input
        .split_first()
        .ok_or_else(|| anyhow!("missing payload tag"))?;
    *input = rest;
    match tag {
        0 => Ok(()),
        1 => {
            let len = take_varint(input)?;
            if let Some(cap) = cap {
                ensure!(len <= cap, "key-cache control payload exceeds cap");
            }
            skip(input, len)
        }
        _ => Err(anyhow!("malformed payload tag")),
    }
}

fn take_sized_vec(input: &mut &[u8], expected: usize, label: &'static str) -> Result<()> {
    let len = take_varint(input)?;
    ensure!(len == expected, "invalid {label} size");
    skip(input, len)
}

pub(crate) fn preflight_legacy_control(bytes: &[u8]) -> Result<bool> {
    let (header, mut rest) =
        postcard::take_from_bytes::<saorsa_gossip_types::MessageHeader>(bytes)?;
    if header.topic != control_topic() {
        return Ok(false);
    }
    ensure!(
        header.kind == saorsa_gossip_types::MessageKind::Ping,
        "invalid key-cache control kind"
    );
    take_payload(&mut rest, Some(MAX_KEY_CACHE_CONTROL_BYTES))?;
    take_sized_vec(&mut rest, SIGNATURE_BYTES, "control signature")?;
    take_sized_vec(&mut rest, KEY_BYTES, "control public key")?;
    ensure!(rest.is_empty(), "trailing key-cache control frame bytes");
    Ok(true)
}

fn preflight_v3(body: &[u8]) -> Result<saorsa_gossip_types::MessageHeader> {
    let (header, mut rest) = postcard::take_from_bytes::<saorsa_gossip_types::MessageHeader>(body)?;
    let reserved_control = header.topic == control_topic();
    if reserved_control {
        ensure!(
            header.kind == saorsa_gossip_types::MessageKind::Ping,
            "invalid key-cache control kind"
        );
    }
    let control_cap = reserved_control.then_some(MAX_KEY_CACHE_CONTROL_BYTES);
    take_payload(&mut rest, control_cap)?;
    take_sized_vec(&mut rest, SIGNATURE_BYTES, "v3 signature")?;
    let variant = take_varint(&mut rest)?;
    ensure!(variant <= 1, "invalid key material tag");
    skip(&mut rest, 32)?;
    if variant == 0 {
        take_sized_vec(&mut rest, KEY_BYTES, "v3 public key")?;
    }
    ensure!(rest.is_empty(), "trailing key-cache frame bytes");
    Ok(header)
}

/// Structural, allocation-free inspection for pre-dispatch routing gates.
/// Authentication and topic policy still belong to the normal dispatcher.
pub(crate) fn inspect_header(bytes: &[u8]) -> Result<(saorsa_gossip_types::MessageHeader, bool)> {
    if let Some(body) = bytes.strip_prefix(WIRE_MARKER) {
        let header = preflight_v3(body)?;
        let reserved_control = header.topic == control_topic();
        return Ok((header, reserved_control));
    }

    let (header, mut rest) =
        postcard::take_from_bytes::<saorsa_gossip_types::MessageHeader>(bytes)?;
    let reserved_control = header.topic == control_topic();
    if reserved_control {
        preflight_legacy_control(bytes)?;
    } else {
        take_payload(&mut rest, None)?;
        take_sized_vec(&mut rest, SIGNATURE_BYTES, "legacy signature")?;
        take_sized_vec(&mut rest, KEY_BYTES, "legacy public key")?;
        // The existing dispatcher tolerates trailing bytes for ordinary,
        // unregistered legacy topics. Keep their topic gates active too.
    }
    Ok((header, reserved_control))
}

#[derive(Debug)]
pub(crate) enum Decoded {
    Full(GossipMessage, PeerId),
    Ref(GossipMessage, PeerId),
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Decoded> {
    let body = bytes
        .strip_prefix(WIRE_MARKER)
        .ok_or_else(|| anyhow!("missing key-cache wire marker"))?;
    let _ = preflight_v3(body)?;
    let (
        WireMessage {
            header,
            payload,
            signature,
            key,
        },
        trailing,
    ) = postcard::take_from_bytes(body)?;
    ensure!(trailing.is_empty(), "trailing key-cache frame bytes");
    ensure!(
        signature.len() == SIGNATURE_BYTES,
        "invalid ML-DSA signature size"
    );
    match key {
        KeyMaterial::Full { key_id, public_key } => {
            ensure!(
                public_key.len() == KEY_BYTES,
                "invalid ML-DSA public key size"
            );
            ensure!(
                PeerId::from_pubkey(&public_key) == key_id,
                "key ID hash mismatch"
            );
            Ok(Decoded::Full(
                GossipMessage {
                    header,
                    payload,
                    signature,
                    public_key,
                },
                key_id,
            ))
        }
        KeyMaterial::Ref { key_id } => Ok(Decoded::Ref(
            GossipMessage {
                header,
                payload,
                signature,
                public_key: Vec::new(),
            },
            key_id,
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ControlPayload {
    magic: [u8; 8],
    version: u8,
    pub(crate) body: Control,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Control {
    Hello { key_ref_version: u8 },
    Ack { key_ids: Vec<PeerId> },
    Request { key_ids: Vec<PeerId> },
    Response { entries: Vec<(PeerId, Vec<u8>)> },
}

pub(crate) fn encode_control(body: Control) -> Result<Bytes> {
    validate_control(&body)?;
    let payload = postcard::to_stdvec(&ControlPayload {
        magic: CONTROL_MAGIC,
        version: 1,
        body,
    })?;
    ensure!(
        payload.len() <= MAX_KEY_CACHE_CONTROL_BYTES,
        "key-cache control payload exceeds cap"
    );
    Ok(payload.into())
}

pub(crate) fn decode_control(payload: &[u8]) -> Result<Control> {
    ensure!(
        payload.len() <= MAX_KEY_CACHE_CONTROL_BYTES,
        "key-cache control payload exceeds cap"
    );
    let (value, trailing): (ControlPayload, _) = postcard::take_from_bytes(payload)?;
    ensure!(trailing.is_empty(), "trailing key-cache control bytes");
    ensure!(
        value.magic == CONTROL_MAGIC && value.version == 1,
        "invalid key-cache control magic or version"
    );
    validate_control(&value.body)?;
    Ok(value.body)
}

fn validate_control(control: &Control) -> Result<()> {
    match control {
        Control::Hello { key_ref_version } => {
            ensure!(*key_ref_version == 1, "unsupported key reference version")
        }
        Control::Ack { key_ids } | Control::Request { key_ids } => {
            ensure!(
                !key_ids.is_empty() && key_ids.len() <= MAX_CONTROL_ITEMS,
                "invalid key ID count"
            );
        }
        Control::Response { entries } => {
            ensure!(
                !entries.is_empty() && entries.len() <= MAX_CONTROL_ITEMS,
                "invalid response entry count"
            );
            for (key_id, key) in entries {
                ensure!(key.len() == KEY_BYTES, "invalid response key size");
                ensure!(
                    PeerId::from_pubkey(key) == *key_id,
                    "response key ID hash mismatch"
                );
            }
        }
    }
    Ok(())
}

/// Process-local counters for outer-key wire compression and recovery.
#[derive(Debug, Clone, Default, Serialize)]
pub struct KeyCacheSnapshot {
    /// Ordinary legacy or v3 full-key frames submitted for outbound transport;
    /// a counted submission can still fail during the transport send.
    pub full_out_frames: u64,
    /// Final serialized bytes in outbound full-key frame submissions.
    pub full_out_bytes: u64,
    /// Reference frames submitted for outbound transport.
    pub ref_out_frames: u64,
    /// Serialized bytes in outbound reference frames.
    pub ref_out_bytes: u64,
    /// Decoded inbound full-key frames (verification follows per-kind dispatch).
    pub full_in_frames: u64,
    /// Serialized bytes in inbound full-key frames.
    pub full_in_bytes: u64,
    /// Decoded inbound reference frames and queued reference misses.
    pub ref_in_frames: u64,
    /// Serialized bytes in inbound reference frames.
    pub ref_in_bytes: u64,
    /// Resolved-key cache hits.
    pub cache_hits: u64,
    /// Resolved-key cache misses.
    pub cache_misses: u64,
    /// Resolved-key LRU evictions.
    pub cache_evictions: u64,
    /// Key request control batches emitted.
    pub requests: u64,
    /// Key response control batches emitted.
    pub responses: u64,
    /// Highest count of pending unresolved frames.
    pub pending_frames_high_water: usize,
    /// Highest byte count of pending unresolved frames.
    pub pending_bytes_high_water: usize,
    /// Pending frames discarded after five seconds.
    pub pending_timeouts: u64,
    /// Frames dropped at the adjacent-session cap.
    pub pending_peer_limit_drops: u64,
    /// Frames dropped at the global pending cap.
    pub pending_global_limit_drops: u64,
    /// Invalid cache-control payloads.
    pub malformed_controls: u64,
    /// Full or response keys whose claimed ID did not match their hash.
    pub hash_mismatches: u64,
    /// Pending frames successfully replayed after key fill.
    pub replay_success: u64,
    /// Pending frames failing replay after key fill.
    pub replay_failure: u64,
}

#[derive(Clone)]
struct PendingFrame {
    bytes: Bytes,
    received_at: Instant,
}

#[derive(Default)]
struct PendingKey {
    frames: VecDeque<PendingFrame>,
    requested: bool,
}

struct SessionState {
    generation: u64,
    hello_sent: bool,
    hello_pending: bool,
    hello_received: bool,
    queued_acks: HashSet<PeerId>,
    queued_requests: HashSet<PeerId>,
    queued_responses: HashSet<PeerId>,
    tokens: f64,
    last_refill: Instant,
}

impl SessionState {
    fn new(generation: u64, now: Instant) -> Self {
        Self {
            generation,
            hello_sent: false,
            hello_pending: false,
            hello_received: false,
            queued_acks: HashSet::new(),
            queued_requests: HashSet::new(),
            queued_responses: HashSet::new(),
            tokens: CONTROL_BURST,
            last_refill: now,
        }
    }

    fn consume_token(&mut self, now: Instant) -> bool {
        self.tokens = (self.tokens
            + now
                .saturating_duration_since(self.last_refill)
                .as_secs_f64())
        .min(CONTROL_BURST);
        self.last_refill = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

pub(crate) struct KeyCache {
    resolved: LruCache<PeerId, Vec<u8>>,
    acknowledged: LruCache<(PeerId, u64, PeerId), ()>,
    announced_inbound: LruCache<(PeerId, u64, PeerId), ()>,
    sessions: HashMap<PeerId, SessionState>,
    pending: HashMap<(PeerId, u64, PeerId), PendingKey>,
    pending_frames: usize,
    pending_bytes: usize,
    stats: KeyCacheSnapshot,
}

impl Default for KeyCache {
    fn default() -> Self {
        Self {
            // SAFETY: both capacities are positive constants.
            resolved: LruCache::new(unsafe { NonZeroUsize::new_unchecked(2048) }),
            // SAFETY: both capacities are positive constants.
            acknowledged: LruCache::new(unsafe { NonZeroUsize::new_unchecked(4096) }),
            // SAFETY: both capacities are positive constants.
            announced_inbound: LruCache::new(unsafe { NonZeroUsize::new_unchecked(4096) }),
            sessions: HashMap::new(),
            pending: HashMap::new(),
            pending_frames: 0,
            pending_bytes: 0,
            stats: KeyCacheSnapshot::default(),
        }
    }
}

impl KeyCache {
    pub(crate) fn snapshot(&self) -> KeyCacheSnapshot {
        self.stats.clone()
    }

    pub(crate) fn observe_session(&mut self, session: AuthenticatedSession, now: Instant) {
        match self.sessions.get(&session.peer) {
            Some(s) if s.generation == session.generation => return,
            Some(_) => self.clear_peer(session.peer),
            None => {}
        }
        if self.sessions.len() < MAX_SESSIONS {
            self.sessions
                .insert(session.peer, SessionState::new(session.generation, now));
        }
    }

    fn clear_peer(&mut self, peer: PeerId) {
        self.sessions.remove(&peer);
        let keys: Vec<_> = self
            .pending
            .keys()
            .copied()
            .filter(|(p, _, _)| *p == peer)
            .collect();
        for key in keys {
            self.remove_pending(&key);
        }
        let ack_keys: Vec<_> = self
            .acknowledged
            .iter()
            .filter_map(|(key, _)| (key.0 == peer).then_some(*key))
            .collect();
        for key in ack_keys {
            self.acknowledged.pop(&key);
        }
        let inbound_keys: Vec<_> = self
            .announced_inbound
            .iter()
            .filter_map(|(key, _)| (key.0 == peer).then_some(*key))
            .collect();
        for key in inbound_keys {
            self.announced_inbound.pop(&key);
        }
    }

    pub(crate) fn forget_stale(&mut self, session: AuthenticatedSession) {
        if self
            .sessions
            .get(&session.peer)
            .is_some_and(|state| state.generation == session.generation)
        {
            self.clear_peer(session.peer);
        }
    }

    pub(crate) fn supports_ref(&mut self, session: AuthenticatedSession, signer: PeerId) -> bool {
        self.sessions.get(&session.peer).is_some_and(|state| {
            state.generation == session.generation && state.hello_sent && state.hello_received
        }) && self
            .acknowledged
            .get(&(session.peer, session.generation, signer))
            .is_some()
    }

    pub(crate) fn supports_v3(&self, session: AuthenticatedSession) -> bool {
        self.sessions.get(&session.peer).is_some_and(|state| {
            state.generation == session.generation && state.hello_sent && state.hello_received
        })
    }

    pub(crate) fn received_hello(&mut self, session: AuthenticatedSession) {
        if let Some(state) = self.sessions.get_mut(&session.peer) {
            if state.generation == session.generation {
                state.hello_received = true;
            }
        }
    }

    pub(crate) fn acknowledge(&mut self, session: AuthenticatedSession, keys: &[PeerId]) {
        if self
            .sessions
            .get(&session.peer)
            .is_some_and(|s| s.generation == session.generation)
        {
            for key in keys {
                self.acknowledged
                    .put((session.peer, session.generation, *key), ());
            }
        }
    }

    pub(crate) fn note_verified_key(
        &mut self,
        key_id: PeerId,
        key: Vec<u8>,
        session: Option<AuthenticatedSession>,
    ) {
        if key.len() != KEY_BYTES || PeerId::from_pubkey(&key) != key_id {
            self.stats.hash_mismatches = self.stats.hash_mismatches.saturating_add(1);
            return;
        }
        if !self.resolved.contains(&key_id) && self.resolved.len() == self.resolved.cap().get() {
            self.stats.cache_evictions = self.stats.cache_evictions.saturating_add(1);
        }
        self.resolved.put(key_id, key);
        if let Some(session) = session {
            let announcement = (session.peer, session.generation, key_id);
            let novel = !self.announced_inbound.contains(&announcement);
            self.announced_inbound.put(announcement, ());
            if let Some(state) = self.sessions.get_mut(&session.peer) {
                if state.generation == session.generation
                    && novel
                    && state.queued_acks.len() < MAX_QUEUED_IDS_PER_SESSION
                {
                    state.queued_acks.insert(key_id);
                }
            }
        }
    }

    pub(crate) fn lookup(&mut self, key_id: PeerId) -> Option<Vec<u8>> {
        let key = self.resolved.get(&key_id).cloned();
        if key.is_some() {
            self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
        } else {
            self.stats.cache_misses = self.stats.cache_misses.saturating_add(1);
        }
        key
    }

    #[cfg(test)]
    pub(crate) fn evict_key(&mut self, key_id: PeerId) {
        self.resolved.pop(&key_id);
    }

    pub(crate) fn queue_miss(
        &mut self,
        session: AuthenticatedSession,
        key_id: PeerId,
        bytes: Bytes,
        now: Instant,
    ) -> bool {
        self.expire(now);
        let peer_frames = self
            .pending
            .iter()
            .filter(|((p, g, _), _)| *p == session.peer && *g == session.generation)
            .map(|(_, v)| v.frames.len())
            .sum::<usize>();
        let peer_bytes = self
            .pending
            .iter()
            .filter(|((p, g, _), _)| *p == session.peer && *g == session.generation)
            .flat_map(|(_, v)| v.frames.iter())
            .map(|f| f.bytes.len())
            .sum::<usize>();
        if peer_frames >= MAX_PENDING_PER_SESSION
            || peer_bytes.saturating_add(bytes.len()) > MAX_PENDING_BYTES_PER_SESSION
        {
            self.stats.pending_peer_limit_drops =
                self.stats.pending_peer_limit_drops.saturating_add(1);
            return false;
        }
        if self.pending_frames >= MAX_PENDING_GLOBAL
            || self.pending_bytes.saturating_add(bytes.len()) > MAX_PENDING_BYTES_GLOBAL
        {
            self.stats.pending_global_limit_drops =
                self.stats.pending_global_limit_drops.saturating_add(1);
            return false;
        }
        let entry = self
            .pending
            .entry((session.peer, session.generation, key_id))
            .or_default();
        if !entry.requested {
            if let Some(state) = self.sessions.get_mut(&session.peer) {
                if state.generation == session.generation
                    && state.queued_requests.len() < MAX_QUEUED_IDS_PER_SESSION
                {
                    state.queued_requests.insert(key_id);
                    entry.requested = true;
                }
            }
        }
        if !entry.requested {
            self.stats.pending_peer_limit_drops =
                self.stats.pending_peer_limit_drops.saturating_add(1);
            return false;
        }
        entry.frames.push_back(PendingFrame {
            bytes: bytes.clone(),
            received_at: now,
        });
        self.pending_frames += 1;
        self.pending_bytes += bytes.len();
        self.stats.pending_frames_high_water = self
            .stats
            .pending_frames_high_water
            .max(self.pending_frames);
        self.stats.pending_bytes_high_water =
            self.stats.pending_bytes_high_water.max(self.pending_bytes);
        true
    }

    pub(crate) fn take_pending(
        &mut self,
        session: AuthenticatedSession,
        key_id: PeerId,
    ) -> Vec<Bytes> {
        let key = (session.peer, session.generation, key_id);
        self.remove_pending(&key)
            .map(|v| v.frames.into_iter().map(|f| f.bytes).collect())
            .unwrap_or_default()
    }

    fn remove_pending(&mut self, key: &(PeerId, u64, PeerId)) -> Option<PendingKey> {
        let entry = self.pending.remove(key)?;
        self.pending_frames = self.pending_frames.saturating_sub(entry.frames.len());
        self.pending_bytes = self
            .pending_bytes
            .saturating_sub(entry.frames.iter().map(|f| f.bytes.len()).sum::<usize>());
        Some(entry)
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        let keys: Vec<_> = self.pending.keys().copied().collect();
        for key in keys {
            let mut empty = false;
            if let Some(entry) = self.pending.get_mut(&key) {
                while entry.frames.front().is_some_and(|frame| {
                    now.saturating_duration_since(frame.received_at) >= PENDING_TTL
                }) {
                    if let Some(frame) = entry.frames.pop_front() {
                        self.pending_frames = self.pending_frames.saturating_sub(1);
                        self.pending_bytes = self.pending_bytes.saturating_sub(frame.bytes.len());
                        self.stats.pending_timeouts = self.stats.pending_timeouts.saturating_add(1);
                    }
                }
                empty = entry.frames.is_empty();
            }
            if empty {
                self.pending.remove(&key);
                if let Some(state) = self.sessions.get_mut(&key.0) {
                    if state.generation == key.1 {
                        state.queued_requests.remove(&key.2);
                    }
                }
            }
        }
    }

    pub(crate) fn queue_responses(&mut self, session: AuthenticatedSession, ids: &[PeerId]) {
        if let Some(state) = self.sessions.get_mut(&session.peer) {
            if state.generation == session.generation {
                for id in ids {
                    if self.resolved.contains(id)
                        && state.queued_responses.len() < MAX_QUEUED_IDS_PER_SESSION
                    {
                        state.queued_responses.insert(*id);
                    }
                }
            }
        }
    }

    pub(crate) fn next_control(
        &mut self,
        session: AuthenticatedSession,
        now: Instant,
    ) -> Option<Control> {
        self.expire(now);
        let state = self.sessions.get_mut(&session.peer)?;
        if state.generation != session.generation {
            return None;
        }
        if !state.hello_sent {
            if state.hello_pending {
                return None;
            }
            if !state.consume_token(now) {
                return None;
            }
            state.hello_pending = true;
            return Some(Control::Hello { key_ref_version: 1 });
        }
        if !state.hello_received {
            return None;
        }
        if !state.queued_requests.is_empty() {
            if !state.consume_token(now) {
                return None;
            }
            let key_ids = take_ids(&mut state.queued_requests);
            self.stats.requests = self.stats.requests.saturating_add(1);
            return Some(Control::Request { key_ids });
        }
        if !state.queued_responses.is_empty() {
            if !state.consume_token(now) {
                return None;
            }
            let key_ids = take_ids(&mut state.queued_responses);
            let entries: Vec<_> = key_ids
                .into_iter()
                .filter_map(|id| self.resolved.get(&id).cloned().map(|key| (id, key)))
                .collect();
            if entries.is_empty() {
                return None;
            }
            self.stats.responses = self.stats.responses.saturating_add(1);
            return Some(Control::Response { entries });
        }
        if !state.queued_acks.is_empty() {
            if !state.consume_token(now) {
                return None;
            }
            let key_ids = take_ids(&mut state.queued_acks);
            return Some(Control::Ack { key_ids });
        }
        None
    }

    pub(crate) fn control_result(
        &mut self,
        session: AuthenticatedSession,
        control: &Control,
        sent: bool,
    ) {
        let Some(state) = self.sessions.get_mut(&session.peer) else {
            return;
        };
        if state.generation != session.generation {
            return;
        }
        match control {
            Control::Hello { .. } => {
                state.hello_pending = false;
                if sent {
                    state.hello_sent = true;
                }
            }
            Control::Ack { key_ids } if !sent => {
                for id in key_ids {
                    if state.queued_acks.len() < MAX_QUEUED_IDS_PER_SESSION {
                        state.queued_acks.insert(*id);
                    }
                }
            }
            Control::Request { key_ids } if !sent => {
                for id in key_ids {
                    if state.queued_requests.len() < MAX_QUEUED_IDS_PER_SESSION {
                        state.queued_requests.insert(*id);
                    }
                }
            }
            Control::Response { entries } if !sent => {
                for (id, _) in entries {
                    if state.queued_responses.len() < MAX_QUEUED_IDS_PER_SESSION {
                        state.queued_responses.insert(*id);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn sessions(&self) -> Vec<AuthenticatedSession> {
        self.sessions
            .iter()
            .map(|(peer, state)| AuthenticatedSession {
                peer: *peer,
                generation: state.generation,
            })
            .collect()
    }

    pub(crate) fn record_outbound(&mut self, reference: bool, bytes: usize) {
        if reference {
            self.stats.ref_out_frames = self.stats.ref_out_frames.saturating_add(1);
            self.stats.ref_out_bytes = self.stats.ref_out_bytes.saturating_add(bytes as u64);
        } else {
            self.stats.full_out_frames = self.stats.full_out_frames.saturating_add(1);
            self.stats.full_out_bytes = self.stats.full_out_bytes.saturating_add(bytes as u64);
        }
    }
    pub(crate) fn record_inbound(&mut self, reference: bool, bytes: usize) {
        if reference {
            self.stats.ref_in_frames = self.stats.ref_in_frames.saturating_add(1);
            self.stats.ref_in_bytes = self.stats.ref_in_bytes.saturating_add(bytes as u64);
        } else {
            self.stats.full_in_frames = self.stats.full_in_frames.saturating_add(1);
            self.stats.full_in_bytes = self.stats.full_in_bytes.saturating_add(bytes as u64);
        }
    }
    pub(crate) fn malformed_control(&mut self) {
        self.stats.malformed_controls = self.stats.malformed_controls.saturating_add(1);
    }
    pub(crate) fn hash_mismatch(&mut self) {
        self.stats.hash_mismatches = self.stats.hash_mismatches.saturating_add(1);
    }
    pub(crate) fn replay_result(&mut self, success: bool) {
        if success {
            self.stats.replay_success = self.stats.replay_success.saturating_add(1);
        } else {
            self.stats.replay_failure = self.stats.replay_failure.saturating_add(1);
        }
    }
}

fn take_ids(ids: &mut HashSet<PeerId>) -> Vec<PeerId> {
    let batch: Vec<_> = ids.iter().take(MAX_CONTROL_ITEMS).copied().collect();
    for id in &batch {
        ids.remove(id);
    }
    batch
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use saorsa_gossip_types::{MessageHeader, MessageKind};

    fn session(id: u8, generation: u64) -> AuthenticatedSession {
        AuthenticatedSession {
            peer: PeerId::new([id; 32]),
            generation,
        }
    }

    fn sample_message() -> GossipMessage {
        let payload = Bytes::from_static(b"wire payload");
        let mut header = MessageHeader::new(TopicId::new([7; 32]), MessageKind::Eager, 10);
        header.msg_id = [4; 32];
        header.seal_payload_hash(Some(&payload));
        GossipMessage {
            header,
            payload: Some(payload),
            signature: vec![9; SIGNATURE_BYTES],
            public_key: vec![3; KEY_BYTES],
        }
    }

    #[test]
    fn v3_full_and_ref_round_trip_and_reject_malformed_material() {
        let message = sample_message();
        let legacy = postcard::to_stdvec(&message).expect("legacy frame");
        let full = encode(&message, false).expect("full frame");
        let reference = encode(&message, true).expect("reference frame");
        assert!(full.len() > legacy.len());
        assert_eq!(
            legacy.len() - reference.len(),
            KEY_BYTES + 2 - WIRE_MARKER.len() - 32 - 1
        );
        assert!(matches!(
            decode(&full).expect("full decode"),
            Decoded::Full(_, _)
        ));
        assert!(matches!(
            decode(&reference).expect("reference decode"),
            Decoded::Ref(_, _)
        ));
        assert!(decode(&reference[..reference.len() - 1]).is_err());
        let mut trailing = full.to_vec();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let mut bad_tag = full.to_vec();
        let tag_offset = WIRE_MARKER.len()
            + postcard::to_stdvec(&(&message.header, &message.payload, &message.signature))
                .expect("prefix")
                .len();
        bad_tag[tag_offset] = 0xff;
        assert!(decode(&bad_tag).is_err());
        let mismatched = WireMessage {
            header: message.header.clone(),
            payload: message.payload.clone(),
            signature: message.signature.clone(),
            key: KeyMaterial::Full {
                key_id: PeerId::new([0; 32]),
                public_key: message.public_key.clone(),
            },
        };
        let mut wrong_id = WIRE_MARKER.to_vec();
        wrong_id.extend(postcard::to_stdvec(&mismatched).expect("frame"));
        assert!(decode(&wrong_id).is_err());
        let oversized = WireMessage {
            header: message.header.clone(),
            payload: message.payload.clone(),
            signature: message.signature.clone(),
            key: KeyMaterial::Full {
                key_id: PeerId::new([0; 32]),
                public_key: vec![0; KEY_BYTES + 1],
            },
        };
        let mut oversized_wire = WIRE_MARKER.to_vec();
        oversized_wire.extend(postcard::to_stdvec(&oversized).expect("frame"));
        assert!(decode(&oversized_wire)
            .unwrap_err()
            .to_string()
            .contains("public key size"));
        let mut bad_key = message.clone();
        bad_key.public_key.pop();
        assert!(encode(&bad_key, false).is_err());
        let mut bad_sig = message;
        bad_sig.signature.pop();
        assert!(encode(&bad_sig, true).is_err());
    }

    #[test]
    fn legacy_v1_v2_bytes_are_exact_and_round_trip() {
        let mut header = MessageHeader::new(TopicId::new([0; 32]), MessageKind::Eager, 10);
        header.msg_id = [1; 32];
        let message = GossipMessage {
            header,
            payload: Some(Bytes::from_static(&[5])),
            signature: vec![3, 4],
            public_key: vec![1, 2],
        };
        let mut expected_v1 = vec![1];
        expected_v1.extend([0; 32]);
        expected_v1.extend([1; 32]);
        expected_v1.extend([0, 0, 10, 1, 1, 5, 2, 3, 4, 2, 1, 2]);
        let v1 = postcard::to_stdvec(&message).expect("v1");
        assert_eq!(v1, expected_v1);
        let decoded: GossipMessage = postcard::from_bytes(&v1).expect("v1 decode");
        assert_eq!(postcard::to_stdvec(&decoded).expect("v1 reencode"), v1);

        let mut v2_message = message;
        v2_message
            .header
            .seal_payload_hash(v2_message.payload.as_deref());
        let mut expected_v2 = vec![2];
        expected_v2.extend([0; 32]);
        expected_v2.extend([1; 32]);
        expected_v2.extend([0, 0, 10, 1]);
        expected_v2.extend(blake3::hash(&[5]).as_bytes());
        expected_v2.extend([1, 1, 5, 2, 3, 4, 2, 1, 2]);
        let v2 = postcard::to_stdvec(&v2_message).expect("v2");
        assert_eq!(v2, expected_v2);
        let decoded: GossipMessage = postcard::from_bytes(&v2).expect("v2 decode");
        assert_eq!(postcard::to_stdvec(&decoded).expect("v2 reencode"), v2);
    }

    #[test]
    fn controls_pin_count_size_and_hash_bounds() {
        let ids = vec![PeerId::new([1; 32]); 32];
        assert!(encode_control(Control::Request {
            key_ids: ids.clone()
        })
        .is_ok());
        let mut too_many = ids;
        too_many.push(PeerId::new([2; 32]));
        assert!(encode_control(Control::Request { key_ids: too_many }).is_err());
        let key = vec![7; KEY_BYTES];
        let id = PeerId::from_pubkey(&key);
        let entries = vec![(id, key); 32];
        let payload =
            encode_control(Control::Response { entries }).expect("32 keys fit serialized cap");
        assert!(payload.len() <= MAX_KEY_CACHE_CONTROL_BYTES);
        assert!(decode_control(&payload).is_ok());
        assert!(decode_control(&vec![0; MAX_KEY_CACHE_CONTROL_BYTES + 1]).is_err());
        assert!(encode_control(Control::Response {
            entries: vec![(PeerId::new([0; 32]), vec![7; KEY_BYTES])]
        })
        .is_err());
        let bad_response = postcard::to_stdvec(&ControlPayload {
            magic: CONTROL_MAGIC,
            version: 1,
            body: Control::Response {
                entries: vec![(PeerId::new([0; 32]), vec![7; KEY_BYTES])],
            },
        })
        .expect("bad response encoding");
        assert!(decode_control(&bad_response)
            .unwrap_err()
            .to_string()
            .contains("hash mismatch"));

        let mut header = MessageHeader::new(control_topic(), MessageKind::Ping, 1);
        header.seal_payload_hash(None);
        let outer = |len| GossipMessage {
            header: header.clone(),
            payload: Some(Bytes::from(vec![0; len])),
            signature: vec![0; SIGNATURE_BYTES],
            public_key: vec![0; KEY_BYTES],
        };
        let at_cap = postcard::to_stdvec(&outer(MAX_KEY_CACHE_CONTROL_BYTES)).expect("outer");
        assert!(preflight_legacy_control(&at_cap).expect("exact cap"));
        let over_cap = postcard::to_stdvec(&outer(MAX_KEY_CACHE_CONTROL_BYTES + 1)).expect("outer");
        assert!(preflight_legacy_control(&over_cap).is_err());
    }

    #[test]
    fn hello_ack_and_reconnect_are_session_scoped() {
        let now = Instant::now();
        let mut cache = KeyCache::default();
        let first = session(1, 3);
        let author = PeerId::new([9; 32]);
        cache.observe_session(first, now);
        assert!(matches!(
            cache.next_control(first, now),
            Some(Control::Hello { .. })
        ));
        cache.control_result(first, &Control::Hello { key_ref_version: 1 }, true);
        cache.received_hello(first);
        assert!(!cache.supports_ref(first, author));
        cache.acknowledge(first, &[author]);
        assert!(cache.supports_ref(first, author));
        let next = session(1, 4);
        cache.observe_session(next, now);
        assert!(!cache.supports_v3(next));
        assert!(!cache.supports_ref(next, author));
        assert!(matches!(
            cache.next_control(next, now),
            Some(Control::Hello { .. })
        ));
    }

    #[test]
    fn failed_control_send_does_not_enable_refs_or_lose_request() {
        let now = Instant::now();
        let mut cache = KeyCache::default();
        let adjacent = session(1, 1);
        let author = PeerId::new([8; 32]);
        cache.observe_session(adjacent, now);
        let hello = cache.next_control(adjacent, now).expect("hello");
        cache.received_hello(adjacent);
        cache.acknowledge(adjacent, &[author]);
        assert!(!cache.supports_ref(adjacent, author));
        cache.control_result(adjacent, &hello, false);
        assert!(!cache.supports_v3(adjacent));
        let hello_retry = cache
            .next_control(adjacent, now + Duration::from_secs(1))
            .expect("hello retry");
        cache.control_result(adjacent, &hello_retry, true);
        assert!(cache.supports_ref(adjacent, author));

        assert!(cache.queue_miss(adjacent, author, Bytes::from_static(b"miss"), now));
        let request = cache
            .next_control(adjacent, now + Duration::from_secs(2))
            .expect("request");
        cache.control_result(adjacent, &request, false);
        assert!(matches!(
            cache.next_control(adjacent, now + Duration::from_secs(3)),
            Some(Control::Request { key_ids }) if key_ids == vec![author]
        ));
    }

    #[test]
    fn pending_miss_coalesces_request_and_expires_individual_frames() {
        let now = Instant::now();
        let mut cache = KeyCache::default();
        let session = session(1, 1);
        let author = PeerId::new([8; 32]);
        cache.observe_session(session, now);
        assert!(matches!(
            cache.next_control(session, now),
            Some(Control::Hello { .. })
        ));
        cache.control_result(session, &Control::Hello { key_ref_version: 1 }, true);
        cache.received_hello(session);
        for _ in 0..MAX_PENDING_PER_SESSION {
            assert!(cache.queue_miss(session, author, Bytes::from(vec![0; 1024]), now));
        }
        assert!(!cache.queue_miss(session, author, Bytes::from(vec![0; 1024]), now));
        assert!(
            matches!(cache.next_control(session, now), Some(Control::Request { key_ids }) if key_ids == vec![author])
        );
        assert!(cache.next_control(session, now).is_none());
        cache.expire(now + PENDING_TTL);
        assert_eq!(cache.pending_frames, 0);
        assert_eq!(cache.stats.pending_timeouts, MAX_PENDING_PER_SESSION as u64);
        assert!(cache.take_pending(session, author).is_empty());
    }

    #[test]
    fn shared_control_bucket_sustains_one_frame_per_second() {
        let now = Instant::now();
        let mut cache = KeyCache::default();
        let session = session(1, 1);
        cache.observe_session(session, now);
        cache.received_hello(session);
        cache.note_verified_key(PeerId::new([1; 32]), vec![1; KEY_BYTES], None);
        let hello = cache.next_control(session, now).expect("hello");
        cache.control_result(session, &hello, true);
        cache
            .sessions
            .get_mut(&session.peer)
            .expect("session")
            .queued_acks
            .insert(PeerId::new([2; 32]));
        assert!(cache.next_control(session, now).is_some());
        cache
            .sessions
            .get_mut(&session.peer)
            .expect("session")
            .queued_acks
            .insert(PeerId::new([3; 32]));
        assert!(cache.next_control(session, now).is_none());
        assert!(cache
            .next_control(session, now + Duration::from_secs(1))
            .is_some());
    }
}
