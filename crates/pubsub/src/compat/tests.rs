use super::*;
use crate::{AntiEntropyPayload, MessageHeader, PlumtreePubSub, PubSub};
use saorsa_gossip_legacy_compat_fixture::{
    legacy_identity, legacy_pubsub, legacy_transport, legacy_types,
};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

struct FloorFixture {
    path: PathBuf,
    _dir: tempfile::TempDir,
}
impl FloorFixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("gossip-compat-")
            .tempdir()
            .unwrap();
        Self {
            path: dir.path().join("floors"),
            _dir: dir,
        }
    }
    fn install(&self, policy: &LegacyMigration) {
        policy
            .install_floors(ModernFloors::initialize(&self.path).unwrap())
            .unwrap();
    }
}

struct RecordingTransport {
    local: PeerId,
    sessions: Mutex<HashMap<PeerId, AuthenticatedSession>>,
    sent: Mutex<Vec<(PeerId, Bytes)>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    block: std::sync::atomic::AtomicBool,
}
impl RecordingTransport {
    fn new(local: PeerId) -> Arc<Self> {
        Arc::new(Self {
            local,
            sessions: Mutex::default(),
            sent: Mutex::default(),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            block: false.into(),
        })
    }
    fn connect(&self, peer: PeerId, generation: u64) -> AuthenticatedSession {
        let session = AuthenticatedSession { peer, generation };
        self.sessions.lock().unwrap().insert(peer, session);
        session
    }
    fn drain(&self) -> Vec<(PeerId, GossipMessage)> {
        std::mem::take(&mut *self.sent.lock().unwrap())
            .into_iter()
            .map(|(p, b)| (p, postcard::from_bytes(&b).unwrap()))
            .collect()
    }
}
#[async_trait::async_trait]
impl GossipTransport for RecordingTransport {
    async fn dial(&self, _: PeerId, _: SocketAddr) -> Result<()> {
        Ok(())
    }
    async fn dial_bootstrap(&self, _: SocketAddr) -> Result<PeerId> {
        Ok(self.local)
    }
    async fn listen(&self, _: SocketAddr) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    async fn send_to_peer(&self, peer: PeerId, _: GossipStreamType, bytes: Bytes) -> Result<()> {
        self.sent.lock().unwrap().push((peer, bytes));
        self.entered.notify_one();
        Ok(())
    }
    async fn receive_message(&self) -> Result<(PeerId, GossipStreamType, Bytes)> {
        anyhow::bail!("no receiver")
    }
    fn local_peer_id(&self) -> PeerId {
        self.local
    }
    fn authenticated_session(&self, peer: PeerId) -> Option<AuthenticatedSession> {
        self.sessions.lock().unwrap().get(&peer).copied()
    }
    async fn send_to_peer_guarded(
        &self,
        peer: PeerId,
        _: GossipStreamType,
        admit: saorsa_gossip_transport::SessionAdmission,
    ) -> Result<()> {
        if self.block.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        // Exercise the policy's own generation check, independently of the
        // UDP adapter's additional pinned-connection rejection.
        let session = self
            .authenticated_session(peer)
            .ok_or_else(|| anyhow!("disconnected"))?;
        let bytes = admit(session)?;
        self.sent.lock().unwrap().push((peer, bytes));
        self.entered.notify_one();
        Ok(())
    }
}

fn grant(peer: PeerId, topic: TopicId, revision: u64) -> LegacyGrant {
    LegacyGrant {
        peer,
        topic,
        receiver: AUDITED_RECEIVER.into(),
        verifier_revision: 1,
        revision,
        issuer: "fixture operator".into(),
        reason: "Signed KV legacy recovery".into(),
        expires: SystemTime::now() + Duration::from_secs(60),
    }
}
fn register(policy: &LegacyMigration, key: &MlDsaKeyPair) -> TopicId {
    let p = SignedKvTopic::new("fixture/kv", SignedKvFamily::Delta, 1, [key.peer_id()]).unwrap();
    let topic = p.topic();
    policy.register(p).unwrap();
    topic
}
fn inner(key: &MlDsaKeyPair, name: &str, payload: &[u8]) -> Bytes {
    let mut signable = b"x0x-msg-v3".to_vec();
    signable.extend_from_slice(key.peer_id().as_bytes());
    signable.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
    signable.extend_from_slice(name.as_bytes());
    signable.extend_from_slice(payload);
    let sig = key.sign(&signable).unwrap();
    let mut out = vec![3];
    out.extend_from_slice(key.peer_id().as_bytes());
    for part in [key.public_key(), &sig, name.as_bytes()] {
        out.extend_from_slice(&(part.len() as u16).to_be_bytes());
        out.extend_from_slice(part);
    }
    out.extend_from_slice(payload);
    out.into()
}
fn message(
    key: &MlDsaKeyPair,
    topic: TopicId,
    kind: MessageKind,
    payload: Bytes,
    version: u8,
    id: u8,
) -> GossipMessage {
    let mut header = MessageHeader {
        version: 1,
        payload_hash: None,
        topic,
        msg_id: [id; 32],
        kind,
        hop: 2,
        ttl: 7,
    };
    if version == 2 {
        header.seal_payload_hash(Some(&payload));
    }
    let signature = key.sign(&postcard::to_stdvec(&header).unwrap()).unwrap();
    GossipMessage {
        header,
        payload: Some(payload),
        signature,
        public_key: key.public_key().to_vec(),
    }
}
fn wire(m: &GossipMessage) -> Bytes {
    postcard::to_stdvec(m).unwrap().into()
}
fn node(key: &MlDsaKeyPair) -> (PlumtreePubSub<RecordingTransport>, Arc<RecordingTransport>) {
    let transport = RecordingTransport::new(key.peer_id());
    (
        PlumtreePubSub::new_with_task_control(
            key.peer_id(),
            Arc::clone(&transport),
            key.clone(),
            false,
        ),
        transport,
    )
}

