---
name: astro-sight
description: |
  Tree-sitter AST analysis CLI: call graphs, cross-file symbol references (definition/reference), and git diff impact analysis.
  Use for: call graph, symbol refs, impact analysis, refactoring safety, codebase exploration. Supports 14 languages.
---

# astro-sight

A tree-sitter based AST analysis CLI that gives AI agents structural understanding of codebases. It extracts call graphs, finds symbol references across files, and analyzes the impact of code changes — the kind of analysis that's hard to do with grep alone.

Supports 14 languages: Rust, C, C++, Python, JavaScript, TypeScript, TSX, Go, PHP, Java, Kotlin, Swift, C#, Bash.

## Commands

### `calls` — Call Graph Extraction

Extracts function call relationships from a source file. Use this when you need to understand what a function calls, or to map out the call chain in a file.

```bash
# All call edges in a file
astro-sight calls --path <file>

# Only calls made by a specific function
astro-sight calls --path <file> --function <function_name>
```

The output is JSON with a `calls` array. Each entry has `caller` (name + range), `callee` (name + range), and `call_site` (line + column). This is useful for understanding dependencies — if you're about to change a function, the callers tell you what might break.

### `refs` — Cross-File Symbol Reference Search

Finds all occurrences of a symbol name across a directory, distinguishing definitions from references. Uses `.gitignore`-aware directory walking and parallel file scanning.

```bash
# Find all references to a symbol
astro-sight refs --name <symbol_name> --dir <directory>

# Narrow down with a glob pattern
astro-sight refs --name <symbol_name> --dir <directory> --glob "**/*.rs"
```

The output JSON contains a `references` array with `path`, `line`, `column`, `context` (the source line), and `kind` ("definition" or "reference"). Definitions are sorted first. This is more precise than grep because it matches tree-sitter identifier nodes, not arbitrary text.

### `context` — Smart Context / Diff Impact Analysis

The most powerful command. Reads a unified diff from stdin and analyzes which symbols are affected, whether signatures changed, and which callers in the workspace are impacted. This is the CodeRabbit-style analysis that answers "what does this change break?".

```bash
# Analyze impact of uncommitted changes
git diff | astro-sight context --dir .

# Analyze impact since last commit
git diff HEAD~1 | astro-sight context --dir .

# Analyze staged changes
git diff --cached | astro-sight context --dir .

# Analyze a specific file's changes
git diff -- src/engine/symbols.rs | astro-sight context --dir .
```

The output JSON contains a `changes` array per file, each with:
- `hunks` — the diff hunk ranges
- `affected_symbols` — functions/structs/classes touched by the change, with `change_type` (added/modified/removed)
- `signature_changes` — detected function signature changes (old vs new)
- `impacted_callers` — functions in other files that call the changed symbols

### `symbols` — Symbol Extraction

Lists all function/class/struct/enum definitions in a file.

```bash
astro-sight symbols --path <file>
```

### `ast` — AST Fragment Extraction

Extracts the AST at a specific position or range, useful for understanding syntax structure.

```bash
astro-sight ast --path <file> --line <n> --col <n>
astro-sight ast --path <file>  # full file, top-level nodes
```

### `session` — NDJSON Streaming

For batch operations, pipe multiple JSON requests through a single process:

```bash
echo '{"command":"calls","path":"src/main.rs","function":"main"}
{"command":"refs","name":"MyType","dir":"src/"}' | astro-sight session
```

The `context` command in session mode takes the diff as a `diff` field instead of stdin:
```bash
echo '{"command":"context","dir":".","diff":"--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,4 @@\n+new line"}' | astro-sight session
```

## Workflow Patterns

### Code Review: "What does this PR change and what might it break?"

This is the primary workflow. Run `context` first to get the big picture, then drill into specifics with `calls` and `refs`.

```bash
# Step 1: Get the full impact analysis
git diff origin/main | astro-sight context --dir .

# Step 2: For each affected symbol, check its call graph
astro-sight calls --path src/engine/symbols.rs --function extract_symbols

# Step 3: Find all references to understand full usage
astro-sight refs --name "extract_symbols" --dir src/
```

### Refactoring Safety: "Is it safe to change this function's signature?"

```bash
# Find everywhere the function is referenced
astro-sight refs --name "process_data" --dir src/

# Check what calls it (to understand caller expectations)
astro-sight calls --path src/processor.rs --function process_data
```

### Codebase Exploration: "How does this module work?"

```bash
# List all symbols in the file
astro-sight symbols --path src/engine/parser.rs

# See what the main entry point calls
astro-sight calls --path src/main.rs --function main
```

## Interpreting Results

When presenting results to users:

- **`context` output**: Focus on `affected_symbols` and `impacted_callers` — these are the actionable insights. Signature changes are particularly important to highlight because they indicate breaking changes.
- **`calls` output**: Present as a dependency list — "function X calls: A, B, C". If filtering by function, this shows what that function depends on.
- **`refs` output**: Group by file and highlight definition vs reference. The definition tells you where to look for the implementation; references tell you the blast radius.

## Notes

- All output is JSON, easy to parse and chain with other tools
- The `context` command needs the workspace directory (`--dir`) to resolve file paths from the diff and search for impacted callers
- `refs` respects `.gitignore` automatically via the `ignore` crate
- `refs` uses `rayon` for parallel file scanning, so it's fast on large codebases
- Caching (`--no-cache` to disable) is available for `ast` and `symbols` commands
