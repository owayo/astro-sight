---
name: astro-sight
description: >-
  tree-sitter AST でコード構造を解析する CLI。コード識別子 (関数/クラス/変数/型/メソッド名)
  を探すときは Grep でなく必ずこれ — refs --name/--names。diff/PR レビューは review
  --git、編集前後の影響分析は context/impact。dead-code/symbols/calls/imports/
  sequence/lint/session も提供。識別子検索・シンボル参照・呼び出し関係・構造把握・コードレビューの場面で発動。
allowed-tools: Bash(astro-sight:*)
---

# astro-sight

tree-sitter AST-based code structure CLI. The primary **Grep replacement for code identifiers** (`refs`), plus diff review (`review`), impact analysis (`context` / `impact`), dead-code detection, and structural queries. All output is compact JSON.

## When to Use (Decision Checklist)

**Before running Grep, ask: "Does my search contain code identifiers?"** If yes → astro-sight, not Grep.

| Need | Command |
|---|---|
| Find a function / class / variable / type / constant / method name | `refs --name <sym>` (pipe-separated `FOO`/`Bar` → `refs --names FOO,Bar`) |
| Review a diff / PR / bug-fix end-to-end | `review --dir . --git` (external patch: `--diff-file <patch>`) |
| What a change breaks (before editing) | `context --dir . --git` |
| Unresolved impacts (after editing) | `impact --dir . --git` |
| Dead (unreferenced) exported symbols | `dead-code --dir .` (diff-scoped: `--git`) |
| File / directory structure | `symbols --path <file>` / `symbols --dir <dir>` |
| Exact syntax node at a cursor, or parse-error debug | `ast --path <file> --line <n> --col <n>` |
| Who calls a function / what it calls | `calls --path <file> --function <name>` |
| What a file imports | `imports --path <file>` |
| Visual call-flow diagram | `sequence --path <file> --function <name>` |
| Files that usually change together | `cochange --dir . --paths <file>` |
| Repeated AST/text policy | `lint --path <file> --rules rules.yaml` |
| 2+ mixed queries in one process | `session` (NDJSON) |

**Grep is fine for**: error messages, config values, TODO comments, file-path patterns — anything that is NOT a code identifier.

## Quick Reference

```bash
astro-sight review --dir . --git                   # 1. one-shot diff/PR review
astro-sight context --dir . --git                  # 2. what a change breaks (before editing)
astro-sight impact --dir . --git                   # 3. unresolved impacts (after editing)
astro-sight dead-code --dir . --git                # 4. dead exported symbols
astro-sight refs --name <symbol> --dir .           # 5. references (REPLACES Grep for identifiers)
astro-sight refs --names sym1,sym2 --dir .         # 6. batch symbol search (REPLACES Grep "FOO|Bar")
astro-sight symbols --path <file>                  # 7. file structure
astro-sight calls --path <file> --function <name>  # 8. caller/callee relationships
astro-sight imports --path <file>                  # 9. imports/exports
echo '{"command":"refs","name":"S","dir":"."}
{"command":"symbols","path":"src/main.rs"}' | astro-sight session   # 10. batch mixed queries
```

## Commands

### `refs` — Cross-File Symbol Search (Use Instead of Grep)

The primary Grep replacement. Matches only tree-sitter identifier nodes — no false positives from comments, strings, or partial matches.

```bash
astro-sight refs --name <symbol> --dir <directory>
astro-sight refs --name <symbol> --dir <directory> --glob "**/*.rs"   # narrow by glob
astro-sight refs --names sym1,sym2,sym3 --dir <directory>             # multiple symbols (NDJSON, one line each)
```

Output: `refs` array with `path`, `ln`, `col`, `ctx` (source line), `kind` (`def`/`ref`). `ctx` shows the source line — no need to Read files afterward. For very common symbols, narrow with `--glob` or lower `ASTRO_SIGHT_BATCH_WORKERS`.

### `calls` — Call Graph Extraction

```bash
astro-sight calls --path <file>                    # all call edges in a file
astro-sight calls --path <file> --function <name>  # only calls made by one function
```

Output (compact): `calls` array grouped by `caller`, each with `range` and `callees` (`name`, `ln`, `col`). `--pretty` for full format.

### `context` — Diff Impact Analysis

Reads a unified diff and finds affected symbols, signature changes, and impacted callers. Answers "what does this change break?".

```bash
astro-sight context --dir . --git                      # auto git diff (recommended)
astro-sight context --dir . --git --staged             # staged changes
astro-sight context --dir . --git --base HEAD~3        # custom base ref
astro-sight context --dir . --diff-file changes.patch  # diff file (also: --diff "<str>", or pipe `git diff`)
```

