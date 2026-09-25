<p align="center">
  <img src="docs/images/app.png" width="128" alt="ASTro-sight">
</p>

<h1 align="center"><b>AST</b>ro-sight</h1>

<p align="center">
  AST CLI for AI agents that parses 16 languages with tree-sitter and reports symbols, references, diff impact, API changes, and dead code as JSON or TOON
</p>

<!-- standard:badges:start -->
<h3 align="center">Supported Platforms</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-FCC624?logo=linux&amp;logoColor=black" alt="Linux">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/astro-sight/actions/workflows/ci.yml"><img src="https://github.com/owayo/astro-sight/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/astro-sight/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/astro-sight" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/astro-sight" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

astro-sight answers the structural questions an AI coding agent has while it edits code: where a symbol is defined and used, what a diff breaks, which public APIs changed, and which exports nothing calls any more.

It matches identifier nodes in the tree-sitter syntax tree, so `refs --name new` does not pick up comments or strings the way `grep new` does. Output is compact JSON by default, with TOON or automatic selection when tokens matter, and the same queries run from the CLI, an NDJSON session, or an MCP server.

## Features

- **References by identifier**: `refs` lists the definitions and references of a symbol across a directory, and `refs --names` looks up many names in one pass
- **Diff impact**: `context` maps a unified diff to changed symbols, signature changes, and callers, and `impact` exits 1 while callers outside the diff are left unresolved (made for Stop hooks)
- **Structured review**: `review` combines the impact, files that usually change together according to git blame (`cochange`), public API changes, and dead symbols in one run
- **File structure**: `symbols`, `calls`, `imports`, and `ast` return a file's outline, call graph, dependencies, and exact syntax nodes, and `sequence` draws the call flow as a Mermaid sequence diagram
- **Dead code and rules**: `dead-code` reports exported symbols that nothing references, leaving out the ones test runners and frameworks call at run time, and `lint` checks AST pattern rules written in YAML
- **Token-aware output**: compact JSON, TOON, or `--format auto`; results for frequent names stop at 100 references or about 3,000 tokens by default, with a `result_summary` of what was left out
- **Agent integration**: skills for Claude Code and Codex, an MCP server, and an NDJSON `session` that batches queries

Supported languages (extensions and parser versions are in [docs/languages.md](docs/languages.md)):

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?logo=rust&amp;logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/C-A8B9CC?logo=c&amp;logoColor=white" alt="C">
  <img src="https://img.shields.io/badge/C++-00599C?logo=cplusplus&amp;logoColor=white" alt="C++">
  <img src="https://img.shields.io/badge/Python-3776AB?logo=python&amp;logoColor=white" alt="Python">
  <img src="https://img.shields.io/badge/JavaScript-F7DF1E?logo=javascript&amp;logoColor=black" alt="JavaScript">
  <img src="https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&amp;logoColor=white" alt="TypeScript">
  <img src="https://img.shields.io/badge/TSX-61DAFB?logo=react&amp;logoColor=black" alt="TSX">
  <img src="https://img.shields.io/badge/Go-00ADD8?logo=go&amp;logoColor=white" alt="Go">
  <img src="https://img.shields.io/badge/PHP-777BB4?logo=php&amp;logoColor=white" alt="PHP">
  <img src="https://img.shields.io/badge/Java-ED8B00?logo=openjdk&amp;logoColor=white" alt="Java">
  <img src="https://img.shields.io/badge/Kotlin-7F52FF?logo=kotlin&amp;logoColor=white" alt="Kotlin">
  <img src="https://img.shields.io/badge/Swift-F05138?logo=swift&amp;logoColor=white" alt="Swift">
  <img src="https://img.shields.io/badge/C%23-512BD4?logo=dotnet&amp;logoColor=white" alt="C#">
  <img src="https://img.shields.io/badge/Bash-4EAA25?logo=gnubash&amp;logoColor=white" alt="Bash">
  <img src="https://img.shields.io/badge/Ruby-CC342D?logo=ruby&amp;logoColor=white" alt="Ruby">
  <img src="https://img.shields.io/badge/Zig-F7A41D?logo=zig&amp;logoColor=white" alt="Zig">
