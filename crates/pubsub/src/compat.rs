//! ADR-013 opt-in migration for audited x0x Signed KV envelopes.
//!
//! Registration and grants are trusted local policy operations. They are never
//! inferred from received traffic. See `docs/design/legacy-kv-compat.md` for the
//! consumer authentication contract and the remaining legacy availability risk.

use crate::{GossipMessage, SignaturePolicy};
use anyhow::{anyhow, ensure, Result};
use bytes::Bytes;
use saorsa_gossip_identity::MlDsaKeyPair;
use saorsa_gossip_transport::{AuthenticatedSession, GossipStreamType, GossipTransport};
use saorsa_gossip_types::{MessageKind, PeerId, TopicId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const MAX_TOPICS: usize = 128;
const MAX_GRANTS: usize = 1024;
const MAX_AUTHORS: usize = 1024;
const MAX_INNER_BYTES: usize = 1024 * 1024;
const MAX_CONTROL_IDS: usize = 1024;
const MAX_CONTROL_BYTES: usize = 1 + 2 + MAX_CONTROL_IDS * 32;
const MAX_CONTROL_WORK: usize = 4096;
const MAX_VARIANT_BYTES: usize = 4 * 1024 * 1024;

/// Required receiver profile: the audited outer adapter plus inner V3 support.
///
/// This is a pairing requirement, not an existing x0x release identifier. Stock
/// x0x 0.30.1 cannot consume V3; its paired signer/decoder must be audited before
/// enabling grants. The old receiver identifier is deliberately ineligible.
pub const AUDITED_RECEIVER: &str = "x0x/0.30.1+signed-kv-inner-v3;saorsa-gossip-pubsub/0.5.66";

/// Reviewed Signed KV topic family. Both require the topic-bound V3 inner envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignedKvFamily {
    /// The concrete KV store delta topic chosen by the consuming application.
    Delta,
    /// The same store's `<delta-topic>/state-sync` recovery channel.
    StateSync,
}

/// Immutable verifier policy for one concrete Signed store topic.
///
/// The caller must establish the store's Signed policy and audited receive/apply
/// path before registration. This is not a generic raw-payload exemption.
#[derive(Clone)]
pub struct SignedKvTopic {
    topic_name: String,
    family: SignedKvFamily,
    revision: u64,
    authors: HashSet<PeerId>,
}

impl SignedKvTopic {
    /// Register exact topic bytes, family, positive revision and known authors.
    /// Keys embedded in an envelope must derive an author in this roster.
    pub fn new(
        delta_topic: &str,
        family: SignedKvFamily,
        revision: u64,
        known_authors: impl IntoIterator<Item = PeerId>,
    ) -> Result<Self> {
        ensure!(
            !delta_topic.is_empty() && delta_topic.len() <= 512,
            "invalid KV topic"
        );
        ensure!(revision > 0, "verifier revision must be positive");
        let authors: HashSet<_> = known_authors.into_iter().take(MAX_AUTHORS + 1).collect();
        ensure!(
            !authors.is_empty() && authors.len() <= MAX_AUTHORS,
            "invalid author roster"
        );
        let topic_name = match family {
            SignedKvFamily::Delta => delta_topic.to_owned(),
            SignedKvFamily::StateSync => format!("{delta_topic}/state-sync"),
        };
        Ok(Self {
            topic_name,
            family,
            revision,
            authors,
        })
    }

    /// The exact gossip topic covered by this policy.
    pub fn topic(&self) -> TopicId {
        TopicId::from_entity(&self.topic_name)
    }

    /// The reviewed topic family.
    pub fn family(&self) -> SignedKvFamily {
        self.family
    }

