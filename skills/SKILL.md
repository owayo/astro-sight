---
name: astro-sight
description: "STOP before using Grep for code identifiers (function/class/variable/type/constant/method names, including pipe-separated patterns like FOO|Bar). Use astro-sight `refs` for identifier search, `review` for diff/PR review, `context`/`impact` around edits, `dead-code` for exported symbol cleanup, and `symbols`/`ast`/`calls`/`imports`/`lint`/`cochange`/`sequence`/`session` for structural analysis via tree-sitter AST."
when_to_use: |
  - Searching for any code identifier (function, class, variable, type, constant, method) — use `refs --name` / `refs --names` instead of `Grep`
  - Pipe-separated identifier search like `FOO|Bar|baz` — use `refs --names FOO,Bar,baz --dir .`
  - Reviewing a diff, PR, or recent changes end-to-end — start with `review --dir . --git`
  - Before editing code — run `context --dir . --git` to analyze impact
  - After editing code — run `impact --dir . --git` to detect unresolved impacts
  - Touching exported APIs / public modules — add `dead-code --dir . --git` to detect dead symbols
  - Understanding a file or directory — use `symbols --path <file>` or `symbols --dir <dir>`
  - Call graphs, imports, sequence diagrams, co-change history, or repeated AST/text rules — use the matching command (`calls` / `imports` / `sequence` / `cochange` / `lint`)
  - Running 2+ mixed astro-sight queries in one process — use `session` (NDJSON)
  Skip for non-code text searches (error messages, config values, TODO comments, file-path patterns) — Grep handles those.
allowed-tools: Bash(astro-sight:*)
---

# astro-sight

## When to Use (Decision Checklist)

**Before running Grep, ask: "Does my search contain code identifiers?"**

- Searching for a **function, class, variable, type, constant, or method name**? → `astro-sight refs` (NOT Grep)
- Searching with **pipe-separated identifiers** like `FOO|Bar|baz`? → `astro-sight refs --names FOO,Bar,baz --dir .` (NOT Grep)
- Asked to **review a diff / PR / recent changes end-to-end**? → `astro-sight review --dir . --git`
- Need to **understand a file's structure** (functions, classes, structs)? → `astro-sight symbols --path <file>`
- Need to **understand a directory's structure**? → `astro-sight symbols --dir <dir>`
- Need to inspect the **exact syntax node at a cursor/range** or debug a parse error? → `astro-sight ast`
- Need to know **who calls a function** or **what a function calls**? → `astro-sight calls`
- Want to know **what a code change breaks**? → `astro-sight context --dir . --git`
- Want to detect **unresolved impacts after editing**? → `astro-sight impact --dir . --git`
- Need to see **what a file imports**? → `astro-sight imports`
- Need to **batch 2+ mixed astro-sight queries** in one process? → `astro-sight session`
- Need to **check repeated AST/text patterns**? → `astro-sight lint`
- Need a **visual call flow diagram**? → `astro-sight sequence`
- Need to find **files that usually change together**? → `astro-sight cochange`
- Want a **structured one-shot review** of a diff (impact + missing cochanges + API surface diff + dead symbols)? → `astro-sight review`
- Need to review an **external patch or rename-aware diff file**? → `astro-sight review --dir . --diff-file <patch>`
- Need to find **dead (unreferenced) exported symbols**? → `astro-sight dead-code --dir .`
- Need to find **dead code related to a diff**? → `astro-sight dead-code --dir . --git`

**Grep is fine for**: error messages, config values, TODO comments, file path patterns — anything that is NOT a code identifier.

## Quick Reference (Start Here)

