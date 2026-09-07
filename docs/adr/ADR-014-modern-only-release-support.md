# ADR-014: Modern-Only Release Support and RejectV1 Default

- **Status:** Accepted
- **Date:** 2026-09-07
- **Acceptance:** David Accept locked 2026-09-07 via Jarvis (r2 tip sha256 bf29f4dc…)
- **Revision:** r2 — name modern-only receipt template; index/Supersedes graph only
- **Decision owners:** David Irvine
- **Reviewers:** Senior Engineer (CLEAN); David Irvine (Accept)
- **Supersedes:** The *release-support and authentic-stock validation scope* of [ADR-013](ADR-013-explicit-legacy-gossip-egress.md) — specifically Considered Options §3 (fleet-wide upgrade "not selected"), Decision / Scope language that keeps unmodified stock as a required supported endpoint, Rollout / Operational support-ownership clauses that assume ongoing stock support, and Validation §6 authentic v0.30.1 mixed-version + unchanged `convergence-release` stock hard-require. **Does not supersede** ADR-013's disabled-by-default migration facility design, grant/session rules, verifier-before-cache obligations, or RejectV1-overrides-grants rule.
- **Superseded by:** none
- **Related:** [x0x #517](https://github.com/saorsa-labs/x0x/issues/517), GitHub Actions run `34058040463`, [ADR-012](ADR-012-payload-covering-gossip-signature.md), [ADR-013](ADR-013-explicit-legacy-gossip-egress.md), Proposed x0x ADR-0063 (patched V3 pairing — **separate**, not this decision), `release517-support-decision-brief.md` (option 2), `release517-modern-policy-implementation-plan.md`

## Context

ADR-012 established payload-covering outer v2 gossip signatures and a data-driven
`AcceptV1` → `RejectV1` sunset (fleet upgrade mandate or scoped exception). ADR-013
Accepted the *migration facility* boundary (disabled-by-default grants, exact-topic
verifiers, no error-driven downgrade) while still requiring **authentic unmodified
x0x v0.30.1** mixed-version phases and the stock-hard `just convergence-release`
recipe as the release predicate. Considered Option 3 (fleet-wide upgrade / remove
legacy gate) was explicitly **not selected** there — it needed a separate operational
support decision.

That support decision is now forced. Run `34058040463` and issue #517 show both
authentic-v0.30.1 mixed-version historical KV recovery phases fail against current
outer-v2 production publication. Offline codec probe: published pubsub 0.5.76
outer-v2 fails the 0.5.66 decoder; old-v1 controls still decode. Current-to-legacy
is not repaired by the disabled migration facility, and repairing it is a different
product (audited V3 pairing / enablement), not a silent green of #517.

David locked **option 2 — modern-only support** and **Accepted** this ADR
(2026-09-07 via Jarvis). It does **not** edit Accepted ADR-012 or ADR-013
text (TOOLING.md: Accepted ADRs are immutable; supersession is index-only).

## Decision Drivers

- Stop treating unmodified stock v0.30.1 as a release-blocking supported endpoint when
  production current publication is outer-v2 and stock cannot decode it.
- Keep outer-v2 cryptographic protection (ADR-012) byte-identical; no blanket v1
  downgrade, no classical crypto fallback, no timeout padding.
- Keep the ADR-013 migration facility **disabled**; no product path restores
  `AcceptV1` or issues grants under the modern supported profile.
- Make `RejectV1` the reviewed **default** for modern supported instances (ADR-012
  sunset / ADR-013 RejectV1-overrides-grants), with operational proof in diagnostics.
- Preserve failed authentic-stock evidence under original labels; do not relabel
  #517 or run `34058040463` as PASS.
- Leave patched V3 pairing (`x0x/0.30.1+signed-kv-inner-v3;saorsa-gossip-pubsub/0.5.66`
  and Proposed x0x ADR-0063) as **future / separate** acceptance and human enablement.

## Considered Options

1. **Keep release held for stock support (brief option 1).** Rejected for this
   decision: leaves every modern release blocked on an endpoint current v2 cannot
   satisfy without a separate repair that does not exist as stock-compatible today.
2. **Modern-only support (brief option 2).** **Selected.** Retire unmodified
   v0.30.1 from the release support contract; require upgrade to the named modern
   supported version; `RejectV1` reviewed default; migration facility stays disabled.
3. **Audited V3 pair as a prerequisite (brief option 3).** Deferred. Valid future
   compatibility track with its own profile, matrices, `AcceptV1`/grant enablement
   decision, and human G8. Not required to supersede stock as a modern release gate.

## Decision

We will adopt **modern-only release support**:

1. **Stock retired from the modern release contract.** Unmodified x0x v0.30.1 is
   no longer a required supported peer for declaring a modern x0x / saorsa-gossip
   release green. Operators with stock installs must **upgrade** to the **named
   modern supported version** recorded at acceptance / release notes (product tip
   and dependency pins), not remain on stock under a compatibility promise.

2. **`RejectV1` is the reviewed default** for the modern supported profile.
   Consuming applications (notably x0x `PubSubManager` construction) SHALL install
   `SignaturePolicy::RejectV1` after `PlumtreePubSub` construction and fail closed
   if the policy cannot be read back. Library crates may still ship construction
   defaults of `AcceptV1` for transitional builds; the **supported modern product
   instance** must not.

3. **Migration facility stays disabled.** No CLI, environment, or config path may
   restore `AcceptV1` on the modern supported profile. No migration topic
   registration, floor store, session grant, or audited legacy receiver is enabled
   by this ADR. ADR-013's facility design remains available for a **future**
   explicit enablement decision; this ADR does not enable it.

4. **Historical failures stay failures.** #517 and run `34058040463` (and their
   retained receipts / ZIP custody) remain **failed compatibility evidence**. This
   decision does not repair, waive, or green them.

