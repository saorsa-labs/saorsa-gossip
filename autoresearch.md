# Autoresearch: runtime unit-test coverage >90%

Worktree: autoresearch-rendezvous

## Objective
Raise `saorsa-gossip-runtime` unit-test line coverage above 90% by adding legitimate tests only. Do not change production behavior just to satisfy coverage.

## Metrics
- **Primary**: `line_coverage` (%, higher is better) from `scripts/coverage-per-crate.sh --crate runtime`.
- **Secondary**: `lines_hit`, `lines_found`, experiment wall time, and check status.

## How to Run
`./autoresearch.sh` — runs runtime crate coverage and outputs `METRIC line_coverage=<pct>` plus supporting metrics.

## Files in Scope
- `crates/runtime/src/*.rs` — add focused unit tests for existing runtime/coordinator/rendezvous behavior.
- `autoresearch.md`, `autoresearch.sh`, `autoresearch.checks.sh`, `autoresearch.ideas.md` — session state and experiment harness.

## Off Limits
- Do not modify production logic solely to inflate coverage.
- Do not weaken tests, coverage script, or workspace lint policy.
- Do not use network/VPS resources or broad process-kill commands.

## Constraints
- Add tests only where possible.
- Tests must validate meaningful behavior and not overfit to coverage implementation details.
- Keep production code panic-free; test code may use unwrap/expect if useful.
- Coverage command: `scripts/coverage-per-crate.sh --crate runtime`.

## What's Been Tried
- Initial scout identified low/zero coverage in `rendezvous.rs`, `coordinator.rs`, and direct runtime builder/default accessors. Good targets: mocked transport/pubsub/membership unit tests around provider summary processing, coordinator advert caching/broadcasting, and runtime config defaults.