```bash
# 1. Review a diff / PR in one shot
astro-sight review --dir . --git

# 2. Analyze what a diff breaks before editing
astro-sight context --dir . --git

# 3. Detect unresolved impacts after edits
astro-sight impact --dir . --git

# 4. Find dead exported symbols before merge
astro-sight dead-code --dir . --git

# 5. Find all references to a symbol (REPLACES Grep for identifiers)
astro-sight refs --name <symbol_name> --dir .

# 6. Batch symbol search (REPLACES Grep "FOO|Bar|baz")
astro-sight refs --names sym1,sym2,sym3 --dir .

# 7. Understand file structure (functions, classes, structs)
astro-sight symbols --path <file>

# 8. Show caller/callee relationships
astro-sight calls --path <file> --function <function_name>

# 9. Extract imports/exports
astro-sight imports --path <file>

# 10. Inspect exact syntax when structure alone is not enough
astro-sight ast --path <file> --line <n> --col <n>

# 11. Repeated AST/text checks
astro-sight lint --path <file> --rules rules.yaml

# 12. Check change hotspots
astro-sight cochange --dir . --file src/service.rs

# 13. Visualize call flow
astro-sight sequence --path src/main.rs --function main

# 14. Batch operations — multiple queries in one process
echo '{"command":"refs","name":"Sym1","dir":"."}
{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

## Review-First Workflow

- Reviewing a diff or PR? Start with `astro-sight review --dir . --git` before splitting into `context`, `impact`, `dead-code`, or `cochange`.
- Editing code? Run `astro-sight context --dir . --git` first, then `astro-sight impact --dir . --git` after the edit.
- Touching exported APIs or public modules? Add `astro-sight dead-code --dir . --git` before concluding the change.
- Repeating the same structural review policy across files? Reach for `astro-sight lint` instead of ad-hoc text search.
- Asking 2+ mixed astro-sight questions in one loop? Use `astro-sight session` instead of paying startup cost for each command.
- Need a call-flow overview for explanation or review? Use `astro-sight sequence` after `calls` or `symbols` identifies the target function.
- Need exact syntax around a confusing match or parse error? Escalate from `symbols` to `astro-sight ast --path <file> --line <n> --col <n>`.

## Escalation Path

- Start with `review`, `context`, or `symbols` for the broad picture.
- Escalate to `ast` when symbol lists are not enough and you need the exact syntax node at a cursor/range.
- Escalate to `session` when you already know you will run several mixed queries in sequence.
- Escalate to `lint` or `cochange` when the problem is repeated policy or historical coupling, not one-off structure lookup.

## Low-Adoption But Useful

- Need **2+ mixed astro-sight queries** in one loop and want to avoid repeated startup cost? → `astro-sight session`
- Need the **exact AST node** at a cursor/range, or want to confirm whether a parse error is structural? → `astro-sight ast --path <file> --line <n> --col <n>`
- Need a **single JSON review** that combines impact, cochange, API surface changes, and dead symbols? → `astro-sight review --dir . --git`
- Need to check a **repeated rule** like banned APIs, required patterns, or AST-based policy? → `astro-sight lint --path <file> --rules rules.yaml` before writing an ad-hoc text scan
- Need to predict **co-change fallout** before missing a related file? → `astro-sight cochange --dir . --file <file>` before guessing from filenames alone
- Need to explain a non-trivial call path after `calls` identifies the target? → `astro-sight sequence --path <file> --function <name>`
- Reviewing a multi-commit branch? → pass the same `--base <rev>` to `review`, `context`, or `impact`; `review --git --base <rev>` also uses that base for blame-backed `missing_cochanges`.

## Commands

### `refs` — Cross-File Symbol Search (Use Instead of Grep)

The primary Grep replacement. Finds all occurrences of a symbol name across a directory using tree-sitter AST parsing. Unlike Grep, it only matches actual identifier nodes — no false positives from comments, strings, or partial matches.

```bash
# Find all references to a symbol
astro-sight refs --name <symbol_name> --dir <directory>

# Narrow down with a glob pattern
astro-sight refs --name <symbol_name> --dir <directory> --glob "**/*.rs"

