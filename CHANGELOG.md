# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.58] - 2026-05-29

### Changed

- Re-pin `ant-quic` 0.27.24 → 0.27.25 across the workspace. ant-quic 0.27.25
  adds the ACK-v2 empty-response duplicate-safe retry (an intermittent
  `invalid ACK-v2 response envelope: len=0` mid-exchange drop is now retried
  with the same request id instead of surfacing a hard `Connection` error).
  No saorsa-gossip code changes; this release re-pins and re-publishes the
  workspace against the fixed transport.

## [0.5.57] - 2026-05-29

### Changed

- **X0X-0074d — per-peer FIFO Critical send gate.** Concurrent Critical
  pub/sub sends to the same peer are now serialized through a bounded
  per-peer FIFO gate instead of hard-dropping the second one. The single
  in-flight Critical send per peer holds a `tokio::Semaphore` permit; the
  rest wait FIFO up to `OUTBOUND_CRITICAL_QUEUE_PER_PEER = 64`. Reservation
  is synchronous/non-blocking (runs under the topics lock, bounds spawned
  tasks); the FIFO wait happens inside the already-detached per-peer send
  task, so the dispatcher worker is never pinned. This drives the
  production `dropped_critical_hard_error` counter — previously non-zero and
  climbing fleet-wide after 0074c — toward zero: only genuine queue overflow
  (past the bound) now records a hard error.
- Critical Data sends no longer occupy the budget's in-flight lane; the gate
  is their sole limiter.

### Added

- **`dropped_critical_cooling`** admission counter. Critical sends skipped
  because the peer is actively cooling/suppressed (or has a recovery probe
  in flight) are now recorded here instead of being miscounted as
  `dropped_critical_hard_error`. Legitimate transient backpressure, distinct
  from the soak-blocking hard-error contract. (Additive snapshot field.)

## [0.5.56] - 2026-05-28

### Added

