# saorsa-gossip

Post-quantum gossip overlay for the Saorsa ecosystem: membership, Plumtree
pub/sub, presence, delta-CRDT sync and MLS groups over `ant-quic`. Its main
consumer is `x0x`. Rust 2021, MSRV 1.88, MIT OR Apache-2.0.

## Workspace map
A virtual workspace; published crates share the workspace version and are
released to crates.io together.
- `bin/cli` — package `saorsa-gossip`: the canonical facade crate (re-exports the
  components as `saorsa_gossip::{runtime, types, transport, pubsub, crdt, ...}`)
  plus the CLI binary
- `bin/coordinator` — `saorsa-coordinator` node binary
- `crates/types` (core types, stream classes Critical/Normal/Bulk),
  `identity` (ML-DSA), `transport` (adapter over `ant-quic`),
  `membership` (HyParView + SWIM), `pubsub` (Plumtree), `presence` (beacons),
  `crdt-sync` (delta-CRDT anti-entropy), `groups` (MLS), `coordinator`
  (seedless-bootstrap adverts), `rendezvous` (shards; no DNS/DHT), `runtime`
- `test-support/legacy-compat` — unpublished fixture of 0.5.66-era wire witnesses,
  used by `pubsub` tests
- `saorsa-gossip-workspace-hack` — cargo-hakari crate (see below)

`ant-quic` comes from crates.io (version in root `Cargo.toml`, with a comment
on why that minimum), not a sibling path. Discovery (mDNS, UPnP, outbound
connection orchestration) lives in ant-quic; don't reimplement it here (ADR-011).

## Build and test
- `just --list`. `just check` = fmt-check, lint, nextest, doc;
  `just test-all` adds doctests and an examples build. CI also runs an MSRV
  (1.88) build and `cargo audit`.
- **cargo-hakari is version-pinned** (0.9.38; kept in sync across the justfile,
  `.config/hakari.toml`, `.githooks/pre-commit`, CI and `release.yml`). After
  dependency changes that alter feature unification, run `just hakari-generate`
  (it refuses a mismatched hakari). `just hooks-install` enables the shared
  pre-commit hook.
- Release: a pushed `v*.*.*` tag must match the workspace version; `release.yml` strips
  workspace-hack deps (`cargo hakari remove-deps`) before `cargo publish`.
- Downstream impact: `x0x` pins these crates exactly (`=0.5.N`) and bumps them
  deliberately, but it gitignores `Cargo.lock`, so caret dependencies declared
  here (e.g. `ant-quic`) float to the newest compatible publish in x0x's fresh CI.

## Wire-compatibility decisions (read before touching signing or pubsub encoding)
- ADR-012: gossip messages carry a payload-covering ML-DSA signature (the v2
  format; v1 signed only the header).
- ADR-013: legacy v1 egress is an explicit, disabled-by-default migration facility
  with bounded grants; `RejectV1` overrides grants. Design notes:
  `docs/design/legacy-kv-compat.md`.
- ADR-014: modern-only release support — `RejectV1` is the default for supported
  instances; stock x0x v0.30.1 is no longer a release-blocking endpoint.
- No classical-crypto fallback anywhere (ADR-002).

## Docs
- ADRs: `docs/adr/` (index in `README.md`, process in `TOOLING.md`). Before
  changing architecture, protocols, crypto, wire formats, public APIs or
  operational invariants, check them; new decisions go in a Proposed ADR from
  `docs/adr/TEMPLATE.md`. Accepted ADRs are immutable (supersede instead), and
  only a human marks an ADR Accepted.
- Design notes: `docs/design/` (pubsub fan-out backpressure, ant-quic consumption).
- `DESIGN.md` — original protocol design; `docs/benchmarks.md`.
- `docs/infrastructure/INFRASTRUCTURE.md` predates the current VPS fleet; the
  workspace-level AGENTS.md is authoritative for nodes and ports.