# Multiple symbols at once (NDJSON output, one line per symbol)
astro-sight refs --names sym1,sym2,sym3 --dir <directory>
```

Output: `refs` array with `path`, `ln`, `col`, `ctx` (source line), `kind` (`"def"` or `"ref"`). No need to Read files afterward — `ctx` already shows the source line. Batch mode (`--names`) outputs NDJSON with one `{"symbol":..., "refs":[...]}` per line.

Single-symbol and multi-symbol searches both merge worker-local results directly with fold/reduce instead of retaining a per-file intermediate `Vec` for the whole tree. For very common symbols, still narrow with `--glob` or lower `ASTRO_SIGHT_BATCH_WORKERS` because the final output itself can be large.

### `calls` — Call Graph Extraction

Extracts function call relationships from a source file.

```bash
# All call edges in a file
astro-sight calls --path <file>

# Only calls made by a specific function
astro-sight calls --path <file> --function <function_name>
```

Output (compact): `calls` array grouped by `caller` (string), each with `range` and `callees` array (`name`, `ln`, `col`). Use `--pretty` for full format.

### `context` — Diff Impact Analysis

Reads a unified diff and finds affected symbols, signature changes, and impacted callers. Answers "what does this change break?".

```bash
# Auto-run git diff (recommended — no pipe needed)
astro-sight context --dir . --git

# Analyze staged changes
astro-sight context --dir . --git --staged

# Custom base ref
astro-sight context --dir . --git --base HEAD~3

# Inline diff string
astro-sight context --dir . --diff "$(git diff)"

# Diff file
astro-sight context --dir . --diff-file changes.patch

# Pipe from git diff (legacy)
git diff | astro-sight context --dir .
```

Output: `changes` array per file with `affected_symbols`, `signature_changes`, `impacted_callers`.

### `impact` — Unresolved Impact Detection (Stop Hook)

Detects unresolved impacts after code changes. Uses `context` internally, then checks if impacted callers are in files NOT included in the diff. Designed for AI agent stop hooks.

```bash
# Auto-run git diff (recommended)
astro-sight impact --dir . --git

# Staged changes
astro-sight impact --dir . --git --staged

# AI agent hook mode (appends triage hint on detection)
astro-sight impact --dir . --git --hook

# Pipe from stdin
git diff | astro-sight impact --dir .
```

Exit codes: `0` = no unresolved impacts (silent), `1` = unresolved impacts found (stderr text output). With `--hook`, appends a triage hint for AI agents.

### `review` — Structured Diff Review (One-Shot)

Integrates `context` (impact analysis), `cochange` (missing co-change detection), API surface diff (added/removed/modified public symbols), and dead symbol detection into a single command. Ideal for PR review or pre-merge checks.

```bash
# Auto-run git diff (recommended)
astro-sight review --dir . --git

# Staged changes
astro-sight review --dir . --git --staged

# Custom base ref
astro-sight review --dir . --git --base HEAD~3

# External patch file (useful when rename-aware diff is already generated)
astro-sight review --dir . --diff-file changes.patch

# Laravel project: exclude framework conventions (Controllers, migrations, etc.) from dead_symbols
astro-sight review --dir . --git --framework laravel

# Additional ad-hoc exclusions for dead_symbols detection
astro-sight review --dir . --git --exclude-dir generated --exclude-glob 'app/Legacy/**'
```

Output: JSON with `impact` (ContextResult), `missing_cochanges` (files expected to change together but absent from diff), `api_changes` (added/removed/modified public symbols), `dead_symbols` (public symbols with zero non-definition references in changed files).

`--framework` / `--exclude-dir` / `--exclude-glob` narrow the `dead_symbols` portion only; they share semantics with the same flags on `dead-code`. `review` always excludes `vendor/` / `tests/` / build artifacts from `dead_symbols`.

When `--git --base <rev>` is set, `review` uses the same base for both the diff and blame-backed `missing_cochanges`. This keeps multi-commit branch reviews aligned with the requested comparison range.

### `imports` — Import/Export Extraction

Extracts import/export relationships using language-specific tree-sitter queries. 14 languages (Bash excluded).

```bash
astro-sight imports --path <file>

