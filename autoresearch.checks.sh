#!/usr/bin/env bash
set -euo pipefail

# Focused checks for the rendezvous package.
# Tests, fmt, and clippy — suppressing success output, showing only errors.

echo "==> cargo test"
cargo test -p saorsa-gossip-rendezvous --quiet 2>&1 | tail -30

echo "==> cargo fmt --check"
cargo fmt -p saorsa-gossip-rendezvous -- --check 2>&1 || { echo "FAIL: fmt check failed" >&2; exit 1; }

echo "==> cargo clippy"
cargo clippy -p saorsa-gossip-rendezvous --all-features -- -D clippy::panic -D clippy::unwrap_used -D clippy::expect_used 2>&1 | tail -30
