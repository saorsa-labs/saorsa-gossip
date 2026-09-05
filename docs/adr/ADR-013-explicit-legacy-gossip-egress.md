# ADR-013: Explicit Legacy Gossip Egress During the Payload-Signature Migration

- **Status:** Proposed
- **Date:** 2026-09-05
- **Decision owners:** David Irvine (proposed; acceptance pending)
- **Reviewers:** saorsa-gossip and consuming-application maintainers (pending)
- **Supersedes:** If accepted, the mixed-version migration scope of [ADR-012](ADR-012-payload-covering-gossip-signature.md); its v2 signature and payload-verification requirements remain in force
- **Superseded by:** none
- **Related:** [saorsa-gossip #46](https://github.com/saorsa-labs/saorsa-gossip/issues/46), [x0x #517](https://github.com/saorsa-labs/x0x/issues/517), [ADR-001](ADR-001-protocol-layering.md), [ADR-002](ADR-002-post-quantum-cryptography.md), [ADR-008](ADR-008-stream-multiplexing.md)

## Context

ADR-012 adds a signed payload hash because a bare v1 gossip signature authenticates
the header but not the payload. Its mixed-version decision and validation cover a
v1 sender reaching a v2 receiver. They do not establish the reverse direction.
The current sender emits v2 to every recipient, including unchanged legacy peers
that cannot parse the added header field. A receive-side `AcceptV1` setting cannot
make those peers decode v2.

The x0x patch-release gate exposed this omission with an authentic v0.30.1 binary
and a current candidate using saorsa-gossip 0.5.75. Historical state failed to
converge in both owner/joiner arrangements; legacy receivers logged PubSub decode
errors. Later live traffic and unauthorized-write rejection succeeded in parts of
the gate. These observations do not identify the ordering or contents of each
failed runtime frame. Both application directions can depend on current-to-legacy
requests or replies; two failed scenarios do not imply symmetric wire failure.

### Verified evidence and its limits

An offline witness extracted the actual `MessageHeader` definitions and serde
implementations from published `saorsa-gossip-types` 0.5.66 and 0.5.75, and the
`GossipMessage` definitions from the matching pubsub versions. With a fresh
ephemeral ML-DSA-65 key, it tested EAGER, IHAVE, IWANT and AntiEntropy outer frames:

| Direction / control | Result |
| --- | --- |
| Old v1 to current decoder | 4/4 decode, signature verification, and byte-identical reserialization pass |
| Explicit current v1 to old decoder | 4/4 byte-identical serialization and signature controls pass; this is not the production send default |
| Current v2 to old decoder | 4/4 reject with `Hit the end of buffer, expected more data` |
| Current v2 self-roundtrip / changed signed header | 4/4 valid signatures pass / 4/4 tampered headers reject |

The witness uses an opaque 96-byte payload for every kind. It proves outer wire
and signature behavior, not valid IHAVE/IWANT/AntiEntropy payload semantics, a
complete legacy handler execution, or capture of the failing runtime frames.
Its v2 payload hash was checked, but production-path payload-tamper rejection is
an acceptance requirement below, not a result claimed from this witness.

The authentic x0x v0.30.1 public macOS arm64 binary has SHA-256
`5dcc909e06f14d70024f698601e6924c19304a0d28eac2d0db1b28da6910f7db`.
Its embedded source paths identify pubsub 0.5.66; its tagged manifest declares
gossip `0.5.66` compatible ranges. The candidate binary has SHA-256
`49ad3a00313426befe159b2a61c354709d9ecea331847620f4575c9fef30e357`
and embeds pubsub 0.5.75; its build lock resolves types/pubsub 0.5.75. Neither binary
yielded an embedded types-version path, so the old types version is source-based
provenance, not independently recovered from the executable. The witness exited
0; its result JSON SHA-256 is
`aa9041c6f2e0dd625ffd186bbad494ee335c65ab6b704e2767ec5abbd0e5d17b`.
The issue records link the investigation; an implementation PR must attach the
reproducible fixture and full dependency provenance rather than treating this
summary as a test artifact.

### Source constraints

These references are pinned to upstream commit
`6e17f02047017f07f8aa1d82052cdeb2409dfbf1` (0.5.75):

- [MessageHeader serde](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/types/src/lib.rs#L287)
  conditionally serializes the v2 tail and preserves the v1 layout. Parser-only
  changes on the current receiver do not solve current-to-old emission.
- [SignaturePolicy](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L250)
  controls receipt. [Local publication](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L6063)
  seals v2, then serializes once for the whole recipient set. There is no
  per-recipient wire-version policy in this send path.
- [Immediate EAGER forwarding](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L6433)
  preserves the received outer message. [IWANT cache replies](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L6644)
  and [anti-entropy cache replies](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L6765)
  instead sign cached headers with the relay's own key. A repair must cover both.
- [PubSub dispatch](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L7699)
  handles EAGER, IHAVE, IWANT and AntiEntropy. Membership, discovery and presence
  are not implicitly included in this migration.
- [EAGER admission](https://github.com/saorsa-labs/saorsa-gossip/blob/6e17f02047017f07f8aa1d82052cdeb2409dfbf1/crates/pubsub/src/lib.rs#L6249)
  currently deduplicates/caches before its storm-control topic validator. That
  existing hook cannot alone implement the pre-cache verification required here.

An unchanged v1 receiver cannot acquire ADR-012 payload verification through a
sender patch. x0x's independently signed V2 application envelope can preserve
payload, topic and author authentication across a legacy outer envelope, as
ADR-012's corrected consuming-surface audit explains. That does not establish
safety for bare `Agent::publish`, other applications, or unauthenticated control
payloads. Application authorization remains a separate obligation.

## Decision Drivers

- Restore explicitly supported mixed-version application paths without removing
  v2 protection from current traffic or waiving the authentic legacy gate.
- Keep ML-DSA signing and payload-hash verification; introduce no classical crypto
  fallback and no new claim of payload integrity at an unchanged v1 receiver.
- Make downgrade authorization explicit, bounded and auditable, independent of
  packet loss, malformed input, or unauthenticated capability absence.
- Preserve application authorship through immediate forwarding and cache replay,
  while keeping PubSub's limits and protocol layering.
- Give upstream maintainers a small, enforceable migration boundary rather than
  an unbounded generic compatibility switch.

## Considered Options

1. **Send v1 globally, or pin every consumer to pre-v2 gossip.** Rejected: removes
   accepted protection from current bare consumers and disguises the regression.
2. **Strip the v2 field and retain the original signature.** Rejected: changes the
   signed header. A relay cannot sign the modified header as the original author.
3. **Require a fleet-wide upgrade and remove the legacy gate.** Not selected:
   changes the supported migration contract. It requires a separate operational
   support decision; it is not evidence that compatibility has been repaired.
4. **Distribute origin-signed v1/v2 variants or introduce a dual-proof wire
   envelope.** Deferred: requires new origin, cache and forwarding semantics.
   An unchanged old relay may discard any modern extension. This does not remove
   the old receiver's intrinsic limitation and is larger than the chosen scope.
5. **Explicit per-peer, per-topic compatibility using independently verified
   application envelopes and relay re-enveloping.** Selected for the first
   migration implementation, with the restrictions below. No generic unsafe
   payload exception is part of this proposal.

## Decision

If accepted, implement option 5 as an opt-in migration facility. This document
does not authorize deployment or mark that facility implemented. It supersedes
only ADR-012's incomplete mixed-version scope: its v2 cryptographic requirements
and data-driven receive-policy sunset remain in force. Accepted ADR-012 remains
byte-for-byte unchanged.

### 1. Scope and defaults

New local publication and control generation continue to use v2 by default.
Existing v2 signature/hash verification and `SignaturePolicy::RejectV1` remain
mandatory; a migration grant cannot override `RejectV1`. Existing `AcceptV1`
behavior outside a registered migration topic is not broadened by this change.
Configuration must reject a request for bidirectional legacy service while
`RejectV1` is selected, rather than silently changing the receive policy or
claiming that an egress-only grant makes that node interoperable.

The initial consumer is x0x's signed KV delta/state-sync topics. Each concrete
topic must register an inner-envelope verifier and the exact legacy receiver
version whose verify-before-apply behavior has been audited. An implementation
must enumerate these topic families and production receive/apply paths. Other
topics, raw publishing, and other consumers are excluded until separately
reviewed and explicitly registered; the historical ADR-012 audit is not a blanket
authorization.

The registration is a verifier interface, not a `payload_is_safe` boolean. On
EAGER ingress, local publish, and transit conversion it must validate the complete
unchanged inner envelope, its signature, topic binding and author/key binding.
It returns verified metadata bound to those exact bytes. Unknown keys, unsupported
inner versions and unverifiable payloads fail closed before PubSub cache/seen
mutation or forwarding. The consumer still performs its normal membership,
ownership, checkpoint, replay and write-authorization checks before applying data;
the adapter cannot replace them or turn an authenticated author into an authorized
writer. Verification work must have explicit resource bounds.

### 2. Explicit peer permission, not error-driven negotiation

The first implementation uses a trusted local operator/application roster. A
legacy permission names the authenticated adjacent transport `PeerId`, exact
registered topic, verifier policy/version, issuer and reason, finite expiry,
and policy revision. Addresses, peer display names, received v1 frames, timeouts,
EOF errors and absent adverts never create a permission. A signed legacy message
alone does not prove its sender cannot speak v2.

Permissions are issued for a process lifetime and bound to an authenticated
session after the transport identity check. A reconnect requires a fresh session
binding from the still-valid roster; key rotation never inherits the old grant.
A process restart requires explicit trusted roster reissuance, not automatic
replay of cached grants. Enforce expiry using a monotonic lifetime within the
process, capped by the issuer's remaining wall-clock validity. If the clock or
policy state cannot establish validity, deny the grant. Expired/revoked revisions
cannot be resurrected by clock rollback or queued work.

Persist a monotonic `V2Required` floor for each peer identity that trusted policy
has established as modern. A v2 frame relayed from another signer is not proof
of the adjacent peer's negotiated capability. A floor/grant conflict denies
legacy egress. Missing initialization, corrupt or unreadable floor storage
disables legacy grants; normal v2 service remains available. A normal restart,
reconnect, roster replay or software downgrade never clears the floor. This
implementation provides no runtime floor-reset or demotion API: any operational
identity reset needs a separately reviewed recovery procedure.

Future automatic capability exchange is out of scope. It would need authenticated
peer binding, a fresh session challenge, replay protection and a bootstrap channel
independent of the incompatible gossip frame. Old peers unable to participate
still require explicit policy; no invented legacy acknowledgement is sufficient.

### 3. Wire selection and signer provenance

Keep three identities distinct: adjacent authenticated transport peer, signer of
the current outer envelope, and independently verified inner application author.
Neither a relay key nor a preserved `msg_id` is proof of original authorship.

On a registered migration topic, all outgoing paths use one destination policy:

| Destination / content | Required behavior |
| --- | --- |
| No valid legacy grant, including unknown or known-modern peer | Emit v2; never retry as v1 on failure |
| Valid legacy grant; verified inner EAGER content | Emit the exact v1 layout with a valid signature over that v1 header |
| Valid legacy grant; permitted local PubSub control | Emit a locally signed v1 control within section 4 limits |
| EAGER content lacking valid registered inner proof | Reject migration admission/conversion; do not cache or forward it through the migration facility |

For a locally originated message, sign the required variants with the local key.
For a transit message already in the destination's format, preserve a valid
original outer signature where available. If a format change or cache replay
requires signing, use the relay's own key and expose it as the outer signer. Never
retain a mismatched signature or imply that the original signer approved the new
header. Preserve the exact inner envelope, topic and logical `msg_id`; retain
existing hop/TTL semantics without resetting the message's propagation budget.

In particular, old-origin v1 content forwarded by a modern relay to another
modern peer must first pass the registered inner verifier, then be sealed and
signed as outer v2 by that relay. Forwarding the original v1 unchanged would
violate the modern adjacency floor. Its new v2 hash proves the bytes accepted by
that relay; original end-to-end authorship comes from the inner proof, not the
new outer signature. No such conversion is authorized for a bare generic payload.

Cover local EAGER, immediate EAGER forwarding, cached IWANT and AntiEntropy EAGER
serves, IHAVE, IWANT, AntiEntropy generation, scheduled retries and retransmits.
Select recipients before serialization, with at most one serialized copy per
needed version for a given signed message. Preserve concurrency, timeout,
backpressure, size and byte-cache limits, accounting for additional signatures
and variants. Revalidate the grant/floor at actual send admission; revocation or
expiry cancels unsent queued v1 work and cached serialized variants. It cannot
recall bytes already admitted to the transport.

### 4. Receive, control and cache boundary

On registered topics, enforce adjacent-peer floors and receive policy before
seen/cache promotion, and verify inner EAGER content before deduplication can
consume its `msg_id`. An invalid inner copy arriving first must not suppress a
later valid copy. Retain proof metadata with cached bytes; policy/verifier
changes require revalidation before reuse. A trusted modern adjacency requires
outer v2 even if the inner author is legacy. Direct legacy ingress additionally
requires a valid topic/session grant and `AcceptV1`. A current peer therefore
cannot launder a weak bare payload into modern trust merely by re-signing it.

This does not authenticate a v1 header's `msg_id` against the inner bytes. An
attacker can substitute a different, valid same-topic inner envelope under a
captured v1 header. Inner verification prevents application forgery but cannot
prove that this envelope belongs to that outer identifier; later relay sealing
does not repair the missing origin binding. Preserve existing logical IDs for
legacy IHAVE/IWANT interoperability, and explicitly retain valid-inner replay and
`msg_id` alias suppression as legacy availability risks. No Byzantine convergence
guarantee is added. Removing this residual risk requires a separate authenticated
content-identity and recovery design; using a local payload-hash cache alone
would not repair legacy control messages keyed by `msg_id`.

Legacy PubSub control is restricted to IHAVE, IWANT and AntiEntropy for that
permitted topic and adjacent session; EAGER carries verified application data.
No implicit permission extends to Ping, Ack, Find, Presence or Shuffle. Local
control messages are signed by their emitting node. Incoming control must pass
the existing signature check and a signer-to-authenticated-adjacent-peer check,
then strict payload shape, message-ID count, byte, rate and outstanding-work
limits before allocation or cache serves. Control is not transit-forwarded as an
authoritative statement by a different peer. The implementation must demonstrate
these constraints against the authentic legacy handlers; an incompatible check
is an explicit design failure, not a reason to disable authentication.

Inner application signatures do not protect IHAVE/IWANT/AntiEntropy payloads.
The old endpoint still has v1's weaker control-payload guarantee, and a permitted
malicious peer can cause bounded extra recovery work or withhold data. This
proposal accepts that migration availability risk only within the scoped
boundary; it cannot promise bare-gossip end-to-end payload protection through
an unchanged old hop. There is no new global claim that every received message
has v2 integrity while legacy receipt remains enabled.

### 5. Rollout and sunset

Ship the facility disabled. Enable only reviewed application adapters and explicit
legacy identities after the acceptance matrix passes. Roster issuance must be
available to an operator or trusted consumer without requiring the old peer to
decode v2; a failed join must not enable it automatically.

Keep bounded counters for legacy egress/ingress by kind and policy reason,
denied downgrade, expired/revoked grant, invalid inner proof and v2 payload
mismatch. Distinguish origin publication from relay/cache traffic. Use bounded
peer/topic diagnostic storage and aggregate fleet metrics; never log payloads,
private keys or unbounded peer-label cardinality.

Use ADR-012's measured v1 traffic for the sunset, augmented with grant inventory
and offline supported-peer inventory: silence from a disconnected legacy peer
is not evidence that it upgraded. Removing legacy grants and flipping the default
receive policy require human fleet/support review. This proposal does not choose
an unsupported calendar cut-off or permit permanent unreviewed exceptions.

## Consequences

### Positive

- Current publication retains ADR-012's payload binding; an old receiver can
  participate in explicitly audited application flows without a flag-day.
- A transport failure cannot negotiate away security. Peer permissions and
  original application authorship remain inspectable across relay/cache paths.
- The chosen boundary can be tested independently of x0x's separate KV merge and
  pruning defect; fixing one is not evidence that the other is resolved.

### Negative / Trade-offs

- Requires upstream egress, cache and ingress work plus a consumer verifier and
  durable policy integration. It is not a one-line call-site flag or a proven
  compatible patch today.
- Variant signing, proof validation and floor persistence add cost and state.
  Explicit grants impose onboarding/restart work for legacy deployments.
- An unchanged old endpoint retains legacy control and outer-payload limitations.
  Bare consumers and unaudited topics do not gain compatibility from this policy.
- A valid inner envelope substituted under another v1 `msg_id` can suppress
  recovery without forging that envelope. Application authorization still applies;
  legacy routes retain this availability limitation even after re-enveloping.
- Expiry, unavailable policy storage or an unaudited old handler may stop recovery
  on the affected legacy path. Fail-closed behavior is deliberate and observable.

### Neutral / Operational

- ADR-001 dissemination and ADR-008 transport boundaries remain; no new central
  broker, transport channel fallback or membership wire migration is introduced.
- ADR-002's post-quantum signing choice and ADR-012's v2 verification remain.
- Human acceptance must confirm this initial topic/version scope, the legacy
  control/replay/outer-ID availability risks and ownership of grant issuance and
  sunset review. Bounded resource use does not bound adversarial convergence delay.
  Maintainers must name the operational owners before enablement; an AI-authored
  Proposed document is not that acceptance. Numeric resource limits and adapter
  API shape belong in the reviewed implementation with measured evidence, not
  guessed values in this ADR.

## Validation

An implementation is acceptable only with the following evidence on the exact
dependency tree and artifact proposed for release:

1. **Exact signed wire fixtures.** Preserve the 0.5.66/0.5.75 directional witness
   and add real valid control payloads, absent/empty payloads and size limits.
   Both parsers and signature verifiers must accept the intended v1 variant.
   Current v2 production verification must reject changed payload/hash/header,
   invalid signatures and unsupported layouts. Do not count an opaque outer
   fixture as successful control-handler interoperation.
2. **No downgrade by observation.** Unknown identity, missing advert, EOF, timeout,
   forged/replayed capability, reconnect and key rotation never grant v1 egress.
   Modern recipients receive v2 when sharing fanout with approved legacy peers.
   Persisted floors survive restart; corrupt/unavailable state and floor conflicts
   deny legacy. Expiry, clock rollback, revision replay and revoke-after-queue
   prevent unsent stale grants from being used. `RejectV1` still rejects v1.
3. **Full production send matrix.** Exercise local/relay EAGER, IWANT serves,
   AntiEntropy serves and all three control kinds, including retry paths. Test
   modern origin -> modern relay -> old receiver; old origin -> modern relay ->
   modern receiver; and modern origin -> old relay -> modern receiver. Record
   transport peer, outer signer and verified inner author at each hop, without
   retaining private keys. Include direct two-node traffic in both directions.
4. **Inner proof before trust.** Use the real x0x V2 envelope and receive/apply
   paths. Altered payload, topic and author bindings fail before seen/cache
   mutation. Send a v1 copy with invalid inner proof first under the valid
   message's `msg_id`, then show the valid copy still delivers. Test reordered
   v1/v2 duplicates, cache
   replay, policy changes, missing keys and invalid writers. Re-enveloping never
   substitutes relay identity for the application author or bypasses KV ownership.
   Bare/raw publishes and unregistered topics cannot inherit compatibility.
   Separately substitute a different valid inner envelope under a captured v1
   `msg_id`: demonstrate the residual replay/suppression behavior, prove that no
   forged or unauthorized state applies, and bound its resource effects. Do not
   report successful inner verification as proof of outer-ID authenticity.
5. **Bounded adversarial control and efficiency.** Reject malformed/oversized
   lists, wrong control signer and off-topic requests; bound recovery amplification,
   queue work and verification concurrency. Exercise revoked/expired cached
   variants. Measure signature count, serialization count, byte-cache use, peak
   memory, current-only throughput and mixed recovery latency against baseline.
   Retain existing limits; explain any measured regression before acceptance.
6. **Authentic release gates.** Run both existing x0x v0.30.1 mixed-version phases
   with original historical/live/unauthorized-write predicates and deadlines, then
   the full required ten-run convergence recipe. No unsupported-phase waiver,
   new deadline or retry-until-green substitutes for a passing gate. Retain failed
   artifacts and capture enough fixture-scoped frame/handler evidence to attribute
   recovery without reading user payloads. Record binary hashes and resolved
   dependency versions, not only product version labels. The independent KV fix
   must pass its own regressions as well.

## Notes for AI-assisted work

Prepared from pinned source inspection and an offline signed-wire witness.
Independent agent review informs the draft; it is not human engineering approval.
No fallback, release, deployment or acceptance is performed by this document.
AI tools must not mark it Accepted without human review. Accepted ADRs remain
immutable; future changes create a superseding ADR rather than editing ADR-012.