    fn verify(&self, bytes: &[u8]) -> Result<VerifiedInner> {
        ensure!(
            bytes.len() <= MAX_INNER_BYTES,
            "inner envelope exceeds bound"
        );
        ensure!(
            bytes.first() == Some(&3),
            "only topic-bound V3 inner envelopes are eligible"
        );
        let author_bytes: [u8; 32] = bytes
            .get(1..33)
            .ok_or_else(|| anyhow!("truncated author"))?
            .try_into()?;
        let mut rest = &bytes[33..];
        let key = take_field(&mut rest)?;
        let signature = take_field(&mut rest)?;
        let topic = take_field(&mut rest)?;
        ensure!(
            key.len() == 1952 && signature.len() == 3309,
            "invalid ML-DSA-65 shape"
        );
        ensure!(topic == self.topic_name.as_bytes(), "inner topic mismatch");
        let author = PeerId::from_pubkey(key);
        ensure!(
            author.as_bytes() == &author_bytes && self.authors.contains(&author),
            "unknown or mismatched inner author"
        );
        // V3 signs the canonical wire topic length. Never fall back to the
        // ambiguous V2 preimage, even for an otherwise valid rostered author.
        let topic_len = u16::try_from(topic.len())?;
        let mut signed =
            Vec::with_capacity(b"x0x-msg-v3".len() + 32 + 2 + topic.len() + rest.len());
        signed.extend_from_slice(b"x0x-msg-v3");
        signed.extend_from_slice(&author_bytes);
        signed.extend_from_slice(&topic_len.to_be_bytes());
        signed.extend_from_slice(topic);
        signed.extend_from_slice(rest);
        ensure!(
            MlDsaKeyPair::verify(key, &signed, signature)?,
            "invalid inner signature"
        );
        Ok(VerifiedInner {
            author,
            digest: *blake3::hash(bytes).as_bytes(),
            revision: self.revision,
        })
    }
}

fn take_field<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8]> {
    let prefix: [u8; 2] = rest
        .get(..2)
        .ok_or_else(|| anyhow!("truncated inner field"))?
        .try_into()?;
    let n = usize::from(u16::from_be_bytes(prefix));
    let field = rest
        .get(2..2 + n)
        .ok_or_else(|| anyhow!("truncated inner field"))?;
    *rest = &rest[2 + n..];
    Ok(field)
}

/// Verified metadata binds author and policy to the complete unchanged bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedInner {
    /// Independently authenticated application author, not the outer signer.
    pub author: PeerId,
    /// Hash of the entire inner envelope, including its signature.
    pub digest: [u8; 32],
    /// Verifier policy revision used for admission.
    pub revision: u64,
}

/// Explicit, finite trusted-roster permission. Never deserialized from the wire.
#[derive(Clone)]
pub struct LegacyGrant {
    /// Authenticated adjacent transport identity.
    pub peer: PeerId,
    /// One exact registered topic.
    pub topic: TopicId,
    /// Audited receiver version; must equal [`AUDITED_RECEIVER`].
    pub receiver: String,
    /// Exact verifier revision.
    pub verifier_revision: u64,
    /// Strictly increasing per-peer/topic policy revision, including revocations.
    pub revision: u64,
    /// Trusted local issuer, retained for inventory (at most 128 bytes).
    pub issuer: String,
    /// Operator reason, retained for inventory (at most 256 bytes).
    pub reason: String,
    /// Wall-clock expiry; converted to a monotonic deadline at issuance.
    pub expires: SystemTime,
}

struct BoundGrant {
    grant: LegacyGrant,
    deadline: Instant,
    session: AuthenticatedSession,
}

/// Durable append-only modern floors. Opening missing/corrupt storage fails;
/// callers must explicitly initialize it once. There is no clear/demotion API.
/// The directory must be on trusted durable storage, protected from replacement.
pub struct ModernFloors {
    path: PathBuf,
    peers: HashSet<PeerId>,
    stamp: Option<FloorStamp>,
    #[cfg(test)]
    reloads: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FloorStamp {
    modified: SystemTime,
    len: u64,
}

impl FloorStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        ensure!(metadata.is_file(), "floor journal is not a file");
        Ok(Self {
            modified: metadata.modified()?,
            len: metadata.len(),
        })
    }
}

