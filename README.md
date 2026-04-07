<p align="center">
  <img src="docs/images/app.png" width="128" alt="astro-sight">
</p>

<h1 align="center">astro-sight</h1>

<p align="center">
  AI エージェント向け AST 情報生成 CLI。tree-sitter ベースの高速構文解析で、指定位置の AST 断片・シンボル定義・スニペットを JSON で返す。
</p>

<h3 align="center">Supported Languages</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/C-A8B9CC?logo=c&logoColor=white" alt="C">
  <img src="https://img.shields.io/badge/C++-00599C?logo=cplusplus&logoColor=white" alt="C++">
  <img src="https://img.shields.io/badge/Python-3776AB?logo=python&logoColor=white" alt="Python">
  <img src="https://img.shields.io/badge/JavaScript-F7DF1E?logo=javascript&logoColor=black" alt="JavaScript">
  <img src="https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&logoColor=white" alt="TypeScript">
  <img src="https://img.shields.io/badge/TSX-61DAFB?logo=react&logoColor=black" alt="TSX">
  <img src="https://img.shields.io/badge/Go-00ADD8?logo=go&logoColor=white" alt="Go">
  <img src="https://img.shields.io/badge/PHP-777BB4?logo=php&logoColor=white" alt="PHP">
  <img src="https://img.shields.io/badge/Java-ED8B00?logo=openjdk&logoColor=white" alt="Java">
  <img src="https://img.shields.io/badge/Kotlin-7F52FF?logo=kotlin&logoColor=white" alt="Kotlin">
  <img src="https://img.shields.io/badge/Swift-F05138?logo=swift&logoColor=white" alt="Swift">
  <img src="https://img.shields.io/badge/C%23-512BD4?logo=dotnet&logoColor=white" alt="C#">
  <img src="https://img.shields.io/badge/Bash-4EAA25?logo=gnubash&logoColor=white" alt="Bash">
  <img src="https://img.shields.io/badge/Ruby-CC342D?logo=ruby&logoColor=white" alt="Ruby">
</p>

## Install

### Homebrew (macOS/Linux)

```bash
brew install owayo/astro-sight/astro-sight
```

### From Source

```bash
git clone https://github.com/owayo/astro-sight.git
cd astro-sight
make install
```

### From GitHub Releases

