# Usage

How to use each subcommand, with output examples. The commands that take a diff (`context` / `impact` / `review` / `dead-code` / `cochange`) are in [Diff Analysis](diff-analysis.md), output formats in [Output Format](output-format.md), and the configuration file in [Configuration](configuration.md).

## Global Options

```bash
# Compact JSON by default (one line, for AI agents)
astro-sight symbols --path src/main.rs

# Indented output for humans (JSON only)
astro-sight symbols --pretty --path src/main.rs

# TOON output (the same content in fewer tokens)
astro-sight symbols --path src/main.rs --format toon

# Pick whichever of json / toon is estimated to use fewer tokens
astro-sight symbols --path src/main.rs --format auto
```

`--format json|toon|auto` selects the output format. The default is `json`, and `format` in `config.toml` changes the default too (precedence: **CLI `--format` > `config.toml` > `json`**). Details are in [Output Format](output-format.md).

## Review Flow for Agents

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

## ast: Extract AST Fragments

```bash
# The AST node at a position
astro-sight ast --path src/main.rs --line 10 --col 0

# A range
astro-sight ast --path src/main.rs --line 10 --col 0 --end-line 20 --end-col 0

# The top-level AST of the whole file
astro-sight ast --path src/main.rs

# Set the depth and the number of context lines
astro-sight ast --path src/main.rs --line 10 --col 0 --depth 5 --context 5
```

`text` and `snippet` are truncated at 256 characters, so the response size stays under control even on the huge lines of minified or generated code.

## symbols: Extract Symbols

```bash
# List the functions, structs, classes, and so on in a file (compact output)
astro-sight symbols --path src/main.rs

# Compact output with docstrings
astro-sight symbols --path src/main.rs --doc

# The full output (includes hash, range, and doc)
astro-sight symbols --path src/main.rs --full

# Symbols of every source file in a directory, as NDJSON
astro-sight symbols --dir src/

# Filter with a glob
astro-sight symbols --dir src/ --glob "**/*.rs"
```