- **PubSubManager subscribe-readiness barrier (#21).** Downstream
  consumers can now rely on a deterministic "first message will not be
  dropped" guarantee instead of the previous "subscribe + sleep" warm-up
  pattern. ant-quic stays at 0.27.24.

## [0.5.55] - 2026-05-27

### Changed

- **X0X-0074c — dedicated per-peer Critical Data lane.** Split the single
  per-peer outbound Data permit into a dedicated Critical lane plus a
  best-effort lane shared by Normal/Bulk, so Normal/Bulk can no longer
  starve Critical DM / control-plane traffic. Release decrements the
  correct lane idempotently by slot. Validated: 164/164 pubsub tests,
  workspace clippy -D warnings + fmt clean, 4-model adversarial review
  unanimous SHIP.
- Pins ant-quic 0.27.24 (connection-level `receive_window` memory bound).

### Removed

- The inert **X0X-0074b** Bulk-evict-before-Critical-drop prototype
  (`CancellationToken`, `PeerSendOutcome::Evicted`,
  `evicted_bulk_for_critical`), now superseded by the X0X-0074c dedicated
  lane.

### Known limitation

- `dropped_critical_hard_error` is not driven to zero: a second
  *concurrent Critical send to the same peer* still hard-drops (the
  single Critical lane holds one in-flight send). Tracked as **X0X-0074d**
  (GitHub #22).

## [0.5.54] - 2026-05-26

### Changed

- Demoted per-message degradation logs to DEBUG (IWANT-for-unknown,
  per-peer X0X-0074 hard-error, per-peer send-timeout), cutting ~90% of
  WARN-level syslog volume under severe cross-region degradation. Gate
  counters and aggregated WARNs are preserved. See pubsub commit 59153b1.
  162/162 pubsub tests, fmt + clippy clean.

## [0.5.53] - 2026-05-25

### Changed

- **PubSub fan-out detach + `peek_message_kind`.** Validated by a
  6h / 12-window VPS soak: scenario `drop_full=0` in all windows across
  36.3M dispatched messages (vs the 27,560-drop incident that motivated
  the fix); dispatcher adaptive-rate policy OK. See x0x ADR 0010 and
  `docs/design/pubsub-fanout-backpressure.md`.

## [0.5.52] - 2026-05-23

### Fixed

- Clawpatch auto-review fixes (6h x0x soak GO). Re-pins ant-quic to the
  freshly-published 0.27.23. Patch release.

## [0.5.51] - 2026-05-16

Four review-driven correctness fixes shipped as PRs #16–#19 against the
open issue backlog (#14, #9, #15, #13).

### Fixed

- **#14** — `UdpTransportAdapter::close()` (both `GossipTransport` and
  legacy `TransportAdapter` impls) now iterates `Node::connected_peers()`
  and calls `Node::disconnect()` for every live ant-quic connection
  before clearing local tracking, instead of only emptying the lazy
  adapter map. Fixes graceful shutdown.
- **#9** — `SwimDetector` background tasks (probe, probe-timeout,
  suspect-timeout) use `tokio::time::interval_at(now + period, period)`
  so the first tick fires after one full period, closing a `handle_ack`
  proptest race.
- **#15** — `GossipRuntimeBuilder::build()` enforces identity / transport
  peer-id coherence (creates transport from identity keypair when only
  identity is given; errors clearly on transport-only or on peer-id
  mismatch), closing the silent "Peer not connected" footgun.

### Added

- **#13** — Layer 2 privacy: `LogPeerId` / `LogTopicId` newtypes in
  `saorsa-gossip-types` (per-process random salt + BLAKE3 keyed hash).
  Migrated all PII-bearing `warn!` / `error!` callsites across pubsub,
  membership, presence, transport, runtime, crdt-sync, and coordinator.
  616/616 workspace tests pass; clippy and fmt clean.

## [0.5.50] - 2026-05-15

### Added

- **X0X-0071** — libp2p-style P1–P7 peer-scoring engine, topic-scoped by
  `(TopicId, PeerId)` and wired into the production `handle_eager` path so
  `PubSubStageStatsSnapshot.peer_scores_v2` is populated from real EAGER
  traffic. Telemetry-only — threshold enforcement is X0X-0071b. (PR #6,
  commit 3aeea04)

### Changed

- May 2026 coverage gains across runtime / rendezvous / coordinator
  (runtime 43% → 95.26%; rendezvous 94.37%) — purely test-additive.
- main hygiene: rendezvous clippy `inconsistent_digit_grouping`, runtime
  `clippy::len_zero`, two broken intra-doc links, regenerated
  workspace-hack. (PR #8, commit 30caeb5)

### Known

- `membership::pending_probes_cleared_on_ack` proptest is intermittent
  (issue #9, pre-dates this release; fixed in 0.5.51).

## [0.5.49] - 2026-05-13

Reviewer round 3 corrections to X0X-0074 admission control. Three of
four findings affected saorsa-gossip; one (topic classifier) belongs to
the x0x consumer.

### Fixed (reviewer round 3 findings 2026-05-13)

- **P1.2 — IHAVE flush and anti-entropy bypass admission.** Background
  protocol traffic (`spawn_ihave_flusher`, `spawn_anti_entropy_task`)
  called `claim_topic_send_attempts_for_state` directly without
  routing peers through the admission gate. Bulk-classified topics
  could push anti-entropy into the per-peer pipeline under pressure,
  undercutting the X0X-0074 mechanism's stated purpose. Now both
  background paths route through a new free-function helper
  `filter_peers_through_admission_in_state` that reuses the same
  decision logic as `parallel_send_to_peers` and tracks the admitted
  Bulk set for explicit release at end-of-iteration.
- **P2.2 — Bulk depth leaked on future cancellation in
  parallel_send_to_peers.** The fanout path released Bulk admissions
  via an explicit call at function end. If the future was dropped /
  cancelled between admission increment (in
  `filter_peers_through_admission`) and the explicit release at
  function end, per-peer depth leaked permanently, causing false
  `BulkBackpressure` until the daemon restarted. Replaced the manual
  release with a `BulkAdmissionSetGuard` RAII that releases on drop —
  same pattern `send_to_peer_bounded` already used via
  `BulkAdmissionGuard`. Now uniformly covers no-claim, partial-claim,
  send-task panic, AND future-cancellation paths.

### Added

- `filter_peers_through_admission_in_state` — free-function admission
  filter usable from background tasks that already hold a
  `&TopicState` reference under a `topics.write()` guard. Returns
  `(admitted, bulk_admitted)`.
- `release_bulk_admissions_free` — companion that decrements the
  per-peer Bulk depth for a list of peers.
- `BulkAdmissionSetGuard` — RAII variant of `BulkAdmissionGuard` for
  the publish-fanout admitted-set.
- `HyParViewMembership::swim_arc()` — clones the shared
  `Arc<SwimDetector<T>>` for downstream consumers that need
  `Arc<dyn PeerHealthOracle>`. Pairs with the existing `swim() ->
  &SwimDetector<T>` accessor; the detector keeps a single
  authoritative state regardless of how many shared references exist.
  Underlying `swim: SwimDetector<T>` field is now
  `swim: Arc<SwimDetector<T>>` (private implementation detail).

### Validation

- cargo fmt --all -- --check: clean
- cargo clippy --all-features --all-targets -- -D warnings: clean
- cargo nextest run --workspace: 522/522 pass

### Notes (round 3, P2.1 lives in x0x)

x0x's topic classifier omitted `x0x.directory.*` shard topics
(production constants in `src/groups/discovery.rs:9`, published from
`src/bin/x0xd.rs:5597`, `:5856`, `:5996`). Those default to Normal
admission today; the fix lands in x0x 0.19.44 alongside the
saorsa-gossip 0.5.49 dep bump.

### Notes (round 3, P1.1 lives in x0x)

x0x's `PubSubManager::new` did not wire the SWIM `PeerHealthOracle`
into `PlumtreePubSub::with_health_oracle`, leaving the peer-health
snapshot empty so X0X-0073b's Suspect/Dead cooling branches and
X0X-0074's Suspect/Dead admission drops never engaged in x0x
production. The Arc-friendly accessor added in this release
(`swim_arc()`) is the cross-crate piece; the wiring change lands in
x0x 0.19.44.

## [0.5.48] - 2026-05-12

Documentation-only correction over 0.5.47. No behavioural changes.
Same admission/cooling semantics as 0.5.47 — this release simply makes
the source-level docs match what's implemented so downstream readers
aren't misled.

### Fixed (reviewer round 2 findings 2026-05-12)

- **P3 doc drift in `crates/pubsub/src/admission.rs` module docstring.**
  The module-level decision matrix still said:
  - "Critical → always admit. Bulk admissions are evicted first to
    make room" — but 0.5.47 records a hard-error counter instead;
    Bulk eviction is not implemented.
  - "Normal → admit unless H = Dead. Suspect and cooled peers still
    receive Normal admissions" — but 0.5.47 drops Normal on Suspect
    too, per the X0X-0074 ticket.

  Updated to accurately describe the as-implemented decision matrix
  and called out the unimplemented "Bulk-evict-before-Critical-drop"
  contract as a known MVP limitation tracked by **X0X-0074b**
  (filed in this cycle).

### Known limitation (logged for X0X-0074b)

The original X0X-0074 ticket text described Critical's contract as
"always admitted, bounded queue with bounded wait (then drop oldest
Bulk before dropping any Critical)". As of 0.5.48, the MVP records a
`dropped_critical_hard_error` counter when a Critical admission fails
to claim an outbound budget permit, but does not actively evict an
in-flight Bulk send to make room. Reaching the full contract
requires either an explicit per-peer priority queue (replacing the
permit/slack model) or transport-layer cancellation of Bulk sends.
The soak gate interpretation today is "non-zero counter = soak
blocked", not "Critical succeeded under pressure". X0X-0074b owns
the full eviction-before-drop contract.

## [0.5.47] - 2026-05-12

Reviewer-driven corrections to 0.5.46. **0.5.46 is yanked** — it shipped
with four bugs (3 P1 + 1 P2) that this release fixes. Use 0.5.47 instead.

### Fixed (reviewer findings 2026-05-12)

- **P1.1 — Critical traffic silently skippable after admission.**
  `AdmissionControl::admit` correctly returned `Admit` for Critical,
  but the downstream `claim_topic_send_attempts` could still return
  no permit (outbound budget exhausted). The previous code just
  short-circuited without recording the hard error. 0.5.47:
  - `parallel_send_to_peers` and `send_to_peer_bounded` count
    `admitted - attempts_count` Critical claim failures as
    `dropped_critical_hard_error` and emit a `warn!` log.
  - New `AdmissionStats::record_critical_hard_error()` is the public
    counter accessor.
  - Regression test:
    `admission_records_critical_hard_error_when_outbound_budget_exhausted`.
- **P1.2 — Production topic names did not match the classifier.**
  DM uses `x0x/dm/v1/bus` (slash style); the prior classifier had
  `x0x.dm.` (dot style), routing DM to Normal in production. Same
  for release manifests (`x0x/release`) and DM capability adverts
  (`x0x/caps/v1`). 0.5.47 adds the slash-style prefixes to the
  classifier and seeds them in `register_x0x_topic_priorities`.
  Tests now assert the actual production topic strings from
  `src/dm_inbox.rs`, `src/upgrade/manifest.rs`, and
  `src/dm_capability.rs` classify correctly.
- **P1.3 — Bulk admission reservations could leak.**
  `filter_peers_through_admission` incremented per-peer Bulk depth,
  but the release path iterated `claims.attempts()` AFTER it became
  empty — the loop was a no-op so depth monotonically leaked,
  eventually starving peers of future Bulk admissions with false
  `BulkBackpressure`. Partial claim failures also only released
  sent/timed-out attempts. 0.5.47:
  - `parallel_send_to_peers` snapshots the Bulk-admitted peer set up
    front and releases it via `release_bulk_admissions(&bulk_admitted)`
    exactly once at function exit. Covers no-claim, partial-claim,
    panic, and normal completion paths uniformly.
  - `send_to_peer_bounded` uses a `BulkAdmissionGuard` RAII helper
    so the release happens on drop regardless of early-return path.
  - Regression test:
    `admission_bulk_depth_releases_after_each_publish` (5 publishes
    to same peer; per-peer Bulk depth must remain 0).
- **P2 — Normal admission contradicted the ticket.**
  The X0X-0074 ticket says Normal is "admitted unless peer is under
  suspicion or peer score below threshold". 0.5.46 dropped only on
  Dead. 0.5.47 drops on Suspect too, recording into the new
  `dropped_normal_peer_suspect` counter. The score-threshold check
  is deferred until X0X-0071 lands and a score is plumbed through.
  The `normal_drops_only_when_peer_is_dead` test was renamed to
  `normal_drops_under_suspicion_and_when_dead` and updated.

### Updated decision matrix

| Topic | Health | Cooled | Decision |
|---|---|---|---|
| Critical | * | * | Admit (drop = hard error in counter) |
| Normal | Dead | * | Drop / `PeerDead` |
| Normal | **Suspect** | * | **Drop / `PeerSuspect`** (changed in 0.5.47) |
| Normal | Alive/None | * | Admit |
| Bulk | Dead | * | Drop / `PeerDead` |
| Bulk | Suspect | * | Drop / `PeerSuspect` |
| Bulk | Alive | cooled | Drop / `PeerCooled` |
| Bulk | Alive | not cooled, queue ≥ slack | Drop / `BulkBackpressure` |
| Bulk | Alive | not cooled, queue < slack | Admit |

### Tests

- 522/522 workspace tests pass (10 added since 0.5.45: 2 publish-path
  integration + 2 admission integration tests landed in 0.5.46, 2
  Bulk-depth + Critical-hard-error regression tests added in 0.5.47).
- fmt + clippy `-D warnings` clean.

## [0.5.46] - 2026-05-12 [YANKED]

**Yanked 2026-05-12** — shipped with four bugs (3 P1 + 1 P2) found in
external review. Use 0.5.47 instead.

Bundles two pieces of SOTA-Borrow Phase 2 — X0X-0073b (cooling-decision
integration on top of the X0X-0073 primitives + X0X-0069 oracle bridge)
and X0X-0074 (substrate-level admission control with topic priority).
Both pieces compose: admission relieves pressure before it enters the
per-peer pipeline; cooling handles peers that still time out at the
reduced load.

### Added (X0X-0074 admission control)

- `saorsa_gossip_types::TopicPriority` — `Bulk` / `Normal` / `Critical`
  enum. Defaults to `Normal` for unregistered topics.
- `saorsa_gossip_types::AdmissionDecision` — `Admit` / `Drop { reason }`.
- `saorsa_gossip_types::AdmissionDropReason` — `PeerDead` / `PeerSuspect` /
  `BulkBackpressure` / `PeerCooled`.
- `saorsa_gossip_pubsub::admission` module:
  - `AdmissionControl` — engine. `admit(topic, peer, health,
    is_peer_cooled)` returns the decision. Atomic counters update on
    every call.
  - `TopicPriorityRegistry` — `register(topic, priority)` +
    `priority_for(topic)`. Applications seed at startup.
  - `AdmissionConfig` — defaults `per_peer_bulk_slack_threshold = 64`
    (calibrated against the 4 h soak window-3 inflection), Critical
    queue cap 256 (telemetry only).
  - `AdmissionStats` + `AdmissionStatsSnapshot` — per-(priority, reason)
    atomic counters. `dropped_critical_hard_error` must remain zero in
    production; non-zero is a soak-blocking violation.
- `PlumtreePubSub::admission()` accessor returns the engine for
  application-side topic priority registration.
- `PlumtreePubSub::with_admission(admission)` builder for tests / custom
  configurations.
- `PubSubStageStatsSnapshot.admission` aggregated counters + per-peer
  bulk-queue depths populated in
  `admission_state_by_peer.priority_queue_depths`.

### Added (X0X-0073b cooling-decision integration)

Already landed on main as a276ab4; bundled into this release:
- 1 s background `spawn_peer_health_snapshot_refresher` populates a
  local `peer_health_snapshot` from `oracle.health_of(peer).await`.
  Hot path consumes the snapshot synchronously — no async-under-lock.
- `record_send_timeout_inner_at` branches on the snapshot:
  - `PeerHealth::Dead` → `escalate_on_dead` = 2× immediate
  - `PeerHealth::Suspect` at threshold → hold cooling, spawn indirect
    probe, no suppression event
  - `PeerHealth::Alive` / `None` → `next_cooldown` adaptive escalation
- Success path: `rtt_tracker.record(peer, observed)` on every bundle
  success; `decay_on_success` decays cooldown while retaining
  `last_suppressed_at` so the next escalation builds from the decayed
  value.
- `with_health_oracle` API change (`self` → `Self`): now consumes self
  and spawns the refresher task.

### Behaviour summary

| Topic | Health | Cooled | Decision |
|---|---|---|---|
| Critical | * | * | Admit (dropping Critical is a hard error) |
| Normal | Dead | * | Drop / `PeerDead` |
| Normal | Alive/Suspect/None | * | Admit |
| Bulk | Dead | * | Drop / `PeerDead` |
| Bulk | Suspect | * | Drop / `PeerSuspect` |
| Bulk | Alive | cooled | Drop / `PeerCooled` |
| Bulk | Alive | not cooled, queue ≥ slack | Drop / `BulkBackpressure` |
| Bulk | Alive | not cooled, queue < slack | Admit |

### Integration points

- `parallel_send_to_peers` (publish fan-out): filters peers through
  admission before claiming attempts. Bulk admissions release after
  per-peer task completion. Single `topics.read()` batches the cooled
  lookup for the whole peer list.
- `send_to_peer_bounded` (single-peer sends — IWANT, recovery probes,
  etc.): admission gate before claiming, release on completion for
  Bulk.
- Telemetry surfaces via `PubSubStageStatsSnapshot.admission` and
  per-peer depths in `admission_state_by_peer.priority_queue_depths`.

### Tests

- `saorsa-gossip-types`: 3 new tests for ordering / labels / drop-reason
  variants. 29/29 pass.
- `saorsa-gossip-pubsub::admission`: 7 unit tests covering critical-
  always-admit / normal-only-drops-on-dead / bulk-rules / backpressure /
  per-peer-snapshot / registry-overwrite / unregistered-defaults-normal.
- `saorsa-gossip-pubsub::tests::admission_*`: 2 async integration tests
  exercising the publish path with seeded health snapshots:
  `admission_drops_bulk_send_to_suspect_peer_and_records_counter`
  (Bulk + Suspect → Drop, transport never touched, counter +1) and
  `admission_admits_critical_send_even_when_peer_dead` (Critical
  bypasses health, counter zero on hard-error). Both run in <30 ms —
  the admission gate avoids the 2.5 s per-peer transport timeout.
- 520/520 workspace tests pass; fmt + clippy `-D warnings` clean.

### Notes

- This is the release that gates the 4 h Phase A soak validating the
  original X0X-0073 acceptance (≥ 5× cooling event reduction, no peer
  > 30 s continuously cooled, Phase A ≥ 98 %).
- X0X-0071 (P1-P7 peer scoring) is now functionally unblocked.

## [0.5.45] - 2026-05-12

X0X-0075 Part A — per-topic and per-peer suppression diagnostics
shipped with ant-quic 0.27.22 pin (X0X-0075 Part B's
`ConnectionTransportStats` becomes consumable by downstream x0x).

### Why

Reviewer (2026-05-12): "the current timeline counts suppression, but
we need to know whether the suppressed topics are release, identity,
discovery, DM inbox, group cards, or test discovery." Without that,
admission-control tuning (X0X-0074) and cooling-decision integration
(X0X-0073b) cannot be validated — the visibility is the gate.

### Added

- `PubSubStageStatsSnapshot` gains three new fields:
  - `suppressed_peers_by_topic: BTreeMap<String, Vec<String>>` —
    which peers cool on which topic, deduplicated + sorted per topic.
  - `peer_scores_by_topic: BTreeMap<String, BTreeMap<String,
    PeerScoreBreakdownSnapshot>>` — per-(topic, peer) score components
    + active cooling state cross-reference.
  - `admission_state_by_peer: BTreeMap<String, AdmissionStateSnapshot>`
    — peer-indexed admission state inferred from active cooling.
    `priority_queue_depths` reserved for X0X-0074.
- `PeerScoreBreakdownSnapshot` — role, score, send_health,
  outbound_send_timeouts, cooling_events, eager_eligible, plus
  optional suppression_state, recent_timeout_count, cooldown_ms,
  last_cool_at_unix_ms when the peer is currently cooled.
- `AdmissionStateSnapshot` — state (cooled / recovery_probe /
  recovery_ready / alive), suppressed_topics_count,
  cooled_topics_count, recovery_probe_topics_count,
  recovery_ready_topics_count, priority_queue_depths (empty until
  X0X-0074).
- `SuppressedPeerSnapshot.last_suppressed_unix_ms` — wall-clock Unix
  millisecond when the (peer, topic) entered suppression or
  recovery-probe state. Persisted in `SuppressedPeerState`; updated
  by both `record_suppression` and `record_recovery_probe`.
- Three builder functions on `PlumtreePubSub`:
  `build_suppressed_peers_by_topic`,
  `build_peer_scores_by_topic`,
  `build_admission_state_by_peer`. Called inside
  `pubsub_stage_stats_snapshot` after the flat snapshots are
  built so the topic-indexed views are consistent with the legacy
  fields they derive from.
- 2 new unit tests:
  `diagnostics_group_suppression_by_topic_and_peer_admission_state`
  and `diagnostics_peer_scores_by_topic_include_active_cooling_breakdown`.

### Changed

- Workspace `ant-quic` dependency bumped 0.27.15 → 0.27.22 to bring
  the X0X-0075 Part B `ConnectionTransportStats` surface and a
  number of intervening fixes since 0.27.15:
  - 0.27.16-0.27.20: X0X-0062 cancellation-safe direct-DM ACK loop
  - 0.27.21: caller-supplied ACK-v2 request id (X0X-0066 enablement)
  - 0.27.22: X0X-0075 Part B `ConnectionTransportStats`
- No behavioural change in saorsa-gossip from the ant-quic bump itself
  — only the new surface (consumable by downstream x0x).

### Notes

- The cooling-decision integration that consumes these diagnostics
  ships as X0X-0073b / X0X-0069b (next saorsa-gossip release).
- Admission control (X0X-0074) is now functionally unblocked — its
  only blocker was the visibility this release provides.

## [0.5.44] - 2026-05-12

X0X-0073 (MVP, v2) — reviewer-driven corrections to the 0.5.43
adaptive cooling primitives. Same scope (primitives only;
cooling-decision integration still ships as X0X-0073b), but the
semantics now match the X0X-0073 ticket text.

### Why

0.5.43 shipped a mean EWMA where the ticket asks for "p95 via EWMA
(N=32 samples)", and was missing the libp2p-style 0.97/sec success
decay. External review (2026-05-12) flagged five P1/P2 issues; 0.5.44
addresses all of them before any downstream consumer wires the
primitives up.

### Fixed (reviewer findings)

- **p95 vs mean EWMA**: `PerPeerRttTracker` now keeps a 32-sample
  ring buffer per peer and computes p95 on demand. The EWMA is
  retained as a separate diagnostic (`ewma_ms`); the timeout
  calculation reads p95 (`p95_ms`) only. This matches the ticket's
  stated semantics and captures the tail behaviour the multiplier
  is intended to absorb.
- **`adaptive_timeout` ceiling-after-floor**: a `legacy_floor`
  larger than `PER_PEER_TIMEOUT_CEILING` is now capped at the
  ceiling instead of being returned verbatim. The advertised 10 s
  upper bound is now enforced on every code path.
- **Cooldown success decay**: new
  `AdaptiveCoolingConfig::decay_on_success(current, elapsed)` applies
  libp2p's 0.97/sec exponential decay toward `self.initial`. Exposes
  `ADAPTIVE_COOLDOWN_DECAY_PER_SEC = 0.97`.
- **Escalation factor clamp**: `escalation_factor` is now private;
  `with_escalation_factor(0)` is clamped to 1 so a misconfiguration
  cannot produce a zero cooldown after the first event.
- **`PlumtreePubSub::with_health_oracle` doc**: trimmed to describe
  the bridge surface actually wired in 0.5.42–0.5.44 (oracle
  installed, `peer_health` + `request_indirect_probe` consult it;
  cooling-decision path does NOT yet read the oracle). The
  Suspect-grace / Dead-escalation behaviour the prior doc claimed
  ships as X0X-0069b.

### Added

- `PerPeerRttTracker::p95_ms(peer)` — sliding-window p95 in ms.
- `AdaptiveCoolingConfig::decay_on_success(current, elapsed)` —
  0.97/sec decay toward `self.initial`.
- `AdaptiveCoolingConfig::escalate_on_dead(current)` — immediate 2×
  cooldown escalation when SWIM returns `Dead` (used by X0X-0069b).
- `AdaptiveCoolingConfig::with_escalation_factor(factor)` — builder
  that clamps to ≥ 1.
- `ADAPTIVE_COOLDOWN_DECAY_PER_SEC` constant.
- 8 new unit tests covering p95 vs mean, ring eviction of stale
  outliers, ceiling-vs-oversize-floor, decay-on-success at floor /
  proportional / long-elapsed / never-below-floor, Dead escalation
  doubling / zero-base / cap, escalation-factor zero-clamp.

### Notes

- API-only changes; no behavioural integration yet. Downstream
  X0X-0069b will wire the new helpers into
  `record_send_timeout_at`'s threshold-crossing path under
  background snapshot of the SWIM oracle (no async-under-hot-lock).
- The X0X-0073 ticket's behaviour + 4 h soak acceptance criteria
  remain unmet by this MVP — they belong to X0X-0073b/0069b which
  integrate these primitives into the cooling path. The X0X-0073
  ticket state reflects MVP-shipped, not done-against-original-
  acceptance.

## [0.5.43] - 2026-05-12

X0X-0073 (MVP) — adaptive cooling primitives. Layer 2 of x0x's
SOTA-Borrow Phase 2 fleet-survival portfolio.

### Why

The legacy cooling values (`PER_PEER_REPUBLISH_TIMEOUT = 2500ms`,
`PEER_SUPPRESSION_COOLDOWN = 120000ms`, `PEER_SUPPRESSION_BACKOFF_MAX
= 1800000ms`) were calibrated for intra-region paths. Cross-Pacific
paths (helsinki ↔ singapore/sydney, ~280 ms one-way) routinely exceed
2.5 s under fanout_burst load. The 120 s initial cooldown is too
long for transient WAN slowness, and 30 min escalation max is
draconian for what's often a passing congestion bump.

X0X-0073 ships **per-peer adaptive primitives** so downstream
consumers (X0X-0069b cooling-decision integration, X0X-0071 P1-P7
scoring) can read calibrated per-peer values instead of fleet-wide
constants. This release ships the primitives + thresholds; the actual
consumption in pub-sub's cooling decision path lands as X0X-0073b
follow-up (same MVP pattern as X0X-0069 → X0X-0069b).

### Added

- New `saorsa_gossip_pubsub::timing` module:
  - `PerPeerRttTracker` — per-peer EWMA of observed send durations
    (alpha 0.20, ~5-sample convergence). `record(peer, observed)`
    adds a sample, `ewma_ms(peer)` reads the current EWMA,
    `adaptive_timeout(peer, legacy_floor)` returns `max(2.5 × ewma,
    PER_PEER_TIMEOUT_FLOOR)` capped at `PER_PEER_TIMEOUT_CEILING`.
  - `AdaptiveCoolingConfig` — replaces fixed cooldown constants.
    Defaults: initial = 30 s (was 120 s), max = 300 s (was 1800 s),
    escalation factor = 2× per consecutive cool. `with_initial` /
    `with_max` builder for tests + tuning.
  - Constants: `ADAPTIVE_COOLDOWN_INITIAL = 30s`,
    `ADAPTIVE_COOLDOWN_MAX = 300s`, `PER_PEER_TIMEOUT_FLOOR = 1500ms`,
    `PER_PEER_TIMEOUT_CEILING = 10s`, `PER_PEER_TIMEOUT_MULTIPLIER
    = 2.5`, `PER_PEER_RTT_EWMA_ALPHA = 0.20`,
    `PER_PEER_RTT_SAMPLE_CAP = 32`.
- 12 new unit tests in `timing::tests` covering EWMA convergence,
  sample-count cap, adaptive-timeout floor / ceiling / cold-start,
  cooldown escalation + cap + overflow safety.

### Scope notes

This is the **MVP** for X0X-0073. The primitives ship now so X0X-0069b
(cooling-decision integration) and X0X-0071 (P1-P7 scoring) unblock
immediately. The actual consumption of these primitives inside
`PeerCoolingState::next_cooldown` and the `record_send_timeout_at`
path lands as X0X-0073b once a downstream ticket needs the integrated
behaviour. Cooling behaviour for callers that don't opt in is
unchanged.

### Reference

- Reference: x0x ticket X0X-0073
- SOTA reference: libp2p gossipsub v1.1 (1-minute recommended
  backoff, 0.97/sec decay)
- See x0x `docs/design/sota-borrow-phase-2-fleet-survival.md` for the
  portfolio context

## [0.5.42] - 2026-05-12

X0X-0069 (MVP) — SWIM peer-health oracle bridge between the membership
crate's `SwimDetector` and the pub-sub crate's per-topic cooling
decisions. Layer 3 of x0x's SOTA-Borrow Phase 2 fleet-survival
portfolio.

### Why

Pub-sub's per-topic cooling (saorsa-gossip-pubsub 0.5.36+'s
`PeerCoolingState`) currently treats every per-peer timeout as evidence
the peer is failing, with no awareness of global SWIM membership state.
A cross-region peer transiently slow under load gets cooled for the full
`PEER_SUPPRESSION_COOLDOWN` (2 minutes), even when the membership
crate's `SwimDetector` already knows the peer is responsive on other
paths. Lifeguard (Dadgar/Hendrickson 2018) reference: SWIM Suspicion
prevents false-positive failure marks under sustained packet loss
(12 vs 2 stable members in their experiments).

This release ships the **bridge surface** so downstream cooling
decisions can consult SWIM. The actual threshold-decision integration
(use the bridge to hold cooling on Suspect peers, escalate on Dead
peers) is filed as X0X-0069b follow-up, intentionally scoped out so
this release does not change cooling behaviour for callers that don't
opt in.

### Added

- `saorsa_gossip_types::PeerHealth` enum (`Alive` / `Suspect` / `Dead`)
  maps SWIM states onto pub-sub's vocabulary without exposing the
  membership crate's internal `SwimPeerEntry`.
- `saorsa_gossip_types::PeerHealthOracle` async trait — minimal
  read-only surface (`health_of`, `request_indirect_probe`) for the
  cross-crate bridge.
- `impl<T: GossipTransport + 'static> PeerHealthOracle for SwimDetector<T>`
  in `saorsa-gossip-membership` — maps `PeerState` →`PeerHealth`,
  delegates indirect-probe requests to the existing
  `SwimDetector::request_indirect_probes` async path.
- `PlumtreePubSub::with_health_oracle(oracle)` builder method that
  installs the oracle on the pub-sub instance.
- `PlumtreePubSub::peer_health(peer)` — async accessor exposing the
  oracle verdict per peer; downstream tickets (X0X-0073 adaptive
  cooling, X0X-0071 P1-P7 scoring, X0X-0074 admission control) read
  via this single bridge surface.
- `PlumtreePubSub::request_indirect_probe(target)` — best-effort
  nudge for SWIM to issue indirect probes, called by accumulating-
  timeout paths to keep oracle signal fresh.
- `GossipRuntimeBuilder::peer_health_oracle(oracle)` wires the oracle
  through the runtime so the pub-sub instance picks it up
  automatically; if the runtime owns its own `SwimDetector`, wiring
  is one builder call.
- New tests in `crates/pubsub`:
  - `peer_health_oracle_unwired_returns_none`
  - `peer_health_oracle_returns_swim_state_when_wired`
  - `request_indirect_probe_forwards_to_oracle`

### Scope notes

This is the **MVP** for X0X-0069. The bridge plumbing ships now so
downstream tickets unblock immediately. The actual cooling-decision
integration (consult oracle in `record_send_timeout_at` threshold-
crossing, hold cooling for `Suspect`, escalate for `Dead`) lands as
X0X-0069b once a downstream consumer needs it.

### Reference

- Reference: x0x ticket X0X-0069
- SOTA reference: SWIM paper (Das/Gupta/Motivala 2002), Lifeguard
  extensions (Dadgar/Hendrickson 2018)
- See x0x `docs/design/sota-borrow-phase-2-fleet-survival.md` for
  the portfolio context

## [0.5.41] - 2026-05-12

X0X-0068 — Bounded discovery cache by age + bytes, with per-topic telemetry.
Layer 1 of the SOTA-Borrow Phase 2 fleet-survival portfolio (see x0x
`docs/design/sota-borrow-phase-2-fleet-survival.md`).

### Why

The 2026-05-12 4h cert soak on x0x's 6-node bootstrap mesh showed the
classic "load grows with state" pattern: `continuous_max_pp_to` climbed
0→1835 across 6 windows, and `recv_pump.dropped_full` ticked > 0 at
window 11. saorsa-gossip's per-topic message cache was bounded by count
(`MAX_CACHE_SIZE = 2_048`) but not by bytes or age. With 11-16 KB group
cards on `x0x.discovery.groups`, worst-case per-topic state is ≈32 MB
before signature overhead; multiplied across active topics, ≈100 MB+
state per anti-entropy reconciliation cycle. Cross-Pacific paths
(helsinki↔singapore, helsinki↔sydney) can't sustain that bandwidth and
cool repeatedly.

### Added

- `BoundedMessageCache` replaces the bare `LruCache` for per-topic
  message storage. Bounds applied in priority order:
  1. **Age cap** — `MAX_CACHE_AGE_SECS = 60`. Entries older than this
     are evicted on every cache touch.
  2. **Bytes cap** — `MAX_CACHE_BYTES_PER_TOPIC = 16 MB`. LRU eviction
     when the per-topic byte total would exceed the cap (with
     `MESSAGE_CRYPTO_OVERHEAD_BYTES = 5_500` + `MESSAGE_HEADER_OVERHEAD_BYTES
     = 256` factored into the byte estimate).
  3. **Count cap** — existing `MAX_CACHE_SIZE = 2_048` retained as the
     hard upper bound.
- `CacheStatsSnapshot { msg_count, total_bytes, oldest_age_secs,
  evicted_by_age, evicted_by_bytes, evicted_by_count }` exposed per
  topic via `TopicCacheStatsSnapshot { topic, cache }`.
- `PubSubStats.topic_caches: Vec<TopicCacheStatsSnapshot>` lets
  consumers see the caps engaging in production.
- `PubSubCacheConfig { max_messages_per_topic, max_bytes_per_topic,
  max_age }` — operator-tunable. `GossipRuntimeConfig::pubsub_cache`
  + `GossipRuntimeConfig::pubsub_cache(...)` builder method wire it
  through.
- `PlumtreePubSub::new_with_cache_config` /
  `::new_with_task_control_and_cache_config` constructors that take
  `PubSubCacheConfig`. The existing zero-argument constructor remains
  backwards compatible (uses `PubSubCacheConfig::default()`).
- New tests in `crates/pubsub`:
  - `bounded_cache_evicts_by_age`
  - `bounded_cache_evicts_by_bytes_under_pressure`
  - `bounded_cache_evicts_by_count_hard_cap`
  - `bounded_cache_age_takes_precedence_over_bytes`
  - `bounded_cache_eviction_counters_track_correctly`
  - `bounded_cache_get_prunes_expired`
  - `bounded_cache_simulated_load_stays_within_caps`

### Reference

- Reference: x0x ticket X0X-0068
- SOTA reference: High-Scalability gossip protocol guide
- Hunt 12f forecast (§147 in x0x `docs/design/hunt-12f-stale-release-fast-drop.md`)

## [0.5.36] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.12 (X0X-0037 — duplicate-safe
ACK-v2 timeout retry: B3 envelope with request_id + receiver dedupe cache +
sender single retry). No source changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.11"` → `"0.27.12"`.
- **Workspace version**: 0.5.35 → 0.5.36 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.35] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.11 (X0X-0036 part 2 —
ACK-v2 priority + per-stage diagnostics + 500 ms receiver-side bounded
response write timeout). No source changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.10"` → `"0.27.11"`.
- **Workspace version**: 0.5.34 → 0.5.35 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.34] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.10 (X0X-0036 part 1 —
probe scavenger priority + single-flight + global cap). No source
changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.9"` → `"0.27.10"`.
- **Workspace version**: 0.5.33 → 0.5.34 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.33] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.9 (X0X-0035 fix —
ACK-v2 / relay-CONNECT-UDP bidi accept-race resolved via prefix-peek
demux). No source changes in saorsa-gossip itself.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.8"` → `"0.27.9"`.
- **Workspace version**: 0.5.32 → 0.5.33 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo test --all-features --workspace --lib` — full workspace passes.

## [0.5.32] - 2026-05-07

Workspace lockstep bump consuming ant-quic 0.27.8 (X0X-0034 fix — bidi ACK
protocol + supersede-race grace window). No source changes in saorsa-gossip
itself; this is purely a transport-layer pin update so downstream consumers
(x0x) can pull a single coordinated upstream version.

### Changed

- **`Cargo.toml`** workspace dependency: `ant-quic = "0.27.5"` → `"0.27.8"`.
  All 11 saorsa-gossip-* crates inherit via `workspace = true`.
- **Workspace version**: 0.5.31 → 0.5.32 across every crate.

### Verified

- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-features --all-targets -- -D warnings` clean.
- `cargo nextest run --all-features` — full workspace passes.

