# Development

Notes that supplement [Development](../README.md#development) in the README, which covers the `make setup` and `make ci` flow.

## Prerequisites

Development assumes [mise](https://mise.jdx.dev/). The Rust version is pinned in `mise.toml`, and every Makefile target calls `cargo` of that version through `mise exec --`, so the version is the same even when mise is not activated in the shell. Besides mise, you need a C compiler (to build the tree-sitter parsers; Xcode Command Line Tools on macOS) and git (the integration tests create temporary repositories and run `git`).

To use `cargo` on `PATH` without mise, add `SYSTEM_TOOLS=1`. The Rust version may then differ from `mise.toml`.

```bash
make ci SYSTEM_TOOLS=1
make install SYSTEM_TOOLS=1
```

## Same Checks as CI

The CI quality job (Linux and macOS) only runs `make setup` and `make ci`, so a local `make ci` runs exactly the CI checks. Tests run with the default features, the same as the released binary. Only clippy runs with `--all-features`, to check the heap profiler code (the `dhat-heap` feature) as well. On Windows, the build job runs `cargo test --locked` directly.

Cargo commands get `--locked` by default. When a `[patch]` in `.cargo/config.toml` swaps in local tree-sitter crates, `Cargo.lock` changes only on your machine and `--locked` stops the build. Empty `CARGO_FLAGS` in that case, as in `make ci CARGO_FLAGS=`.

After installing the binary, `make install` also writes the skills for Claude Code and Codex. `SKILL_TARGETS` selects which ones (default `claude codex`).

```bash
make install SKILL_TARGETS=          # no skills
make install SKILL_TARGETS=claude    # the Claude Code skill only
```

`make uninstall` removes only the binary and keeps the skills it wrote.

## Heap Profiling

With the `dhat-heap` feature, a run writes a breakdown of the heap to `dhat-heap.json` in the working directory. Use it to measure where memory goes on a large repository.

```bash
mise exec -- cargo run --release --locked --features dhat-heap -- symbols --dir .
```

## Usage Statistics Tool

`tools/usage-stats` ([Usage Statistics](integrations.md#usage-statistics)) is a Cargo project separate from the root, and `make ci` does not cover it. To install it as a command, run `make -C tools/usage-stats install`.

## Releases

Run **Actions > Release > Run workflow** on GitHub Actions. One run goes through the following steps in order.

1. Rewrite the version in `Cargo.toml` and `Cargo.lock`, commit it, tag it `v<version>`, and push
2. Build 6 targets (Linux x86_64 / x86_64 musl / ARM64, macOS Intel / Apple Silicon, Windows x86_64) and create a GitHub Release with the archives and `SHA256SUMS`
3. Rewrite the formula in the Homebrew tap (`owayo/homebrew-astro-sight`) to point directly at this version's archives, and submit an update manifest to winget-pkgs

The version format is `yy.m.counter` (for example `26.9.103`). The year and month follow Japan Standard Time; the counter goes back to 100 when the month changes and increases by 1 with every release within the same month. If a tag for the same version already exists, the run stops without doing anything.

With `dry_run` enabled, the run only computes the next version and prints it to the log, without a commit, a tag, a build, or a release. The CI build job builds the 6 targets with the same settings on every push to main and every PR.

When the repository lacks the settings for the tap or for winget-pkgs, that job prints a warning and is skipped, and the release itself does not stop. The results appear in the run's Summary.
