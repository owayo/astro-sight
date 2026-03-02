---
name: astro-sight
description: "STOP before using Grep for code identifiers (including pipe-separated patterns like FOO|Bar). Use `astro-sight refs` — AST-based, zero false positives. Use `symbols` to understand file structure. Use `git diff | astro-sight context` before editing code. 14 languages."
---

# astro-sight

## When to Use (Decision Checklist)

**Before running Grep, ask: "Does my search contain code identifiers?"**

- Searching for a **function, class, variable, type, constant, or method name**? → `astro-sight refs` (NOT Grep)
- Searching with **pipe-separated identifiers** like `FOO|Bar|baz`? → `astro-sight refs --names FOO,Bar,baz --dir .` (NOT Grep)
- Need to **understand a file's structure** (functions, classes, structs)? → `astro-sight symbols --path <file>`
- Need to **understand a directory's structure**? → `astro-sight symbols --dir <dir>`
- Need to know **who calls a function** or **what a function calls**? → `astro-sight calls`
- Want to know **what a code change breaks**? → `astro-sight context --dir . --git`
- Need to see **what a file imports**? → `astro-sight imports`
- Need a **visual call flow diagram**? → `astro-sight sequence`
- Need to find **files that usually change together**? → `astro-sight cochange`

**Grep is fine for**: error messages, config values, TODO comments, file path patterns — anything that is NOT a code identifier.

## Quick Reference (Top Commands by Usage)

```bash
# 1. Find all references to a symbol (REPLACES Grep for identifiers)
astro-sight refs --name <symbol_name> --dir .

# 2. Batch symbol search (REPLACES Grep "FOO|Bar|baz")
astro-sight refs --names sym1,sym2,sym3 --dir .

# 3. Analyze what a diff breaks (run BEFORE editing code)
astro-sight context --dir . --git

# 4. Understand file structure (functions, classes, structs)
astro-sight symbols --path <file>

# 5. Understand directory structure (all files, NDJSON)
astro-sight symbols --dir <dir> --glob "**/*.rs"

# 6. Show caller/callee relationships
astro-sight calls --path <file> --function <function_name>

# 7. Batch operations — multiple queries in one process
echo '{"command":"refs","name":"Sym1","dir":"."}
{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

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

# Pipe from git diff (legacy)
git diff | astro-sight context --dir .
```

Output: `changes` array per file with `affected_symbols`, `signature_changes`, `impacted_callers`.

### `imports` — Import/Export Extraction

Extracts import/export relationships using language-specific tree-sitter queries. 13 languages (Bash excluded).

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

### "Before editing code" (run FIRST)
```bash
astro-sight context --dir . --git
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

## Notes

- 14 languages: Rust, C, C++, Python, JavaScript, TypeScript, TSX, Go, PHP, Java, Kotlin, Swift, C#, Bash
- All output is compact JSON (short keys: `lang`, `ln`, `col`, `ctx`, `refs`, `src`, `def`/`ref`, `fn` etc.)
- `refs` results include `ctx` (source line) — no need to Read files afterward
- `refs` respects `.gitignore` and uses parallel scanning
- Multiple symbol searches: use `refs --names` for batching (or `session` for mixed commands)
- **Input validation**: Empty `--name`/`--names`, empty `--paths`/`--paths-file` are rejected with `INVALID_REQUEST` error
