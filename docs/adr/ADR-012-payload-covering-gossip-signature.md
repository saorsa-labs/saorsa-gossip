# ADR-012: Payload-Covering Gossip Message Signature

- **Status:** Accepted
- **Date:** 2026-08-18 (accepted 2026-08-19)
- **Decision owners:** David Irvine
- **Reviewers:** omp, Claude
- **Supersedes:** none
- **Superseded by:** none
- **Related:** hardening item 3 (payload-covering gossip signature); x0x #323 (interior zero-window guard); x0x #349 (CRDT authorship — closed standalone by fail-closed Layer A; this ADR inherits its v1-content interop story, not the vulnerability); saorsa-gossip v0.5.69/0.5.70 forward guards

## Context

> **Premise correction (2026-08-18, after a full consuming-surface audit).**
> An earlier draft of this ADR claimed x0x traffic has an active
> payload-forgery gap. That is **wrong** and is corrected here. x0x does **not**
> rely on the bare `saorsa-gossip` signature: it wraps its own V2 envelope
> (`x0x/src/gossip/pubsub.rs`) that signs the **full payload** + topic + author
> (`build_signing_payload` :1225, verified :1177/:1235 with an AgentId↔pubkey
> binding, unverified messages dropped at :830). So for x0x the payload **is**
> authenticated end-to-end. This ADR is therefore **ecosystem
> defense-in-depth**, not an x0x incident fix: it moves payload-covering
> signing *down into `saorsa-gossip`* so a naive consumer that does **not**
> build its own envelope (as x0x had to) is safe by default. The audit did find
> concrete application-layer gaps — tracked separately, see "Audit outcomes"
> below — but none stem from the gossip signature being header-only for a
> consumer that uses the crate's own signing as intended.

The bare `saorsa-gossip` PubSub message signature does **not** authenticate the
payload. In `crates/pubsub/src/lib.rs`:

- `GossipMessage` = `{ header: MessageHeader, payload: Option<Bytes>, signature, public_key }` (lib.rs:2013-2024).
- `verify_signature` verifies the ML-DSA signature over **`postcard(header)` only** (lib.rs:5493-5502). The payload is never in the signed bytes.
- The header binds the payload *indirectly* through `msg_id = BLAKE3(topic ‖ epoch ‖ signer ‖ payload_hash)` (types/lib.rs:197, `calculate_msg_id`).
- **But the receive path never checks that binding.** `handle_eager` (and the sibling receive handlers at lib.rs:6038, 7031, 7057, 7085) verify the header signature, then trust `header.msg_id` as-is for dedup. `calculate_msg_id` is called only on the **send** path (lib.rs:5524) and in tests — never on receive.
- Worse, the binding is **not even recomputable** at the receiver: `epoch` is `SystemTime::now() - epoch_start` at the sender (lib.rs:5439-5444) and is **not carried on the wire**. A receiver cannot reconstruct `msg_id` to validate it against the payload.

### The gap (what an attacker can do)

A malicious forwarder (any Plumtree relay hop) can take a validly-signed
message, **keep the header and signature byte-for-byte**, and replace `payload`
with arbitrary bytes. The receiver's signature check passes (header unchanged),
dedup keys on the header's `msg_id` (unchanged), and the **swapped payload is
delivered as authentic**. The payload is unauthenticated at the gossip layer.