Download the latest binary from [Releases](https://github.com/owayo/astro-sight/releases).

#### macOS (Apple Silicon)

```bash
curl -L https://github.com/owayo/astro-sight/releases/latest/download/astro-sight-aarch64-apple-darwin.tar.gz | tar xz
sudo mv astro-sight /usr/local/bin/
```

#### macOS (Intel)

```bash
curl -L https://github.com/owayo/astro-sight/releases/latest/download/astro-sight-x86_64-apple-darwin.tar.gz | tar xz
sudo mv astro-sight /usr/local/bin/
```

#### Linux (x86_64)

```bash
curl -L https://github.com/owayo/astro-sight/releases/latest/download/astro-sight-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv astro-sight /usr/local/bin/
```

#### Linux (ARM64)

```bash
curl -L https://github.com/owayo/astro-sight/releases/latest/download/astro-sight-aarch64-unknown-linux-gnu.tar.gz | tar xz
sudo mv astro-sight /usr/local/bin/
```

#### Windows

Download `astro-sight-x86_64-pc-windows-msvc.zip` from [Releases](https://github.com/owayo/astro-sight/releases), extract, and add to PATH.

## Usage

### グローバルオプション

```bash
# デフォルトは compact JSON（1行出力、AI エージェント向け）
astro-sight symbols --path src/main.rs

# 人間向け整形出力
astro-sight symbols --pretty --path src/main.rs
```

### ast - AST 断片抽出

```bash
# 指定位置の AST ノードを取得
astro-sight ast --path src/main.rs --line 10 --col 0

# 範囲指定
astro-sight ast --path src/main.rs --line 10 --col 0 --end-line 20 --end-col 0

# ファイル全体のトップレベル AST
astro-sight ast --path src/main.rs

# 深さとコンテキスト行数を指定
astro-sight ast --path src/main.rs --line 10 --col 0 --depth 5 --context 5
```

### symbols - シンボル抽出

```bash
# ファイル内の関数・構造体・クラス等を一覧（compact: name, kind(短縮形), ln のみ）
astro-sight symbols --path src/main.rs

# docstring 付き compact 出力
astro-sight symbols --path src/main.rs --doc

# 旧来の完全出力（hash, range, doc 全て含む）
astro-sight symbols --path src/main.rs --full

# ディレクトリ内の全ソースファイルのシンボルを NDJSON で出力
astro-sight symbols --dir src/

# glob でフィルタ
astro-sight symbols --dir src/ --glob "**/*.rs"
```

### calls - コールグラフ抽出

```bash
# ファイル内の全呼び出し関係を抽出
astro-sight calls --path src/main.rs

# 特定関数の呼び出し先のみ
astro-sight calls --path src/main.rs --function cmd_ast
```

compact 出力例（caller でグルーピング）:
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

`--pretty` で従来のフルフォーマット（caller/callee オブジェクト + call_site）を出力。

### refs - クロスファイル参照検索

```bash
# シンボル名でワークスペース内を検索
astro-sight refs --name "extract_symbols" --dir src/

# glob パターンでファイルを絞り込み
astro-sight refs --name "AstgenResponse" --dir src/ --glob "**/*.rs"

# 複数シンボルを一括検索（NDJSON 出力、1シンボル1行）
astro-sight refs --names "AppService,AstgenResponse" --dir src/
```

`--name` は空文字を受け付けない。`--names` も空要素のみ（例: `",,,"`）の場合は `INVALID_REQUEST` を返す。`--dir` にはディレクトリのみ指定でき、ファイルパスを渡した場合も `INVALID_REQUEST` を返す。

出力例:
```json
{
  "symbol": "extract_symbols",
  "refs": [
    { "path": "src/engine/symbols.rs", "ln": 9, "col": 7, "ctx": "pub fn extract_symbols(...)", "kind": "def" },
    { "path": "src/main.rs", "ln": 156, "col": 24, "ctx": "let syms = symbols::extract_symbols(...)", "kind": "ref" }
  ]
}
```

### context - スマートコンテキスト（diff → 影響分析）

unified diff を受け取り、変更の影響範囲を分析する。AI コードレビュー支援機能。

```bash
# git diff を自動取得して影響分析（推奨）
astro-sight context --dir . --git

# ステージ済み変更を分析
astro-sight context --dir . --git --staged

# カスタムベースを指定
astro-sight context --dir . --git --base HEAD~3

# stdin からパイプ
git diff HEAD~1 | astro-sight context --dir .

# インライン diff 文字列
astro-sight context --dir . --diff "$(git diff HEAD~1)"

# diff ファイルから読み込み
git diff HEAD~1 > /tmp/changes.diff
astro-sight context --dir . --diff-file /tmp/changes.diff
```

出力例:
```json
{
  "changes": [
    {
      "path": "src/engine/symbols.rs",
      "hunks": [{ "old_start": 10, "old_count": 5, "new_start": 10, "new_count": 8 }],
      "affected_symbols": [
        { "name": "extract_symbols", "kind": "function", "change_type": "modified" }
      ],
      "signature_changes": [
        { "name": "extract_symbols", "old_signature": "fn extract_symbols(...)", "new_signature": "fn extract_symbols(..., include_refs: bool)" }
      ],
      "impacted_callers": [
        { "path": "src/main.rs", "name": "cmd_symbols", "line": 154 }
      ]
    }
  ]
}
```

### impact - 未解決の影響検出（stop hook 用）

`context` の結果から、diff に含まれないファイルへの影響を「未解決」と判定する。AI エージェントの stop hook で使用し、未対応の影響先があればブロックして続行を促す。

```bash
# git diff を自動取得して未解決影響を検出（推奨）
astro-sight impact --dir . --git

# ステージ済み変更を検査
astro-sight impact --dir . --git --staged

# カスタムベースを指定
astro-sight impact --dir . --git --base HEAD~3

# stdin からパイプ
git diff HEAD~1 | astro-sight impact --dir .
```

- 未解決なし → exit 0（出力なし）
- 未解決あり → stderr にテキスト出力 + exit 1

出力例（exit 1 時）:
```
Unresolved impacts found:

src/engine/symbols.rs changed [extract_symbols]:
  → src/service.rs:89
  → src/main.rs:42
```

claw-hooks との連携例（`.claw-hooks.toml`）:
```toml
[[stop_hooks]]
commands = ["astro-sight impact --git --dir ."]
condition = { command_exists = "astro-sight" }
```

### review - 構造化 diff レビュー

`context` の影響分析に加えて、`cochange` による変更漏れ候補、公開 API 差分、死蔵シンボルを 1 回の実行でまとめて返す。PR レビューや pre-merge チェック向け。

```bash
# git diff を自動取得してレビュー（推奨）
astro-sight review --dir . --git

# ステージ済み変更をレビュー
astro-sight review --dir . --git --staged

# カスタムベースを指定
astro-sight review --dir . --git --base HEAD~3

# 既に生成済みの patch / PR diff を使う
astro-sight review --dir . --diff-file /tmp/pr.patch
```

出力例:
```json
{
  "impact": { "changes": [...] },
  "missing_cochanges": [
    { "file": "src/service.rs", "expected_with": "src/commands.rs", "confidence": 0.75 }
  ],
  "api_changes": {
    "added": [],
    "removed": [],
    "modified": [
      {
        "name": "greet",
        "kind": "function",
        "file": "src/new.rs",
        "old_signature": "pub fn greet() -> i32 {",
        "new_signature": "pub fn greet(name: &str) -> i32 {"
      }
    ]
  },
  "dead_symbols": []
}
```

### doctor - 対応言語チェック

```bash
astro-sight doctor
```

### session - NDJSON ストリーミング

```bash
echo '{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

stdin から NDJSON リクエストを受け取り、stdout に NDJSON レスポンスを返す。複数リクエストの連続処理に対応。`ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, `cochange` をサポートする。1行あたり 100MB（改行を除く生入力サイズ）を上限としている。`ASTRO_SIGHT_WORKSPACE` を指定した場合はそのディレクトリ配下だけを扱い、空文字・非 UTF-8・存在しないパスなどの不正な値は `INVALID_REQUEST` で終了する。

```bash
# calls コマンド
echo '{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session

# refs コマンド
echo '{"command":"refs","name":"AstgenResponse","dir":"src/"}' | astro-sight session

# context コマンド（diff を直接渡す）
echo '{"command":"context","dir":".","diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n+use new_mod;"}' | astro-sight session
```

`refs` を session で使う場合も `name` または `names` の指定が必須（空文字不可）。

### バッチ処理（ast, symbols, calls）

複数ファイルを一度に処理し、NDJSON（1ファイル1行）で出力。rayon による並列処理。

```bash
# カンマ区切りで複数ファイルを指定
astro-sight symbols --paths src/lib.rs,src/cli.rs,src/main.rs

# ファイルリストから読み込み
find src -name '*.rs' > /tmp/files.txt
astro-sight symbols --paths-file /tmp/files.txt

# バッチ ast / calls も同様
astro-sight ast --paths src/lib.rs,src/main.rs --depth 2
astro-sight calls --paths src/lib.rs,src/main.rs
```

`--paths` / `--paths-file` は 1 件以上の有効なパスが必要。空リストは `INVALID_REQUEST` を返す。

個別ファイルのエラーは行内 JSON エラーとして出力される（プロセスは成功終了）:
```jsonl
{"location":{"path":"src/lib.rs"},"language":"rust","symbols":[...]}
{"error":{"code":"FILE_NOT_FOUND","message":"File not found: nonexistent.rs"}}
```

### mcp - MCP サーバーモード

stdio 上で JSON-RPC 2.0 (Model Context Protocol) サーバーとして動作。Claude Desktop, Cursor 等から利用可能。

```bash
astro-sight mcp
```

公開ツール（11 種）:
- `ast_extract` - AST 断片抽出
- `symbols_extract` - シンボル抽出
- `calls_extract` - コールグラフ抽出
- `refs_search` - クロスファイル参照検索（単一シンボル）
- `refs_batch_search` - 複数シンボル一括参照検索
- `context_analyze` - diff 影響分析
- `imports_extract` - import/export 関係抽出
- `lint` - YAML ルールによる AST パターンマッチ
- `sequence_diagram` - Mermaid シーケンス図生成
- `cochange_analyze` - 共変更パターン検出
- `doctor` - 対応言語チェック

MCP クライアント設定例:
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

### エラー出力

エラーは JSON 形式で stdout に出力し、exit code 1 で終了:

```bash
$ astro-sight ast --path nonexistent.rs
{"error":{"code":"FILE_NOT_FOUND","message":"File not found: nonexistent.rs"}}
```

## Supported Languages (15)

| Language | Extension | Crate | Version |
|----------|-----------|-------|---------|
| <img src="https://img.shields.io/badge/-000000?logo=rust&logoColor=white" height="16"> Rust | `.rs` | `tree-sitter-rust` | 0.24 |
| <img src="https://img.shields.io/badge/-A8B9CC?logo=c&logoColor=white" height="16"> C | `.c`, `.h` | `tree-sitter-c` | 0.24 |
| <img src="https://img.shields.io/badge/-00599C?logo=cplusplus&logoColor=white" height="16"> C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx` | `tree-sitter-cpp` | 0.23 |
| <img src="https://img.shields.io/badge/-3776AB?logo=python&logoColor=white" height="16"> Python | `.py`, `.pyi` | `tree-sitter-python` | 0.25 |
| <img src="https://img.shields.io/badge/-F7DF1E?logo=javascript&logoColor=black" height="16"> JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | `tree-sitter-javascript` | 0.25 |
| <img src="https://img.shields.io/badge/-3178C6?logo=typescript&logoColor=white" height="16"> TypeScript | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-61DAFB?logo=react&logoColor=black" height="16"> TSX | `.tsx` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-00ADD8?logo=go&logoColor=white" height="16"> Go | `.go` | `tree-sitter-go` | 0.25 |
| <img src="https://img.shields.io/badge/-777BB4?logo=php&logoColor=white" height="16"> PHP | `.php`, `.phtml` | `tree-sitter-php` | 0.24 |
| <img src="https://img.shields.io/badge/-ED8B00?logo=openjdk&logoColor=white" height="16"> Java | `.java` | `tree-sitter-java` | 0.23 |
| <img src="https://img.shields.io/badge/-7F52FF?logo=kotlin&logoColor=white" height="16"> Kotlin | `.kt`, `.kts` | `tree-sitter-kotlin` | =0.3.5 * |
| <img src="https://img.shields.io/badge/-F05138?logo=swift&logoColor=white" height="16"> Swift | `.swift` | `tree-sitter-swift` | 0.7 |
| <img src="https://img.shields.io/badge/-512BD4?logo=dotnet&logoColor=white" height="16"> C# | `.cs` | `tree-sitter-c-sharp` | 0.23 |
| <img src="https://img.shields.io/badge/-4EAA25?logo=gnubash&logoColor=white" height="16"> Bash | `.sh`, `.bash`, `.zsh` | `tree-sitter-bash` | 0.25 |
| <img src="https://img.shields.io/badge/-CC342D?logo=ruby&logoColor=white" height="16"> Ruby | `.rb`, `.rake`, `.gemspec` | `tree-sitter-ruby` | [owayo/tree-sitter-ruby](https://github.com/owayo/tree-sitter-ruby) |

全言語で tree-sitter クエリによる精密なシンボル抽出に対応。

> **\* Kotlin バージョン固定について:** `tree-sitter-kotlin` 0.3.8 以降は `links = "tree-sitter"` を宣言しており、コアクレート `tree-sitter` 0.26 と Cargo の native library リンク名が競合してビルドできない。そのため `=0.3.5` に固定している。
>
> ```
> error: failed to select a version for `tree-sitter`.
>     ... required by package `tree-sitter-kotlin v0.3.8`
> package `tree-sitter` links to the native library `tree-sitter`,
> but it conflicts with a previous package which links to `tree-sitter` as well:
> package `tree-sitter v0.26.6`
> Only one package in the dependency graph may specify the same links value.
> ```

## Output Format

全コマンドの出力はデフォルト compact JSON（`--pretty` で整形）。compact モードではトークン削減のためキー名を短縮:

- `language` → `lang`（calls, imports, lint, sequence, compact ast/symbols）
- `location` → `path`（compact ast/symbols）
- `references` → `refs`、`line` → `ln`、`column` → `col`、`context` → `ctx`（refs）
- `source` → `src`（imports）
- `kind`: `"definition"` → `"def"`、`"reference"` → `"ref"`（refs）
- `SymbolKind`: `"function"` → `"fn"`、`"interface"` → `"iface"`、`"variable"` → `"var"` 等（compact symbols）
- `calls`: caller でグルーピング、callee は `{name, ln, col}` に簡略化

compact 出力例（ast/symbols）:
```json
{"path":"src/main.rs","lang":"rust","schema":{"range":"[startLine,startCol,endLine,endCol]"},"ast":[...]}
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"main","kind":"fn","ln":20}]}
```

`--full`/`--pretty` で従来のフルフォーマット（`location`, `language`, `hash`, `range` 等）を出力。
`version` フィールドは `doctor` と MCP `initialize` 応答のみ。

## Cache

ファイル内容の BLAKE3 ハッシュをキーとするコンテンツアドレスキャッシュ。ファイルが変更されなければ再解析をスキップし、変更されればハッシュが変わるため自動的に無効化される。

- **対象コマンド**: `ast`, `symbols`（単一ファイルモードのみ）
- **キャッシュキー**: `BLAKE3(ファイル内容)` + コマンド固有サフィックス（オプション組み合わせ別）
- **保存先**: `~/.cache/astro-sight/`
- **ディレクトリシャード**: ハッシュの先頭 2 文字でサブディレクトリを分割（例: `ab/cdef1234....symbols.json`）
- **`--pretty` 時はキャッシュをスキップ**（compact 出力のみキャッシュ）
- **`--no-cache`** で無効化可能

## Claude Code との連携

### スキルインストール

`skill-install` サブコマンドで [Claude Code](https://docs.anthropic.com/en/docs/claude-code/skills) / [Codex](https://developers.openai.com/codex/skills/) のスキルとして登録できます。

```bash
# Claude Code 用（~/.claude/skills/astro-sight/SKILL.md）
astro-sight skill-install claude