Output: `changes` per file with `affected_symbols`, `signature_changes`, `impacted_callers`. Vendor/build-artifact exclusion applies — see Notes.

### `impact` — Unresolved Impact Detection (Stop Hook)

Uses `context` internally, then flags impacts whose callers live in files NOT included in the diff. Designed for AI agent stop hooks.

```bash
astro-sight impact --dir . --git            # auto git diff (recommended)
astro-sight impact --dir . --git --staged   # staged changes
astro-sight impact --dir . --git --hook     # appends triage hint on detection
```

Exit codes: `0` = no unresolved impacts (silent), `1` = unresolved impacts found (stderr). `--hook` appends a triage hint for AI agents.

### `review` — Structured Diff Review (One-Shot)

Integrates `context` (impact) + `cochange` (missing co-change) + API surface diff (added/removed/modified public symbols) + dead symbol detection. Ideal for PR review or pre-merge checks.

```bash
astro-sight review --dir . --git
astro-sight review --dir . --git --base HEAD~3
astro-sight review --dir . --diff-file changes.patch                       # external rename-aware patch
astro-sight review --dir . --git --framework laravel                       # exclude framework conventions from dead_symbols
astro-sight review --dir . --git --exclude-dir generated --exclude-glob 'app/Legacy/**'
```

Output: `impact` (ContextResult), `missing_cochanges`, `api_changes` (added/removed/modified public symbols), `dead_symbols`.

- `api_changes.compatible_modified`: signature-string changes that keep existing call-site compatibility — React HOC wrap, unreferenced object-member removal, trailing optional/default params on TS/TSX top-level functions, trailing kwonly+default / positional-default params on Python top-level functions & module-level methods (`trailing_optional_params`). Treated as informational, not `--hook` blocking; decorator diffs / duplicate same-name defs stay conservatively blocking.
- `--framework` filters `dead_symbols` only. `--exclude-dir` / `--exclude-glob` affect both impact and `dead_symbols` (same meaning as on `dead-code`). `review` always excludes vendor / tests / build from `dead_symbols`.
- `--git --base <rev>` uses the same base for the diff and for blame-backed `missing_cochanges`.
- If all changed files are lexer-only languages (e.g. Xojo), `impact` / `api_changes` / `dead_symbols` come back empty — use `symbols` / `refs` / `dead-code` per file instead.
- CLI-only (not available in `session`).

### `imports` — Import/Export Extraction

Language-specific tree-sitter queries. 14 languages (Bash excluded).

```bash
astro-sight imports --path <file>
astro-sight imports --paths src/main.rs,src/lib.rs   # batch
```

Output: `imports` array with `src`, `ln`, `kind` (Import/Use/Include/Require), `ctx`.

### `symbols` — Symbol Extraction

Lists function/class/struct/enum definitions. Compact by default (name, kind, line) for token efficiency.

```bash
astro-sight symbols --path <file>            # single file
astro-sight symbols --path <file> --doc      # include docstrings
astro-sight symbols --path <file> --full     # full legacy output (hash, range, doc)
astro-sight symbols --dir <directory>        # directory scan (NDJSON)
astro-sight symbols --dir <directory> --glob "**/*.rs"
```

### `sequence` — Mermaid Sequence Diagram

```bash
astro-sight sequence --path <file>
astro-sight sequence --path <file> --function main
```

Output: `diagram` (Mermaid text), `participants` (ordered list).

### `cochange` — Co-change Analysis

Blame-based: starts from source files (auto-derived from `git diff` or explicit), runs `git blame` on changed lines to get the latest-modifying commits, then aggregates co-occurring files from each commit's diff-tree.

```bash
astro-sight cochange --dir . --git --base HEAD~5                   # auto-derive from diff
astro-sight cochange --dir . --paths src/service.rs               # explicit sources
astro-sight cochange --dir . --git --base HEAD~10 --rename --copy  # track renames/copies
astro-sight cochange --dir . --git --base HEAD~5 --no-smoothing    # raw confidence (default prior alpha=1.0, beta=8.0)
```

Output: `entries` with `file_a`, `file_b`, `co_changes`, `confidence`, `denominator` (|C|), `score` (smoothed); `commits_analyzed` reports |C|. Requires `--git` or `--paths` / `--paths-file`. `--min-confidence` must be finite in `0.0..=1.0`; smoothing priors finite non-negative. Default `--git` source collection skips vendor / node_modules / dist / lock / minified assets; explicit `--paths` are kept as-is.

### `ast` — AST Fragment Extraction

```bash
astro-sight ast --path <file> --line <n> --col <n>   # node at position/range
astro-sight ast --path <file>                        # full file, top-level nodes
```

### `lint` — AST Pattern Matching