impl ModernFloors {
    /// Explicit first-time initialization. Refuses to overwrite existing state.
    pub fn initialize(path: impl AsRef<Path>) -> Result<Self> {
        use std::io::Write;
        let path = path.as_ref();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Windows cannot use the Unix directory-fsync path below. Write-through
            // also flushes NTFS metadata changes associated with the journal write.
            options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_WRITE_THROUGH);
        }
        let mut file = options.open(path)?;
        file.write_all(b"SG-FLOORS-1\n")?;
        file.sync_all()?;
        drop(file);
        #[cfg(not(windows))]
        if let Some(parent) = path.parent() {
            // A basename has an empty parent, which denotes the current directory.
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            std::fs::File::open(parent)?.sync_all()?;
        }
        Self::open(path)
    }

    /// Load a pre-existing floor journal. Missing, partial and corrupt records deny grants.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut floors = Self {
            path,
            peers: HashSet::new(),
            stamp: None,
            #[cfg(test)]
            reloads: 0,
        };
        floors.refresh()?;
        Ok(floors)
    }

    fn read_bounded(path: &Path) -> Result<Vec<u8>> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(12 + 64 * 65536 + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<HashSet<PeerId>> {
        ensure!(bytes.starts_with(b"SG-FLOORS-1\n"), "invalid floor journal");
        let body = &bytes[12..];
        ensure!(
            body.len().is_multiple_of(64) && body.len() <= 64 * 65536,
            "invalid floor records"
        );
        let mut peers = HashSet::new();
        for record in body.as_chunks::<64>().0 {
            ensure!(
                blake3::hash(&record[..32]).as_bytes() == &record[32..],
                "corrupt floor record"
            );
            peers.insert(PeerId::new(record[..32].try_into()?));
        }
        Ok(peers)
    }

    fn refresh(&mut self) -> Result<()> {
        let stamp = FloorStamp::read(&self.path)?;
        if self.stamp == Some(stamp) {
            return Ok(());
        }
        let disk = Self::decode(&Self::read_bounded(&self.path)?)?;
        ensure!(
            FloorStamp::read(&self.path)? == stamp,
            "floor journal changed during read"
        );
        ensure!(self.peers.is_subset(&disk), "floor journal rolled back");
        self.peers = disk;
        self.stamp = Some(stamp);
        #[cfg(test)]
        {
            self.reloads += 1;
        }
        Ok(())
    }

    fn require_v2(&mut self, peer: PeerId) -> Result<()> {
        use std::io::Write;
        self.refresh()?;
        if self.peers.contains(&peer) {
            return Ok(());
        }
        ensure!(self.peers.len() < 65536, "floor journal full");
        let mut file = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        let mut record = peer.as_bytes().to_vec();
        record.extend_from_slice(blake3::hash(peer.as_bytes()).as_bytes());
        // Do not bless a concurrent external append without decoding it.
        self.stamp = None;
        file.write_all(&record)?;
        file.sync_all()?;
        self.peers.insert(peer);
        Ok(())
    }
}

#[derive(Default)]
struct State {
    topics: HashMap<TopicId, SignedKvTopic>,
    grants: HashMap<(PeerId, TopicId), BoundGrant>,
    revisions: HashMap<(PeerId, TopicId), u64>,
    floors: Option<ModernFloors>,
    floor_failed: bool,
    reject_v1: bool,
    last_wall: Option<SystemTime>,
    // One global control work budget: bounded cardinality even with hostile peers.
    control_window: Option<Instant>,
    control_work: usize,
    response_window: Option<Instant>,
    response_bytes: usize,
    // Bounded serialized variants; no grant is cached with the bytes.
    variants: HashMap<([u8; 32], u8), Bytes>,
    variant_bytes: usize,
    stats: MigrationStats,
    #[cfg(test)]
    inner_verifications: usize,
}

/// Bounded aggregate counters; no peer labels or payloads are logged.
#[derive(Clone, Copy, Debug, Default)]
pub struct MigrationStats {
    /// Admitted legacy egress by EAGER/IHAVE/IWANT/AntiEntropy.
    pub legacy_egress: [u64; 4],
    /// Admitted legacy ingress by EAGER/IHAVE/IWANT/AntiEntropy.
    pub legacy_ingress: [u64; 4],
    /// Absent, expired, revoked, session-mismatched or floor-conflicting grants.
    pub denied_legacy: u64,
    /// Expired permissions encountered during admission (monotonic or wall time).
    pub expired_grants: u64,
    /// Explicit trusted revocations recorded.
    pub revoked_grants: u64,
    /// V2 payload/hash mismatches observed by PubSub verification.
    pub v2_payload_mismatches: u64,
    /// Failed inner verifier calls.
    pub invalid_inner: u64,
    /// Additional relay/variant signatures.
    pub variant_signatures: u64,
}

/// Disabled-by-default migration policy shared by all send paths.
#[derive(Default)]
pub struct LegacyMigration {
    state: Mutex<State>,
}