Compact output:
```json
{
  "path": "src/service.rs",
  "lang": "rust",
  "symbols": [
    { "name": "AppService", "kind": "struct", "ln": 23 },
    { "name": "default", "kind": "fn", "ln": 40, "cx": 1, "cn": "AppService" }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | Symbol name |
| `kind` | Short form of the kind (`fn` / `method` / `class` / `struct` / `enum` / `iface` / `trait` / `var` / `const` / `mod` / `import` / `type` / `field` / `param`) |
| `ln` | Line of the definition (0-indexed) |
| `cx` | Cyclomatic complexity, for functions and methods only (1 + the number of branch nodes). The counting rules are below the table |
| `cn` | Name of the container that encloses the symbol. For a method inside `impl Default for AppService`, it is `AppService`. Use it to tell methods with the same name apart |
| `doc` | Docstring (only with `--doc`) |

`cx` follows McCabe's counting rules, so the same logic gets the same value across languages. Whether a catch-all arm such as `default:` or `_ =>` counts depends on the grammar, though, and the value can be off by ±1.

- Branches inside nested functions / closures, local functions, and `async` blocks are not counted
- For switch/match, only the arms count, not the statement itself
- A lone `else` (other than `else if`) is not counted
- The ternary operator counts; the null-coalescing operator does not
- Expression forms of branching (C# switch expressions, PHP `match`) and Ruby's modifier-form guard clauses (`return 0 if x`) get the same value as the statement forms

### Extracted Declarations

Besides declarations of functions, methods, classes, structs, enums, types, and so on, the following forms are each extracted as a symbol. Extracted symbols are also covered by the public API diff (api.add / api.rm / api.mod in `review`) and by dead-code (exceptions are noted per item).

- JavaScript / TypeScript destructuring (`export const { auth, signOut } = NextAuth()` / `const [first, second] = pair()`) yields one symbol per bound name. Property keys (`key` in `{ key: renamed }`) are not included, and `ln` is the line of each name. Local destructuring inside a function is extracted just like a normal `const`
- `var` declarations, generator functions (`function*`), and `abstract class`
- Java / C# `record` (treated as `class`) and Go type aliases (`type A = B`)
- Rust trait methods (including required methods without a body). They inherit the trait's visibility, so removing a method of a `pub trait` or changing its signature shows up as api.rm / api.mod. They are not covered by dead-code

TypeScript interface / abstract methods and Go interface methods are not extracted at the moment.

### Excluding and Reporting Generated Files

`refs --dir` and `symbols --dir` leave out of the scan, by default, files that match either of the following:

- The file name is that of a minified file, a bundle, or an IDE helper
- A generated-file declaration comment (`@generated`, `Code generated by ...`, `DO NOT EDIT THIS FILE`, and so on) appears within the first 4KiB and 40 lines of the file

Only the declaration form at the start of a comment line counts as a marker, so string literals and ordinary comments that merely mention "automatically generated comments" do not trigger it.

When at least one file is excluded, a machine-readable `skipped` always appears on stdout; no `skipped` means nothing was excluded. `paths` holds the first 50 paths in a deterministic sort order. The total is always in `generated`, and `truncated` tells whether paths were left out.

```json
{"symbol":"foo","refs":[],"skipped":{"generated":2,"paths":["gen/a.rs","gen/b.rs"]}}
```

`symbols --dir` prints NDJSON, so it adds one control record with the same `skipped` object at the end. `refs --names` with several names keeps its "one record per symbol" form and adds the shared `skipped` once, to the first record. Batch responses of session / MCP keep their root array.

To scan without the exclusion, pass the global option `--include-generated`.

```bash
astro-sight --include-generated refs --name foo --dir .
astro-sight --include-generated symbols --dir src
```

`skip_generated = false` in the configuration file does the same, and so does the environment variable `ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1`, kept for backward compatibility. When the last segment of a glob names a concrete file, as in `**/parser.c`, the explicit request is honored and that file is scanned. An ordinary filtered scan such as `**/*.c` keeps the default exclusion.

## calls: Extract the Call Graph

```bash
# Every call relation in a file
astro-sight calls --path src/main.rs

# Only the callees of one function
astro-sight calls --path src/main.rs --function cmd_ast
```

Compact output (grouped by caller):
```json
{
  "lang": "rust",
  "calls": [
    {
      "caller": "cmd_ast",
      "range": [63, 0, 120, 1],
      "callees": [
        { "name": "read_file", "ln": 65, "col": 24 },
        { "name": "CacheStore::hash", "ln": 66, "col": 16 }
      ]
    }
  ]
}
```

With `--pretty`, the output switches to the complete form, where callers and callees are objects and call sites are included.

`--function <name>` narrows the output to the **calls going out of** `<name>` (its callees). To find who calls `<name>` (its callers), use `refs --name <name>` instead of `calls` (it includes calls from other files, and `ctx` holds the calling line).

## imports: Extract Import Dependencies

```bash
# The modules a file refers to
astro-sight imports --path src/main.ts

# Several files, processed in input order
astro-sight imports --paths src/main.ts,src/worker.ts
```

The import / use / include / require statements of all 16 languages are extracted from the tree-sitter AST, with `src`, `ln`, `kind`, and `ctx`. For JavaScript / TypeScript / TSX, besides ordinary import statements and `require()`, `import("./module")` and `` import(`./module`) `` without substitutions are recognized too. Template literals containing `${expr}` are left out because their target cannot be determined statically, and for call forms only the first argument is treated as the dependency.

## refs: Search References Across Files

```bash
# Search the workspace by symbol name
astro-sight refs --name "extract_symbols" --dir src/

# Narrow the files with a glob pattern
astro-sight refs --name "AstgenResponse" --dir src/ --glob "**/*.rs"

# Search several symbols at once (NDJSON, one line per symbol)
astro-sight refs --names "AppService,AstgenResponse" --dir src/