Today's mitigations are not a substitute:
- The interior zero-window guard (x0x #323, lib.rs:2400-2460) is a *heuristic
  corruption tripwire* ("a healthy ML-DSA-signed x0x frame ends in signature
  bytes with a near-zero tail"), explicitly noting "payload signature remains
  the sole authority for delivery" (lib.rs:2406). It catches transport
  reassembly artifacts, not a crafted swap.
- x0x wraps an **inner** ML-DSA signature on some surfaces (`DmEnvelope`,
  named-group metadata events), which *are* safe regardless of the gossip
  layer. The exposure is precisely the surfaces that publish to gossip
  **without** an inner payload signature.

### Audit outcomes (completed 2026-08-18)

Every x0x gossip publish surface was traced to its receive→apply path. Result:
**latent-hardening, not an active gossip-layer vulnerability** — x0x's V2
envelope covers the payload everywhere it is used as intended. Full matrix:

| Surface | Verdict |
|---|---|
| Identity / Machine / User announce | COVERED (inner sig + receiver verify, cited) |
| Revocation records | COVERED |
| Release/upgrade manifest | COVERED |
| Rendezvous `ProviderSummary` | COVERED when key cached; else hint-only, QUIC-handshake-gated |
| DM bus envelope + durable-ACK | COVERED (agent sig + machine attestation, both verified pre-dispatch) |
| DM capability advert (steady + targeted response) | COVERED |
| Named-group metadata events (all membership variants) | COVERED (V2 author binding + per-variant commit/sig verify) |
| Group card discovery / reseal / listed-to-contacts | COVERED (`card.verify_signature`) |
| Presence beacon | COVERED (signed + verified in the presence dep) |
| KV store deltas + state-sync | COVERED (V2 author = writer auth, fail-closed on unsigned; verified owner-checkpoint) |

**Application-layer gaps the audit surfaced (tracked separately, NOT closed by
this ADR):**

- **CRDT task deltas — EXPOSED (x0x #349).** The task-CRDT apply path merges on
  the payload's self-declared `peer_id`, never the envelope-verified
  `msg.sender` (`x0x/src/crdt/sync.rs:362`). Only checkbox claim/complete carry
  a verified `OpAttestation`; task creation/rename/reassign/reorder are
  authorship-unchecked. A group member can forge task-content ops attributed to
  another. This is an *apply-path* bug (it ignores the authenticated identity
  the envelope already provides), independent of this ADR.
- **Targeted capability request — EXPOSED, low impact.** No inner signature;
  worst case is triggering a peer to rebroadcast its already-signed advert
  (amplification, no capability granted).
- **Raw `Agent::publish` passthrough (`POST /publish`) — EXPOSED by design.** A
  generic unsigned byte transport; authentication is the caller's
  responsibility.
- **Soft spots (defense-in-depth):** `SecureShareDelivered` has no independent
  inner signature (relies on the V2 author binding + an admin-role check);
  group-card discovery accepts empty-signature cards for pre-D.3 legacy compat.

**Implication for this ADR's priority:** it is hardening, not an incident. It
still matters — it removes the requirement that every consumer reinvent x0x's
envelope — but it is not urgent, and x0x #349 is the higher-priority concrete
fix to land first.

## Decision Drivers

- Close the payload-forgery gap at the gossip layer so no consumer must depend
  on wrapping its own signature.
- Fixed cost independent of payload size (hot paths carry large CRDT/KV deltas).
- Mixed-version fleet: 0.37/0.38 x0x peers, saorsa-gossip 0.5.70 in the field —
  no flag-day.
- Preserve the existing header-signature machinery and the msg_id/dedup design.

## Considered Options

1. **Add a `payload_hash: [u8; 32]` field to `MessageHeader`, covered by the
   existing header signature; verify `blake3(payload) == header.payload_hash` on
   receive.** Version-gated (`version` 1→2). Fixed +32 bytes to the signed input
   regardless of payload size. The receiver check needs no epoch — it hashes the
   bytes it holds and compares. `msg_id` derivation is unchanged (it already
   folds `payload_hash`), so dedup is untouched.
2. **Sign `postcard(header) ‖ payload` wholesale.** Conceptually simplest, but
   the signed input grows with payload size, and the cache-replay re-sign cost
   (lib.rs:83-86 already budgets ML-DSA re-signing on replayed EAGER data)
   scales with payload — a real cost on the large-delta hot paths.
3. **Do nothing at the gossip layer; mandate inner signatures on every
   consumer.** Pushes a crypto obligation onto every current and future
   consumer; one forgotten surface is a silent hole. Rejected — it is the
   status quo that produced this gap.

## Decision

We will take **Option 1**: a signed `payload_hash` field in the header, checked
against the received payload.

- Bump `MessageHeader.version` to 2. Add `payload_hash: [u8; 32]`.
- **Send:** set `payload_hash = blake3(payload)` (empty hash for payload-less
  control frames like IHAVE); the existing header signature now covers it.
- **Receive (v2):** after the header signature verifies, require
  `blake3(payload.unwrap_or_default()) == header.payload_hash`; on mismatch,
  drop and record it against `(topic, sender)` via the existing
  `record_invalid_message` path (lib.rs:5660) — a payload swap becomes the same
  loud, scored signal a bad signature already is.
- **Mixed version:** a v2 receiver accepts v1 messages during the migration
  window per a documented policy knob (accept-and-warn → later reject), so
  0.5.70 peers keep interoperating until the fleet advances. v1 senders are
  unaffected. This mirrors x0x's own capability-gated v2 rollout style.

### The v1 sunset (accepted design)

The migration window is not open-ended; the flip from accept-v1 to reject-v1 is
**data-driven, not calendar-driven**:

- **Policy knob:** `gossip_signature_policy: accept_v1 | reject_v1`, shipped
  defaulting to `accept_v1` (accept-and-warn). A later release flips the default
  to `reject_v1`; operators can set it early. This is a config default flip, not
  a hard protocol-version gate, so the fleet is never partitioned by a release
  boundary.
- **Flip trigger:** the accept-and-warn arm emits a scored, per-`(topic, sender)`
  telemetry counter of v1/unsigned receipts (the same counter x0x #349's
  `unsigned_delta_accepted` warn established at the consumer). The default flips
  to `reject_v1` once that rate across the observed fleet is at/near zero — the
  fleet tells us when it is safe, rather than us guessing a date.
- **Scope carried from #349:** x0x #349 (CRDT content authorship) is closed
  **standalone** by its fail-closed Layer A on the signed path — it does **not**
  wait on this sunset. What this ADR's sunset owns is the *interop* consequence:
  once `reject_v1` flips, unsigned v1 peers stop interoperating at the gossip
  layer entirely (not just for CRDT content). The flip therefore requires either
  a fleet upgrade mandate or a consciously-scoped unsigned exception, decided
  from the warn-log data — an operational decision, not a security one.

## Consequences

### Positive

- Payload forgery by any forwarder is cryptographically detected at every
  receiver; no consumer needs its own inner signature for integrity.
- Cost is a fixed 32-byte hash in the signed input and one `blake3` over bytes
  already in hand — negligible vs. ML-DSA verify.
- Dedup, msg_id, and the zero-window guard are all unchanged.

### Negative / Trade-offs

- A wire-format version bump and a migration window with a temporary
  accept-v1 policy — mixed-version complexity until the fleet is on v2.
- Does not add replay/topic-rebind protection beyond what `msg_id` already
  gives (epoch is still sender-local and unverifiable); that is out of scope
  and noted for a possible follow-up ADR.

### Neutral / Operational

- Adds a scored invalid-message class (payload mismatch) to peer-scoring
  telemetry.
- Requires the exposure enumeration above to be closed before the change is
  scheduled, to set its priority (hardening vs. security fix).

## Validation

- Unit test: a message whose payload is swapped after signing fails receive with
  a payload-mismatch drop and a `record_invalid_message` increment — the test
  that would pass today (silent delivery) must fail without the check.
- Interop test: a v1 sender → v2 receiver is accepted under the migration policy
  and rejected once the policy flips.
- Fleet metric: payload-mismatch drops should be ~0 in a healthy network; a
  non-zero rate is either an attack or a version-skew bug.

## Notes for AI-assisted work

Drafted by Claude from a direct code trace (file:line citations above). The
consuming-surface exposure enumeration is complete (see "Audit outcomes" — the
COVERED/EXPOSED matrix is embedded in the body). Reviewed by omp (premise
correction and matrix independently re-derived at source) and accepted by David
2026-08-19. Implementation follows via saorsa-gossip; this Accepted ADR is now
immutable — future changes create a superseding ADR rather than editing it.