impl LegacyMigration {
    /// Install durable floors once; failure leaves normal v2 service available.
    pub fn install_floors(&self, floors: ModernFloors) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        ensure!(
            s.floors.is_none() && !s.floor_failed,
            "floor store already initialized or failed"
        );
        s.floors = Some(floors);
        Ok(())
    }

    /// Register or advance one audited verifier. Replacement invalidates grants and variants.
    pub fn register(&self, topic: SignedKvTopic) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        let id = topic.topic();
        if let Some(old) = s.topics.get(&id) {
            ensure!(topic.revision > old.revision, "verifier revision replay");
        } else {
            ensure!(s.topics.len() < MAX_TOPICS, "topic registry full");
        }
        s.grants.retain(|(_, t), _| *t != id);
        s.variants.clear();
        s.variant_bytes = 0;
        s.topics.insert(id, topic);
        Ok(())
    }

    /// Persist a monotonic modern floor before changing live policy.
    pub fn require_v2(&self, peer: PeerId) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        let result = s
            .floors
            .as_mut()
            .ok_or_else(|| anyhow!("floor storage missing"))?
            .require_v2(peer);
        if result.is_err() {
            s.floor_failed = true;
        }
        s.grants.retain(|(p, _), _| *p != peer);
        result
    }

    /// Issue a grant bound to an authenticated transport session. The transport
    /// must provide this token after identity authentication, never from an address.
    pub fn grant(&self, grant: LegacyGrant, session: AuthenticatedSession) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        ensure!(
            !s.reject_v1,
            "bidirectional legacy service conflicts with RejectV1"
        );
        ensure!(grant.receiver == AUDITED_RECEIVER, "unaudited receiver");
        ensure!(grant.peer == session.peer, "session peer mismatch");
        ensure!(
            !grant.issuer.is_empty()
                && grant.issuer.len() <= 128
                && !grant.reason.is_empty()
                && grant.reason.len() <= 256,
            "invalid grant attribution"
        );
        let policy = s
            .topics
            .get(&grant.topic)
            .ok_or_else(|| anyhow!("unregistered topic"))?;
        ensure!(
            policy.revision == grant.verifier_revision,
            "verifier mismatch"
        );
        Self::check_clock(&mut s)?;
        Self::check_floors(&mut s)?;
        ensure!(
            !s.floors
                .as_ref()
                .is_some_and(|f| f.peers.contains(&grant.peer)),
            "V2Required floor conflict"
        );
        let key = (grant.peer, grant.topic);
        ensure!(
            grant.revision > s.revisions.get(&key).copied().unwrap_or(0),
            "grant revision replay"
        );
        ensure!(
            s.revisions.contains_key(&key) || s.revisions.len() < MAX_GRANTS,
            "grant inventory full"
        );
        let remaining = grant.expires.duration_since(SystemTime::now())?;
        ensure!(
            !remaining.is_zero() && remaining <= Duration::from_secs(86400),
            "grant lifetime must be at most 24 hours"
        );
        let deadline = Instant::now()
            .checked_add(remaining)
            .ok_or_else(|| anyhow!("deadline overflow"))?;
        s.revisions.insert(key, grant.revision);
        s.grants.insert(
            key,
            BoundGrant {
                grant,
                deadline,
                session,
            },
        );
        Ok(())
    }

    /// Revoke pending work; tombstones prevent replay within this process.
    pub fn revoke(&self, peer: PeerId, topic: TopicId, revision: u64) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        let key = (peer, topic);
        ensure!(
            revision > s.revisions.get(&key).copied().unwrap_or(0),
            "revocation revision replay"
        );
        ensure!(
            s.revisions.contains_key(&key) || s.revisions.len() < MAX_GRANTS,
            "grant inventory full"
        );
        s.revisions.insert(key, revision);
        s.grants.remove(&key);
        s.stats.revoked_grants = s.stats.revoked_grants.saturating_add(1);
        Ok(())
    }

    /// Inspect bounded aggregate migration counters.
    pub fn stats(&self) -> Result<MigrationStats> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?
            .stats)
    }

    pub(crate) fn record_payload_mismatch(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.stats.v2_payload_mismatches = s.stats.v2_payload_mismatches.saturating_add(1);
        }
    }

    pub(crate) fn set_signature_policy(&self, policy: SignaturePolicy) {
        if let Ok(mut s) = self.state.lock() {
            s.reject_v1 = policy == SignaturePolicy::RejectV1;
            if s.reject_v1 {
                s.grants.clear();
            }
        }
    }

    pub(crate) fn admit_cache_serve(&self, topic: TopicId, bytes: usize) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        if !s.topics.contains_key(&topic) {
            return Ok(());
        }
        let now = Instant::now();
        if s.response_window
            .is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1))
        {
            s.response_window = Some(now);
            s.response_bytes = 0;
        }
        ensure!(
            bytes <= 16 * 1024 * 1024 - s.response_bytes,
            "migration cache-serve byte rate exceeded"
        );
        s.response_bytes += bytes;
        Ok(())
    }

    pub(crate) fn registered(&self, topic: TopicId) -> bool {
        self.state
            .lock()
            .map(|s| s.topics.contains_key(&topic))
            .unwrap_or(true)
    }

    pub(crate) fn verify_inner(
        &self,
        topic: TopicId,
        payload: &[u8],
    ) -> Result<Option<VerifiedInner>> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        let Some(policy) = s.topics.get(&topic) else {
            return Ok(None);
        };
        let result = policy.verify(payload);
        #[cfg(test)]
        {
            s.inner_verifications += 1;
        }
        if result.is_err() {
            s.stats.invalid_inner = s.stats.invalid_inner.saturating_add(1);
        }
        result.map(Some)
    }

    fn check_clock(s: &mut State) -> Result<()> {
        let now = SystemTime::now();
        if s.last_wall.is_some_and(|last| now < last) {
            s.grants.clear();
            return Err(anyhow!("clock rollback invalidated grants"));
        }
        s.last_wall = Some(now);
        Ok(())
    }

    fn check_floors(s: &mut State) -> Result<()> {
        ensure!(!s.floor_failed, "floor storage failed");
        let result = s
            .floors
            .as_mut()
            .ok_or_else(|| anyhow!("floor storage uninitialized"))?
            .refresh();
        if result.is_err() {
            s.floor_failed = true;
            s.grants.clear();
        }
        result
    }

    fn permitted(s: &mut State, topic: TopicId, session: AuthenticatedSession) -> bool {
        if s.grants
            .get(&(session.peer, topic))
            .is_some_and(|g| Instant::now() >= g.deadline || SystemTime::now() >= g.grant.expires)
        {
            s.grants.remove(&(session.peer, topic));
            s.stats.expired_grants = s.stats.expired_grants.saturating_add(1);
        }
        let valid = !s.reject_v1
            && Self::check_clock(s).is_ok()
            && s.grants.get(&(session.peer, topic)).is_some_and(|g| {
                g.session == session
                    && Instant::now() < g.deadline
                    && SystemTime::now() < g.grant.expires
                    && s.topics
                        .get(&topic)
                        .is_some_and(|p| p.revision == g.grant.verifier_revision)
            })
            && Self::check_floors(s).is_ok()
            && !s
                .floors
                .as_ref()
                .is_some_and(|f| f.peers.contains(&session.peer));
        if !valid {
            s.stats.denied_legacy = s.stats.denied_legacy.saturating_add(1);
        }
        valid
    }

    pub(crate) fn ingress(
        &self,
        from: PeerId,
        session: Option<AuthenticatedSession>,
        message: &GossipMessage,
    ) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?;
        if !s.topics.contains_key(&message.header.topic) {
            return Ok(());
        }
        let kind = kind_index(message.header.kind)?;
        if message.header.version == 1 {
            let session = session.ok_or_else(|| {
                anyhow!("legacy ingress requires authenticated session provenance")
            })?;
            ensure!(
                session.peer == from && Self::permitted(&mut s, message.header.topic, session),
                "legacy ingress denied"
            );
            s.stats.legacy_ingress[kind] = s.stats.legacy_ingress[kind].saturating_add(1);
        }
        if kind != 0 {
            ensure!(
                session.is_some_and(|session| session.peer == from),
                "control requires adjacent session provenance"
            );
            ensure!(
                PeerId::from_pubkey(&message.public_key) == from,
                "control signer is not adjacent peer"
            );
            let work = control_work(message)?;
            let now = Instant::now();
            if s.control_window
                .is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1))
            {
                s.control_window = Some(now);
                s.control_work = 0;
            }
            ensure!(
                s.control_work + work <= MAX_CONTROL_WORK,
                "control work rate exceeded"
            );
            s.control_work += work;
        }
        Ok(())
    }

    pub(crate) async fn send<T: GossipTransport + 'static>(
        self: &Arc<Self>,
        transport: Arc<T>,
        signing_key: Arc<MlDsaKeyPair>,
        peer: PeerId,
        stream: GossipStreamType,
        bytes: Bytes,
    ) -> Result<()> {
        let (header, _) = postcard::take_from_bytes::<saorsa_gossip_types::MessageHeader>(&bytes)?;
        if !self.registered(header.topic) {
            return transport.send_to_peer(peer, stream, bytes).await;
        }
        let (message, trailing): (GossipMessage, _) = postcard::take_from_bytes(&bytes)?;
        ensure!(trailing.is_empty(), "trailing gossip bytes");
        self.verify_inner_if_eager(&message)?;
        if message.header.kind != MessageKind::Eager {
            ensure!(
                signing_key.peer_id() == transport.local_peer_id(),
                "control signing key does not match authenticated transport identity"
            );
        }
        let legacy = {
            let mut s = self
                .state
                .lock()
                .map_err(|_| anyhow!("migration policy poisoned"))?;
            s.grants.contains_key(&(peer, header.topic))
                && transport
                    .authenticated_session(peer)
                    .is_some_and(|session| Self::permitted(&mut s, header.topic, session))
        };
        let wire_digest = *blake3::hash(&bytes).as_bytes();
        if legacy {
            let policy = Arc::clone(self);
            transport
                .send_to_peer_guarded(
                    peer,
                    stream,
                    Arc::new(move |session| {
                        let mut s = policy
                            .state
                            .lock()
                            .map_err(|_| anyhow!("migration policy poisoned"))?;
                        ensure!(
                            session.peer == peer && Self::permitted(&mut s, header.topic, session),
                            "queued legacy send canceled"
                        );
                        // Reverify at admission, including after policy/key roster changes.
                        if message.header.kind == MessageKind::Eager {
                            s.topics
                                .get(&header.topic)
                                .ok_or_else(|| anyhow!("topic removed"))?
                                .verify(
                                    message
                                        .payload
                                        .as_deref()
                                        .ok_or_else(|| anyhow!("missing inner payload"))?,
                                )?;
                        } else {
                            ensure!(
                                PeerId::from_pubkey(&message.public_key) == signing_key.peer_id(),
                                "transit control denied"
                            );
                            control_work(&message)?;
                        }
                        let out = variant(&mut s, &message, 1, &signing_key, wire_digest)?;
                        let kind = kind_index(message.header.kind)?;
                        s.stats.legacy_egress[kind] = s.stats.legacy_egress[kind].saturating_add(1);
                        Ok(out)
                    }),
                )
                .await
        } else {
            // Unknown and modern destinations always receive outer v2. A v1
            // transit/cache envelope is re-signed by this relay, retaining inner author.
            if message.header.version != 2 {
                let bytes = {
                    let mut s = self
                        .state
                        .lock()
                        .map_err(|_| anyhow!("migration policy poisoned"))?;
                    variant(&mut s, &message, 2, &signing_key, wire_digest)?
                };
                transport.send_to_peer(peer, stream, bytes).await
            } else {
                transport.send_to_peer(peer, stream, bytes).await
            }
        }
    }

    fn verify_inner_if_eager(&self, message: &GossipMessage) -> Result<()> {
        if message.header.kind == MessageKind::Eager {
            self.verify_inner(
                message.header.topic,
                message
                    .payload
                    .as_deref()
                    .ok_or_else(|| anyhow!("missing inner payload"))?,
            )?;
        } else {
            control_work(message)?;
        }
        Ok(())
    }
}