5. **Release harness.** Replace the stock `just convergence-release` hard-require
   of authentic v0.30.1 (`X0XD_LEGACY_BINARY` must be set) with the named
   **modern-only convergence receipt** below. Do **not** silently skip the recipe
   or call an incomplete run green.

   **Named receipt template:**
   `docs/release/modern-only-convergence-receipt.template.md`
   (consuming x0x tree; mirror path acceptable under `review-artifacts/` until
   seated). Each modern release candidate fills one dated receipt instance
   (`docs/release/receipts/modern-only-YYYYMMDD-<tip>.md` or equivalent custody
   path) that MUST record:

   | Field | Requirement |
   | --- | --- |
   | `adr` | This ADR id + Proposed/Accepted status at fill time |
   | `named_modern_version` | Product version / tip / tree SHA being gated |
   | `deps` | Resolved `saorsa-gossip-pubsub` (and related) versions + archive checksums |
   | `outer_signature_policy` | Must be `reject_v1` on the candidate instance |
   | `legacy_grants` | Must be `disabled` |
   | `stock_v0_30_1_phases` | Explicitly `not_in_modern_predicate` (not PASS) |
   | `failed_compat_evidence` | Cite #517 and run `34058040463` as FAIL if referenced |
   | `ordered_rust_gates` | fmt / clippy `-D warnings` / check — result |
   | `full_suite` | no-fail-fast suite — result |
   | `ci` | green CI — result / run ids |
   | `convergence` | 10/10 clean **non-stock** `--expect-fixed` phases only — result |
   | `build_sign_package_audit` | release build / sign / package / audit — result |
   | `signer` | Human release owner attestation |

   Retained modern gates: outer-v2 verification, `RejectV1`, no legacy grants,
   normal KV authorization, exact dependency/binary custody, ordered Rust gates,
   green CI, full no-fail-fast suite, release build/sign/package/audit, and all
   **non-stock** `--expect-fixed` convergence phases for 10/10 clean runs.

6. **Preserve the stock/legacy matrix for future enablement.** The authentic-stock
   mixed-version phases and predicates remain documented and runnable as the gate
   for any future legacy-compatibility enablement (including a patched V3 pair).
   They are removed only from the **modern release** predicate.

