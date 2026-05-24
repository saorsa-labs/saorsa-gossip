# Design: pubsub fan-out backpressure under degraded peers

**Status:** DRAFT — design only. Implementation must pass a degraded-network
soak before merge/release (see §Validation). Branch:
`feat/pubsub-detach-fanout-backpressure`.

**Origin:** post-publish soak of x0x 0.19.47
(`proofs/launch-readiness-soak-20260524T130012Z`) hit
`recv_pump.dropped_full`=27,560 under degraded Hetzner-DE↔DO-APAC links. Full
audit: `~/Desktop/Devel/projects/.clawpatch-orphans/AUDIT-REPORT.md` §7 +
`APAC-UDP-DEGRADATION-INFRA.md`. Confirmed NOT a regression (4 independent
reviews); pre-existing resilience gap.

## Root cause

`PlumtreePubSub::parallel_send_to_peers` (crates/pubsub/src/lib.rs ~3854):
- Already de-serialized: each peer send is `tokio::spawn`ed, permit-bounded,
  with an adaptive per-peer timeout (`send_to_peer_with_timeout`, ~`PER_PEER_REPUBLISH_TIMEOUT` = 2500ms ceiling).
- **But line ~3944 `send_tasks.collect_results().await` makes the dispatcher
  worker await the whole fan-out** (to feed `claims.record_results(...)` →
  RTT tracker / cooling / admission accounting).

Under degraded peers the slowest send in each fan-out hits the ~2.5s timeout, so
the EAGER **forward** path (`handle_eager`, the per-message dispatcher hot path
at ~4347) pins a worker ~2.5s/message. With 32 pubsub workers, aggregate drain
collapses to ≈13 msg/s. Loss-amplified gossip volume exceeds that → the 10k-deep
recv channel (`recv_depth.pubsub`) fills (observed 9999/10000) →
`recv_pump.dropped_full`. The 460,880 ms `dispatcher.pubsub.max_elapsed_ms` is
queueing delay, not handler CPU.

Healthy mesh: sends complete fast, no pinning, drain ≫ volume → drop_full=0
(pre-publish soak 12/12 GO). The gap only manifests under sustained slow-peer
conditions.

## Why this is delicate

- `handle_message` / `parallel_send_to_peers` take `&self`, not `Arc<Self>` —
  detaching work requires an `Arc<Self>` handle or restructuring.
- `SendAttemptClaims` has a `Drop` impl; `BulkAdmissionSetGuard` (RAII) releases
  per-peer Bulk admission depth exactly once. Moving these into a detached task
  must preserve "release exactly once, at send completion" or admission/permit
  depth leaks.
- `record_results` feeds the **cooling/admission feedback loop**. If detached,
  the feedback for message N may land after message N+1's admission decision —
  a possible behaviour change in how fast peers get cooled. Needs thought.
- This path is already optimized (X0X-0006/0007/0074). The remaining await is
  deliberate (telemetry correctness), not an oversight.

## Candidate fixes (ranked)

1. **Detach fan-out accounting from the dispatcher worker (preferred).**
   Spawn the `collect_results().await` + `record_results` + bulk-guard drop as a
   detached task; the dispatcher worker returns immediately after enqueuing the
   permit-bounded sends. Concurrency stays bounded by the existing permit pool.
   Requires `Arc<Self>` (or moving the needed Arcs: transport, admission,
   stage_stats, rtt_tracker, claims). Risk: cooling-feedback timing shift;
   permit/admission leak if guard move is wrong. **Most throughput benefit.**

2. **recv_pump load-shedding tier (x0x side).** When `recv_depth.pubsub` >
   threshold (e.g. 80%), shed lazy frames (IHAVE/membership) before they enter
   the channel, preserving EAGER/data delivery. Contained change at the
   `NetworkNode` recv enqueue; doesn't touch the dispatch concurrency. Lower
   throughput benefit but lower risk. Complements (1).

3. **Bound the aggregate fan-out wait** (not per-peer): cap the
   `collect_results` await at e.g. 300ms; stragglers' results are recorded
   late/async. Smaller change than (1) but still needs the detached-tail.

4. **Tuning only** (fallback): raise pubsub_workers and/or recv channel
   capacity. Brute-force; pushes the cliff out, doesn't remove it.

Recommendation: implement (1) + (2) together; (1) restores drain throughput,
(2) protects the channel during transients.

## Validation (REQUIRED before merge/release)

The bug only reproduces under degraded cross-region links — NOT on healthy
localhost (proven: `e2e_stress_gossip` 5000 msgs + `--slow-subscriber` = 0
drops). So:
1. Reproduce drop_full on the testnet under a degraded condition (either wait
   for natural APAC degradation, or inject loss/latency on one node, e.g.
   `tc qdisc add dev eth0 root netem loss 10% delay 150ms` on a VPS toward APAC).
2. Confirm pre-fix soak shows drop_full > 0 under that condition.
3. Confirm post-fix soak shows drop_full = 0 (or ≫ lower) under the SAME
   condition, AND a healthy-mesh soak stays 12/12 GO (no regression).
4. Watch for new failure modes: permit/admission-depth leaks
   (`dropped_critical_hard_error` must stay 0), cooling behaviour, ordering.

Ship as saorsa-gossip 0.5.53 → x0x 0.19.48 (dependency-order release), only
after (3) passes.