# Batch mode
astro-sight imports --paths src/main.rs,src/lib.rs
```

Output: `imports` array with `src`, `ln`, `kind` (Import/Use/Include/Require), `ctx`.

### `symbols` — Symbol Extraction

Lists all function/class/struct/enum definitions in a file or directory. Default output is compact (name, kind, line only — no hash/range/doc) for token efficiency.

```bash
# Single file (compact: name, kind, line)
astro-sight symbols --path <file>

# Include docstrings in compact output
astro-sight symbols --path <file> --doc

# Full legacy output (hash, range, doc)
astro-sight symbols --path <file> --full

# Directory scan (NDJSON output, compact)
astro-sight symbols --dir <directory>

# Directory scan with glob filter
astro-sight symbols --dir <directory> --glob "**/*.rs"
```

### `sequence` — Mermaid Sequence Diagram

Generates a Mermaid sequence diagram from a file's call graph.

```bash
astro-sight sequence --path <file>
astro-sight sequence --path <file> --function main
```

Output: `diagram` (Mermaid text), `participants` (ordered list).

### `cochange` — Co-change Analysis

Finds files that frequently change together in git history.

```bash
astro-sight cochange --dir .
astro-sight cochange --dir . --file src/service.rs
astro-sight cochange --dir . --lookback 200 --min-confidence 0.3
```

Output: `entries` array with `file_a`, `file_b`, `confidence`.

### `ast` — AST Fragment Extraction

Extracts the AST at a specific position or range.

```bash
astro-sight ast --path <file> --line <n> --col <n>
astro-sight ast --path <file>  # full file, top-level nodes
```

### `lint` — AST Pattern Matching

Lint with custom YAML rules (tree-sitter query or text pattern).

```bash
astro-sight lint --path <file> --rules rules.yaml
```

### `dead-code` — Dead Code Detection

Finds exported symbols with zero non-definition references. With diff flags, limits scan to diff-related files; without diff, scans the entire project.

By default, package-manager trees (`vendor/`, `node_modules/`, `.venv/` 等), test directories (`tests/`, `Tests/`, `__tests__/`, `spec/`, `testdata/`), and build artifacts (`target/`, `dist/`, `build/`, `out/`) are excluded. Use `--include-vendor` / `--include-tests` / `--include-build` to opt back in.

```bash
# Full project scan
astro-sight dead-code --dir .

# Rust files only
astro-sight dead-code --dir . --glob "**/*.rs"

# Diff-related files only (git diff)
astro-sight dead-code --dir . --git

# Staged changes only
astro-sight dead-code --dir . --git --staged

# Framework preset: Laravel conventions (excludes migrations, Controllers, Middleware,
# FormRequest, Console Commands, GraphQL resolvers, Listeners, Providers, IDE helpers)
astro-sight dead-code --dir . --framework laravel

