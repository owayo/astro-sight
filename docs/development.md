# Development

Notes that supplement [Development](../README.md#development) in the README, which covers the `make setup` and `make ci` flow.

## Prerequisites

Development assumes [mise](https://mise.jdx.dev/). The versions of Rust and of the test runner [cargo-nextest](https://nexte.st/) are pinned in `mise.toml`, and every Makefile target calls `cargo` of that version through `mise exec --`, so the versions are the same even when mise is not activated in the shell. `mise.lock` pins the download URLs and checksums; after changing a version, regenerate it with `mise lock --platform linux-x64,linux-arm64,macos-x64,macos-arm64,windows-x64` (CI installs the tools with `--locked`, so a stale lock stops it). Besides mise, you need a C compiler (to build the tree-sitter parsers; Xcode Command Line Tools on macOS) and git (the integration tests create temporary repositories and run `git`).

To use `cargo` on `PATH` without mise, add `SYSTEM_TOOLS=1`. The versions may then differ from `mise.toml`, and `make test` also needs `cargo-nextest` on `PATH`.

```bash
make ci SYSTEM_TOOLS=1
make install SYSTEM_TOOLS=1
```

## Same Checks as CI

On Linux and macOS, CI only runs `make setup` and the targets that make up `make ci` (`make check` and `make test`), so a local `make ci` runs the same checks. Tests run with the default features, the same as the released binary. Clippy runs with both the default features and `--all-features`, to check the heap profiler code (the `dhat-heap` feature) as well.

`make test` runs the unit and integration tests with cargo-nextest, each test in its own process (settings in `.config/nextest.toml`), and then the doctests with `cargo test --doc`, which nextest does not run. To run only some tests, call nextest through mise, as in `mise exec -- cargo nextest run <filter>`.

The lint job, the test jobs, and the six release-target builds start independently. CI splits the tests across runners with nextest's `--partition slice:<m>/<n>`: two runners each on Linux and macOS (`make test TEST_PARTITION=slice:<m>/2`) and five on Windows, which calls `cargo nextest run` directly. Only the first runner of each OS runs the doctests; the others pass `TEST_DOC=0`. The first macOS runner runs `make ci`, so it also covers the lint for macOS without taking another macOS runner. The number of runners aims to have every one finish around the time the release builds do (about 7 minutes). macOS runners vary a lot in speed, so the first macOS runner, which also runs the lint, can take longer than the builds.

Tests keep the default concurrency (the number of CPUs). Most of their time goes to starting `git` and `astro-sight` processes, and more concurrency than CPUs made them slower on the 4-core Linux and Windows runners. The one CPU-bound stress test (`deep_nesting::rust_impact_proofs_handle_deep_nesting`, with a 300-second deadline per command) starts first and counts as two tests. On Windows, the tests put temporary files in `runner.temp`, on the same drive as the workspace; with the default `TEMP` on `C:` the same tests took 1.6 times as long. The test step also puts Git's own `git.exe` (in the directory that `git --exec-path` prints) first on `PATH`. The `git.exe` found on `PATH` is a wrapper that starts it as another process, and skipping the wrapper made the tests about 25% faster on the same runner.

Only pushes to main save the build caches; pull requests restore them without saving, so they do not fill the 10 GB cache limit and evict the caches of main. Each kind of job has its own cache key (`lint`, `test`, `build-<target>`), and the test runners of an OS share one key that the first runner saves.

Each CI job installs the tools in `mise.toml` through `.github/actions/setup-tools`. Rust does not use the rustup that comes with the runner image: mise installs rustup and the toolchain into job-local `RUSTUP_HOME` and `CARGO_HOME` directories under `runner.temp`. rust-cache puts the `rustc -vV` of every installed toolchain into the cache key, so when the image's own stable toolchain differed from the version in `mise.toml`, the key changed with the image version and jobs missed the caches that main had saved (on macOS, the release builds went from about 3 minutes to 9–12). The job-local homes hold only the `mise.toml` toolchain, so the key no longer depends on the image, and the action stops the job if rustup sees any other toolchain. Installing rustup adds about a second per job. Nothing is installed with `cargo install`, so `CARGO_HOME/bin` is not cached (`cache-bin: false`).

Cargo commands get `--locked` by default. When a `[patch]` in `.cargo/config.toml` swaps in local tree-sitter crates, `Cargo.lock` changes only on your machine and `--locked` stops the build. Empty `CARGO_FLAGS` in that case, as in `make ci CARGO_FLAGS=`.

After installing the binary, `make install` also writes the skills for Claude Code and Codex. `SKILL_TARGETS` selects which ones (default `claude codex`).

```bash
make install SKILL_TARGETS=          # no skills
make install SKILL_TARGETS=claude    # the Claude Code skill only
```

`make uninstall` removes only the binary and keeps the skills it wrote.

After an install, check `command -v astro-sight` and `astro-sight --version`. If a mise shim is selected, `mise which astro-sight` shows the actual executable. A different copy can take precedence over the default `/usr/local/bin` installation. Set `INSTALL_PATH` to the directory containing the selected executable, for example `make install INSTALL_PATH="$HOME/.cargo/bin"`; do not replace the shim itself.

The Bash, Swift, and TypeScript grammars have local patches. Their dependency versions match the vendored package versions exactly so a dependency update cannot silently bypass the patches. Update each grammar and its regression tests together. Kotlin 0.3.5 remains pinned because newer releases require an incompatible tree-sitter runtime.

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

## Reference Scan Performance

With Python 3.9 or later, compare two release binaries using the same Rust toolchain and release profile:

```powershell
python tools/bench-refs.py `
  --baseline C:/temp/astro-before.exe `
  --candidate C:/temp/astro-after.exe `
  --output-directory C:/temp/astro-refs-bench --mode session --repeats 9
```

Use `--mode session` for request times inside one persistent process per case, or `--mode cli` (the default) to include process startup. The mixed-language session case sends the three names as separate requests. The script saves deterministic inputs, checks identical stdout for every run, warms up each binary once, and alternates their order. Cases cover a name deep inside Rust `use` paths, repeated path segments, a leaf name, an absent name, ordinary shallow functions, and the mixed-language fixtures. It writes every elapsed-time and peak-working-set measurement to `samples.csv`, with medians, ranges, executable hashes, and environment metadata in `summary.json`. On Windows, the peak comes from GetProcessMemoryInfo after each CLI process exits, or after each request in session mode and includes mapped pages; it is not an allocation count. Session peaks include the warmup and earlier requests within that case. Other platforms record null for this metric.

Keep generated inputs outside the repository so they do not become extra sources for reference and API analyses. Record the source revision, dependency versions, toolchain, build command, machine load, and whether the file cache was warm with the results. Compare large improvements against the full range of samples; small differences can be scheduler noise. Rust closure binding and usage-role checks still have separate ancestor walks, so these measurements do not establish linear time for every Rust reference query.

## Symbol Extraction Performance

Use the same harness with `--suite symbols` to compare enclosing-type assignment on files with 1,000, 4,000, and 16,000 classes, each with one method. Controls cover functions without classes, a real Rust source file, and mixed-language fixtures. The fixture directory runs only in CLI mode; session mode measures single-file requests. CLI requests use `--no-cache` so the persistent result cache cannot bypass extraction. Session requests bypass that cache layer. Both modes verify identical stdout on every run and save time and peak-working-set samples.

```powershell
python tools/bench-refs.py --suite symbols --mode session --repeats 9 `
  --baseline C:/temp/astro-before.exe --candidate C:/temp/astro-after.exe `
  --output-directory C:/temp/astro-symbols-bench
```

The ownership index preserves the existing smallest-line-span rule, including extraction-order ties, exact column boundaries, and known Go receivers. Large inputs use O((C+F) log(C+F)) time and O(C+F) auxiliary space for C containers and F unassigned functions. Small inputs use direct comparison. This bound describes ownership assignment, not parsing or the entire `symbols` command. Results and limits from the maintenance run are in [the measurement record](benchmarks/2026-10-30-symbols/README.md).