# Codex 用（~/.codex/skills/astro-sight/SKILL.md）
astro-sight skill-install codex
```

登録後は「コールグラフを調べて」「この関数の呼び出し元は？」「diff の影響範囲は？」等の質問で自動的に起動します。
PR や patch 全体をまとめて見たい場合は、`astro-sight review --dir . --git` まで含めて指示すると一括レビューに入りやすくなります。

### CLAUDE.md に追記して確実に使わせる

スキルだけでは Claude Code が Grep/Read にフォールバックすることがあります。
プロジェクトの `CLAUDE.md` またはグローバルの `~/.claude/CLAUDE.md` に以下を追記すると、構造分析時に astro-sight を優先的に使用するようになります:

```markdown
# Code Structure Analysis — astro-sight MANDATORY Rules

## STOP-AND-CHECK Rule (CRITICAL: Check BEFORE every Grep call)

**Before calling Grep, ask yourself**: "Does my search target contain code identifiers (function/class/variable/type/constant names)?"
- **YES → Use `astro-sight refs`** (Grep FORBIDDEN)
- **NO → Grep OK** (error messages, config values, TODOs, file paths, etc.)

⚠️ **Pipe-separated patterns**: `Grep "FOO|Bar|baz"` with code identifiers is also FORBIDDEN. Use `refs --names` instead.