## [0.5.30] - 2026-05-03

### Changed

- PlumTree topic-peer refresh now admits newly connected peers as LAZY first,
  then uses score-aware mesh maintenance to fill only the bounded EAGER degree.
- IWANT repair and first-contact EAGER senders now go through the same bounded
  score-aware maintenance path instead of directly expanding EAGER membership.
- Added rate-limited opportunistic grafting so a substantially better LAZY peer
  can replace one low-score EAGER peer while the mesh remains within degree
  bounds.
- ADR-009 now documents how PlumTree `MIN_EAGER_DEGREE`, `MAX_EAGER_DEGREE`,
  LAZY peers, and opportunistic grafting map to Gossipsub-style mesh parameters.

## [0.5.29] - 2026-05-03

### Added

- PubSub peer-score diagnostics now include per-topic score components for
  EAGER/LAZY/cooled peers, including decayed send successes, timeouts, cooling
  events, recovery probes, and recovery successes.

### Changed

- PlumTree mesh selection now folds send-side health into peer scores so
  outbound timeouts and cooling events reduce future EAGER eligibility, while
  successful recovery sends rebuild trust gradually.
- Topic peer refresh now uses the same score-aware degree maintenance as the
  background maintainer instead of bulk-promoting connected LAZY peers.

### Fixed

