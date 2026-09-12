# Legacy compatibility fixture

This non-publishable crate keeps the ADR-013 witnesses on the real published
0.5.66 decoder, transport types, identity implementation, and handlers. Updating
these pins to the current workspace version would invalidate those witnesses.

Pubsub uses this fixture through a path-only dev dependency without a version.
Cargo removes that dependency when normalizing the registry manifest, so the
packaged graph contains only the current 0.5.78 internal dependencies. The
compatibility tests are repository tests; run them from this workspace:

```sh
cargo test -p saorsa-gossip-pubsub@0.5.78 --lib compat::tests
```

The fixture is not a release artifact. Exclude it along with workspace-hack
when staging the publishable workspace with `cargo package --workspace`.