// Run the actual published 0.5.66 decoder, independently of current header serde.
fn old_verify(m: &GossipMessage) {
    let bytes = wire(m);
    let (old, trailing): (legacy_pubsub::GossipMessage, _) =
        postcard::take_from_bytes(&bytes).unwrap();
    assert!(trailing.is_empty());
    assert_eq!(old.header.version, 1);
    assert!(MlDsaKeyPair::verify(
        &old.public_key,
        &postcard::to_stdvec(&old.header).unwrap(),
        &old.signature
    )
    .unwrap());
    assert_eq!(postcard::to_stdvec(&old).unwrap(), bytes);
    match old.header.kind {
        legacy_types::MessageKind::IHave | legacy_types::MessageKind::IWant => {
            let _: Vec<[u8; 32]> = postcard::from_bytes(old.payload.as_deref().unwrap()).unwrap();
        }
        legacy_types::MessageKind::AntiEntropy => {
            let _: AntiEntropyPayload =
                postcard::from_bytes(old.payload.as_deref().unwrap()).unwrap();
        }
        _ => {}
    }
}

#[tokio::test]
async fn mixed_publish_controls_cache_and_forward_preserve_authors_and_floors() {
    let author = MlDsaKeyPair::generate().unwrap();
    let relay = MlDsaKeyPair::generate().unwrap();
    let old = MlDsaKeyPair::generate().unwrap();
    let modern = MlDsaKeyPair::generate().unwrap();
    let (pubsub, transport) = node(&relay);
    let policy = pubsub.legacy_migration();
    let floor = FloorFixture::new();
    floor.install(policy);
    let topic = register(policy, &author);
    let old_session = transport.connect(old.peer_id(), 1);
    let modern_session = transport.connect(modern.peer_id(), 2);
    policy
        .grant(grant(old.peer_id(), topic, 1), old_session)
        .unwrap();
    policy.require_v2(modern.peer_id()).unwrap();
    pubsub
        .topics
        .write_topic(&topic)
        .await
        .entry(topic)
        .or_insert_with(|| pubsub.new_topic_state())
        .eager_peers
        .extend([old.peer_id(), modern.peer_id()]);
    let payload = inner(&author, "fixture/kv", b"historical Signed KV fixture");
    pubsub.publish_local(topic, payload.clone()).await.unwrap();
    let sent = transport.drain();
    assert_eq!(sent.len(), 2);
    let mut cached_id = [0; 32];
    for (peer, m) in sent {
        cached_id = m.header.msg_id;
        assert_eq!(m.payload, Some(payload.clone()));
        assert!(pubsub.verify_message_signature(&m));
        assert_eq!(m.header.version, if peer == old.peer_id() { 1 } else { 2 });
        if peer == old.peer_id() {
            old_verify(&m);
        }
    }
    // Authentic-shaped IWANT handler -> cached EAGER, preserving inner author.
    let iwant = message(
        &old,
        topic,
        MessageKind::IWant,
        postcard::to_stdvec(&vec![cached_id]).unwrap().into(),
        1,
        40,
    );
    pubsub
        .handle_authenticated_message(old_session, wire(&iwant))
        .await
        .unwrap();
    let sent = transport.drain();
    assert_eq!(sent.len(), 1);
    old_verify(&sent[0].1);
    assert_eq!(PeerId::from_pubkey(&sent[0].1.public_key), relay.peer_id());
    assert_eq!(
        policy
            .verify_inner(topic, sent[0].1.payload.as_deref().unwrap())
            .unwrap()
            .unwrap()
            .author,
        author.peer_id()
    );
    // Scheduled IHAVE fanout uses the same destination selector.
    {
        let mut topics = pubsub.topics.write_topic(&topic).await;
        let state = topics.get_mut(&topic).unwrap();
        state.lazy_peers.extend([old.peer_id(), modern.peer_id()]);
        state.pending_ihave.push(cached_id);
    }
    PlumtreePubSub::flush_ihave_batches(
        &pubsub.topics,
        &pubsub.transport,
        &pubsub.signing_key,
        &pubsub.stage_stats,
        &pubsub.outbound_budgets,
        &pubsub.send_path_context(),
    )
    .await;
    let sent = transport.drain();
    assert!(!sent.is_empty());
    for (p, m) in sent {
        assert_eq!(m.header.version, if p == old.peer_id() { 1 } else { 2 });
        if p == old.peer_id() {
            old_verify(&m);
        }
    }
    // IHAVE -> IWANT control, and generated AntiEntropy digest.
    let ihave = message(
        &old,
        topic,
        MessageKind::IHave,
        postcard::to_stdvec(&vec![[99u8; 32]]).unwrap().into(),
        1,
        41,
    );
    pubsub
        .handle_authenticated_message(old_session, wire(&ihave))
        .await
        .unwrap();
    let sent = transport.drain();
    assert_eq!(sent[0].1.header.kind, MessageKind::IWant);
    old_verify(&sent[0].1);
    pubsub
        .send_anti_entropy_digest(topic, old.peer_id())
        .await
        .unwrap();
    let sent = transport.drain();
    assert_eq!(sent[0].1.header.kind, MessageKind::AntiEntropy);
    old_verify(&sent[0].1);
    // Empty remote digest solicits cached EAGER recovery.
    let ae = message(
        &old,
        topic,
        MessageKind::AntiEntropy,
        postcard::to_stdvec(&AntiEntropyPayload::Digest { msg_ids: vec![] })
            .unwrap()
            .into(),
        1,
        42,
    );
    pubsub
        .handle_authenticated_message(old_session, wire(&ae))
        .await
        .unwrap();
    let sent = transport.drain();
    assert_eq!(sent.len(), 1);
    old_verify(&sent[0].1);
    // Old-origin transit is sealed by the relay for a modern adjacency.
    let incoming = message(
        &old,
        topic,
        MessageKind::Eager,
        inner(&author, "fixture/kv", b"second historical value"),
        1,
        43,
    );
    pubsub
        .handle_authenticated_message(old_session, wire(&incoming))
        .await
        .unwrap();
    // Detached forwarding completion uses an explicit accounting drain (no sleeps).
    tokio::time::timeout(Duration::from_millis(2500), async {
        loop {
            let notified = transport.entered.notified();
            if !transport.sent.lock().unwrap().is_empty() {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
    let sent = transport.drain();
    assert!(sent
        .iter()
        .any(|(p, m)| *p == modern.peer_id() && m.header.version == 2));
    let (_, m) = sent.iter().find(|(p, _)| *p == modern.peer_id()).unwrap();
    assert_eq!(m.header.msg_id, incoming.header.msg_id);
    assert_eq!(m.header.hop, 2);
    assert_eq!(m.header.ttl, 7);
    assert_eq!(PeerId::from_pubkey(&m.public_key), relay.peer_id());
    assert!(pubsub.verify_message_signature(m));
    // A modern adjacency cannot send legacy outer frames, even with valid inner data.
    assert!(pubsub
        .handle_authenticated_message(modern_session, wire(&incoming))
        .await
        .is_err());
}

#[tokio::test]
async fn invalid_inner_first_cannot_poison_cache_and_policy_changes_revalidate() {
    let author = MlDsaKeyPair::generate().unwrap();
    let relay = MlDsaKeyPair::generate().unwrap();
    let (pubsub, transport) = node(&relay);
    let policy = pubsub.legacy_migration();
    let floor = FloorFixture::new();
    floor.install(policy);
    let topic = register(policy, &author);
    let session = transport.connect(author.peer_id(), 7);
    policy
        .grant(grant(author.peer_id(), topic, 1), session)
        .unwrap();
    let valid = message(
        &author,
        topic,
        MessageKind::Eager,
        inner(&author, "fixture/kv", b"good"),
        1,
        8,
    );
    let mut invalid = valid.clone();
    let mut payload = invalid.payload.unwrap().to_vec();
    *payload.last_mut().unwrap() ^= 1;
    invalid.payload = Some(payload.into());
    assert!(pubsub
        .handle_authenticated_message(session, wire(&invalid))
        .await
        .is_err());
    assert!(pubsub.topics.read_topic(&topic).await.get(&topic).is_none());
    pubsub
        .handle_authenticated_message(session, wire(&valid))
        .await
        .unwrap();
    {
        let mut topics = pubsub.topics.write_topic(&topic).await;
        let cached = topics
            .get_mut(&topic)
            .unwrap()
            .get_message(&[8; 32])
            .unwrap();
        assert_eq!(cached.inner_proof.unwrap().author, author.peer_id());
    }
    let unknown = MlDsaKeyPair::generate().unwrap();
    for bytes in [
        inner(&unknown, "fixture/kv", b"good"),
        inner(&author, "wrong-topic", b"good"),
        Bytes::from_static(b"raw"),
    ] {
        assert!(pubsub.publish_local(topic, bytes).await.is_err());
    }
    let next =
        SignedKvTopic::new("fixture/kv", SignedKvFamily::Delta, 2, [unknown.peer_id()]).unwrap();
    policy.register(next).unwrap();
    // Previously cached proof is not authority after a verifier roster change.
    assert!(pubsub
        .handle_iwant_admitted(author.peer_id(), topic, vec![[8; 32]])
        .await
        .is_err());
    assert!(transport.drain().is_empty());
}

#[tokio::test]
async fn queued_revocation_expiry_reconnect_and_reject_v1_fail_closed() {
    for case in 0..4 {
        let key = MlDsaKeyPair::generate().unwrap();
        let peer = PeerId::new([44; 32]);
        let transport = RecordingTransport::new(key.peer_id());
        let policy = Arc::new(LegacyMigration::default());
        let floor = FloorFixture::new();
        floor.install(&policy);
        let topic = register(&policy, &key);
        let session = transport.connect(peer, 1);
        policy.grant(grant(peer, topic, 1), session).unwrap();
        let m = message(
            &key,
            topic,
            MessageKind::Eager,
            inner(&key, "fixture/kv", b"queued"),
            2,
            1,
        );
        transport.block.store(true, Ordering::SeqCst);
        let p = Arc::clone(&policy);
        let t = Arc::clone(&transport);
        let task = tokio::spawn(async move {
            p.send(t, Arc::new(key), peer, GossipStreamType::PubSub, wire(&m))
                .await
        });
        transport.entered.notified().await;
        match case {
            0 => policy.revoke(peer, topic, 2).unwrap(),
            1 => {
                policy
                    .state
                    .lock()
                    .unwrap()
                    .grants
                    .get_mut(&(peer, topic))
                    .unwrap()
                    .deadline = Instant::now()
            }
            2 => {
                transport.connect(peer, 2);
            }
            _ => policy.set_signature_policy(SignaturePolicy::RejectV1),
        }
        transport.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert!(transport.drain().is_empty());
        if case == 0 {
            assert!(policy.grant(grant(peer, topic, 1), session).is_err());
        }
        if case == 3 {
            assert!(policy.grant(grant(peer, topic, 2), session).is_err());
        }
    }
}

#[test]
fn durable_floor_basename_initialization_and_restart() {
    // Reserve a unique basename in the current directory without changing the
    // process-wide cwd. TempPath retains cleanup ownership after unlinking.
    let journal = tempfile::Builder::new()
        .prefix("gossip-compat-floors-")
        .tempfile_in(".")
        .unwrap()
        .into_temp_path();
    std::fs::remove_file(&journal).unwrap();
    let basename = Path::new(journal.file_name().unwrap());
    assert_eq!(basename.parent(), Some(Path::new("")));

    let mut floors = ModernFloors::initialize(basename).unwrap();
    let peer = PeerId::new([24; 32]);
    floors.require_v2(peer).unwrap();
    drop(floors);
    assert!(ModernFloors::open(basename).unwrap().peers.contains(&peer));

    let persisted = std::fs::read(basename).unwrap();
    assert!(ModernFloors::initialize(basename).is_err());
    assert_eq!(std::fs::read(basename).unwrap(), persisted);
}

#[test]
fn durable_floor_restart_corruption_rollback_and_grant_defaults() {
    let key = MlDsaKeyPair::generate().unwrap();
    let peer = PeerId::new([22; 32]);
    let session = AuthenticatedSession {
        peer,
        generation: 1,
    };
    let policy = LegacyMigration::default();
    let topic = register(&policy, &key);
    assert!(policy.grant(grant(peer, topic, 1), session).is_err());
    let fixture = FloorFixture::new();
    fixture.install(&policy);
    policy.require_v2(peer).unwrap();
    let persisted = std::fs::read(&fixture.path).unwrap();
    assert!(ModernFloors::initialize(&fixture.path).is_err());
    assert_eq!(std::fs::read(&fixture.path).unwrap(), persisted);
    assert!(policy.grant(grant(peer, topic, 1), session).is_err());
    let restarted = LegacyMigration::default();
    register(&restarted, &key);
    restarted
        .install_floors(ModernFloors::open(&fixture.path).unwrap())
        .unwrap();
    assert!(restarted.grant(grant(peer, topic, 2), session).is_err());
    let another = PeerId::new([23; 32]);
    let another_session = AuthenticatedSession {
        peer: another,
        generation: 2,
    };
    restarted
        .grant(grant(another, topic, 1), another_session)
        .unwrap();
    restarted.state.lock().unwrap().last_wall = Some(SystemTime::now() + Duration::from_secs(60));
    assert!(!LegacyMigration::permitted(
        &mut restarted.state.lock().unwrap(),
        topic,
        another_session
    ));
    std::fs::write(&fixture.path, b"SG-FLOORS-1\n").unwrap();
    assert!(restarted.require_v2(another).is_err());
    std::fs::write(&fixture.path, b"corrupt").unwrap();
    assert!(ModernFloors::open(&fixture.path).is_err());
}

#[tokio::test]
async fn control_negative_shapes_signers_topics_and_payload_tampering() {
    let key = MlDsaKeyPair::generate().unwrap();
    let other = MlDsaKeyPair::generate().unwrap();
    let (pubsub, t) = node(&key);
    let policy = pubsub.legacy_migration();
    let f = FloorFixture::new();
    f.install(policy);
    let topic = register(policy, &key);
    let session = t.connect(key.peer_id(), 1);
    policy
        .grant(grant(key.peer_id(), topic, 1), session)
        .unwrap();
    for kind in [
        MessageKind::IHave,
        MessageKind::IWant,
        MessageKind::AntiEntropy,
    ] {
        for payload in [
            Bytes::new(),
            Bytes::from_static(&[0x80, 0x80, 0x80, 0x80, 0x10]),
            Bytes::from(vec![0; MAX_CONTROL_BYTES + 1]),
        ] {
            let m = message(&key, topic, kind, payload, 1, 1);
            assert!(pubsub
                .handle_authenticated_message(session, wire(&m))
                .await
                .is_err());
        }
        let payload: Bytes = if kind == MessageKind::AntiEntropy {
            postcard::to_stdvec(&AntiEntropyPayload::Digest { msg_ids: vec![] })
                .unwrap()
                .into()
        } else {
            postcard::to_stdvec(&vec![[1u8; 32]]).unwrap().into()
        };
        let m = message(&other, topic, kind, payload, 1, 1);
        assert!(pubsub
            .handle_authenticated_message(session, wire(&m))
            .await
            .is_err());
    }
    let mut m = message(
        &key,
        topic,
        MessageKind::Eager,
        inner(&key, "fixture/kv", b"valid"),
        2,
        1,
    );
    m.payload = Some(inner(&key, "fixture/kv", b"different valid inner"));
    assert!(pubsub
        .handle_authenticated_message(session, wire(&m))
        .await
        .is_err());
    m.header.version = 3;
    assert!(!pubsub.verify_message_signature(&m));
    let forbidden = message(&key, topic, MessageKind::Ping, Bytes::new(), 1, 1);
    assert!(pubsub
        .handle_authenticated_message(session, wire(&forbidden))
        .await
        .is_err());
    assert!(pubsub
        .handle_iwant(key.peer_id(), topic, vec![[1; 32]])
        .await
        .is_err());
    assert!(pubsub
        .handle_message(
            key.peer_id(),
            wire(&message(
                &key,
                topic,
                MessageKind::Eager,
                inner(&key, "fixture/kv", b"valid"),
                1,
                1
            ))
        )
        .await
        .is_err());
}

#[async_trait::async_trait]
impl legacy_transport::GossipTransport for RecordingTransport {
    async fn dial(&self, _: legacy_types::PeerId, _: SocketAddr) -> Result<()> {
        Ok(())
    }
    async fn dial_bootstrap(&self, _: SocketAddr) -> Result<legacy_types::PeerId> {
        Ok(legacy_types::PeerId::new(*self.local.as_bytes()))
    }
    async fn listen(&self, _: SocketAddr) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    async fn send_to_peer(
        &self,
        peer: legacy_types::PeerId,
        _: legacy_transport::GossipStreamType,
        bytes: Bytes,
    ) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push((PeerId::new(*peer.as_bytes()), bytes));
        self.entered.notify_one();
        Ok(())
    }
    async fn receive_message(
        &self,
    ) -> Result<(
        legacy_types::PeerId,
        legacy_transport::GossipStreamType,
        Bytes,
    )> {
        anyhow::bail!("fixture has explicit delivery")
    }
    fn local_peer_id(&self) -> legacy_types::PeerId {
        legacy_types::PeerId::new(*self.local.as_bytes())
    }
}

#[tokio::test]
async fn authentic_0566_handlers_interoperate_in_both_directions_and_controls() {
    use legacy_pubsub::PubSub as OldPubSub;
    let current_key = MlDsaKeyPair::generate().unwrap();
    let old_key = MlDsaKeyPair::generate().unwrap();
    let (current, current_transport) = node(&current_key);
    let old_transport = RecordingTransport::new(old_key.peer_id());
    let old = legacy_pubsub::PlumtreePubSub::new(
        legacy_types::PeerId::new(*old_key.peer_id().as_bytes()),
        Arc::clone(&old_transport),
        legacy_identity::MlDsaKeyPair::from_bytes(&old_key.to_bytes().unwrap()).unwrap(),
    );
    let p = current.legacy_migration();
    let f = FloorFixture::new();
    f.install(p);
    let registration = SignedKvTopic::new(
        "fixture/kv",
        SignedKvFamily::Delta,
        1,
        [current_key.peer_id(), old_key.peer_id()],
    )
    .unwrap();
    let topic = registration.topic();
    let old_topic = legacy_types::TopicId::new(topic.to_bytes());
    p.register(registration).unwrap();
    let old_session = current_transport.connect(old_key.peer_id(), 1);
    p.grant(grant(old_key.peer_id(), topic, 1), old_session)
        .unwrap();
    let current_as_old = legacy_types::PeerId::new(*current_key.peer_id().as_bytes());
    current
        .initialize_topic_peers(topic, vec![old_key.peer_id()])
        .await;
    old.initialize_topic_peers(old_topic, vec![current_as_old])
        .await;
    let mut old_rx = old.subscribe_ready(old_topic).await;
    let mut current_rx = current.subscribe_ready(topic).await;
    let payload = inner(
        &current_key,
        "fixture/kv",
        b"current to authentic legacy handler",
    );
    current.publish_local(topic, payload.clone()).await.unwrap();
    let generated = std::mem::take(&mut *current_transport.sent.lock().unwrap());
    assert_eq!(generated.len(), 1);
    old.handle_message(current_as_old, generated[0].1.clone())
        .await
        .unwrap();
    assert_eq!(old_rx.try_recv().unwrap().1, payload);
    assert_eq!(current_rx.try_recv().unwrap().1, payload);
    let returned = inner(&old_key, "fixture/kv", b"legacy to current handler");
    old.publish_local(old_topic, returned.clone())
        .await
        .unwrap();
    let generated = std::mem::take(&mut *old_transport.sent.lock().unwrap());
    assert!(!generated.is_empty());
    for (_, bytes) in generated {
        let m: legacy_pubsub::GossipMessage = postcard::from_bytes(&bytes).unwrap();
        if m.header.kind == legacy_types::MessageKind::Eager {
            current
                .handle_authenticated_message(old_session, bytes)
                .await
                .unwrap();
        }
    }
    assert_eq!(current_rx.try_recv().unwrap().1, returned);
    // Every real legacy control handler must accept the current-generated v1
    // variant and return semantically valid EAGER / IWANT, not just decode it.
    let cached_id = {
        let topics = current.topics.read_topic(&topic).await;
        topics.get(&topic).unwrap().cached_message_ids()[0]
    };
    for (kind, payload, expected) in [
        (
            MessageKind::IHave,
            postcard::to_stdvec(&vec![[91u8; 32]]).unwrap(),
            MessageKind::IWant,
        ),
        (
            MessageKind::IWant,
            postcard::to_stdvec(&vec![cached_id]).unwrap(),
            MessageKind::Eager,
        ),
        (
            MessageKind::AntiEntropy,
            postcard::to_stdvec(&AntiEntropyPayload::Digest { msg_ids: vec![] }).unwrap(),
            MessageKind::Eager,
        ),
    ] {
        old_transport.sent.lock().unwrap().clear();
        let m = message(&current_key, topic, kind, payload.into(), 2, 90);
        current
            .transport
            .send_to_peer(old_key.peer_id(), GossipStreamType::PubSub, wire(&m))
            .await
            .unwrap();
        let bytes = current_transport.sent.lock().unwrap().pop().unwrap().1;
        old.handle_message(current_as_old, bytes).await.unwrap();
        let replies = old_transport.drain();
        assert!(
            replies.iter().any(|(_, m)| m.header.kind == expected),
            "legacy handler {kind:?} did not produce {expected:?}"
        );
        for (_, m) in replies {
            assert!(current.verify_message_signature(&m));
        }
    }
}

#[tokio::test]
async fn legacy_valid_inner_alias_is_availability_risk_not_author_authentication() {
    let key = MlDsaKeyPair::generate().unwrap();
    let (pubsub, t) = node(&key);
    let p = pubsub.legacy_migration();
    let f = FloorFixture::new();
    f.install(p);
    let topic = register(p, &key);
    let session = t.connect(key.peer_id(), 1);
    p.grant(grant(key.peer_id(), topic, 1), session).unwrap();
    let a = message(
        &key,
        topic,
        MessageKind::Eager,
        inner(&key, "fixture/kv", b"value A"),
        1,
        10,
    );
    let mut b = a.clone();
    b.payload = Some(inner(&key, "fixture/kv", b"value B"));
    let mut rx = pubsub.subscribe_ready(topic).await;
    pubsub
        .handle_authenticated_message(session, wire(&b))
        .await
        .unwrap();
    assert_eq!(rx.try_recv().unwrap().1, b.payload.unwrap());
    pubsub
        .handle_authenticated_message(session, wire(&a))
        .await
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "valid same-topic substitution retains legacy ID suppression risk"
    );
    assert_eq!(
        pubsub
            .topics
            .read_topic(&topic)
            .await
            .get(&topic)
            .unwrap()
            .message_cache
            .len(),
        1
    );
}

#[tokio::test]
async fn modern_origin_through_authentic_old_relay_to_modern_receiver() {
    use legacy_pubsub::PubSub as OldPubSub;
    let origin_key = MlDsaKeyPair::generate().unwrap();
    let relay_key = MlDsaKeyPair::generate().unwrap();
    let recipient_key = MlDsaKeyPair::generate().unwrap();
    let (origin, origin_transport) = node(&origin_key);
    let (recipient, recipient_transport) = node(&recipient_key);
    let f1 = FloorFixture::new();
    f1.install(origin.legacy_migration());
    let f2 = FloorFixture::new();
    f2.install(recipient.legacy_migration());
    let topic = register(origin.legacy_migration(), &origin_key);
    register(recipient.legacy_migration(), &origin_key);
    let session1 = origin_transport.connect(relay_key.peer_id(), 1);
    let session2 = recipient_transport.connect(relay_key.peer_id(), 2);
    origin
        .legacy_migration()
        .grant(grant(relay_key.peer_id(), topic, 1), session1)
        .unwrap();
    recipient
        .legacy_migration()
        .grant(grant(relay_key.peer_id(), topic, 1), session2)
        .unwrap();
    let relay_transport = RecordingTransport::new(relay_key.peer_id());
    let relay = legacy_pubsub::PlumtreePubSub::new(
        legacy_types::PeerId::new(*relay_key.peer_id().as_bytes()),
        Arc::clone(&relay_transport),
        legacy_identity::MlDsaKeyPair::from_bytes(&relay_key.to_bytes().unwrap()).unwrap(),
    );
    let old_topic = legacy_types::TopicId::new(topic.to_bytes());
    relay
        .initialize_topic_peers(
            old_topic,
            vec![legacy_types::PeerId::new(
                *recipient_key.peer_id().as_bytes(),
            )],
        )
        .await;
    origin
        .initialize_topic_peers(topic, vec![relay_key.peer_id()])
        .await;
    let mut rx = recipient.subscribe_ready(topic).await;
    let payload = inner(&origin_key, "fixture/kv", b"through an unchanged old relay");
    origin.publish_local(topic, payload.clone()).await.unwrap();
    let sent = origin_transport.sent.lock().unwrap().pop().unwrap().1;
    relay
        .handle_message(
            legacy_types::PeerId::new(*origin_key.peer_id().as_bytes()),
            sent,
        )
        .await
        .unwrap();
    let forwarded = tokio::time::timeout(Duration::from_millis(2500), async {
        loop {
            let notified = relay_transport.entered.notified();
            if let Some((_, bytes)) = relay_transport.sent.lock().unwrap().pop() {
                break bytes;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
    let outer: GossipMessage = postcard::from_bytes(&forwarded).unwrap();
    assert_eq!(PeerId::from_pubkey(&outer.public_key), origin_key.peer_id());
    assert_ne!(PeerId::from_pubkey(&outer.public_key), session2.peer);
    recipient
        .handle_authenticated_message(session2, forwarded)
        .await
        .unwrap();
    assert_eq!(rx.try_recv().unwrap().1, payload);
}

#[tokio::test]
async fn state_sync_scope_raw_defaults_bounds_and_modern_relay_conversion() {
    let key = MlDsaKeyPair::generate().unwrap();
    let relay = MlDsaKeyPair::generate().unwrap();
    let old_peer = PeerId::new([5; 32]);
    let modern_peer = key.peer_id();
    let (pubsub, t) = node(&relay);
    let p = pubsub.legacy_migration();
    let floor = FloorFixture::new();
    floor.install(p);
    let base = register(p, &key);
    let side =
        SignedKvTopic::new("fixture/kv", SignedKvFamily::StateSync, 1, [key.peer_id()]).unwrap();
    let topic = side.topic();
    assert_ne!(topic, base);
    assert_eq!(side.family(), SignedKvFamily::StateSync);
    p.register(side).unwrap();
    let session = t.connect(old_peer, 1);
    let modern = t.connect(modern_peer, 1);
    // A base-topic grant does not spill into its state-sync sibling.
    p.grant(grant(old_peer, base, 1), session).unwrap();
    pubsub.initialize_topic_peers(topic, vec![old_peer]).await;
    let payload = inner(&key, "fixture/kv/state-sync", b"state request");
    pubsub.publish_local(topic, payload.clone()).await.unwrap();
    assert_eq!(t.drain()[0].1.header.version, 2);
    p.grant(grant(old_peer, topic, 1), session).unwrap();
    let payload = inner(&key, "fixture/kv/state-sync", b"a subsequent state request");
    let m = message(&key, topic, MessageKind::Eager, payload.clone(), 2, 55);
    pubsub
        .handle_authenticated_message(modern, wire(&m))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(2500), async {
        loop {
            let notified = t.entered.notified();
            if !t.sent.lock().unwrap().is_empty() {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
    let sent = t.drain();
    old_verify(&sent[0].1);
    assert_eq!(sent[0].1.payload, Some(payload));
    assert_eq!(PeerId::from_pubkey(&sent[0].1.public_key), relay.peer_id());
    let raw_topic = TopicId::from_entity("excluded/raw");
    assert!(p.grant(grant(old_peer, raw_topic, 1), session).is_err());
    pubsub
        .initialize_topic_peers(raw_topic, vec![old_peer])
        .await;
    pubsub
        .publish_local(
            raw_topic,
            Bytes::from_static(b"bare generic payload stays v2"),
        )
        .await
        .unwrap();
    assert_eq!(t.drain()[0].1.header.version, 2);
    // Missing keys, author mismatch, unsupported inner version, and byte bounds.
    let valid = inner(&key, "fixture/kv", b"valid");
    let mut wrong_author = valid.to_vec();
    wrong_author[1] ^= 1;
    assert!(p.verify_inner(base, &wrong_author).is_err());
    let mut wrong_version = valid.to_vec();
    wrong_version[0] = 2;
    assert!(p.verify_inner(base, &wrong_version).is_err());
    assert!(p.verify_inner(base, &vec![3; MAX_INNER_BYTES + 1]).is_err());
    assert!(p.admit_cache_serve(base, 16 * 1024 * 1024).is_ok());
    assert!(p.admit_cache_serve(base, 1).is_err());
    {
        let mut s = p.state.lock().unwrap();
        s.control_work = MAX_CONTROL_WORK;
        s.control_window = Some(Instant::now());
    }
    let c = message(
        &key,
        base,
        MessageKind::IHave,
        postcard::to_stdvec(&vec![[1u8; 32]]).unwrap().into(),
        2,
        11,
    );
    assert!(pubsub
        .handle_authenticated_message(modern, wire(&c))
        .await
        .is_err());
}

#[tokio::test]
async fn migration_cost_and_variant_cache_bounds() {
    let key = MlDsaKeyPair::generate().unwrap();
    let peers = [PeerId::new([81; 32]), PeerId::new([82; 32])];
    for mode in ["disabled", "registered-modern", "mixed"] {
        let (pubsub, t) = node(&key);
        let p = pubsub.legacy_migration();
        let floor = FloorFixture::new();
        let topic = TopicId::from_entity("fixture/kv");
        if mode != "disabled" {
            floor.install(p);
            register(p, &key);
        }
        let session = t.connect(peers[0], 1);
        t.connect(peers[1], 2);
        if mode == "mixed" {
            p.grant(grant(peers[0], topic, 1), session).unwrap();
        }
        pubsub.initialize_topic_peers(topic, peers.to_vec()).await;
        let payloads: Vec<_> = (0u8..50)
            .map(|n| inner(&key, "fixture/kv", &[n; 128]))
            .collect();
        let started = Instant::now();
        for payload in payloads {
            pubsub.publish_local(topic, payload).await.unwrap();
        }
        let elapsed = started.elapsed();
        let sent = t.drain();
        assert_eq!(sent.len(), 100);
        let s = p.state.lock().unwrap();
        assert!(s.variant_bytes <= MAX_VARIANT_BYTES);
        if mode == "mixed" {
            assert_eq!(s.stats.variant_signatures, 50);
            assert_eq!(s.stats.legacy_egress[0], 50);
        }
        println!("migration_cost mode={mode} publishes=50 recipients=2 elapsed_us={} variant_signatures={} variant_cache_bytes={}",elapsed.as_micros(),s.stats.variant_signatures,s.variant_bytes);
    }
}

#[tokio::test]
async fn outer_authentication_and_dedup_precede_inner_verification() {
    let author = MlDsaKeyPair::generate().unwrap();
    let (pubsub, transport) = node(&author);
    let policy = pubsub.legacy_migration();
    let floor = FloorFixture::new();
    floor.install(policy);
    let topic = register(policy, &author);
    let session = transport.connect(author.peer_id(), 1);
    policy
        .grant(grant(author.peer_id(), topic, 1), session)
        .unwrap();
    let valid = message(
        &author,
        topic,
        MessageKind::Eager,
        inner(&author, "fixture/kv", b"authenticated"),
        1,
        91,
    );
    let mut bad_outer = valid.clone();
    bad_outer.signature[0] ^= 1;
    assert!(pubsub
        .handle_authenticated_message(session, wire(&bad_outer))
        .await
        .is_err());
    assert_eq!(policy.state.lock().unwrap().inner_verifications, 0);
    pubsub
        .handle_authenticated_message(session, wire(&valid))
        .await
        .unwrap();
    assert_eq!(policy.state.lock().unwrap().inner_verifications, 1);
    let mut duplicate = valid.clone();
    let mut bytes = duplicate.payload.unwrap().to_vec();
    *bytes.last_mut().unwrap() ^= 1;
    duplicate.payload = Some(bytes.into()); // v1 outer remains valid, inner is invalid
    pubsub
        .handle_authenticated_message(session, wire(&duplicate))
        .await
        .unwrap();
    assert_eq!(policy.state.lock().unwrap().inner_verifications, 1);
    assert_eq!(policy.stats().unwrap().invalid_inner, 0);
    // The same bad inner under a fresh, correctly signed outer ID is checked.
    let fresh = message(
        &author,
        topic,
        MessageKind::Eager,
        duplicate.payload.unwrap(),
        1,
        92,
    );
    assert!(pubsub
        .handle_authenticated_message(session, wire(&fresh))
        .await
        .is_err());
    assert_eq!(policy.state.lock().unwrap().inner_verifications, 2);
    assert_eq!(policy.stats().unwrap().invalid_inner, 1);
    let mut topics = pubsub.topics.write_topic(&topic).await;
    assert!(!topics.get_mut(&topic).unwrap().has_message(&[92; 32]));
}

#[test]
fn floor_cache_reloads_only_changed_journals_and_fails_closed() {
    let fixture = FloorFixture::new();
    let mut floors = ModernFloors::initialize(&fixture.path).unwrap();
    let peer = PeerId::new([61; 32]);
    assert_eq!(floors.reloads, 1);
    for _ in 0..100 {
        floors.refresh().unwrap();
    }
    assert_eq!(floors.reloads, 1);
    let mut external = ModernFloors::open(&fixture.path).unwrap();
    external.require_v2(peer).unwrap();
    floors.refresh().unwrap();
    assert!(floors.peers.contains(&peer));
    assert_eq!(floors.reloads, 2);
    for _ in 0..100 {
        floors.refresh().unwrap();
    }
    assert_eq!(floors.reloads, 2);
    // Same-length valid replacement must also trigger refresh and detect rollback.
    let replacement = PeerId::new([62; 32]);
    let mut bytes = b"SG-FLOORS-1\n".to_vec();
    bytes.extend_from_slice(replacement.as_bytes());
    bytes.extend_from_slice(blake3::hash(replacement.as_bytes()).as_bytes());
    std::fs::write(&fixture.path, bytes).unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&fixture.path)
        .unwrap();
    file.set_modified(floors.stamp.unwrap().modified + Duration::from_secs(2))
        .unwrap();
    assert!(floors
        .refresh()
        .unwrap_err()
        .to_string()
        .contains("rolled back"));
    assert!(floors.peers.contains(&peer));
    drop(file);
    std::fs::remove_file(&fixture.path).unwrap();
    assert!(floors.refresh().is_err());
}

#[test]
fn expired_and_absent_grants_skip_floor_io_but_preserve_tombstones() {
    let author = MlDsaKeyPair::generate().unwrap();
    let policy = LegacyMigration::default();
    let topic = register(&policy, &author);
    let fixture = FloorFixture::new();
    fixture.install(&policy);
    let session = AuthenticatedSession {
        peer: author.peer_id(),
        generation: 1,
    };
    policy
        .grant(grant(session.peer, topic, 1), session)
        .unwrap();
    std::fs::remove_file(&fixture.path).unwrap();
    let mut s = policy.state.lock().unwrap();
    s.grants.get_mut(&(session.peer, topic)).unwrap().deadline = Instant::now();
    assert!(!LegacyMigration::permitted(&mut s, topic, session));
    assert!(!s.grants.contains_key(&(session.peer, topic)));
    assert_eq!(s.revisions.get(&(session.peer, topic)), Some(&1));
    assert_eq!(s.stats.expired_grants, 1);
    assert!(!s.floor_failed, "expired grant must not reach floor IO");
    assert!(!LegacyMigration::permitted(&mut s, topic, session));
    assert_eq!(s.stats.expired_grants, 1);
    assert!(!s.floor_failed, "absent grant must not reach floor IO");
}

#[test]
fn inner_v3_rejects_topic_boundary_rewrites() {
    let key = MlDsaKeyPair::generate().unwrap();
    // Exercise both the mandated family pair and an arbitrary UTF-8 prefix.
    for (short, long, suffix) in [
        ("T", "T/state-sync", "/state-sync"),
        ("é", "é/other", "/other"),
    ] {
        let short_policy =
            SignedKvTopic::new(short, SignedKvFamily::Delta, 1, [key.peer_id()]).unwrap();
        let long_policy = if long == "T/state-sync" {
            SignedKvTopic::new(short, SignedKvFamily::StateSync, 1, [key.peer_id()]).unwrap()
        } else {
            SignedKvTopic::new(long, SignedKvFamily::Delta, 1, [key.peer_id()]).unwrap()
        };
        let short_payload = [suffix.as_bytes(), b"request"].concat();
        let long_wire = inner(&key, long, b"request");
        let short_wire = inner(&key, short, &short_payload);
        assert_eq!(
            long_policy.verify(&long_wire).unwrap().author,
            key.peer_id()
        );
        // Payload prefixes remain legal when signed under their actual topic.
        assert_eq!(
            short_policy.verify(&short_wire).unwrap().author,
            key.peer_id()
        );
        let offset = 33 + 2 + key.public_key().len() + 2 + 3309;
        for (wire, target, policy) in [
            (long_wire, short, &short_policy),
            (short_wire, long, &long_policy),
        ] {
            let mut reframed = wire.to_vec();
            reframed[offset..offset + 2]
                .copy_from_slice(&u16::try_from(target.len()).unwrap().to_be_bytes());
            assert_eq!(
                policy.verify(&reframed).unwrap_err().to_string(),
                "invalid inner signature"
            );
        }
    }
}

#[test]
fn inner_v3_rejects_legacy_signatures_and_version_relabeling() {
    let key = MlDsaKeyPair::generate().unwrap();
    let policy = SignedKvTopic::new("T", SignedKvFamily::Delta, 1, [key.peer_id()]).unwrap();
    // Independently construct a stock x0x V2 signature; changing its envelope
    // version must not make it eligible under the V3 verifier.
    let mut preimage = b"x0x-msg-v2".to_vec();
    preimage.extend_from_slice(key.peer_id().as_bytes());
    preimage.extend_from_slice(b"Tpayload");
    let sig = key.sign(&preimage).unwrap();
    let mut legacy = vec![2];
    legacy.extend_from_slice(key.peer_id().as_bytes());
    for part in [key.public_key(), &sig, b"T"] {
        legacy.extend_from_slice(&u16::try_from(part.len()).unwrap().to_be_bytes());
        legacy.extend_from_slice(part);
    }
    legacy.extend_from_slice(b"payload");
    assert!(policy
        .verify(&legacy)
        .unwrap_err()
        .to_string()
        .contains("only topic-bound V3"));
    legacy[0] = 3;
    assert_eq!(
        policy.verify(&legacy).unwrap_err().to_string(),
        "invalid inner signature"
    );
    let valid = inner(&key, "T", b"payload");
    assert!(policy.verify(&valid).is_ok());
    for version in [0, 1, 2, 4, 255] {
        let mut relabeled = valid.to_vec();
        relabeled[0] = version;
        assert!(policy.verify(&relabeled).is_err());
    }
}

#[test]
fn stock_v2_receiver_profile_cannot_receive_a_grant() {
    let key = MlDsaKeyPair::generate().unwrap();
    let policy = LegacyMigration::default();
    let fixture = FloorFixture::new();
    fixture.install(&policy);
    let topic = register(&policy, &key);
    let peer = MlDsaKeyPair::generate().unwrap().peer_id();
    let session = AuthenticatedSession {
        peer,
        generation: 1,
    };
    let mut old_grant = grant(peer, topic, 1);
    old_grant.receiver = "x0x/0.30.1;saorsa-gossip-pubsub/0.5.66".into();
    assert_eq!(
        policy.grant(old_grant, session).unwrap_err().to_string(),
        "unaudited receiver"
    );
}