# Change the output limits (default 100 results / 3,000 tokens)
astro-sight refs --name "new" --dir . --max-results 500
astro-sight refs --name "new" --dir . --max-results unlimited --token-budget unlimited
```

Output (`astro-sight refs --name extract_symbols --dir .`):
```json
{
  "symbol": "extract_symbols",
  "refs": [
    { "path": "src/engine/symbols/mod.rs", "ln": 107, "col": 7, "ctx": "pub fn extract_symbols(...)", "kind": "def" },
    { "path": "src/commands/api_changes/exported.rs", "ln": 45, "col": 39, "ctx": "let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;", "kind": "ref" }
  ]
}
```

`path` is relative to `--dir`. `--name` does not accept an empty string, and `--names` with only empty elements (for example `",,,"`) returns `INVALID_REQUEST`. `--dir` accepts only a directory; a file path also returns `INVALID_REQUEST`.

### Output Limits and `result_summary`

A frequent identifier returns thousands of results in one call, and the token cost balloons however well the representation is optimized (measured when the limits were introduced: `refs --name new --dir .` on this repository returned 1,846 results ≈ 68,000 tokens). An agent has no way of knowing before the call that an identifier is frequent, so a default limit of **100 results / an estimated 3,000 tokens** applies (the same query then fits in about 2,600 tokens).

- **The analysis does not stop.** Everything is analyzed so that `total` is exact, and only the output is cut. Stopping the scan at a count would lose both the exact total and the breakdown of what was omitted
- `result_summary` appears only when at least one result was omitted. A normal query that stays within the limits gets no `result_summary`, and its output is byte for byte the same as with the limits removed (`unlimited`)
- The limits apply only to the output. `dead-code`, the API diff, and the hook decisions use the full internal results
- Both `--max-results` and `--token-budget` accept `unlimited`. The minimum of `--token-budget` is 256 (below that, the summary itself does not fit)
- `refs --names` shares one budget across the whole call, handed out round-robin. A limit per name would make the total grow with the number of names, and filling from the front would let one frequent name eat the whole budget and leave 0 results for the rest
- The same limits apply to `max_results` / `token_budget` in `session` (a number or `"unlimited"`) and to `refs_search` / `refs_batch_search` in MCP
- **A budget that cannot be met is reported.** The summary has a fixed cost, so with many names and a small budget, the budget is exceeded even with 0 results shown. `result_summary.budget_exceeded: true` then signals that you need a larger budget, fewer names, or a narrower `--glob`. In `refs --names`, the per-name breakdown of omissions (the rollup) is kept within "the budget of the whole call ÷ the number of names", so adding names does not multiply the summary

```json
{
  "symbol": "new",
  "refs": [ /* results within the limits */ ],
  "result_summary": {
    "shown": 64, "total": 1846, "omitted": 1782,
    "limited_by": ["max_results", "token_budget"],
    "limits": { "max_results": 100, "token_budget": 3000 },
    "complete_input": true,
    "by_kind": { "ref": 1782 },
    "by_lang": { "rust": 1776, "php": 5, "ruby": 1 },
    "files": [ { "path": "src/commands/tests/review_hook.rs", "count": 326 } ],
    "other_files": { "files": 128, "count": 1231 },
    "rollup_truncated": { "shown": 5, "available": 133 }
  }
}
```

`by_kind` / `by_lang` / `files` describe **only the omitted results** (a distribution that included the shown results would not let you recover the omitted part by subtraction). `by_lang` helps in repositories that mix several languages (polyglot). Bare-name matches pour in across languages (measured: in one polyglot repository, 2,521 of the 2,522 references to the name `search` were in other languages), and seeing the language mix tells you whether to narrow the search again with `--glob`. `files` has its own limit; the excess is folded into `other_files` and reported in `rollup_truncated` (so that the summary does not become a second output explosion).

`complete_input` tells whether `total` counts every input that could have been analyzed. It is false when files were excluded from the scan as generated, or failed to read or parse, which means `total` is not the true total for the whole repository.

Both single and multi-symbol searches merge results directly with a fold/reduce per worker, without keeping an intermediate `Vec` per file for all files. For a symbol with a very large number of references the output itself is large, so narrow the languages with `--glob`, or lower the number of parallel workers with `ASTRO_SIGHT_BATCH_WORKERS` when needed (the default is the number of available CPUs).

A multi-symbol search (`refs --names`) runs the directory walk, the Aho-Corasick (AC) scan, and parsing once per file, regardless of the number of names. The patterns normally go into a single AC automaton (its size is nearly linear in the number of patterns; measured: 50,000 patterns ≈ 8MB). Only large inputs above `ASTRO_SIGHT_REFS_BATCH_CHUNK` (default 100,000) split the AC, and even then the file walk and parsing happen once and the results are the same regardless of the chunk size.

The auxiliary reference scans for Angular templates and Android XML are limited to 2MB and 1MB per file respectively. If a file grows after its metadata was checked, reading stops at the limit + 1 byte and the file is skipped.

For C/C++ `struct` / `class` / `union` / `enum` tag names, only definitions with a body count as Definitions, and `struct X` in `struct X *p`, `sizeof(struct X)`, casts, parameter types, and member declarations counts as a Reference. Standalone forward declarations count as neither a ref nor a def, so dead-code is unlikely to report a type tag in use as dead.

## doctor: Check Language Support

```bash
astro-sight doctor
```

`doctor` checks which languages are available, and reports the ABI version of each tree-sitter language.

## session: NDJSON Streaming

```bash
echo '{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

