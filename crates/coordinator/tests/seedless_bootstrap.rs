//! Coordinator advert end-to-end lifecycle.
//!
//! Exercises the seedless-bootstrap data plane: peer A constructs a signed
//! `CoordinatorAdvert`, encodes it to wire bytes, and peer B (with no
//! prior knowledge of A) decodes the bytes and routes them through
//! `CoordinatorHandler::handle_advert` into a real ant-quic
//! `BootstrapCache`-backed `GossipCacheAdapter`.
//!
//! This is the "joiner with empty cache + one advert reaches steady state"
//! case from the seedless-bootstrap design (ADR-004), reduced to its data
//! plane and asserted at API boundaries (no transport — that path is
//! covered separately by the pubsub multi-node test).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use ant_quic::bootstrap_cache::{BootstrapCache, BootstrapCacheConfig};
use saorsa_gossip_coordinator::{
    AddrHint, CoordinatorAdvert, CoordinatorHandler, CoordinatorPublisher, CoordinatorRoles,
    GossipCacheAdapter, NatClass,
};
use saorsa_gossip_types::PeerId;
use saorsa_pqc::{MlDsa65, MlDsaOperations, MlDsaPublicKey, MlDsaSecretKey};
use tempfile::TempDir;

async fn fresh_cache_adapter(temp: &TempDir) -> GossipCacheAdapter {
    let config = BootstrapCacheConfig::builder()
        .cache_dir(temp.path().to_path_buf())
        .build();
    let cache = Arc::new(BootstrapCache::open(config).await.expect("open cache"));
    GossipCacheAdapter::new(cache)
}

fn keypair() -> (MlDsaPublicKey, MlDsaSecretKey) {
    MlDsa65::new()
        .generate_keypair()
        .expect("generate ml-dsa keypair")
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port))
}

#[tokio::test]
async fn empty_cache_joiner_ingests_signed_advert_and_reaches_steady_state() {
    // ── Peer A: build + sign + encode an advert ────────────────────────
    let (publisher_pk, publisher_sk) = keypair();
    let publisher_peer = PeerId::from_pubkey(publisher_pk.as_bytes());

    let publisher = CoordinatorPublisher::new(
        publisher_peer,
        CoordinatorRoles::default(),
        vec![addr(40_001), addr(40_002)],
        NatClass::Eim,
    );
    publisher.set_signing_key(publisher_sk).await;

    let advert_bytes = publisher
        .publish_advert()
        .await
        .expect("publish_advert encodes signed bytes");
    assert!(!advert_bytes.is_empty(), "encoded advert must be non-empty");

    // ── Peer B: empty cache, decode + ingest via handler ───────────────
    let temp = tempfile::tempdir().expect("tempdir");
    let joiner_adapter = fresh_cache_adapter(&temp).await;
    let joiner_peer = PeerId::new([0xBB; 32]);
    let handler = CoordinatorHandler::with_cache(joiner_peer, joiner_adapter.clone());

    let decoded = CoordinatorAdvert::from_bytes(&advert_bytes).expect("decode advert");
    assert_eq!(decoded.peer, publisher_peer, "peer field round-trips");
    assert!(
        decoded.is_valid(),
        "advert validity window should be open just after creation"
    );

    let accepted = handler
        .handle_advert(decoded.clone(), &publisher_pk)
        .await
        .expect("handle_advert");
    assert!(accepted, "valid signed advert must be accepted");

    // ── Steady state: cache holds the advert and returns it ────────────
    let cached = joiner_adapter
        .get_advert(&publisher_peer)
        .expect("publisher advert must be cached after ingest");
    assert_eq!(cached.peer, publisher_peer);
    assert_eq!(cached.addr_hints.len(), 2);
}

#[tokio::test]
async fn handle_advert_rejects_pubkey_peer_id_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let adapter = fresh_cache_adapter(&temp).await;

    let (legit_pk, legit_sk) = keypair();
    let legit_peer = PeerId::from_pubkey(legit_pk.as_bytes());

    // Build an advert whose `peer` field claims an unrelated identity but
    // is signed by `legit_sk`. The handler must reject this even though
    // the signature would verify under `legit_pk`, because the peer-id
    // binding (PeerId::from_pubkey == advert.peer) fails.
    let mut advert = CoordinatorAdvert::new(
        PeerId::new([0xFF; 32]),
        CoordinatorRoles::default(),
        vec![AddrHint::new(addr(40_010))],
        NatClass::Unknown,
        60_000,
    );
    advert.sign(&legit_sk).expect("sign with legit key");

    let handler = CoordinatorHandler::with_cache(PeerId::new([0xCC; 32]), adapter.clone());
    let accepted = handler
        .handle_advert(advert, &legit_pk)
        .await
        .expect("handle_advert");

    assert!(
        !accepted,
        "advert with pk/peer-id mismatch must be rejected"
    );
    assert!(
        adapter.get_advert(&legit_peer).is_none(),
        "rejected advert must not be cached"
    );
}