- Score cleanup now retains fresh send-side evidence for the full cooling/backoff
  horizon, preventing long-backoff peers from looking freshly healthy too early.

### Compatibility

- `PubSubStageStatsSnapshot` has a new public `peer_scores` field. Consumers
  that construct this diagnostic snapshot directly with a struct literal may
  need to add the field; serde/JSON consumers remain additive-compatible.

## [0.5.28] - 2026-05-03

### Added

- PubSub outbound sends now enforce a per-peer concurrency budget: one EAGER/data
  send plus a small control-plane budget for IHAVE, IWANT, and anti-entropy.
- `PubSubStageStatsSnapshot::outbound_budget_exhausted` exposes skipped sends
  caused by a peer already consuming its outbound PubSub permits.

### Fixed

- Repeated outbound budget pressure now feeds the existing slow-peer cooling
  path, so one overloaded peer cannot keep spawning fresh send tasks while
  waiting for timeout-based suppression.
- Outbound send permits are released on completion, timeout, panic, and caller
  cancellation, preventing leaked peer budget after aborted sends.

## [0.5.27] - 2026-05-03

### Fixed

- PubSub slow-peer recovery now uses a single post-cooldown probe per
  peer/topic. A failed probe immediately re-suppresses the peer with
  exponential backoff instead of allowing another full timeout window.
