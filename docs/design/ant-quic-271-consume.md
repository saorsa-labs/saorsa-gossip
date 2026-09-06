# Consume ant-quic #271

The workspace uses ant-quic from git at
`c3ca46adef666e83750ec4bae160deb8c5b61094`, the merge commit of
[ant-quic #271](https://github.com/saorsa-labs/ant-quic/pull/271).
The exact revision keeps this draft reproducible while the required transport
changes await a crates.io release. The previous manifest requirement was
`0.27.26` (a compatible-version range, not an exact registry pin).

The current transport, coordinator library, coordinator binary, and CLI all
resolve this git source. The publish-disabled legacy compatibility fixture
continues to use its registry transport and registry ant-quic; seeing both
sources in `cargo tree` is expected. The workspace ignores `Cargo.lock`, so
the validation lockfile is retained with the external bump evidence.

## PR base and release hold

This change stacks on `codex/release-0.5.76`
([gossip #49](https://github.com/saorsa-labs/saorsa-gossip/pull/49)), whose base
at implementation start is `1ab27a463b8a6d025a3626c9d8fab6c1451b618b`.
That branch already contains the 0.5.76 release preparation and isolated legacy
compatibility pins. Targeting it keeps those changes out of this bump's diff.
Both PRs remain draft; this change does not authorize merging or releasing.

Mode (i), registry publication of gossip 0.5.76, is **HOLD** while this git-only
dependency is used. First publish an ant-quic version containing #271, replace
the git pin with that registry version, and rerun validation before registry
publication. No gossip crates.io publish or tag is part of this change.

## G1 follow-up: HOLD

TODO(G1): stack [ant-quic #272](https://github.com/saorsa-labs/ant-quic/pull/272)
and integrate `recv_with_generation` when that API is ready and approved.
At implementation start it remains draft at
`484c8748a9a1eab7bfded4a53c527f3914a8931b`. It is not included in this pin
and is not a prerequisite for this bump. Receive-generation provenance and
its regression coverage remain a separate follow-up; consuming #271 alone
does not establish that G1 is implemented.
