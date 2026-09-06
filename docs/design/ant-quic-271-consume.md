# Consume ant-quic #271 via crates.io 0.27.49

The workspace now depends on published crates.io **ant-quic 0.27.49**, which
replaces the temporary git pin at
`c3ca46adef666e83750ec4bae160deb8c5b61094` (the merge commit of
[ant-quic #271](https://github.com/saorsa-labs/ant-quic/pull/271)).
Expected crates.io checksum:
`39618d742aeca6048da3b5f04a584882bd1503ac37348603c66d9f6e1346d9dc`.

This draft still does **not** clear G4 or authorize registry publish/tag/merge.
The previous temporary git pin kept the bump reproducible before 0.27.49 was
available; the registry pin is the intended Mode (i) dependency once remaining
gates and acceptance are complete.

The current transport, coordinator library, coordinator binary, and CLI all
resolve this registry source. The publish-disabled legacy compatibility fixture
continues to use its registry transport and registry ant-quic; seeing both
sources in `cargo tree` is expected only insofar as fixture isolation pins
remain separate. The workspace ignores `Cargo.lock`, so the validation lockfile
is retained with the external bump evidence.

## PR base and release hold

This change stacks on `codex/release-0.5.76`
([gossip #49](https://github.com/saorsa-labs/saorsa-gossip/pull/49)) tip
`f56a8d129bd0105abd1d03bef232523c039fdf8b`
(tree `bd3e9e7ce546cabe8fc39c9387cae8db173249f9`).
That branch already contains the 0.5.76 release preparation and isolated legacy
compatibility pins. Targeting it keeps those changes out of this bump's diff.
Both PRs remain draft; this change does not authorize merging or releasing.

Mode (i), registry publication of gossip 0.5.76, remains **HOLD** until remaining
acceptance/gates clear. Consuming crates.io ant-quic 0.27.49 removes the git-pin
blocker but does **not** by itself authorize gossip crates.io publish or tag.
No gossip crates.io publish or tag is part of this change.

## G1 follow-up: HOLD

TODO(G1): stack [ant-quic #272](https://github.com/saorsa-labs/ant-quic/pull/272)
and integrate `recv_with_generation` when that API is ready and approved.
It is not included in 0.27.49 and is not a prerequisite for this bump.
Receive-generation provenance and its regression coverage remain a separate
follow-up; consuming #271 / 0.27.49 alone does not establish that G1 is
implemented.
