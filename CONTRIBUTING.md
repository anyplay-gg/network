# Contributing to the AnyPlay Network

Issues and pull requests from anyone are welcome here. This repository and the [launcher](https://github.com/anyplaynet/launcher) are open; the cloud, the website, the AnyPS5 fork and the organisation's `.github` repository take contributions from the team only until 25 April 2027. The rules that hold in every AnyPlay repository (how to contribute, the line the project keeps, and no game data) are in the [organisation's CONTRIBUTING.md](https://github.com/anyplaynet/.github/blob/main/CONTRIBUTING.md), and the [Code of Conduct](https://github.com/anyplaynet/.github/blob/main/CODE_OF_CONDUCT.md) applies. This file adds what is specific to this repository.

## Before you start

- **A small fix** (a typo, a wrong link, a clear bug with a test): open a pull request.
- **Anything bigger:** open an issue first. A new feature, a change to a specification, a new dependency and anything that touches the bridge protocol are always bigger.
- **A question about a specification:** open a "Specification question" issue. It gets the `spec` label.
- **A problem with one game** belongs in a game report in the [launcher](https://github.com/anyplaynet/launcher/issues/new/choose) repository. Code here is never written for one title; title IDs appear in specifications, tests and pull requests, never in code.
- Read [CONTEXT.md](CONTEXT.md) for the words, [docs/DESIGN.md](docs/DESIGN.md) for how the parts fit, and [ADR 0021](docs/adr/0021-the-line-is-drawn-at-the-law.md) for what the network does and never does.

## Build and test

You need Rust 1.91 or newer. These are the commands CI runs on Windows, Linux and macOS. A pull request needs all of them green:

```sh
cargo fmt --all -- --check
cargo fmt --manifest-path fuzz/Cargo.toml -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

On Linux the share worker needs PipeWire and udev headers, `pkg-config` and `clang` (`libpipewire-0.3-dev libudev-dev clang pkg-config`); the voice devices feature needs the ALSA headers (`libasound2-dev`). The fuzz targets in [fuzz/](fuzz/README.md) need `cargo-fuzz` and a nightly toolchain, and run every night in CI.

Tests are hermetic: they start their own servers on loopback and need no internet, no game and no cloud. A test finishes in seconds and checks behaviour, not only that something exists.

## Specifications and decisions

The network's own specifications are in [spec/](spec), and the decisions behind them are in [docs/adr/](docs/adr). Specifications come before code: a handler is written from the specification, and the specification is written from observation, as [docs/OBSERVING.md](docs/OBSERVING.md) describes.

- **A change to a specification or a decision needs an ADR.** Add `docs/adr/NNNN-short-name.md` with the next free number. Say what is decided and why, which options you considered, and what follows from it. If the new decision replaces an older one, say which, as ADR 0021 does. Keep the title a full sentence that states the decision.
- **A new term goes into [CONTEXT.md](CONTEXT.md).** Use the glossary's words, and not the ones it lists under _Avoid_.
- **A gap you know about goes into [docs/gaps.md](docs/gaps.md)**, not into a code comment.

### The bridge protocol is shared

The [bridge protocol](spec/bridge-protocol.md) is spoken by this repository and by the bridge in the [AnyPS5 fork](https://github.com/anyplaynet/AnyPS5), which is C++ and follows the specification, not the Rust code. A change to the wire (a frame layout, a message, a field, a limit, an error) therefore needs more than a code change:

- Say in the pull request what changes on the wire, and whether a bridge built before the change still works with the new net service, and the other way round.
- Say how the version is handled: a change old and new sides cannot both read raises the protocol version that `hello` and `welcome` carry, and a compatible addition says why it is compatible.
- Update the specification in the same pull request, with the ADR the previous section asks for, and say whether the fork has to change with it. Do not merge a change that breaks the fork's current build without a plan for the fork.
- Keep the fuzz targets for the bridge messages in step with the change.

The control protocol between the launcher and the net service, and the cloud API, are shared in the same way with the [launcher](https://github.com/anyplaynet/launcher) and the cloud. State which of them a change affects.

## Game data and observation logs

Never attach or link game dumps, keys, firmware, saves, platform or publisher tokens, or logs that contain game memory. An observation log stays on the PC of whoever made it. In a specification or an issue, describe what a game sent or accepted in your own words and quote a log only as far as it shows a field. Examples use `GAME00001` as the title ID.

## Pull requests

- One topic per pull request, small commits, English commit messages, and a description that says what changed and why.
- Fill in the pull request template.
- A pull request needs one review and green CI. The first run of a first-time contributor waits for a maintainer's approval. CI also checks the links in the Markdown files, the licences of dependencies and security advisories.
- Dependencies must suit both licences below. `cargo-deny` checks this with `deny.toml`.

## Licence

This repository is licensed under MIT or Apache-2.0, at the user's option ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)). By contributing you release your work under both. There is no other agreement to sign.
