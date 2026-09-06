# ADR-013 Signed KV compatibility implementation

This is a disabled-by-default library facility for Senior review, implementing
[ADR-013](../adr/ADR-013-explicit-legacy-gossip-egress.md). It does not enable a
fleet, bump x0x, or establish release acceptance. Tracking: [gossip #46](https://github.com/saorsa-labs/saorsa-gossip/issues/46)
and [x0x #517](https://github.com/saorsa-labs/x0x/issues/517).

## Audited receiver and topic scope

The historical receive/apply audit used x0x v0.30.1, annotated tag object
`506b064101bb105617d75117b5fcace47e34accc`, resolving to commit
`e0c25098af781028093abdc8cbbb37e152217266`, with pubsub/types 0.5.66.
That stock application cannot consume the new inner V3 layout. A paired x0x
signer/decoder change and receive/apply audit are required before enablement.
Only these Signed KV families may be explicitly registered:

| Family | Exact registration | Audited receive/apply path at that x0x commit |
| --- | --- | --- |
| Delta/full-state response | Store's concrete topic string | `src/gossip/pubsub.rs::decode_for_delivery` (743), `decode_v2` (1055), `verify_signature` (1146); then `src/kv/sync.rs::start_with_spawner` (107), `decode_delta`, and `src/kv/store.rs::merge_delta` (373) |
| State request | `<same concrete topic>/state-sync` | Same verified delivery path, then `src/kv/sync.rs` state-request responder (138–171); full-state response is published on the base delta topic |

`decode_for_delivery` discards failed signed-envelope verification before the
subscriber receives data. Its legacy signature covers `x0x-msg-v2 || AgentId || topic ||
payload` with an ambiguous topic/payload boundary; this facility rejects it.
The key must derive the claimed AgentId. The Signed store's merge path
checks the verified writer against its anchored owner and rejects anonymous and
unauthorized writers. State requests cause bounded recovery work, not an ownership
change. The new PubSub verifier is independent of that application authorization.
It never interprets a relay identity as a writer or grants ownership.

`SignedKvTopic::new` requires one exact topic, a positive verifier revision and a
bounded roster of known application authors. It verifies the complete inner V3
wire layout, ML-DSA-65 signature, key/author derivation, cryptographic topic
binding, exact topic equality and byte limit. The returned `VerifiedInner` binds the full envelope hash, author and
verifier revision. Local publication and EAGER admission run this verifier before
cache/seen mutation. Cache entries retain the metadata, and every cache serve and
transit conversion revalidates the actual bytes against current policy.

The trusted caller must establish that this is a Signed KV store using the
reviewed handlers before registering it. This API is not a wire-level capability
claim. No topics, authors, receiver versions or grants are inferred from network
traffic. Bare publishing remains outer v2. The fixed adapter admits no unsigned x0x V1,
ambiguous signed V2, other inner versions, unknown authors, raw payload exemption or arbitrary callback
that merely asserts a payload is safe.

## H1 wire fix: canonical inner V3 and required x0x pairing

H1 is closed in the gossip verifier by accepting **only inner version 3** with
this canonical ML-DSA-65 signing preimage (lengths count bytes, not characters):

```text
"x0x-msg-v3" || author[32] || topic_len:u16be || topic[topic_len] || payload[remaining]
```

The complete inner wire layout is:

```text
0x03 || author[32] || key_len:u16be || key[key_len]
     || signature_len:u16be || signature[signature_len]
     || topic_len:u16be || topic[topic_len] || payload[remaining]
```

The key and signature lengths must be exactly 1952 and 3309. The fixed domain
separates V3 signatures from V2; the signed topic length fixes the boundary even
for `T` and `T/state-sync`. Payload occupies the remainder, so it needs no second
length. Any future trailing field requires a new canonical layout that also
length-prefixes the payload. Both the verifier and fixture signer include the topic length. There is
no V2 fallback or dual-accept path, including for a valid known author's signature.
Changing a V2 envelope's version byte to 3 does not make its signature valid.
Outer gossip versions 1 and 2, their signatures and legacy predicates are unchanged.

**Pairing requirement, not deployed compatibility:** stock x0x v0.30.1 signs and
verifies the ambiguous V2 preimage and will not consume this layout. Its
`src/gossip/pubsub.rs::build_signing_payload` is shared by publication and
`verify_signature`; both need V3 domain/length encoding, plus a V3 encoder,
strict decoder and version dispatch in `decode_auto`/`decode_for_delivery`.
Reserve inner version `0x03` for this layout in the paired x0x implementation.
The paired receive/apply path must retain key/author, Signed ownership and
state-request checks. This patch does not change x0x or certify that pairing.
Previously cached V2 data must be re-signed by its author; a relay cannot relabel
or translate its inner signature. The facility stays disabled pending that work.

`AUDITED_RECEIVER` now requires the profile
`x0x/0.30.1+signed-kv-inner-v3;saorsa-gossip-pubsub/0.5.66`. This is an explicit
operator assertion of the required patched profile, **not an existing release or
a completed audit**. The old stock receiver string is rejected at grant issuance.
Do not issue grants until the paired implementation and authentic receive/apply
tests have been reviewed. Advance existing verifier revisions and issue fresh
session-bound grants when integrating the pairing; no capability is inferred.

`inner_v3_rejects_topic_boundary_rewrites` changes only the topic length in both
directions, for the delta/state-sync pair and an arbitrary UTF-8 prefix pair.
Original messages verify; every rewritten boundary fails signature verification.
Correctly signed payloads starting with the suffix still verify.
`inner_v3_rejects_legacy_signatures_and_version_relabeling` rejects original V2,
V2 relabeled as V3 and unsupported versions. The receiver-profile regression
rejects grants naming the stock V2 receiver. H1 is not an accepted residual.
The PR stays draft; coordinated x0x integration and release gates remain open.

## Policy and transport integration

1. Explicitly initialize `ModernFloors` once on trusted durable storage. On normal
   restarts call `open`, not `initialize`. A missing/invalid/partial/rolled-back
   journal disables legacy grants; normal v2 service continues. Floor additions
   are checksummed append-only records, fsynced before success. Runtime demotion
   and floor reset are absent. Protect the journal against external replacement.
   Admission checks file mtime and length and reuses the decoded floor set when
   unchanged. Changed journals are read once with metadata checks before and after
   decoding; corrupt, missing, changing or in-process rolled-back journals fail
   closed. This is a trusted-storage cache, not protection against an attacker
   restoring both bytes and timestamps. A valid record-aligned rollback while the
   process is stopped cannot be detected by this journal format. Absent, expired
   or session-mismatched grants are rejected before filesystem access; encountered
   expired grants are removed while their revision tombstones remain.
2. Register each exact Signed delta and state-sync topic separately. Replacing a
   verifier requires a higher revision and invalidates its grants/variant cache.
3. The operator/application issues a `LegacyGrant` with adjacent authenticated
   peer, exact topic, exact verifier revision, fixed audited receiver version,
   issuer, reason, increasing policy revision and finite expiry (maximum 24 hours).
   It binds to the transport's current `AuthenticatedSession`. Reconnects require
   an explicit fresh grant with a higher revision. Restarts have no grant replay.
4. `RejectV1` rejects bidirectional grant issuance and clears live grants. Expiry
   uses both a monotonic deadline and wall validity; observed wall rollback clears
   all grants. Revocation tombstones prevent same-process revision replay.
5. The consumer carries the authenticated session token **from the actual receive
   connection** into `handle_authenticated_message`. Looking up the current token
   after dequeuing an old frame is not sufficient. The entry point checks that
   the token is still current. The original `handle_message` entry point denies
   registered legacy ingress and registered controls lacking session provenance.

**Consumer integration prerequisite:** ant-quic 0.27.48 `Node::recv` and the
existing `GossipTransport::receive_message` return `(PeerId, bytes)` without the
receive connection generation. This patch does not fabricate a generation for
that API. Consumers using it must add authenticated receive-session propagation
before enabling this facility. x0x wiring and its dependency bump remain a later
phase. The current API surface safely exposes this prerequisite instead of
allowing stale queued frames to inherit a new connection's grant.

The UDP adapter implements guarded **egress** using a pinned authenticated QUIC
connection. It retains the previous connection while assigning monotonic session
generations so a reused connection pointer cannot resurrect a grant. The bounded
session registry refreshes recency on lookup and evicts the least recently used
identity at capacity, pruning closed connections on insertion. An evicted identity
gets a fresh generation even if its QUIC connection is still live, so it requires
a fresh grant. Closing the adapter clears retained connections. It preserves
existing send semaphore, per-peer queue and timeout limits. After stream allocation
and all waits, it rechecks session and invokes the policy admission callback. It
never reconnects/retries already-selected v1 bytes on another session. Bytes for
which admission has completed are the transport boundary; later revocation cannot
recall them. Other transport implementations default to denying guarded sends.

## Forwarding, control and cache behavior

An internal transport wrapper covers every `PlumtreePubSub` egress call, including
local EAGER, detached EAGER fanout, IWANT and AntiEntropy cache serves, direct
IHAVE/IWANT/AntiEntropy, scheduled IHAVE flushes and scheduled AntiEntropy sends.
It retains the existing concurrency, cooling, admission and timeout machinery.

Unknown/modern destinations receive v2. Registered v1 transit is sealed by the
relay for those destinations. Valid granted destinations receive the exact v1
layout. Format conversion re-signs as the relay, retaining unchanged inner bytes,
topic, logical message ID, hop and TTL. Existing same-format EAGER signatures are
preserved. Locally generated controls require the signing key to match the authenticated
transport identity and are signed locally; control is not accepted
as a relayed statement from another signer. Serialized conversions are cached
with a byte cap, independently of grants; grants are rechecked at actual guarded
send admission, including cache hits. At most one conversion/signature per input
wire digest/version is retained while that bounded entry is resident.

Registered ingress permits only EAGER, IHAVE, IWANT and AntiEntropy. Controls need
an authenticated adjacent session and its own matching outer signer. Payloads
undergo strict enum/count/length checks before vector deserialization and work
admission. No control allowance extends to Ping/Ack/Find/Presence/Shuffle. V2
payload verification remains mandatory, and unsupported outer layouts fail.

| Bound | Implementation limit |
| --- | --- |
| Registered concrete topics | 128 |
| Known authors per verifier | 1,024 |
| Grant/revocation peer-topic keys per process | 1,024; tombstones not evicted |
| Inner envelope | 1 MiB; fixed ML-DSA-65 key/signature lengths |
| Control list | 1,024 IDs; exact postcard shape; at most 32,771 payload bytes |
| Control work | 4,096 IDs/nonempty-work units per second globally |
| Outstanding IHAVE-triggered IWANTs | 1,024 per registered topic |
| AntiEntropy reply batch | 32 cached messages per request |
| Cached response bytes | 16 MiB/second globally on registered topics, including crypto/header accounting |
| Converted serialized variants | 4 MiB globally, separate from existing per-topic cache |
| Floor journal | 65,536 identities; bounded reads; malformed records fail closed |
| UDP session identities | Configured `max_peers` resident entries with LRU eviction; reconnect/re-admission generations increase |

EAGER verifies the outer signature and checks the seen set before inner ML-DSA
verification, then verifies the inner envelope before any new seen/cache entry.
Verification under the policy mutex bounds concurrent inner verification to one
per PubSub policy object. These conservative numeric limits and the cost below
need Senior assessment for the consuming workload. No deadline is increased.
Existing origin/relay and outbound-kind counters remain; migration statistics add
bounded ingress/egress kind, denied-grant, invalid-inner and conversion-signature
counts, plus expired/revoked grants and v2 payload/hash mismatches. No payloads, secret keys or unbounded peer-label diagnostics are added.

A valid signed inner envelope can still be substituted under another captured v1
outer ID. The regression explicitly demonstrates single-ID suppression while
retaining an authenticated inner author. This is a legacy availability risk,
not outer-ID authenticity or Byzantine convergence. Consumer ownership checks
remain necessary; successful inner verification is not write authorization.

## Reproducible evidence and limits

Dev dependencies pin the **published** pubsub/types/identity/transport 0.5.66.
Tests invoke the real legacy decoder and real IHAVE/IWANT/AntiEntropy handlers.
The resolved old membership dependency is 0.5.67; this is recorded, not attributed
to the historical executable. Current workspace crates remain 0.5.75.

See [dependency provenance](fixtures/legacy-compat-dependencies.json) and
[resolved lock fixture](fixtures/legacy-compat.Cargo.lock). The root lock remains
ignored per existing repository policy. To reproduce this dependency resolution,
copy the lock fixture to the root `Cargo.lock` and use `--locked`.

From a clean checkout, with the dependencies already cached locally:

```sh
cp docs/design/fixtures/legacy-compat.Cargo.lock Cargo.lock
cargo metadata --locked --offline --format-version 1 > /tmp/legacy-compat-metadata.json
cargo test -p saorsa-gossip-pubsub@0.5.75 --all-features --locked --offline
cargo test -p saorsa-gossip-transport@0.5.75 --all-features --lib --locked --offline
```

The lock and dependency provenance include the current pubsub `tempfile` test
dependency and Windows-only `windows-sys 0.61.2` dependency.

The published legacy crates exercise the **outer** decoder/relay/control format;
they do not establish stock x0x application acceptance of inner V3. The V3 signer
in these tests is a fixture for the required paired consumer.

The tests cover direct traffic both ways, modern/modern/old and old/modern/modern
forward conversion, modern/original-old/modern forwarding, control handlers,
cache serves, wrong signer, invalid/unknown inner author, malformed/oversized
controls, payload tampering, invalid-inner-first cache poisoning, raw/topic
isolation, expiry, revocation after queueing, reconnect, floor restart/corruption,
clock rollback, RejectV1, and the residual valid-inner alias case. A real loopback
QUIC test exercises guarded admission and cancellation after queueing.

The deterministic cost fixture publishes 50 pre-signed 128-byte application
payloads to two recorded peers. An initial debug-build run on David's Mac measured:

| Mode | Total publish time | Additional conversion signatures | Variant cache |
| --- | ---: | ---: | ---: |
| Migration disabled | 571 ms | 0 | 0 |
| Registered, modern only | 1,112 ms | 0 | 0 |
| Registered, one legacy recipient | 1,812 ms | 50 | 538,700 bytes |

The enabled cost comes from independent inner verification, policy checks and,
for legacy recipients, conversion signatures and durable floor metadata checks (full reads only on
change). The measurements above predate the floor cache and inner V3. This is a
small debug fixture using a recording transport, not network throughput or mixed
historical recovery latency. `/usr/bin/time` over Cargo includes compiler/test
process memory and is not a runtime peak-memory claim. No performance acceptance
is claimed from it.

Full paired-x0x receive/apply tests, authentic historical/live/unauthorized-write
binary phases (including explicit stock V2 rejection), and the unchanged ten-run convergence release recipe remain
required after consumer integration. This phase runs no live daemon, release
binary gate, deployment, x0x dependency bump, merge or tag.
