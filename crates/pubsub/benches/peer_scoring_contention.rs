#![allow(clippy::expect_used, clippy::unwrap_used)]
//! X0X-0071 investigation: PeerScoring lock-contention microbench.
//!
//! Hypothesis under test: the four `record_*` / `note_mesh_join` calls
//! wired into `PlumtreePubSub::handle_eager` (lib.rs:4207/4235/4259/4301)
//! share an `inner: Mutex<HashMap<(TopicId, PeerId), PeerScoreState>>`
//! whose lock holds are large enough — under sustained EAGER fan-out —
//! to back up `transport.recv()` and collapse the fleet.
//!
//! Decisive criterion (per investigation brief): if 16-thread throughput
//! is >10× slower than 1-thread on the same working set, contention is
//! a real factor independent of the network.

use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saorsa_gossip_pubsub::peer_scoring::PeerScoring;
use saorsa_gossip_types::{PeerId, TopicId};

/// Working-set sized to match the failed-soak diagnostics:
/// `peer_score_topics` ranged 162-244 entries per node (window 5).
/// (N_TOPICS=4, N_PEERS=50) = 200 keys is the production access shape.
const N_TOPICS: usize = 4;
const N_PEERS: usize = 50;

fn make_topic(i: usize) -> TopicId {
    let mut b = [0u8; 32];
    b[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    TopicId::new(b)
}

fn make_peer(i: usize) -> PeerId {
    let mut b = [0u8; 32];
    b[8..16].copy_from_slice(&(i as u64).to_le_bytes());
    PeerId::new(b)
}

fn working_set() -> Vec<(TopicId, PeerId)> {
    let mut v = Vec::with_capacity(N_TOPICS * N_PEERS);
    for t in 0..N_TOPICS {
        let topic = make_topic(t);
        for p in 0..N_PEERS {
            v.push((topic, make_peer(p)));
        }
    }
    v
}

/// Pre-populate the engine with the full working set so we measure
/// steady-state path (entry exists, decay + bump) not the cold
/// `entry().or_insert_with(...)` path.
fn populated_engine() -> Arc<PeerScoring> {
    let ps = Arc::new(PeerScoring::new());
    for (t, p) in working_set() {
        ps.note_mesh_join(t, p);
        ps.record_first_delivery(t, p);
    }
    ps
}

/// Group 1 — single-op latency on a pre-populated engine.
///
/// Establishes the per-op floor. Each iteration cycles over the working
/// set so we don't hammer one entry. Reports ns/op.
fn bench_single_op(c: &mut Criterion) {
    let mut g = c.benchmark_group("peer_scoring/single_op");
    g.throughput(Throughput::Elements(1));

    let ps = populated_engine();
    let ws = working_set();
    let mut i = 0;

    g.bench_function("record_first_delivery", |b| {
        b.iter(|| {
            let (t, p) = ws[i % ws.len()];
            i = i.wrapping_add(1);
            ps.record_first_delivery(t, p);
            black_box(())
        })
    });

    let ps = populated_engine();
    let ws = working_set();
    let mut i = 0;
    g.bench_function("note_mesh_join", |b| {
        b.iter(|| {
            let (t, p) = ws[i % ws.len()];
            i = i.wrapping_add(1);
            ps.note_mesh_join(t, p);
            black_box(())
        })
    });

    let ps = populated_engine();
    let ws = working_set();
    let mut i = 0;
    g.bench_function("record_mesh_pruned", |b| {
        b.iter(|| {
            let (t, p) = ws[i % ws.len()];
            i = i.wrapping_add(1);
            ps.record_mesh_pruned(t, p);
            black_box(())
        })
    });

    let ps = populated_engine();
    let ws = working_set();
    let mut i = 0;
    g.bench_function("record_invalid_message", |b| {
        b.iter(|| {
            let (t, p) = ws[i % ws.len()];
            i = i.wrapping_add(1);
            ps.record_invalid_message(t, p);
            black_box(())
        })
    });

    let ps = populated_engine();
    let ws = working_set();
    let mut i = 0;
    g.bench_function("score", |b| {
        b.iter(|| {
            let (t, p) = ws[i % ws.len()];
            i = i.wrapping_add(1);
            black_box(ps.score(&t, &p));
        })
    });

    // The macro shape per inbound EAGER (no duplicate, no bad sig, peer
    // already in eager): note_mesh_join + record_first_delivery.
    let ps = populated_engine();
    let ws = working_set();
    let mut i = 0;
    g.bench_function("inbound_eager_pair", |b| {
        b.iter(|| {
            let (t, p) = ws[i % ws.len()];
            i = i.wrapping_add(1);
            ps.record_first_delivery(t, p);
            ps.note_mesh_join(t, p);
            black_box(())
        })
    });

    g.finish();
}

/// Group 2 — snapshot cost as a function of map size.
///
/// `snapshot()` iterates + decays + scores every entry then sorts the
/// resulting Vec. Called from the `/diagnostics/gossip` polling path on
/// the daemon. The lock it takes is the same one the writers take, so a
/// slow snapshot can stall an inbound EAGER for the duration.
fn bench_snapshot(c: &mut Criterion) {
    let mut g = c.benchmark_group("peer_scoring/snapshot");

    for &size in &[50usize, 200, 1000, 5000, 20_000] {
        let ps = PeerScoring::new();
        for k in 0..size {
            let t = make_topic(k % N_TOPICS);
            let p = make_peer(k);
            ps.note_mesh_join(t, p);
            ps.record_first_delivery(t, p);
        }
        g.throughput(Throughput::Elements(size as u64));
        g.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| black_box(ps.snapshot()))
        });
    }
    g.finish();
}

