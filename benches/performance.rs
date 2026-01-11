//! Performance benchmarks for Saorsa Gossip overlay network
//!
//! These benchmarks measure the performance characteristics of
//! key components under various load conditions.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use saorsa_gossip_crdt_sync::{OrSet, DeltaCrdt};
use saorsa_gossip_identity::Identity;
use saorsa_gossip_pubsub::{PlumtreePubSub, PubSub};
use saorsa_gossip_types::{MessageHeader, MessageKind, PeerId, TopicId};
use std::time::Duration;

/// Benchmark ML-DSA key generation
fn bench_identity_generation(c: &mut Criterion) {
    c.bench_function("identity_generation", |b| {
        b.iter(|| {
            let identity = Identity::generate().unwrap();
            black_box(identity)
        })
    });
}

/// Benchmark ML-DSA signing
fn bench_message_signing(c: &mut Criterion) {
    let identity = Identity::generate().unwrap();
    let message = b"Performance test message for signing";
    
    c.bench_function("message_signing", |b| {
        b.iter(|| {
            let signature = identity.sign(black_box(message)).unwrap();
            black_box(signature)
        })
    });
}

/// Benchmark ML-DSA verification
fn bench_message_verification(c: &mut Criterion) {
    let identity = Identity::generate().unwrap();
    let message = b"Performance test message for verification";
    let signature = identity.sign(message).unwrap();
    
    c.bench_function("message_verification", |b| {
        b.iter(|| {
            let is_valid = identity.verify(black_box(message), &signature).unwrap();
            black_box(is_valid)
        })
    });
}

/// Benchmark message ID calculation
fn bench_message_id_calculation(c: &mut Criterion) {
    let topic = TopicId::new([1u8; 32]);
    let epoch = 12345u64;
    let signer = PeerId::new([2u8; 32]);
    let payload_hash = [3u8; 32];
    
    c.bench_function("message_id_calculation", |b| {
        b.iter(|| {
            let msg_id = MessageHeader::calculate_msg_id(
                black_box(&topic),
                black_box(epoch),
                black_box(&signer),
                black_box(&payload_hash),
            );
            black_box(msg_id)
        })
    });
}

/// Benchmark OR-Set add operations
fn bench_orset_add(c: &mut Criterion) {
    let mut orset = OrSet::<String>::new();
    let peer_id = PeerId::new([1u8; 32]);
    
    c.bench_function("orset_add", |b| {
        b.iter(|| {
            let item = format!("item-{}", rand::random::<u32>());
            orset.add(black_box(item), (peer_id, 1)).unwrap();
        })
    });
}

/// Benchmark OR-Set contains operations
fn bench_orset_contains(c: &mut Criterion) {
    let mut orset = OrSet::<String>::new();
    let peer_id = PeerId::new([1u8; 32]);
    
    // Pre-populate with 1000 items
    for i in 0..1000 {
        orset.add(format!("item-{}", i), (peer_id, i)).unwrap();
    }
    
    c.bench_function("orset_contains", |b| {
        b.iter(|| {
            let item = format!("item-{}", rand::random::<u32>() % 1000);
            let contains = orset.contains(black_box(&item));
            black_box(contains)
        })
    });
}

/// Benchmark OR-Set delta generation
fn bench_orset_delta(c: &mut Criterion) {
    let mut orset = OrSet::<String>::new();
    let peer_id = PeerId::new([1u8; 32]);
    
    // Pre-populate with items
    for i in 0..100 {
        orset.add(format!("item-{}", i), (peer_id, i)).unwrap();
    }
    
    c.bench_function("orset_delta", |b| {
        b.iter(|| {
            let delta = orset.delta(black_box(50)).unwrap();
            black_box(delta)
        })
    });
}

/// Benchmark OR-Set merge operations
fn bench_orset_merge(c: &mut Criterion) {
    let mut orset1 = OrSet::<String>::new();
    let mut orset2 = OrSet::<String>::new();
    let peer1_id = PeerId::new([1u8; 32]);
    let peer2_id = PeerId::new([2u8; 32]);
    
    // Create deltas to merge
    for i in 0..50 {
        orset1.add(format!("item1-{}", i), (peer1_id, i)).unwrap();
        orset2.add(format!("item2-{}", i), (peer2_id, i)).unwrap();
    }
    
    let delta2 = orset2.delta(0).unwrap();
    
    c.bench_function("orset_merge", |b| {
        b.iter(|| {
            let mut orset_clone = orset1.clone();
            orset_clone.merge(black_box(&delta2)).unwrap();
            black_box(orset_clone)
        })
    });
}

