---
name: astro-sight
description: "STOP before using Grep for code identifiers (including pipe-separated patterns like FOO|Bar). Use `refs` for identifiers, `symbols`/`calls`/`imports` for structure, `ast` for exact syntax, `context`/`impact` around edits, and `session`/`sequence`/`cochange`/`lint` when you need mixed queries, flow, hotspots, or repeated rules."
---

# astro-sight

## When to Use (Decision Checklist)

**Before running Grep, ask: "Does my search contain code identifiers?"**

- Searching for a **function, class, variable, type, constant, or method name**? → `astro-sight refs` (NOT Grep)
- Searching with **pipe-separated identifiers** like `FOO|Bar|baz`? → `astro-sight refs --names FOO,Bar,baz --dir .` (NOT Grep)
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

**Grep is fine for**: error messages, config values, TODO comments, file path patterns — anything that is NOT a code identifier.

## Quick Reference (Top Commands by Usage)

```bash
# 1. Find all references to a symbol (REPLACES Grep for identifiers)
astro-sight refs --name <symbol_name> --dir .

# 2. Batch symbol search (REPLACES Grep "FOO|Bar|baz")
astro-sight refs --names sym1,sym2,sym3 --dir .

# 3. Understand file structure (functions, classes, structs)
astro-sight symbols --path <file>

# 4. Analyze what a diff breaks (run BEFORE editing code)
astro-sight context --dir . --git

# 5. Show caller/callee relationships
astro-sight calls --path <file> --function <function_name>

# 6. Extract imports/exports
astro-sight imports --path <file>

# 7. Detect unresolved impacts after edits
astro-sight impact --dir . --git

# 8. Batch operations — multiple queries in one process
echo '{"command":"refs","name":"Sym1","dir":"."}
{"command":"symbols","path":"src/main.rs"}' | astro-sight session

# 9. Visualize call flow
astro-sight sequence --path src/main.rs --function main

# 10. Check change hotspots
astro-sight cochange --dir . --file src/service.rs

# 11. Repeated AST/text checks
astro-sight lint --path <file> --rules rules.yaml

# 12. Structured review (impact + cochange + API diff + dead symbols)
astro-sight review --dir . --git
```

## Low-Adoption But Useful

- Need the **exact AST node** at a cursor/range, or want to confirm whether a parse error is structural? → `astro-sight ast --path <file> --line <n> --col <n>`
- Need **2+ mixed astro-sight queries** in one loop and want to avoid repeated startup cost? → `astro-sight session`
- Need to check a **repeated rule** like banned APIs, required patterns, or AST-based policy? → `astro-sight lint --path <file> --rules rules.yaml`

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
```

Output: JSON with `impact` (ContextResult), `missing_cochanges` (files expected to change together but absent from diff), `api_changes` (added/removed/modified public symbols), `dead_symbols` (public symbols with zero non-definition references in changed files).

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

## Notes

- 15 languages: Rust, C, C++, Python, JavaScript, TypeScript, TSX, Go, PHP, Java, Kotlin, Swift, C#, Bash, Ruby
- All output is compact JSON by default (short keys: `lang`, `ln`, `col`, `ctx`, `refs`, `src`, `def`/`ref`, `fn` etc.)
- Use `--pretty` (global flag) for human-readable formatted JSON output
- `refs` results include `ctx` (source line) — no need to Read files afterward
- `refs` respects `.gitignore` and uses parallel scanning
- Multiple symbol searches: use `refs --names` for batching; reserve `session` for mixed commands
- `session` supports `ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, `cochange` (note: `review` is CLI-only, not available in session mode)
- **Input validation**: Empty `--name`/`--names`, empty `--paths`/`--paths-file` are rejected with `INVALID_REQUEST` error
