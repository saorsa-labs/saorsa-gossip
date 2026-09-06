# ADR-013 Signed KV compatibility implementation

This is a disabled-by-default library facility for Senior review, implementing
[ADR-013](../adr/ADR-013-explicit-legacy-gossip-egress.md). It does not enable a
fleet, bump x0x, or establish release acceptance. Tracking: [gossip #46](https://github.com/saorsa-labs/saorsa-gossip/issues/46)
and [x0x #517](https://github.com/saorsa-labs/x0x/issues/517).

## Audited receiver and topic scope

The audited receiver is x0x v0.30.1, commit
`506b064101bb105617d75117b5fcace47e34accc`, with pubsub/types 0.5.66.
Only these Signed KV families may be explicitly registered:

| Family | Exact registration | Audited receive/apply path at that x0x commit |
| --- | --- | --- |
| Delta/full-state response | Store's concrete topic string | `src/gossip/pubsub.rs::decode_for_delivery` (743), `decode_v2` (1055), `verify_signature` (1146); then `src/kv/sync.rs::start_with_spawner` (107), `decode_delta`, and `src/kv/store.rs::merge_delta` (373) |
| State request | `<same concrete topic>/state-sync` | Same verified delivery path, then `src/kv/sync.rs` state-request responder (138–171); full-state response is published on the base delta topic |

`decode_for_delivery` discards failed signed-envelope verification before the
subscriber receives data. The signature binds `x0x-msg-v2 || AgentId || topic ||
payload`; the key must derive the claimed AgentId. The Signed store's merge path
checks the verified writer against its anchored owner and rejects anonymous and
unauthorized writers. State requests cause bounded recovery work, not an ownership
change. The new PubSub verifier is independent of that application authorization.
It never interprets a relay identity as a writer or grants ownership.

`SignedKvTopic::new` requires one exact topic, a positive verifier revision and a
bounded roster of known application authors. It verifies the complete x0x V2
wire layout, ML-DSA-65 signature, key/author derivation, topic binding and byte
limit. The returned `VerifiedInner` binds the full envelope hash, author and
verifier revision. Local publication and EAGER admission run this verifier before
cache/seen mutation. Cache entries retain the metadata, and every cache serve and
transit conversion revalidates the actual bytes against current policy.

The trusted caller must establish that this is a Signed KV store using the
reviewed handlers before registering it. This API is not a wire-level capability
claim. No topics, authors, receiver versions or grants are inferred from network
traffic. Bare publishing remains v2. The fixed adapter admits no unsigned x0x V1,
other inner versions, unknown authors, raw payload exemption or arbitrary callback
that merely asserts a payload is safe.

## Policy and transport integration

1. Explicitly initialize `ModernFloors` once on trusted durable storage. On normal
   restarts call `open`, not `initialize`. A missing/invalid/partial/rolled-back
   journal disables legacy grants; normal v2 service continues. Floor additions
   are checksummed append-only records, fsynced before success. Runtime demotion
   and floor reset are absent. Protect the journal against external replacement.
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
generations so a reused connection pointer cannot resurrect a grant. It preserves
existing send semaphore, per-peer queue and timeout limits. After stream allocation
and all waits, it rechecks session and invokes the policy admission callback. It
never reconnects/retries already-selected v1 bytes on another session. Bytes for
which admission has completed are the transport boundary; later revocation cannot
recall them. Other transport implementations default to denying guarded sends.

## Forwarding, control and cache behavior

An internal transport wrapper covers every existing PubSub egress call, including
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
| UDP session identities | Configured `max_peers` per process; reconnect generations increase |

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
for legacy recipients, conversion signatures and durable floor reads. This is a
small debug fixture using a recording transport, not network throughput or mixed
historical recovery latency. `/usr/bin/time` over Cargo includes compiler/test
process memory and is not a runtime peak-memory claim. No performance acceptance
is claimed from it.

Full x0x receive/apply tests, authentic v0.30.1 historical/live/unauthorized-write
binary phases, and the unchanged ten-run convergence release recipe remain
required after consumer integration. This phase runs no live daemon, release
binary gate, deployment, x0x dependency bump, merge or tag.