/// Group 3 — writer-vs-writer contention.
///
/// Spawn N threads each calling `record_first_delivery` + `note_mesh_join`
/// in a tight loop. Measure aggregate throughput. Compare to 1-thread
/// baseline. Contention factor = (latency at N threads) / (latency at 1
/// thread).
///
/// Decisive: contention factor > 10× at N=16 → lock is the bottleneck.
fn bench_writer_contention(c: &mut Criterion) {
    let mut g = c.benchmark_group("peer_scoring/writer_contention");
    g.measurement_time(Duration::from_secs(8));

    for &n_threads in &[1usize, 2, 4, 8, 16] {
        let ws = working_set();
        let ps = populated_engine();
        g.throughput(Throughput::Elements(n_threads as u64 * 1000));
        g.bench_with_input(
            BenchmarkId::from_parameter(n_threads),
            &n_threads,
            |b, &nt| {
                b.iter(|| {
                    let mut handles = Vec::with_capacity(nt);
                    for tid in 0..nt {
                        let ps = Arc::clone(&ps);
                        let ws = ws.clone();
                        handles.push(std::thread::spawn(move || {
                            let off = tid * 17;
                            for k in 0..1000 {
                                let (t, p) = ws[(off + k) % ws.len()];
                                ps.record_first_delivery(t, p);
                                ps.note_mesh_join(t, p);
                            }
                        }));
                    }
                    for h in handles {
                        h.join().expect("worker");
                    }
                })
            },
        );
    }
    g.finish();
}

/// Group 4 — writers running concurrently with a snapshot reader.
///
/// Models the production scenario: inbound EAGER traffic recording on
/// the same Mutex that `/diagnostics/gossip` polling is currently
/// iterating. If snapshot-while-writing pushes writer latency way
/// beyond solo-writer latency, the diagnostics path is a real source of
/// lock-hold-induced stalls.
fn bench_snapshot_vs_writers(c: &mut Criterion) {
    let mut g = c.benchmark_group("peer_scoring/snapshot_vs_writers");
    g.measurement_time(Duration::from_secs(8));

    for &n_writers in &[1usize, 4, 8] {
        // Larger working set to give snapshot something to sort.
        let mut ws = Vec::with_capacity(N_TOPICS * 200);
        for t in 0..N_TOPICS {
            let topic = make_topic(t);
            for p in 0..200 {
                ws.push((topic, make_peer(p)));
            }
        }
        let ps = Arc::new(PeerScoring::new());
        for (t, p) in &ws {
            ps.note_mesh_join(*t, *p);
            ps.record_first_delivery(*t, *p);
        }

        g.throughput(Throughput::Elements(n_writers as u64 * 1000));
        g.bench_with_input(
            BenchmarkId::from_parameter(n_writers),
            &n_writers,
            |b, &nw| {
                b.iter(|| {
                    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let snap_handle = {
                        let ps = Arc::clone(&ps);
                        let stop = Arc::clone(&stop);
                        std::thread::spawn(move || {
                            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                                let _ = black_box(ps.snapshot());
                            }
                        })
                    };

                    let mut writers = Vec::with_capacity(nw);
                    for tid in 0..nw {
                        let ps = Arc::clone(&ps);
                        let ws = ws.clone();
                        writers.push(std::thread::spawn(move || {
                            let off = tid * 17;
                            for k in 0..1000 {
                                let (t, p) = ws[(off + k) % ws.len()];
                                ps.record_first_delivery(t, p);
                                ps.note_mesh_join(t, p);
                            }
                        }));
                    }
                    for h in writers {
                        h.join().expect("writer");
                    }
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    snap_handle.join().expect("snap");
                })
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_single_op,
    bench_snapshot,
    bench_writer_contention,
    bench_snapshot_vs_writers,
);
criterion_main!(benches);