7. **ADR-012 immutable.** Outer-v2 payload hash/signature requirements and the
   measured receive-policy sunset design stay in force and byte-identical.
   This ADR is the operational selection of ADR-012's "fleet upgrade mandate"
   route (ADR-012 sunset notes), not an edit of ADR-012.

8. **V3 pairing stays separate.** Proposed x0x ADR-0063 / G1–G8 / profile
   `x0x/0.30.1+signed-kv-inner-v3;saorsa-gossip-pubsub/0.5.66` remain future work.
   They must not toggle this modern supported instance back to `AcceptV1`.

### Out of scope (do not implement under this ADR)

- Product RejectV1 / diagnostics code (Developer, separate PR).
- Editing Accepted ADR-012 or ADR-013 files (including headers). Supersession
  is index + this ADR's `Supersedes:` field only.
- Enabling grants, AcceptV1 restore, or stock-as-supported.
- Undrafting x0x #515, tagging, or release without the modern-only receipt.
- x0x #535 / #539 / #543 and unrelated tracks.

## Consequences

### Positive

- Modern releases are no longer blocked on an impossible stock decode of outer-v2.
- Default `RejectV1` matches ADR-012's intended sunset and ADR-013's
  RejectV1-overrides-grants rule on the supported profile.
- Clear operator message: upgrade; do not expect unmodified v0.30.1 to speak
  current gossip publication.
- Failed stock evidence preserved for audit and for any future enablement design.

### Negative / Trade-offs

- Unmodified stock peers are unsupported on the modern release contract; offline
  or un-upgraded installs will not interoperate with current publication.
- Concentrates urgency on upgrade tooling and release communication.
- Does not by itself fix codec asymmetry for anyone who later wants stock or V3
  pairing — that remains a separate acceptance.

### Neutral / Operational

- Diagnostics SHOULD expose `outer_signature_policy: "reject_v1"` and cumulative
  `outer_v1_receipts` (rejected-frame receipts; not acceptance).
- Ownership: release/support owners name the **named modern version** per release;
  grant/sunset owners remain named before any future facility enablement (unchanged
  from ADR-013 operational requirements for enablement, not for this modern profile).
- **Supersession graph (Senior preference, locked):** do
  **not** edit Accepted ADR-013 (header or body). Record supersession only via
  this ADR's `Supersedes:` field and the saorsa-gossip `docs/adr/README.md` (or
  equivalent index) entry that links ADR-013 → this ADR for the retired *stock
  release-support / Validation §6 / Options §3* scope.

## Validation

Seating / acceptance of this decision requires:

1. **Human engineering review** completed (Senior CLEAN + David Accept 2026-09-07).
   AI must not invent further Accepted flips without a superseding ADR.
2. **No Accepted ADR body edits** for 012/013 in the seating PR.
3. **Named receipt template** `docs/release/modern-only-convergence-receipt.template.md`
   must exist in the consuming x0x tree. No modern candidate may claim the
   release gate without a filled receipt instance that marks stock v0.30.1
   phases `not_in_modern_predicate` and references this ADR.
4. **Product proof (Developer, separate PR — not this seating):** every x0x
   `PubSubManager` constructor path reports `RejectV1`; valid outer V1 EAGER is
   not delivered and increments V1 receipts; valid outer V2 still delivers;
   production-style publish remains v2; no AcceptV1 restore path; diagnostics
   contract as in `release517-modern-policy-implementation-plan.md`.
5. **Evidence custody:** #517 and run `34058040463` remain cited as FAIL under
   original labels in the receipt / release notes if referenced.
6. **Non-goals check:** no grant registration, no V3 enablement, no stock green.

## Notes for AI-assisted work

Drafted by Architect; Senior CLEAN; David Accept 2026-09-07. Supersession via
index/`Supersedes` only (no ADR-013 edits). This seating is docs-only. Developer
implements RejectV1 + diagnostics in a separate product PR. Do not green #517,
undraft releases without the modern-only receipt, or edit Accepted ADR-012/013.