fn variant(
    s: &mut State,
    message: &GossipMessage,
    version: u8,
    key: &MlDsaKeyPair,
    wire_digest: [u8; 32],
) -> Result<Bytes> {
    let cache_key = (wire_digest, version);
    if let Some(bytes) = s.variants.get(&cache_key) {
        return Ok(bytes.clone());
    }
    let mut out = message.clone();
    if out.header.version != version {
        out.header.version = version;
        out.header.payload_hash = None;
        if version == 2 {
            out.header.seal_payload_hash(out.payload.as_deref());
        }
        out.signature = key.sign(&postcard::to_stdvec(&out.header)?)?;
        out.public_key = key.public_key().to_vec();
        s.stats.variant_signatures = s.stats.variant_signatures.saturating_add(1);
    }
    let bytes: Bytes = postcard::to_stdvec(&out)?.into();
    if s.variant_bytes + bytes.len() > MAX_VARIANT_BYTES {
        s.variants.clear();
        s.variant_bytes = 0;
    }
    if bytes.len() <= MAX_VARIANT_BYTES {
        s.variant_bytes += bytes.len();
        s.variants.insert(cache_key, bytes.clone());
    }
    Ok(bytes)
}

fn kind_index(kind: MessageKind) -> Result<usize> {
    match kind {
        MessageKind::Eager => Ok(0),
        MessageKind::IHave => Ok(1),
        MessageKind::IWant => Ok(2),
        MessageKind::AntiEntropy => Ok(3),
        _ => Err(anyhow!("message kind outside migration scope")),
    }
}