This is a MANDATORY rule. astro-sight uses tree-sitter AST parsing — matches only identifier nodes, zero false positives from comments/strings.

## Decision Table

| Search Pattern | Correct Tool | Reason |
|---|---|---|
| `Grep "functionName"` | ❌ → `astro-sight refs --name functionName --dir .` | Code identifier |
| `Grep "ClassName"` | ❌ → `astro-sight refs --name ClassName --dir .` | Code identifier |
| `Grep "MY_CONST\|OtherVar"` | ❌ → `astro-sight refs --names MY_CONST,OtherVar --dir .` | Pipe-separated identifiers |
| `Grep "import.*module"` | ❌ → `astro-sight imports --path file` | Import analysis |
| `Grep "TODO"` | ✅ Grep OK | Non-code search |
| `Grep "error message text"` | ✅ Grep OK | String literal search |
| `Grep "config_key"` | ✅ Grep OK | Config value search |

## Workflow Rules (MANDATORY for code changes)
- **Before editing code**: Run `astro-sight context --dir . --git` to check impact
- **After editing code**: Run `astro-sight impact --dir . --git` to detect unresolved impacts
- **One-shot diff review**: Run `astro-sight review --dir . --git` for impact + cochange + API diff + dead symbols
- **Understanding a file**: Run `astro-sight symbols --path <file>` to see structure
- **Understanding a directory**: Run `astro-sight symbols --dir <dir>` to see all symbols
- **Finding symbol usage**: Run `astro-sight refs` (Grep FORBIDDEN)
- **Finding multiple symbols**: Run `astro-sight refs --names sym1,sym2 --dir .`
- **Who calls this function?**: Run `astro-sight calls --path <file> --function <name>`
- **What does this file import?**: Run `astro-sight imports --path <file>`
- **Files that change together**: Run `astro-sight cochange --dir . --file <file>`
- **Visualize call flow**: Run `astro-sight sequence --path <file> --function <name>`