- GRAFT and degree maintenance no longer re-admit peers whose cooldown has
  expired until a recovery probe succeeds.
- Recovery-probe diagnostics now distinguish `cooldown`, `recovery_ready`, and
  `recovery_probe` states, and clear stale probe diagnostics when topics are
  reaped or disappear before result recording.

## [0.5.26] - 2026-05-03

### Fixed

- PubSub slow-peer cooling now also applies to single-peer recovery/control sends
  (IWANT, anti-entropy, and EAGER replies to IWANT). These paths previously used
  the bounded send timeout but did not update the per-topic cooling state, so a
  cooled peer could keep consuming 750 ms send slots outside normal EAGER fanout.

## [0.5.25] - 2026-05-03

### Added

- PubSub sender-side slow-peer cooling for PlumTree fanout. Peers that repeatedly
  hit the per-peer send timeout on a topic are temporarily demoted from EAGER to
  LAZY and skipped during the cooldown, freeing dispatcher capacity without a
  daemon restart.
- `PubSubStageStatsSnapshot::suppressed_peers` diagnostics showing cooled
  peer/topic entries, cooldown expiry, recent timeout rate, and affected topic
  count.

### Fixed

- Periodic topic peer refresh no longer immediately re-promotes a peer that is
  actively cooling after send-side timeouts.