</p>

## Installation

<!-- standard:install:start -->
### Homebrew (macOS/Linux)

```bash
brew install owayo/astro-sight/astro-sight
```

### winget (Windows)

```powershell
winget install owayo.astro-sight
```

### Cargo

Requires Rust 1.98 or later.

```bash
cargo install --git https://github.com/owayo/astro-sight --locked
```

### From GitHub Releases

Download the archive for your platform from [Releases](https://github.com/owayo/astro-sight/releases/latest), extract it, and put `astro-sight` on your `PATH`. Each release also includes `SHA256SUMS` for checking the downloads.

| Platform | Archive |
|---|---|
| Linux (x86_64) | `astro-sight-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (x86_64, musl) | `astro-sight-x86_64-unknown-linux-musl.tar.gz` |
| Linux (ARM64) | `astro-sight-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `astro-sight-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `astro-sight-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `astro-sight-x86_64-pc-windows-msvc.zip` |

On macOS, if you downloaded the archive with a browser, remove the quarantine attribute before running it: `xattr -d com.apple.quarantine astro-sight`.

### From Source

Requires [mise](https://mise.jdx.dev/) (the Rust toolchain is pinned in `mise.toml`).

```bash
git clone https://github.com/owayo/astro-sight.git
cd astro-sight
make install
```

`make install` installs to `/usr/local/bin`. Set `INSTALL_PATH` to change it (for example `make install INSTALL_PATH="$HOME/.local/bin"`).
<!-- standard:install:end -->

After `winget install`, open a new terminal: the portable package reaches `PATH` by rewriting it, which terminals that were already open do not pick up. The musl archive is a statically linked build for Linux without glibc (Alpine) or with an old glibc (some Docker images).

Building from source also needs a C compiler for the tree-sitter parsers (Xcode Command Line Tools on macOS). After the binary, `make install` writes the Claude Code and Codex skills described in [AI Agent Integration](#ai-agent-integration).

## Usage

Every command prints compact JSON on one line. `--pretty` indents the JSON, and `--format toon` or `--format auto` switches to TOON or to whichever of the two is estimated to take fewer tokens.

### Recommended Flow for Agents

```bash
# 1. Start the review of a whole diff with review
astro-sight review --dir . --git

# 2. Pair context and impact around an edit
astro-sight context --dir . --git
astro-sight impact --dir . --git

# 3. Read structure with symbols, and look up identifiers with refs
astro-sight symbols --path src/main.rs
astro-sight refs --name "AppService" --dir src/

# 4. Use ast only to pin down an exact syntax node
astro-sight ast --path src/main.rs --line 10 --col 0

# 5. Put structural rules you check repeatedly in lint, and use sequence when call order matters or calls go 3 or more levels deep
astro-sight lint --path src/main.rs --rules rules.yaml
astro-sight sequence --path src/main.rs --function main

# 6. Batch two or more different queries into one session
printf '%s\n' \
  '{"command":"symbols","path":"src/main.rs"}' \
  '{"command":"refs","name":"AppService","dir":"src"}' \
  | astro-sight session
```

### Find References

```bash
astro-sight refs --name extract_symbols --dir .
```

```json
{
  "symbol": "extract_symbols",
  "refs": [
    { "path": "src/engine/symbols/mod.rs", "ln": 107, "col": 7, "ctx": "pub fn extract_symbols(...)", "kind": "def" },
    { "path": "src/commands/api_changes/exported.rs", "ln": 45, "col": 39, "ctx": "let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;", "kind": "ref" }
  ]
}
```

`path` is relative to `--dir`, and `ln` and `col` are 0-based. When a name has more than 100 references or the output would exceed about 3,000 tokens, the output stops there and a `result_summary` describes the omitted references; `--max-results` and `--token-budget` change the limits (`unlimited` removes them).

### Check Unresolved Impacts

```bash
astro-sight impact --dir . --git
```

```text
Unresolved impacts found:

src/engine/symbols/mod.rs changed [extract_symbols]:
  → src/service.rs:284 [extract_symbols]
  → src/commands/api_changes/exported.rs:45 [extract_symbols]
```

When every caller has been updated, `impact` prints nothing and exits 0. Otherwise it prints the list above to stderr and exits 1. Line numbers in this text are 1-based so that an editor can open them directly.

The full reference is split by topic:

- Every command with its options, batch mode, `session`, and the MCP server: [docs/usage.md](docs/usage.md)
- `context`, `impact`, `review`, `dead-code`, and `cochange` in depth: [docs/diff-analysis.md](docs/diff-analysis.md)
- Compact keys, TOON, and `--format auto`: [docs/output-format.md](docs/output-format.md)

## Configuration

`astro-sight init` writes a configuration file to `~/.config/astro-sight/config.toml` (`--path` writes it elsewhere; an existing file is overwritten without asking). `--config <path>` reads another file for a single run.

```toml
debug = false          # write debug logs to files
format = "json"        # default output format: "json" | "toon" | "auto"
skip_generated = true  # leave generated files out of directory scans
```

`--format` on the command line takes precedence over `format`. The log directory, the cache under `~/.cache/astro-sight/`, and `--no-cache` are described in [docs/configuration.md](docs/configuration.md).

## AI Agent Integration

Register astro-sight as a skill so that Claude Code or Codex reaches for it on questions such as "who calls this function?" or "what does this diff affect?":

```bash
astro-sight skill-install claude   # ~/.claude/skills/astro-sight/SKILL.md
astro-sight skill-install codex    # ~/.codex/skills/astro-sight/SKILL.md
```

To use it as an MCP server over stdio, start `astro-sight mcp` from the repository you want to analyze. Files outside that directory are rejected.

```json
{
  "mcpServers": {
    "astro-sight": {
      "command": "astro-sight",
      "args": ["mcp"]
    }
  }
}
```

With only the skill, an agent can still fall back to grep. Rules for `CLAUDE.md` / `AGENTS.md` that make it prefer astro-sight, and a Stop hook that keeps it working while `impact` reports unresolved callers, are in [docs/integrations.md](docs/integrations.md).

## Development

<!-- standard:dev:start -->
Requires [mise](https://mise.jdx.dev/). Tool versions are pinned in `mise.toml`.

```bash
make setup   # Install the toolchain (mise) and dependencies
make ci      # Run the same checks as CI (no changes)
```

| Command | Description |
|---|---|
| `make setup` | Install the toolchain (mise) and dependencies |
| `make build` | Build a debug binary |
| `make release` | Build a release binary |
| `make run` | Run the debug binary (arguments via ARGS="...") |
| `make test` | Run the tests |
| `make lint` | Run clippy with warnings as errors |
| `make fmt` | Format the code (rewrites files) |
| `make fmt-check` | Check the formatting (no changes) |
| `make check` | Run fmt-check and lint (no changes) |
| `make ci` | Run the same checks as CI (no changes) |
| `make install` | Install the release binary to INSTALL_PATH (default /usr/local/bin) |
| `make uninstall` | Remove the binary from INSTALL_PATH |
| `make clean` | Remove build artifacts |

Run `make` to list every target. Releases are published from GitHub Actions (**Actions → Release → Run workflow**).
<!-- standard:dev:end -->

Besides mise, the build needs a C compiler for the tree-sitter parsers, and the tests need `git`. Without mise, add `SYSTEM_TOOLS=1` to use the tools on `PATH`. When a `[patch]` in `.cargo/config.toml` swaps in local tree-sitter crates, `Cargo.lock` changes only on your machine and `--locked` stops the build; run `make ci CARGO_FLAGS=` in that case.

Choosing the skills that `make install` writes, heap profiling, the usage statistics tool, and how a release is built are covered in [docs/development.md](docs/development.md).

## License

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
