# saorsa-gossip — workspace task runner
#
# Standard recipes: check / fmt / lint / test / doc / build / coverage
# Heavy recipes:    test-all / bench-* / soak / nat-loopback / testnet-smoke
#
# Heavy / multi-minute benches are gated by SAORSA_BENCH_HEAVY=1.
# Testnet recipes that touch VPS bootstrap nodes are gated by SAORSA_TESTNET=1.

default:
    @just --list

# ===== Standard =====

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-features --all-targets -- -D warnings

test:
    cargo nextest run --workspace --all-features

test-verbose:
    cargo nextest run --workspace --all-features --no-capture

doc:
    cargo doc --workspace --all-features --no-deps --document-private-items

build:
    cargo build --workspace --all-features

build-release:
    cargo build --workspace --all-features --release

clean:
    cargo clean

check: fmt-check lint test doc

quick-check: fmt-check lint test

# Per-crate LCOV coverage (uses scripts/coverage-per-crate.sh)
coverage:
    bash scripts/coverage-per-crate.sh

coverage-strict:
    bash scripts/coverage-per-crate.sh --fail-under 80

# ===== Heavy: test-all =====

# Run every test we can locally:
#   - all unit + integration tests (including #[ignore]d)
#   - all doctests
#   - examples build-check
#   - benches compile-check
# Does NOT run multi-minute throughput/large-transfer/volume benches
# unless SAORSA_BENCH_HEAVY=1 (use `just bench-heavy` for those).
test-all: test-all-rust test-doctests examples-build bench-compile
    @echo ""
    @echo "✓ test-all complete (heavy benches skipped — run \`just bench-heavy\` if needed)"

test-all-rust:
    cargo nextest run --workspace --all-features --run-ignored all

test-doctests:
    cargo test --doc --workspace --all-features

examples-build:
    cargo build --workspace --all-features --examples

bench-compile:
    cargo bench --workspace --all-features --no-run

# ===== Benchmarks (criterion) =====

# Runs every fast criterion bench. Heavy benches require SAORSA_BENCH_HEAVY=1.
bench-all:
    cargo bench --workspace --all-features

# Transport throughput (loopback) — moderate runtime.
bench-throughput:
    cargo bench --package saorsa-gossip-transport --bench throughput

# Heavy: 10MB → 1GB single-stream transfers. Multi-minute.
bench-large-transfer:
    @if [ "${SAORSA_BENCH_HEAVY:-0}" = "1" ]; then \
        cargo bench --package saorsa-gossip-transport --bench large_transfer; \
    else \
        echo "skipped — set SAORSA_BENCH_HEAVY=1 to run (multi-minute)"; \
    fi

# Heavy: 10k / 100k / 1M small messages. Multi-minute.
bench-message-volume:
    @if [ "${SAORSA_BENCH_HEAVY:-0}" = "1" ]; then \
        cargo bench --package saorsa-gossip-transport --bench message_volume; \
    else \
        echo "skipped — set SAORSA_BENCH_HEAVY=1 to run (multi-minute)"; \
    fi

# Run every heavy bench in one shot.
bench-heavy:
    SAORSA_BENCH_HEAVY=1 just bench-large-transfer
    SAORSA_BENCH_HEAVY=1 just bench-message-volume

# ===== Long-running scenarios =====

# Multi-node soak (default 600s, override with SAORSA_SOAK_SECS).
# Requires the soak integration test to be present (tracked separately).
soak:
    @echo "Soak duration: ${SAORSA_SOAK_SECS:-600}s"
    SAORSA_SOAK_SECS=${SAORSA_SOAK_SECS:-600} \
        cargo test --release --package saorsa-gossip-runtime --test soak -- --ignored --nocapture

# Local two-node loopback (mDNS + hole-punch path via ant-quic).
# Recipe exists now; targeted test files are landed incrementally.
nat-loopback:
    cargo nextest run --package saorsa-gossip-transport --test two_node_loopback --run-ignored all

# Opt-in VPS testnet smoke (touches saorsa-2/3:9000 ant-quic ports).
# REQUIRES SAORSA_TESTNET=1. Honors port-isolation rules in CLAUDE.md.
testnet-smoke:
    @if [ "${SAORSA_TESTNET:-0}" != "1" ]; then \
        echo "Set SAORSA_TESTNET=1 to opt in to testnet tests."; exit 1; \
    fi
    cargo nextest run --package saorsa-gossip-transport --test testnet --run-ignored all
