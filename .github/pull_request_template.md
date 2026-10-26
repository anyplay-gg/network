<!-- Small fix? Open the pull request. Anything bigger: please open an issue first and link it here. -->

## What and why

Closes #

## Checklist

- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass.
- [ ] New behaviour has a hermetic test (own servers on loopback, no internet, no game).
- [ ] A change to a specification or a decision comes with an ADR in `docs/adr/`.
- [ ] A change to the bridge protocol (`spec/bridge-protocol.md`, `crates/bridge-protocol`) states its wire effect: the version, and whether the AnyPS5 fork has to change with it.
- [ ] It works for every game. No title ID in code; what is known per title is in specifications and tests.
- [ ] It stays on the right side of ADR 0021, and no game data is attached: no dumps, keys, firmware, saves, tokens, or logs with game memory.