Lint with custom YAML rules (tree-sitter query or text pattern).

```bash
astro-sight lint --path <file> --rules rules.yaml
```

### `dead-code` — Dead Code Detection

Exported symbols with zero non-definition references. Diff flags limit the scan to diff-related files; without a diff, scans the whole project. Package-manager trees, test dirs, and build artifacts are excluded by default (`--include-vendor` / `--include-tests` / `--include-build` to opt back in).

```bash
astro-sight dead-code --dir .
astro-sight dead-code --dir . --glob "**/*.rs"
astro-sight dead-code --dir . --git                # diff-related files only
astro-sight dead-code --dir . --git --staged
astro-sight dead-code --dir . --framework laravel  # Laravel conventions (migrations, Controllers, Middleware, ...)
astro-sight dead-code --dir . --framework nextjs   # auto-detected when package.json has a `next` dep
astro-sight dead-code --dir . --exclude-dir generated --exclude-glob 'app/Legacy/**'
```

Output: `dir`, `scanned_files`, `dead_symbols` (`name`, `kind`, `file`). Duplicate-named symbols across files are conservatively skipped. Test-framework conventions (PHPUnit `*Test`/`*TestCase` + `testXxx`/`setUp`; Python unittest/pytest; Angular lifecycle hooks like `ngOnInit`) are auto-excluded. A bin-only Rust crate's `pub fn` is excluded from `review`'s `api_changes` (unreachable from outside the crate).

### `session` — NDJSON Batch Mode

Multiple queries in one process (avoids repeated startup):

```bash
echo '{"command":"refs","name":"MyType","dir":"src/"}
{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session
```

Supports `ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, `cochange` (note: `review` is CLI-only).

## Workflow Examples

Single-command uses are in Quick Reference above; these are multi-command flows.

### "How does this module work?"
```bash
astro-sight symbols --dir src/engine/               # all files in directory
astro-sight calls --path src/main.rs --function main
astro-sight sequence --path src/main.rs --function main
```

### "Is it safe to rename this function?"
```bash
astro-sight refs --name old_name --dir .              # all usages
astro-sight calls --path file.rs --function old_name  # callers
```

### "What changed together with this file recently?"
```bash
astro-sight cochange --dir . --paths src/service.rs
```

### "Several different queries in one request"
```bash
echo '{"command":"symbols","path":"src/main.rs"}
{"command":"calls","path":"src/main.rs","function":"main"}
{"command":"context","dir":".","diff":"..."}' | astro-sight session
```

## Notes

- **16 languages**: Rust, C, C++, Python, JavaScript, TypeScript, TSX, Go, PHP, Java, Kotlin, Swift, C#, Bash, Ruby, Zig.
- Compact JSON by default (short keys: `ln`, `col`, `ctx`, `refs`, `src`, `def`/`ref`, `fn`...). Use `--pretty` (global) for human-readable output.
- `refs` respects `.gitignore`; results include `ctx` (source line) so no follow-up Read is needed. Use `refs --names` for symbol-only batches, `session` for mixed commands.
- **Vendor/build exclusion** (`context` / `impact` / `review`): cross-file ref search skips package-manager trees (`vendor/`, `node_modules/`, `.venv/`, `Pods/`, `Carthage/`...) and build artifacts (`target/`, `build/`, `dist/`, `.build/`, `DerivedData/`, `.next/`, `bin/`, `obj/`...) so generic method names (`new`, `save`, `find`) don't flood `impacted_callers`. `ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1` opts back in; `.gitignore` / hidden exclusions are independent and always on. For non-default vendored trees (`pjproject-2.15/`, `third_party/`...) pass `--exclude-dir <NAME>` / `--exclude-glob <PATTERN>` (workspace-relative, negative-override); invalid globs fail up-front with `INVALID_REQUEST`.
- **Input validation**: empty `--name` / `--names` / `--paths` / `--paths-file` rejected with `INVALID_REQUEST`; `--paths-file` capped at 100MB; `cochange` rejects out-of-range `--min-confidence` / negative smoothing priors; `--base` rejects values starting with `-` (blocks git option injection).
- With `ASTRO_SIGHT_WORKSPACE`, session-relative `path` / `dir` resolve from the workspace root (invalid values fail closed). stdout broken pipes are handled gracefully (`symbols --dir src | head` won't panic).
- **Large repos (10k+ files)**: `review --dir .` is the heaviest command (context + cochange + API diff + dead-code in one process) and can exhaust memory. Narrow `--dir` to a subtree, bound diff commands with `--base HEAD~N`, restrict with `--glob`, or split `review` into per-command runs (`impact` → `dead-code` → `cochange`). `symbols --path` is memory-light.
