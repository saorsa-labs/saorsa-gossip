//! Multi-replica OR-Set convergence (strong eventual consistency).
//!
//! Three replicas issue divergent add/remove operations, then exchange
//! state via `merge_state` in every direction. After convergence the three
//! replicas must hold identical element sets (the strong-eventual-
//! consistency property of a delta-CRDT).
//!
//! This is a pure-data integration test — no transport, no async.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use saorsa_gossip_crdt_sync::{OrSet, UniqueTag};
use saorsa_gossip_types::PeerId;

fn peer(byte: u8) -> PeerId {
    PeerId::new([byte; 32])
}

fn tag(peer_byte: u8, seq: u64) -> UniqueTag {
    (peer(peer_byte), seq)
}

/// Sort an OR-Set's elements into a deterministic `BTreeSet` for comparison.
fn snapshot(set: &OrSet<&'static str>) -> BTreeSet<&'static str> {
    set.elements().into_iter().copied().collect()
}

#[test]
fn three_replicas_converge_after_pairwise_merge() {
    // Three replicas, each with its own (peer_id, seq) tag space.
    let mut a: OrSet<&str> = OrSet::new();
    let mut b: OrSet<&str> = OrSet::new();
    let mut c: OrSet<&str> = OrSet::new();

    // Divergent local operations.
    a.add("x", tag(0xA, 1)).expect("a add x");
    a.add("y", tag(0xA, 2)).expect("a add y");

    b.add("y", tag(0xB, 1)).expect("b add y"); // concurrent add of y
    b.add("z", tag(0xB, 2)).expect("b add z");

    c.add("y", tag(0xC, 1)).expect("c add y"); // another concurrent add
    c.add("w", tag(0xC, 2)).expect("c add w");

    // Verify divergence before sync.
    assert_ne!(snapshot(&a), snapshot(&b));
    assert_ne!(snapshot(&b), snapshot(&c));

    // All-to-all merge round (one delta-exchange epoch).
    let a_pre = a.clone();
    let b_pre = b.clone();
    let c_pre = c.clone();
    a.merge_state(&b_pre).expect("a <- b");
    a.merge_state(&c_pre).expect("a <- c");
    b.merge_state(&a_pre).expect("b <- a");
    b.merge_state(&c_pre).expect("b <- c");
    c.merge_state(&a_pre).expect("c <- a");
    c.merge_state(&b_pre).expect("c <- b");

    // Convergence: all three hold {w, x, y, z}.
    let expected: BTreeSet<&str> = ["w", "x", "y", "z"].into_iter().collect();
    assert_eq!(snapshot(&a), expected, "replica A did not converge");
    assert_eq!(snapshot(&b), expected, "replica B did not converge");
    assert_eq!(snapshot(&c), expected, "replica C did not converge");
}

#[test]
fn concurrent_remove_and_add_resolves_via_observed_remove_semantics() {
    // OR-Set semantics: a remove only tombstones tags currently observed.
    // A concurrent add with a *different* tag survives the remove (the
    // hallmark "observed-remove" property that distinguishes OR-Set from
    // 2P-Set).
    let mut a: OrSet<&str> = OrSet::new();
    let mut b: OrSet<&str> = OrSet::new();

    a.add("k", tag(0xA, 1)).expect("a add k");

    // Replica B receives a's state, so it now observes tag (A,1).
    b.merge_state(&a).expect("b <- a (initial)");
    assert!(b.contains(&"k"));

    // Concurrently: A removes k (tombstones (A,1)); B adds k with its own
    // fresh tag (B,1).
    a.remove(&"k").expect("a remove k");
    b.add("k", tag(0xB, 1)).expect("b add k with fresh tag");

    // Exchange.
    let a_pre = a.clone();
    let b_pre = b.clone();
    a.merge_state(&b_pre).expect("a <- b");
    b.merge_state(&a_pre).expect("b <- a");

    // The observed (A,1) is tombstoned on both, but (B,1) was added
    // *after* a's remove, so it survives. Both replicas see "k" present.
    assert!(a.contains(&"k"), "k should survive on A via (B,1)");
    assert!(b.contains(&"k"), "k should survive on B via (B,1)");
    assert_eq!(snapshot(&a), snapshot(&b));
}
