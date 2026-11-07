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

Bash/zsh parse failures are reported explicitly: recognizable declarations inside error regions use `refs.kind: "unknown"`, and `review` separates unverified removals from confirmed deletions. See [shell parse errors](docs/diff-analysis.md#shell-parse-errors).

Rust cfg-condition classification reuses the current ancestor path to avoid repeated parent traversal on deep `use` paths. Reference positions, ordering, and cfg-key exclusions are preserved. See [performance checks](docs/development.md#reference-scan-performance) for the reproducible comparison.

Symbol extraction indexes declaration ranges when assigning methods to their enclosing types, avoiding a comparison of every method with every type in large files. Existing ownership and output ordering rules are preserved. See [symbol extraction benchmarks](docs/development.md#symbol-extraction-performance).

TypeScript review follows same-file private types used in exported signatures, so a changed alias or interface can report an API change and affected callers even when the exported header stays unchanged. See [signature type dependencies](docs/diff-analysis.md#typescript-signature-type-dependencies).

## Features

- **References by identifier**: `refs` lists the definitions and references of a symbol across a directory, and `refs --names` looks up many names in one pass
- **Diff impact**: `context` maps a unified diff to changed symbols, signature changes, and callers, and `impact` exits 1 while callers outside the diff are left unresolved (made for Stop hooks)
- **Rust type aliases**: a transparent public alias introduced in the same file can turn an equivalent function signature change into an informational result when the old and new syntax trees prove the underlying type is unchanged
- **Rust parameter patterns**: verified ordinary parameter renames and destructuring changes keep the same call contract; types and receivers remain checked. See [proof boundaries and container limitations](docs/diff-analysis.md#rust-parameter-patterns).
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

Requires Rust 1.99 or later.

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

Check `astro-sight --version` after installing. If `command -v astro-sight` shows a mise shim, `mise which astro-sight` identifies the executable to update; see [installation checks](docs/development.md#same-checks-as-ci).
<!-- standard:install:end -->

After `winget install`, open a new terminal: the portable package reaches `PATH` by rewriting it, which terminals that were already open do not pick up. The musl archive is a statically linked build for Linux without glibc (Alpine) or with an old glibc (some Docker images).

Building from source also needs a C compiler for the tree-sitter parsers (Xcode Command Line Tools on macOS). After the binary, `make install` writes the Claude Code and Codex skills described in [AI Agent Integration](#ai-agent-integration).

## Usage

Results are compact JSON on one line by default (batch modes print one line per file). `--pretty` indents the JSON, and `--format toon` or `--format auto` switches to TOON or to whichever of the two is estimated to take fewer tokens. TOON output follows the [v4.1 specification](https://github.com/toon-format/spec/blob/main/SPEC.md); an empty array is `[]`.

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

TypeScript/TSX `export const` annotation-only edits appear in `api_changes.type_annotation_changes` with `reason: type_annotation_only` and `compatibility: unverified`. Type compatibility needs a TypeScript check. `review --hook` keeps their references informational; `--strict-public-type-annotations` requests review through `policy.type_annotation`, independently of `--strict-public-const-values`. Annotation formatting alone is omitted and does not block either policy.

Python function parameter annotation changes to a local `Protocol` can appear in `api_changes.modified_closed_in_diff` (`api.mod_closed` in hook output) even when callers are unchanged. The proof follows a local variable annotated as `C` or `C | None`, initialized with `C(...)` or `None`, to a unique class declared locally or imported directly from a repository module. The class must declare every inherited Protocol method with matching parameter names, types, defaults, return types, and async behavior. Existing conforming classes also qualify.

This checks declared contracts using built-in types and containers. Aliased or relative imports, keyword or spread arguments, omitted changed parameters, complex local scopes or rebinding, decorated or inherited implementation classes, data members, unknown types, parse errors, and companion `.pyi` files keep the conservative blocking classification. Dynamic or transitive name resolution is outside this proof.

When only the return annotation of a Python function or method changes, an unchanged call such as `result = entry()` also counts as updated if every later use of `result` was rewritten to compare it with a literal of the new type (`assert result == 0` for `-> int`). Comparisons carried over from the old file, comparisons with variables or other types, and results passed to other names or calls stay blocking. See [Python return annotation changes](docs/diff-analysis.md#python-return-annotation-changes).

Adding a required property to a TypeScript interface or object type alias appears in `api.mod_closed` when the same diff updates every function that returns the type, every `const x: T` / `satisfies T` value, and the callers of every top-level function that takes the type as a parameter. The added property must carry a value whose type fits the new member. Generic or `extends` declarations, required methods, and references in unsupported positions (such as `T[]`) stay blocking even when the reference lines changed. See [TypeScript required property additions](docs/diff-analysis.md#typescript-required-property-additions).

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

Unparsed source files that may contain a requested name (`.vue`, `.m`, `.mm`, `.applescript`, ...) are listed in `truncations`, and a scope where no file can be parsed returns `UNSUPPORTED_LANGUAGE` rather than an empty list. See [Files astro-sight Cannot Parse](docs/usage.md#files-astro-sight-cannot-parse).

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

`review --hook` reports TS/TSX trailing optional/default parameter removal as `mod_compat` with reason `trailing_optional_params_removed` when unchanged remaining parameters, an explicit unchanged return annotation, and a closed inventory of direct internal calls prove the calls still fit. Their references remain in `impact_info`. Aliases, dynamic or ambiguous dependencies, and incomplete source coverage keep the existing conservative classification. This proof covers calls in the analyzed static repository; it does not guarantee external compatibility or equivalent runtime behavior.

`review --hook` also reports removing optional properties from the inline object type of an exported TS/TSX function's only parameter as `mod_compat` with reason `optional_object_props_removed`. The rest of the declaration, including the remaining properties and destructured bindings, must stay the same as AST tokens, and the types that the remaining properties and the return type reference must not change. A closed inventory must show that every JSX element and direct call of the function omits the removed keys: JSX elements must name the function directly and list attributes without spreads, and direct calls count only when the function keeps an explicit return type and receives one object literal. The JSX call sites remain in `impact_info`. A replaced JSX type environment (`jsxFactory`, `jsxFragmentFactory`, a `jsxImportSource` other than `react`, `paths` or `baseUrl` that can redirect `react/jsx-runtime` or that point outside the directory or into `node_modules`, a `package.json` entry point outside the directory, an import specifier that leaves the directory, an augmentation of `react` or `@types/react`, a declaration or export that creates the name `JSX`, or a `@jsx` pragma in any letter case), aliases, re-exports, dynamic access, and incomplete source coverage keep the existing conservative classification. The automatic runtime (`jsx: react-jsx` / `react-jsxdev`) imports nothing visible in the source, so the proof also requires `react` and `@types/react` to be registry dependencies without an `npm:` alias. A `@types/*` package from outside the registry, a dependency whose contents come from outside the repository (git, URL, or a path outside the directory), and `compilerOptions.types` or `/// <reference types>` naming a package that is not a registry dependency also prevent it. Types that the remaining contract imports through a relative path are never proven. Types imported from a registry dependency, and global types that the repository does not declare (from lib or `@types`), are accepted only when the diff changes no other file. The classification rests on evidence inside the analyzed repository; consumers outside it may still pass a removed key and break.

`review --hook` reports adding optional properties to the props of a component wrapped in React's `memo()` (`export const Tag = memo(function Tag({ … }: { … }) { … })`, or an arrow function) as `mod_compat` with reason `optional_object_props`, as it does for an unwrapped function component. The wrapper must stay the same and pass exactly one function to `memo` imported by name from `react`; a comparison function, type arguments, `React.memo`, and other wrappers keep the existing classification. `react` must be a registry dependency in a `package.json` of the component's directory or its ancestors (the root's or the workspace package's), and no config, dependency, or source in the analyzed directory may replace React's types. The diff must not change configs, `package.json`, or lockfiles, and the old content of every changed source must pass the same audit. [docs/diff-analysis.md](docs/diff-analysis.md) lists the conditions.

For a Rust struct in the public API, `api_changes` compares its public shape following the Cargo SemVer guide: the `pub` fields and their types, whether code outside the crate can construct it, `#[repr]`, `#[non_exhaustive]`, type parameters, and `where`. Removing, renaming, retyping, restricting, or moving a `pub` field, adding a `pub` field to a constructible struct, making the struct unconstructible outside the crate, tightening its type parameters, and adding or removing outlives bounds are a blocking `api.mod` even when the same diff updates the uses inside the repository, because uses outside the crate are not updated. Changes that keep existing uses working, such as relaxing construction, loosening bounds, or adding a type parameter with a default, are `mod_compat` with reason `rust_struct_shape_compatible`, and renaming type parameters or changing the fields that are not `pub` of a struct that already cannot be constructed outside the crate is not reported.

A Rust enum in the public API is compared the same way: removing, renaming, or reshaping a variant, changing its fields, adding a variant to an enum without `#[non_exhaustive]`, adding `#[non_exhaustive]`, and changing a discriminant value that code outside the crate can observe (through `as` or a `repr(C)` / `repr(<int>)` layout) are a blocking `api.mod`, and changes that keep existing uses working, such as adding a variant to a `#[non_exhaustive]` enum, are `mod_compat` with reason `rust_enum_shape_compatible`.

A Rust `union` is a symbol of kind `union`, and a public union is compared by its public shape in the same way: removing, renaming, retyping, or restricting a `pub` field, changing `#[repr]`, adding a field to a `#[repr]` union whose fields are all `pub`, and adding a type parameter to a union that code outside the crate can construct (a value is built from one field, so the new parameter may not be inferable) are a blocking `api.mod`. Because a union value is built from a single field, adding a `pub` field is `mod_compat` with reason `rust_union_shape_compatible`, and reordering the fields or changing the fields that are not `pub` is not reported.

When a Rust item is moved to another file and re-exported from its old place with `pub use`, `api_changes` resolves the re-export to the moved declaration and compares the two like an in-place change: a breaking change is a blocking `api.mod`, and an unchanged item is reported as `moved`. A re-export that cannot be resolved (an external crate, a glob, a `#[cfg]`, a renaming `as`) stays a blocking `api.mod`.

When a TypeScript config that compiles a file enables `stripInternal`, `api_changes` compares the API as consumers of the emitted declaration files see it: adding `@internal` to an export or a member is classified the same as deleting it (`api.rm` / `api.rm_dead` for an export, and a change to the containing type or function for a member of an interface or object type).

Editing a caller in the same diff does not by itself close a property removal from an inline object parameter. `mod_closed` also requires proof that the call omits the removed keys. JSX attributes and object literals containing a removed key, and unproven forms such as spreads, computed or quoted keys, variable arguments, and implicit JSX children, remain in `api.mod` and hook `impacts`. For property removals from an interface or object type alias, references that fail the producer or use proof also remain unresolved even on changed lines.

Both removal proofs share one trust boundary. The usage inventory (the calls and JSX elements checked for the removed parameter or keys) parses every source of the scan that follows `.gitignore`, global ignores, and hidden-file rules; a source in another language or an unanalyzable format (such as `.vue`) in that scan prevents both proofs. Outside that scan (ignored and hidden files, except `node_modules` and `.git`), only TS/JS files and embedded sources (`.vue`, `.svelte`, `.astro`, `.mdx`) are read, once per detection for all changed functions together. A file joins the inventory and must pass the same proof when the changed function's name appears in it as an identifier word (including names written with Unicode escapes); the name in an embedded source prevents the proof. TS files, including `.d.ts`, also join when they depend on the declaring file: their `import`, `export … from`, `import()` (also in type positions), `require()`, `import x = require()`, `declare module`, and `/// <reference>` entries are resolved through relative paths, directory indexes, `paths` / `baseUrl`, and repository packages, and an entry that resolves to the declaring file, or that cannot be resolved and is not a registry dependency, counts. A relative entry that reaches a directory with a `package.json` (whose `types`, `main`, or `exports` can point anywhere), or a path where nothing exists, cannot be resolved. A relative path that leaves the directory and comes back (`../../app/src/ui` from inside `app`) is judged by where it ends. In TS files other than `.d.ts`, the declaring file's name (`panel` for `panel.tsx`, the directory name for `index.tsx`) also counts as a mention. JS files are selected by the function's name only, so an ignored JS file (type-checked with `checkJs`) that uses the module through a namespace import and a computed key without writing the function's name is not caught. Files in other languages (shell hooks, Python, Rust) are not read. The inventory, including the added files, must fit within 512 files, 8 MiB, and 100,000 nodes. One detection reads at most 1 GiB and parses at most 64 MiB of outside files, with no limit on the number of files; an unreadable file or a run over the limits prevents the proofs. The audit covers every file except `node_modules` and `.git` regardless of ignore rules: sources, `.d.ts` files, `tsconfig*.json` / `jsconfig*.json`, and `package.json`. Outside the scan, it parses only the TS/JS files whose contents contain `JSX`, `declare`, `@jsx`, `<reference`, or a Unicode escape of an ASCII identifier character (plus the TS files parsed for their dependencies), within 8 MiB per file and the same parse limit. Packages installed from a registry (the contents of `node_modules`) are trusted not to depend on the repository's sources and not to change React's standard JSX contract. Anything that cannot be placed on either side keeps the existing classification. That includes a directory symlink, a symlinked source or config, a tsconfig `files` / `include` / `typeRoots` entry or a `/// <reference path>` that reaches outside the directory or into `node_modules`, an unreadable config or `package.json`, and an audit that cannot read or parse a file within its limits. Without the audit, the props proof does not hold and the trailing-parameter proof accepts no unresolved import. Directories that cannot be opened for lack of permission are skipped, because a type check run by the same user cannot read them either. A directory symlink anywhere in the directory, such as `lib64 -> lib` in a Linux `.venv`, also leaves no audit. An import whose specifier cannot be resolved inside the repository is accepted only when it is a plain named import whose imported and local names all differ from the changed function (`import { useState } from "react"`) and its package is a registry dependency: no `package.json` in the repository has that name, no `paths` key or `baseUrl` entry can redirect it, and no `package.json` in the directory or its ancestors declares, overrides, or patches it with `file:`, `link:`, `workspace:`, `portal:`, `patch:`, a path, or a git or URL specifier. An npm override written as `$name` is judged by the specifier of the dependency it refers to, and one whose target cannot be found is not treated as a registry dependency. `pnpm-workspace.yaml` (`overrides`, `packageExtensions`, `patchedDependencies`, `catalog`, `catalogs`) and `.yarnrc.yml` (`packageExtensions`) in the directory and its ancestors are judged by the same rules, and `catalog:` references are resolved through them. Every package name in an override key counts, including a pnpm parent selector (`parent>child`), and a `>` inside a version range (`bridge@>=1.0.0`) is not a selector; a key that cannot be parsed leaves no audit. A pnpmfile (`.pnpmfile.cjs`, `.pnpmfile.mjs`, `pnpmfile.js`, or a `pnpmfile` / `global-pnpmfile` entry in an `.npmrc` in the directory or its ancestors), which can rewrite any dependency, or a YAML or `.npmrc` file that cannot be read leaves no audit. So does a tsconfig `moduleSuffixes` other than `[""]` or a `rootDirs` with more than one entry, because they change where imports resolve. Hoisting can make a dependency that another package declares resolvable from anywhere. Default, namespace, `default as`, string-named, and same-name imports, re-exports, `import()`, `require()`, and side-effect imports still need a resolved specifier.

For a TS/TSX default export that directly registers a single object method through a local identity function with the same explicit interface parameter and return type, starting to receive existing contract parameters can be `mod_compat` with reason `fixed_callback_contract`. The interface, registration, and all source outside the implementation parameters/body must remain unchanged. Only simple required parameters and a `void` method contract are proved; a direct call through the target's default import is required, and observed calls must supply the contract's arguments. Object escapes, aliases, re-exports, dynamic access, incomplete parsing, and source inventories above 512 files, 8 MiB, or 100,000 nodes retain the conservative classification. The current proof requires a JS/TS-only source inventory with relative imports and no computed access or escaped string literals anywhere in that inventory. Raw `context` evidence remains visible; `impact` and the review hook accept the proven compatible change. This establishes a limited static contract proof, without guaranteeing equivalent behavior or external consumers.

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

Besides mise, the build needs a C compiler for the tree-sitter parsers, and the tests need `git`. The tests run with [cargo-nextest](https://nexte.st/), which `make setup` installs at the version in `mise.toml`. Without mise, add `SYSTEM_TOOLS=1` to use the tools on `PATH` (`make test` then needs `cargo-nextest` on `PATH` as well). When a `[patch]` in `.cargo/config.toml` swaps in local tree-sitter crates, `Cargo.lock` changes only on your machine and `--locked` stops the build; run `make ci CARGO_FLAGS=` in that case.

Choosing the skills that `make install` writes, heap profiling, the usage statistics tool, and how a release is built are covered in [docs/development.md](docs/development.md).

## License

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