# Additional ad-hoc exclusions (directory name or glob pattern)
astro-sight dead-code --dir . --exclude-dir generated --exclude-glob 'app/Legacy/**'
```

Output: JSON with `dir`, `scanned_files` (count), `dead_symbols` array (`name`, `kind`, `file`). Symbols with duplicate names across files are conservatively skipped.

### `session` — NDJSON Batch Mode

For multiple queries in one process (avoids repeated startup):

```bash
echo '{"command":"refs","name":"MyType","dir":"src/"}
{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session
```

## Workflow Examples

### "Search for multiple identifiers" (INSTEAD of `Grep "FOO|Bar|baz"`)
```bash
astro-sight refs --names FOO,Bar,baz --dir .
```

### "I need several different astro-sight queries in one request"
```bash
echo '{"command":"symbols","path":"src/main.rs"}
{"command":"calls","path":"src/main.rs","function":"main"}
{"command":"context","dir":".","diff":"..."}' | astro-sight session
```

### "Before editing code" (run FIRST)
```bash
astro-sight context --dir . --git
```

### "After editing code" (check unresolved impacts)
```bash
astro-sight impact --dir . --git
```

### "How does this module work?"
```bash
astro-sight symbols --dir src/engine/       # All files in directory
astro-sight symbols --path src/engine/parser.rs  # Single file detail
astro-sight calls --path src/main.rs --function main
astro-sight sequence --path src/main.rs --function main
```

### "Is it safe to rename this function?"
```bash
astro-sight refs --name "old_name" --dir .    # See all usages
astro-sight calls --path file.rs --function old_name  # See callers
```

### "What does this PR break?"
```bash
git diff origin/main | astro-sight context --dir .
```

### "What changed together with this file recently?"
```bash
astro-sight cochange --dir . --file src/service.rs
```

### "Show me the call flow visually"
```bash
astro-sight sequence --path src/main.rs --function main
```

### "I need to enforce a repeated AST/text rule"
```bash
astro-sight lint --path src/main.rs --rules rules.yaml
```

### "Give me a full structured review of this diff"
```bash
astro-sight review --dir . --git
```

### "Find unused exported symbols in the project"
```bash
astro-sight dead-code --dir .
```

### "Are there dead symbols related to my changes?"
```bash
astro-sight dead-code --dir . --git
```

## Notes

- 17 languages: Rust, C, C++, Python, JavaScript, TypeScript, TSX, Go, PHP, Java, Kotlin, Swift, C#, Bash, Ruby, Zig, Xojo
- Xojo files (`.xojo_code`, `.xojo_window`, `.xojo_menu`, `.xojo_toolbar`, `.xojo_report`, `.rbbas`) use **case-insensitive identifier matching** (`myVar` and `MYVAR` are the same symbol)
- All output is compact JSON by default (short keys: `lang`, `ln`, `col`, `ctx`, `refs`, `src`, `def`/`ref`, `fn` etc.)
- Use `--pretty` (global flag) for human-readable formatted JSON output
- `refs` results include `ctx` (source line) — no need to Read files afterward
- `refs` respects `.gitignore` and uses bounded parallel scanning with fold/reduce aggregation
- Multiple symbol searches: use `refs --names` for batching; reserve `session` for mixed commands
- `session` supports `ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, `cochange` (note: `review` is CLI-only, not available in session mode)
- With `ASTRO_SIGHT_WORKSPACE`, session-relative `path` / `dir` values resolve from the workspace root, not the process cwd; invalid workspace values fail closed
- **Input validation**: Empty `--name`/`--names`, empty `--paths`/`--paths-file` are rejected with `INVALID_REQUEST` error. `--base` for `context`/`impact`/`review` rejects values starting with `-` (blocks option-style injection into `git diff` / `git show` / `git blame`)
- **Large repositories (10k+ source files)**: `review --dir .` runs `context` + `cochange` + API diff + dead-code in one process and is the heaviest command. On very large monorepos it can exhaust memory. Mitigations:
  - Narrow `--dir` to a module-level subtree (`--dir packages/server` instead of `--dir .`)
  - For diff-based commands (`review` / `impact` / `context` / `dead-code --git`), bound the window with `--base HEAD~N`
  - Prefer `--glob` to restrict to the primary language (e.g. `--glob '**/*.php'`)
  - Split `review` into per-command runs (`impact` → `dead-code` → `cochange`) if the unified run is too heavy
  - `refs` avoids per-file intermediate result retention, but very common symbols can still produce huge final output; narrow with `--glob` or lower `ASTRO_SIGHT_BATCH_WORKERS`
  - `symbols --path` is memory-light for single-file structure checks
