# AI Agent Integration

There are 4 ways to have an AI agent such as Claude Code or Codex use astro-sight: register it as a skill, write rules in `CLAUDE.md` / `AGENTS.md`, stop the agent on unresolved impacts with a Stop hook, and register it as an MCP server. They can be combined.

## Installing the Skill

The `skill-install` subcommand registers astro-sight as a skill for [Claude Code](https://docs.anthropic.com/en/docs/claude-code/skills) or [Codex](https://developers.openai.com/codex/skills/).

```bash
# For Claude Code (~/.claude/skills/astro-sight/SKILL.md)
astro-sight skill-install claude

# For Codex (~/.codex/skills/astro-sight/SKILL.md)
astro-sight skill-install codex
```

Once registered, the skill starts on questions such as "trace the call graph", "who calls this function?", or "what does this diff affect?". It does not always start, though; to make sure the agent uses it, see "Making Agents Use It Through CLAUDE.md / AGENTS.md" below. Rules of thumb:

- To look at a whole PR or patch at once, mention `astro-sight review --dir . --git` in the instruction; the agent then tends to start with a single review
- Right before calling `grep` / `rg`, check whether the search pattern contains even one identifier such as a function, type, or constant name. If it does, use `astro-sight refs --name <symbol> --dir .` or `refs --names` instead, to avoid accidental matches in comments and strings. Decide by the pattern itself, not by the file type or the task around it
- To check imports, callees, or the call flow after reading the structure with `symbols`, put `symbols` and `imports` / `calls` / `sequence` into one `session` from the start. It starts fewer processes and keeps steps from being skipped
- When call order matters, or the chain of callers and callees goes 3 or more levels deep, check the branches and hand-offs with `sequence --path <file> --function <name>` in addition to the `calls` list
- Turn review points you use repeatedly into AST / text rules for `lint`. Check related files that were not changed with `missing_cochanges` in `review` or with `cochange --paths <file>` first

## Making Agents Use It Through CLAUDE.md / AGENTS.md

With only the skill, Claude Code / Codex can still fall back to Grep / Read. Adding the following to a project's `CLAUDE.md` / `AGENTS.md`, or to the global `~/.claude/CLAUDE.md`, makes the agent prefer astro-sight for structure analysis:

````markdown
# Code Structure Analysis — astro-sight MANDATORY Rules

## STOP-AND-CHECK Rule (CRITICAL: Check BEFORE every Grep/grep/rg call)

**Immediately before every Grep, `grep`, or `rg` call, ask yourself**: "Does my search target contain code identifiers (function/class/variable/type/constant/method names)?" Classify the search pattern itself; do not infer from the file type or the surrounding task.
- **YES → Use `astro-sight refs`** (Grep, `grep`, `rg` ALL FORBIDDEN)
- **NO → Grep OK** (error messages, config values, TODOs, file paths, etc.)

This applies to EVERY supported language — including Zig, Swift, C#, Ruby. Never assume a language is unsupported and fall back to Grep.

This rule also applies inside shell commands: wrapping `grep` / `rg` in Bash is not an exception.

⚠️ **Pipe-separated patterns**: `Grep "FOO|Bar|baz"` with code identifiers is also FORBIDDEN. Use `refs --names` instead.

This is a MANDATORY rule. astro-sight uses tree-sitter AST parsing — matches only identifier nodes, zero false positives from comments/strings.

## Decision Table

| Search Pattern | Correct Tool | Reason |
|---|---|---|
| `Grep "functionName"` | ❌ → `astro-sight refs --name functionName --dir .` | Code identifier |
| `Grep "ClassName"` | ❌ → `astro-sight refs --name ClassName --dir .` | Code identifier |
| `Grep "MY_CONST\|OtherVar"` | ❌ → `astro-sight refs --names MY_CONST,OtherVar --dir .` | Pipe-separated identifiers |
| `Grep "import.*module"` | ❌ → `astro-sight imports --path file` | Import analysis |
| `grep/rg "identifier"` | ❌ → `astro-sight refs` | CLI grep/rg is also forbidden for identifiers |
| `grep "name" one/file.ts` (single file) | ❌ → `astro-sight refs --name name --dir . --glob one/file.ts` | Single-file identifier search is still identifier search |
| `grep -rn "Foo" src/`, `grep -rn "Foo" --include=*.tsx .` | ❌ → `astro-sight refs --name Foo --dir . --glob 'src/**'` | Recursive identifier search |
| `grep -n "Foo" -A 20 file.ts` (wants surrounding lines) | ❌ → `refs` for the exact hit lines, then Read those offsets | `-A`/`-B` is not a reason to fall back to grep |
| `cargo test 2>&1 \| grep "^error"` | ✅ grep OK | **Piped output filter — not searching files.** astro-sight cannot replace this |
| `Grep "TODO"` | ✅ Grep OK | Non-code search |
| `Grep "error message text"` | ✅ Grep OK | String literal search |
| `Grep "config_key"` | ✅ Grep OK | Config value search |

## Workflow Rules (MANDATORY for code changes)
- **Reviewing a diff / PR (START HERE)**: Run `astro-sight review --dir . --git` for impact + cochange + API diff + dead symbols before any piecemeal analysis
- **Before changing a function/type**: Run `astro-sight refs --name <symbol> --dir .` to list every call site first (`context` / `impact --git` only see an existing diff — on a clean tree they return nothing)
- **Mid-edit, before touching more files**: Run `astro-sight context --dir . --git` to see what the diff so far breaks
- **After editing code**: Run `astro-sight impact --dir . --git` to detect unresolved impacts
- **Understanding a file**: Run `astro-sight symbols --path <file>` to see structure
- **Understanding a directory**: Run `astro-sight symbols --dir <dir>` to see all symbols
- **Exact AST node / parse debug**: Run `astro-sight ast --path <file> --line <n> --col <n>`
- **Finding symbol usage**: Run `astro-sight refs` (Grep FORBIDDEN)
- **Finding multiple symbols**: Run `astro-sight refs --names sym1,sym2 --dir .`
- **Who calls this function?**: Run `astro-sight refs --name <name> --dir .` (`ctx` shows each call site, across files). `calls --function <name>` answers the opposite question — what `<name>` itself calls
- **What does this file import?**: Run `astro-sight imports --path <file>`
- **Files that change together**: Run `astro-sight cochange --dir . --paths <file>` (or `--git --base <rev>` to derive from a diff)
- **Visualize call flow**: When execution order matters or the flow spans 3+ caller/callee interactions, run `astro-sight sequence --path <file> --function <name>`
- **Find dead code**: Run `astro-sight dead-code --dir .` or `--git` for diff-scoped
- **Enforce repeated structural rules**: Run `astro-sight lint --path <file> --rules rules.yaml`
- **Multiple mixed queries in one run**: If `symbols` will be followed by `imports` / `calls` / `sequence`, start with NDJSON `astro-sight session`

## Command Quick Reference

```
astro-sight refs --name <symbol> --dir .           # Symbol reference search (REPLACES Grep for identifiers)
astro-sight refs --name <symbol> --dir . --max-results unlimited  # opt out of the default 100-ref cap
astro-sight refs --names sym1,sym2 --dir .         # Batch symbol search (REPLACES Grep "FOO|Bar")
astro-sight symbols --path <file>                  # File structure overview
astro-sight symbols --dir <dir>                    # Directory structure overview (NDJSON)
astro-sight ast --path <file> --line <n> --col <n> # Exact AST node at cursor (parse debug)
astro-sight calls --path <file> --function <name>  # What a function calls (callees; for callers use refs)
astro-sight context --dir . --git                  # Impact of the current diff (needs uncommitted changes)
astro-sight impact --dir . --git                   # Detect unresolved impacts (run AFTER editing code)
astro-sight review --dir . --git                   # Structured diff review (impact + cochange + API + dead)
astro-sight dead-code --dir . --git                # Find dead/unreferenced exported symbols
astro-sight imports --path <file>                  # Import relationships
astro-sight sequence --path <file>                 # Call flow visualization
astro-sight cochange --dir . --paths <file>        # Files that usually change together (or --git)
astro-sight lint --path <file> --rules rules.yaml  # Enforce repeated structural rules
astro-sight session                                # NDJSON multi-query batch (stdin→stdout)
```

## Efficiency Rules
- **`refs` results include `ctx` (source line)** → No need for additional Read/Grep
- **Batch multiple symbol searches with `refs --names`** (simpler than session)
- **For very common symbols, combine `--glob` with `ASTRO_SIGHT_BATCH_WORKERS`** to keep output size and peak RSS bounded
- **Need surrounding lines (the `grep -A/-B` habit)?** → run `refs` first, then Read at the hit lines (astro-sight shows 1 line only)
- **Do not repeat a zero-result identifier search with Grep/rg**; a zero-result AST query is still an analysis result
````

## Stopping on Unresolved Impacts with a Stop Hook

`astro-sight impact --dir . --git` exits 1 while callers outside the diff are left unresolved. Registered as a Stop hook of an AI agent, it keeps the agent from finishing until the affected code is fixed. A claw-hooks configuration and what happens when the command is registered directly in the Claude Code hooks are described in the `impact` section of [Diff Analysis](diff-analysis.md).

## Registering the MCP Server

For MCP clients such as Claude Desktop or Cursor:

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

## Usage Statistics

`tools/usage-stats` is a helper tool that aggregates astro-sight's adoption rate and the distribution of its subcommands from the Claude Code / Codex logs.

A use is counted only when `astro-sight` appears as a command executed in a shell and a known subcommand can be extracted. Path strings such as `/skills/astro-sight/SKILL.md`, mentions in prompts, and check runs without a subcommand such as `astro-sight --version` / `astro-sight --help` are not counted. The subcommand actually executed is extracted even with global flags such as `--pretty` / `--debug` / `--config <path>` or a wrapper such as `/usr/bin/time -o <file> astro-sight ...` in between. Codex logs are parsed in both the older form (`function_call` / `exec_command`) and the current form (`tools.exec_command` inside `custom_tool_call` / `exec`), and command examples embedded in JavaScript strings or comments are not counted as executions. `wait`, which is used for automatic continuation, is not a choice of code analysis or editing, so it is left out of both the denominator of the adoption rate and the tool distribution.

```bash
cargo run --manifest-path tools/usage-stats/Cargo.toml -- --json --days 1
```
