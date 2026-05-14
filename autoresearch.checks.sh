#!/usr/bin/env bash
set -euo pipefail

cargo test -p saorsa-gossip-runtime --quiet
cargo clippy -p saorsa-gossip-runtime --all-features -- -D clippy::panic -D clippy::unwrap_used -D clippy::expect_used