/// Benchmark pub/sub subscription
fn bench_pubsub_subscribe(c: &mut Criterion) {
    let pubsub = PlumtreePubSub::new();
    let topic = TopicId::new([42u8; 32]);
    
    c.bench_function("pubsub_subscribe", |b| {
        b.iter(|| {
            let rx = pubsub.subscribe(black_box(topic));
            black_box(rx)
        })
    });
}

/// Benchmark pub/sub publish
fn bench_pubsub_publish(c: &mut Criterion) {
    let pubsub = PlumtreePubSub::new();
    let topic = TopicId::new([42u8; 32]);
    let message = bytes::Bytes::from("Benchmark message");
    
    c.bench_function("pubsub_publish", |b| {
        b.iter(|| {
            pubsub.publish(black_box(topic), black_box(message.clone())).unwrap();
        })
    });
}

/// Benchmark topic ID generation from entity
fn bench_topic_id_from_entity(c: &mut Criterion) {
    let entity_ids = vec![
        "channel-general",
        "project-alpha",
        "org-example",
        "user-12345",
        "group-beta",
    ];
    
    c.bench_function("topic_id_from_entity", |b| {
        b.iter(|| {
            let entity_id = entity_ids[rand::random::<usize>() % entity_ids.len()];
            let topic_id = TopicId::from_entity(black_box(entity_id));
            black_box(topic_id)
        })
    });
}

/// Benchmark peer ID generation from public key
fn bench_peer_id_from_pubkey(c: &mut Criterion) {
    let identity = Identity::generate().unwrap();
    let pubkey = identity.public_key();
    
    c.bench_function("peer_id_from_pubkey", |b| {
        b.iter(|| {
            let peer_id = PeerId::from_pubkey(black_box(&pubkey));
            black_box(peer_id)
        })
    });
}

/// Benchmark message header creation
fn bench_message_header_creation(c: &mut Criterion) {
    let topic = TopicId::new([42u8; 32]);
    
    c.bench_function("message_header_creation", |b| {
        b.iter(|| {
            let header = MessageHeader::new(
                black_box(topic),
                black_box(MessageKind::Eager),
                black_box(10),
            );
            black_box(header)
        })
    });
}

/// Benchmark BLAKE3 hashing
fn bench_blake3_hash(c: &mut Criterion) {
    let data = b"Performance test data for BLAKE3 hashing";
    
    c.bench_function("blake3_hash", |b| {
        b.iter(|| {
            let hash = blake3::hash(black_box(data));
            black_box(hash)
        })
    });
}

/// Benchmark serialization/deserialization
fn bench_serialization(c: &mut Criterion) {
    let header = MessageHeader::new(
        TopicId::new([42u8; 32]),
        MessageKind::Eager,
        10,
    );
    
    c.bench_function("serialization", |b| {
        b.iter(|| {
            let serialized = bincode::serialize(black_box(&header)).unwrap();
            black_box(serialized)
        })
    });
    
    let serialized = bincode::serialize(&header).unwrap();
    
    c.bench_function("deserialization", |b| {
        b.iter(|| {
            let deserialized: MessageHeader = bincode::deserialize(black_box(&serialized)).unwrap();
            black_box(deserialized)
        })
    });
}

/// Benchmark concurrent pub/sub operations
fn bench_concurrent_pubsub(c: &mut Criterion) {
    let pubsub = PlumtreePubSub::new();
    let topic = TopicId::new([42u8; 32]);
    let message = bytes::Bytes::from("Concurrent benchmark message");
    
    c.bench_function("concurrent_pubsub", |b| {
        b.to_async(&tokio::runtime::Runtime::new().unwrap())
            .iter(|| async {
                // Subscribe
                let mut rx = pubsub.subscribe(topic);
                
                // Publish
                pubsub.publish(topic, message.clone()).unwrap();
                
                // Receive
                let _received = rx.recv().await;
            })
    });
}

criterion_group!(
    benches,
    bench_identity_generation,
    bench_message_signing,
    bench_message_verification,
    bench_message_id_calculation,
    bench_orset_add,
    bench_orset_contains,
    bench_orset_delta,
    bench_orset_merge,
    bench_pubsub_subscribe,
    bench_pubsub_publish,
    bench_topic_id_from_entity,
    bench_peer_id_from_pubkey,
    bench_message_header_creation,
    bench_blake3_hash,
    bench_serialization,
    bench_concurrent_pubsub
);

criterion_main!(benches);