`session` reads NDJSON requests from stdin and writes NDJSON responses to stdout, handling any number of requests in a row. It supports `ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, and `cochange`. Each line is limited to 100MB (the raw input size without the newline). When `ASTRO_SIGHT_WORKSPACE` is set, only paths under that directory are handled, and relative `path` / `dir` values in requests are resolved from the workspace root. An invalid workspace value (an empty string, non-UTF-8, a path that does not exist, and so on) ends the session with `INVALID_REQUEST`.

```bash
# calls
echo '{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session

# refs
echo '{"command":"refs","name":"AstgenResponse","dir":"src/"}' | astro-sight session

# context (pass the diff directly; zsh's echo expands \n and breaks the JSON, so use printf)
printf '%s\n' '{"command":"context","dir":".","diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n+use new_mod;"}' | astro-sight session
```

`refs` in a session also requires `name` or `names` (not an empty string).

## Batch Processing (ast, symbols, calls, imports, lint, sequence)

These commands process several files at once and print NDJSON (one line per file). A dedicated rayon pool processes the files in parallel while keeping the input order. By default the number of workers is the smaller of the available CPUs and 4, so that peak RSS does not grow with the number of CPUs even when tree-sitter parsers keep working memory for huge files in thread-local storage. A positive integer in `ASTRO_SIGHT_BATCH_WORKERS` changes the parallelism, up to the number of available CPUs. Results not yet printed are kept within a window of 8 times the number of workers, so peak RSS does not grow with the number of inputs. When stdout is closed, processing stops at the current window and the remaining files are not analyzed.

```bash
# Several files, comma-separated
astro-sight symbols --paths src/lib.rs,src/cli.rs,src/main.rs

# Read the list from a file
find src -name '*.rs' > /tmp/files.txt
astro-sight symbols --paths-file /tmp/files.txt

# Batch ast / calls / imports / lint / sequence work the same way
astro-sight ast --paths src/lib.rs,src/main.rs --depth 2
astro-sight calls --paths src/lib.rs,src/main.rs
astro-sight imports --paths src/lib.rs,src/main.rs
astro-sight sequence --paths src/lib.rs,src/main.rs --function main
```

`--paths` / `--paths-file` need at least one valid path; an empty list returns `INVALID_REQUEST`. `--paths-file` is read with a 100MB limit.

An error for an individual file is printed as a JSON error on its line (the process still succeeds):
```jsonl
{"path":"src/lib.rs","lang":"rust","symbols":[...]}
{"error":{"code":"FILE_NOT_FOUND","message":"File not found: nonexistent.rs"}}
```

## mcp: MCP Server Mode

`mcp` runs a JSON-RPC 2.0 (Model Context Protocol) server over stdio, for clients such as Claude Desktop and Cursor. The current directory at startup is the workspace, and files outside it are rejected with `PATH_OUT_OF_BOUNDS`. Relative paths are resolved from there too, so start it with the repository you want to analyze as the current directory.

```bash
astro-sight mcp
```

Tools (11):
- `ast_extract` - extract AST fragments
- `symbols_extract` - extract symbols
- `calls_extract` - extract the call graph
- `refs_search` - search references across files (one symbol)
- `refs_batch_search` - search references of several symbols at once
- `context_analyze` - analyze the impact of a diff
- `imports_extract` - extract import/export relations
- `lint` - AST pattern matching with YAML rules
- `sequence_diagram` - generate a Mermaid sequence diagram
- `cochange_analyze` - detect co-change patterns
- `doctor` - check language support

MCP client configuration:
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

## Error Output

Errors are printed to stdout as JSON, and the process exits with code 1:

```bash
astro-sight ast --path nonexistent.rs
```

```json
{"error":{"code":"FILE_NOT_FOUND","message":"File not found: nonexistent.rs"}}
```

When a downstream command ends first and closes the stdout pipe, as in `astro-sight symbols --dir src | head`, astro-sight exits 0 without printing a panic. This keeps ordinary paging and sampling on the command line working; real analysis errors return a JSON error and exit code 1.