- Cooled peers are re-admitted after cooldown expiry through the normal PlumTree
  maintenance/refresh path instead of requiring operator restart.

## [0.5.23] - 2026-04-27

### Fixed

- Presence beacon fanout now sends to peers concurrently with a bounded timeout,
  preventing one slow or wedged peer from delaying delivery to the rest of the
  mesh.
- Increased the per-peer presence beacon send timeout to 15 seconds now that
  fanout is concurrent, reducing false timeouts on high-latency live links
  without reintroducing head-of-line blocking.

## [0.5.22] - 2026-04-26

### Fixed

- Presence beacon sends are bounded by a timeout so a stalled peer cannot wedge
  the beacon task indefinitely.
- Presence beacon fanout snapshots groups and broadcast peers before network I/O,
  so no broadcast peer lock is held across awaited sends.
- Added `PresenceManager::replace_broadcast_peers()` for authoritative fanout
  refresh and stale-peer pruning by callers.

## [0.5.20] - 2026-04-23

### Changed

- Bumped `ant-quic` to `0.27.4`, picking up the dual-stack CPU-spin fix in
  `DualStackSocket::create_io_poller` (AND-combine v4/v6 writability). No
  saorsa-gossip source changes; all 11 workspace crates re-published at
  0.5.20 for a consistent lockstep upgrade across `saorsa-gossip-{types,
  transport, membership, pubsub, presence, crdt-sync, groups, identity,
  coordinator, rendezvous, runtime}`.

