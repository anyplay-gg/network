# Supply chain

What is pinned in this repository, how updates arrive, and what is not covered yet. The network ships no binary of its own: players get the net service inside the [launcher](https://github.com/anyplay-gg/launcher), whose release workflow builds it from a commit of this repository, records that commit in the release's `BUILD-INFO.txt` and attests the result ([launcher docs/supply-chain.md](https://github.com/anyplay-gg/launcher/blob/main/docs/supply-chain.md)). So what matters here is that the source and the CI around it are hard to tamper with.

## What is pinned

| What | How |
|---|---|
| GitHub Actions | Every third-party action is pinned by full commit SHA with the release it was taken from in a `# vX.Y.Z` comment. The SHA is looked up from the release tag with `gh api repos/<owner>/<action>/git/ref/tags/<tag>`. |
| Workflow permissions | `contents: read` for every workflow; nothing here writes to the repository or to a release. |
| Checkouts | `persist-credentials: false`. Nothing in these workflows pushes. |
| Crates | `Cargo.lock` is committed. `cargo-fuzz` is installed with `--locked` at an exact version. |
| Licences, advisories, sources | `cargo deny` with [deny.toml](../deny.toml) on every pull request (the shared `rules` workflow) and every Monday (`advisories.yml`). The licences must allow use under both MIT and Apache-2.0 and under the GPL-2.0 of the launcher that links the bridge protocol; unknown registries and git sources are refused. |

The shared `rules` workflow comes from `anyplay-gg/.github@main`, which is the organisation's own code and is not pinned to a commit.

## How updates arrive

[Dependabot](../.github/dependabot.yml) opens one pull request a week for the actions (all in one) and one for the crates of the workspace and of `fuzz/` (minor and patch updates in one; a major update by itself). It waits a week after a release before it proposes it, so a release that is pulled or found malicious in its first days never reaches us. Security updates ignore that wait.

## Verify

There is no release to verify here. To check a source tree: `cargo deny --all-features check`.

## What is not covered yet

- **Commit and tag signing.** Branch protection and required signed commits are repository settings, not files; they are not switched on.
- **Reproducible builds**, and verification that the net service in a launcher release is the one built from the recorded commit, are open; the launcher's attestation says who built it.
- **The shared rules workflow** is referenced by branch, see above.
- **The Rust toolchain** in CI is `stable` as installed by `dtolnay/rust-toolchain`, not a pinned version.