## Command Quick Reference

```
astro-sight refs --name <symbol> --dir .           # Symbol reference search (REPLACES Grep for identifiers)
astro-sight refs --names sym1,sym2 --dir .         # Batch symbol search (REPLACES Grep "FOO|Bar")
astro-sight symbols --path <file>                  # File structure overview
astro-sight symbols --dir <dir>                    # Directory structure overview (NDJSON)
astro-sight calls --path <file> --function <name>  # Caller/callee relationships
astro-sight context --dir . --git                  # Change impact analysis (run BEFORE editing code)
astro-sight impact --dir . --git                   # Detect unresolved impacts (run AFTER editing code)
astro-sight review --dir . --git                   # Structured diff review (impact + cochange + API + dead)
astro-sight imports --path <file>                  # Import relationships
astro-sight sequence --path <file>                 # Call flow visualization
astro-sight cochange --dir .                       # Co-change patterns
```

## Efficiency Rules
- **`refs` results include `context` (source line)** → No need for additional Read/Grep
- **Batch multiple symbol searches with `refs --names`** (simpler than session)
- **Use Read for surrounding context when editing** (astro-sight shows 1 line only)
```

### MCP サーバーとして登録

Claude Desktop や Cursor 等の MCP クライアントから利用する場合:

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

## License

MIT