## [0.5.14] - 2026-04-09

### Changed

- Updated `ant-quic` to `0.26.1`
- Saorsa Gossip now relies on ant-quic's built-in first-party mDNS discovery and additive UPnP handling for zero-config transport connectivity

### Documentation

- Updated transport documentation to reflect that local discovery and router-assisted reachability live in ant-quic rather than in gossip-specific code
- Refreshed ADR-011 to note the expanded ant-quic transport responsibilities in the current stack

## [0.5.11] - 2026-04-01

### Changed

- Updated `ant-quic` to 0.24.5 (NAT traversal coordination now uses PeerId-based lookups instead of SocketAddr)
- Updated `saorsa-pqc` to 0.5

## [0.5.3] - 2026-03-07

### Fixed

- **Best-effort EAGER forwarding**: A single `send_to_peer` failure no longer aborts the
  entire forwarding loop. Failures are logged as warnings and forwarding continues to
  remaining peers.
- **Serialization error propagation**: Re-serialization failures in `handle_eager` now
  propagate as `Err` rather than being silently swallowed as `Ok(())`.
- **Serialization hoisted outside loop**: `postcard::to_stdvec` is now called once before
  the per-peer forwarding loop instead of once per peer.

### Added

- **`set_topic_peers`** on `PubSub` trait and `PlumtreePubSub`: atomically replaces topic
  peer membership by pruning disconnected peers from eager/lazy sets and adding newly
  connected peers as eager peers. Respects existing PRUNE decisions (lazy peers that are
  still connected remain lazy). Default trait implementation falls back to
  `initialize_topic_peers` (add-only); override for full prune-and-replace semantics.

