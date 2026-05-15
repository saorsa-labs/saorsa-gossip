# saorsa-gossip — workspace task runner
#
# Standard recipes: check / fmt / lint / test / doc / build / coverage
# Heavy recipes:    test-all / bench-* / nat-loopback
#
# Heavy / multi-minute benches are not defined yet.

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
#   - all unit + integration tests
#   - all doctests
#   - examples build-check
# Does NOT run #[ignore]d tests; some ignored tests are intentionally
# documentation for currently unsupported/heavy scenarios and are not expected
# to pass in normal local/CI validation.
test-all: test-all-rust test-doctests examples-build
    @echo ""
    @echo "test-all complete"

test-all-rust:
    cargo nextest run --workspace --all-features

test-doctests:
    cargo test --doc --workspace --all-features

examples-build:
    cargo build --workspace --all-features --examples

# ===== Benchmarks (criterion) =====

# Runs every fast criterion bench when benchmark targets exist.
bench-all:
    cargo bench --workspace --all-features

# Run every heavy bench in one shot.
bench-heavy:
    @echo "No heavy benchmarks defined yet; this is a placeholder."

# ===== Long-running scenarios =====

# Local two-node loopback (mDNS + hole-punch path via ant-quic).
nat-loopback:
    cargo nextest run --package saorsa-gossip-transport --test two_node_loopback
