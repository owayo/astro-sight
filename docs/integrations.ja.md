# AI エージェントとの連携

Claude Code や Codex のような AI エージェントに astro-sight を使わせる方法をまとめる。スキルとして登録する、`CLAUDE.md` / `AGENTS.md` に規則を書く、Stop hook で未解決の影響を止める、MCP サーバーとして登録する、の 4 通りがあり、組み合わせて使える。

## スキルインストール

`skill-install` サブコマンドで [Claude Code](https://docs.anthropic.com/en/docs/claude-code/skills) / [Codex](https://developers.openai.com/codex/skills/) のスキルとして登録できる。

```bash
# Claude Code 用（~/.claude/skills/astro-sight/SKILL.md）
astro-sight skill-install claude

# Codex 用（~/.codex/skills/astro-sight/SKILL.md）
astro-sight skill-install codex
```

登録後は、「コールグラフを調べて」「この関数の呼び出し元は？」「diff の影響範囲は？」などの質問でスキルが起動する。ただし必ず起動するとは限らないので、確実に使わせたい場合は後述の「CLAUDE.md / AGENTS.md に追記して確実に使わせる」を参照。使い分けの目安は次のとおり。

- PR や patch 全体をまとめて見たい場合は、`astro-sight review --dir . --git` まで含めて指示すると一括レビューに入りやすい
- `grep` / `rg` を呼ぶ直前に、検索パターン自体が関数名・型名・定数名などの識別子を 1 つでも含むかを確かめる。含むなら `astro-sight refs --name <symbol> --dir .` か `refs --names` に置き換え、コメントや文字列への偶然の一致を避ける。判断はファイル種別や周辺のタスクではなく、パターンそのもので行う
- `symbols` で構造を読んだあとに import・呼び出し先・呼び出しの流れを確かめるなら、最初から `symbols` と `imports` / `calls` / `sequence` を `session` にまとめる。プロセスの起動を減らしつつ、手順の漏れを防げる
- 呼び出し順序が重要な場合や、caller / callee の連鎖が 3 段以上になる場合は、`calls` の一覧に加えて `sequence --path <file> --function <name>` で分岐と受け渡しの順序を確認する
- 同じレビュー観点を繰り返し使うなら、`lint` で AST / text のルールにする。関連ファイルの変更漏れは、`review` の `missing_cochanges` または `cochange --paths <file>` で先に確認する

## CLAUDE.md / AGENTS.md に追記して確実に使わせる

スキルだけでは、Claude Code / Codex が Grep / Read にフォールバックすることがある。プロジェクトの `CLAUDE.md` / `AGENTS.md`、またはグローバルの `~/.claude/CLAUDE.md` に以下を追記すると、構造分析では astro-sight を優先して使うようになる:

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

## Stop hook で未解決の影響を止める

`astro-sight impact --dir . --git` は、diff の外に未解決の呼び出し元が残っていれば exit 1 を返す。AI エージェントの Stop hook に登録すると、影響先を直すまで作業を終えさせない。claw-hooks での設定例と、Claude Code の hooks に直接登録したときの扱いは、[diff の解析](diff-analysis.ja.md) で `impact` を説明した節に書いた。

## MCP サーバーとして登録

Claude Desktop や Cursor などの MCP クライアントから利用する場合:

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

## 利用状況の分析

`tools/usage-stats` は、Claude Code / Codex の利用ログから astro-sight の採用率とサブコマンドの分布を集計する補助ツールである。

`astro-sight` がシェル上の実行コマンドとして現れ、既知のサブコマンドを抽出できた場合だけを採用として数える。`/skills/astro-sight/SKILL.md` のようなパス文字列、プロンプト内での言及、`astro-sight --version` / `astro-sight --help` のようなサブコマンドなしの確認起動は数えない。`--pretty` / `--debug` / `--config <path>` などのグローバルフラグや、`/usr/bin/time -o <file> astro-sight ...` のようなラッパーを挟んでも、実際に実行されたサブコマンドを抽出する。Codex のログは従来形式（`function_call` / `exec_command`）と現行形式（`custom_tool_call` / `exec` 内の `tools.exec_command`）の両方を解析し、JavaScript の文字列・コメントに埋め込まれたコマンド例は実行として数えない。自動継続に使われる `wait` はコード分析や編集の選択ではないため、採用率の分母とツール分布から除外する。

```bash
cargo run --manifest-path tools/usage-stats/Cargo.toml -- --json --days 1
```