## [0.4.3] - 2026-02-01

### Changed

- Updated ant-quic dependency to 0.21.0 (from 0.20.1)
  - Adapted to API change: `recv()` no longer takes a Duration parameter

## [0.4.2] - 2026-01-31

### Changed

- **Coordinator roles are now hints, not filters** ("measure, don't trust")
  - Bootstrap selection uses all peers; roles only influence sort order
  - `find_coordinator()` now uses gossip cache adverts directly
  - `handle_find_query()` returns all adverts sorted by hint weight
  - Removed capability-based filtering from ant-quic integration

### Documentation

- Added core principle #7: "Measure, Don't Trust" to DESIGN.md
- Updated coordinator crate description in README.md
- Updated ADR-004 (Seedless Bootstrap) with hints clarification
- Updated ADR-009 (Peer Scoring) with hint-based bootstrap example
- Updated module docs in `lib.rs` to clarify roles are hints

### Removed

- `select_best_coordinator()` and `get_addr_for_method()` methods (replaced by hint-sorted advert selection)
- Capability setting in `insert_advert()` (capabilities are now measured, not trusted)

## [0.4.1] - 2026-01-30

### Changed

- Updated ant-quic dependency to 0.20.1 (from 0.20.0)
  - Includes latest bug fixes and improvements from ant-quic

## [0.4.0] - 2026-01-24

### Changed

- **Transport Layer Simplification** (Milestone 3)
  - Upgraded ant-quic from 0.19 to 0.20 with native multi-transport capabilities
  - Removed custom `TransportMultiplexer` in favor of ant-quic's `TransportRegistry`
  - Removed custom `MultiplexedGossipTransport` in favor of direct `UdpTransportAdapter`
  - Removed `BleTransportAdapter` stub - use ant-quic's native BLE transport
  - Simplified `GossipTransport` trait: removed `send_with_request()` method
  - Runtime now uses `UdpTransportAdapter` directly

### Removed

- `TransportMultiplexer`, `TransportRegistry`, `TransportRequest` (use ant-quic native)
- `MultiplexedGossipTransport` wrapper
- `BleTransportAdapter` stub
- `transport_benchmark.rs` example
- ~4,130 lines of redundant code

### Documentation

- Updated README.md transport section
- Updated benchmarks.md to reflect simplified architecture
- Added ADR-011: Transport Layer Simplification

## [0.3.0] - 2026-01-23

### Added

- **Transport Multiplexing Architecture**
  - `TransportAdapter` trait for pluggable transport backends
  - `TransportMultiplexer` for capability-based routing
  - `MultiplexedGossipTransport` implementing `GossipTransport` trait
  - `TransportCapability` enum: `LowLatencyControl`, `BulkTransfer`, `Broadcast`, `OfflineReady`
  - `TransportDescriptor` enum: `Udp`, `Ble`, `Lora`, `Custom`

- **Transport Request Builder**
  - `TransportRequest` with fluent API: `require()`, `exclude()`, `prefer()`
  - Convenience constructors: `low_latency_control()`, `bulk_transfer()`, `offline_ready()`
  - `send_with_request()` method on `GossipTransport`

- **Configuration & Context**
  - `GossipContext` struct for simplified runtime configuration
  - `GossipContextBuilder` with builder pattern
  - Integration with `GossipRuntimeBuilder`

- **Transport Hints in Peer Discovery**
  - `TransportHint` struct for multi-transport peer discovery
  - Extended `CoordinatorAdvert` with `transport_hints` field
  - Helper methods: `has_transport()`, `available_transports()`, `add_transport_hint()`

- **BLE Transport Stub**
  - `BleTransportAdapter` simulating constrained BLE characteristics
  - 512-byte MTU, 50-150ms latency simulation
  - `MtuExceeded` error variant for MTU enforcement

- **Transport Error Handling**
  - `TransportError` enum with thiserror integration
  - Variants: `ConnectionFailed`, `SendFailed`, `ReceiveFailed`, `PeerNotFound`, `Timeout`, `MtuExceeded`, `NotConnected`, `Internal`

- **Benchmarking**
  - `--multiplexed` flag for `transport_benchmark.rs`
  - `--ble` flag for BLE transport simulation
  - Per-transport statistics display
  - `docs/benchmarks.md` guide

- **Property-Based Tests**
  - 9 proptest tests for capability routing invariants
  - Idempotency, preference overwriting, capability consistency tests

### Changed

- Renamed `AntQuicTransport` to `UdpTransportAdapter`
- Moved transport file from `ant_quic_transport.rs` to `udp_transport_adapter.rs`
- Membership module now requests `LowLatencyControl` capability
- PubSub module now requests `BulkTransfer` for large messages

### Fixed

- N/A (feature release)

## [0.2.2] - 2026-01-15

### Added

- Complete Milestone 1: Production Bug Fixes & Improvements
- `GossipCacheAdapter` wrapping `Arc<BootstrapCache>` with gossip-specific advert storage
- Complete FOAF discovery with query forwarding and response aggregation
- Complete HyParView shuffle protocol implementation

### Fixed

- PeerId-to-public-key binding verification in `handle_advert()`
- FOAF response structural validation
- `select_best_from_adverts()` logic bug
- Silent serialization failures now logged
- Lock contention warnings added

## [0.2.1] - 2026-01-10

### Added

- Initial public release
- HyParView membership protocol
- Plumtree PubSub protocol
- SWIM failure detection
- Delta-CRDT synchronization (OR-Set, LWW-Register, Rga)
- Presence beacons and coordinator discovery