// Validate postcard sequence counts and exact length *before* Vec deserialization.
fn control_work(message: &GossipMessage) -> Result<usize> {
    let kind = kind_index(message.header.kind)?;
    ensure!(kind != 0, "expected control");
    let payload = message
        .payload
        .as_deref()
        .ok_or_else(|| anyhow!("missing control payload"))?;
    ensure!(
        payload.len() <= MAX_CONTROL_BYTES,
        "control payload too large"
    );
    let list = if kind == 3 {
        ensure!(
            matches!(payload.first(), Some(0 | 1)),
            "unknown anti-entropy variant"
        );
        &payload[1..]
    } else {
        payload
    };
    let (count, ids) = postcard::take_from_bytes::<u32>(list)?;
    let count = usize::try_from(count)?;
    ensure!(
        count <= MAX_CONTROL_IDS && ids.len() == count * 32,
        "malformed or oversized control list"
    );
    Ok(count.max(1))
}

/// Internal adapter makes every existing foreground/background send share policy.
pub(crate) struct PolicyTransport<T> {
    pub(crate) inner: Arc<T>,
    pub(crate) migration: Arc<LegacyMigration>,
    key: Arc<MlDsaKeyPair>,
}
impl<T> PolicyTransport<T> {
    pub(crate) fn new(inner: Arc<T>, key: Arc<MlDsaKeyPair>) -> Self {
        Self {
            inner,
            migration: Arc::new(LegacyMigration::default()),
            key,
        }
    }
}
#[async_trait::async_trait]
impl<T: GossipTransport + 'static> GossipTransport for PolicyTransport<T> {
    async fn dial(&self, peer: PeerId, addr: std::net::SocketAddr) -> Result<()> {
        self.inner.dial(peer, addr).await
    }
    async fn dial_bootstrap(&self, addr: std::net::SocketAddr) -> Result<PeerId> {
        self.inner.dial_bootstrap(addr).await
    }
    async fn listen(&self, addr: std::net::SocketAddr) -> Result<()> {
        self.inner.listen(addr).await
    }
    async fn close(&self) -> Result<()> {
        self.inner.close().await
    }
    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        self.inner.receive_message().await
    }
    async fn connected_peer_ids(&self) -> Vec<PeerId> {
        self.inner.connected_peer_ids().await
    }
    fn local_peer_id(&self) -> PeerId {
        self.inner.local_peer_id()
    }
    fn authenticated_session(&self, peer: PeerId) -> Option<AuthenticatedSession> {
        self.inner.authenticated_session(peer)
    }
    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream: GossipStreamType,
        bytes: Bytes,
    ) -> Result<()> {
        let enabled = !self
            .migration
            .state
            .lock()
            .map_err(|_| anyhow!("migration policy poisoned"))?
            .topics
            .is_empty();
        if !enabled {
            return self.inner.send_to_peer(peer, stream, bytes).await;
        }
        self.migration
            .send(
                Arc::clone(&self.inner),
                Arc::clone(&self.key),
                peer,
                stream,
                bytes,
            )
            .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
