# astro-sight

AI エージェント向け AST 情報生成 CLI。tree-sitter ベースの高速構文解析で、指定位置の AST 断片・シンボル定義・スニペットを JSON で返す。

## Install

```bash
cargo install --path .
```

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
# ファイル内の関数・構造体・クラス等を一覧
astro-sight symbols --path src/main.rs
```

### calls - コールグラフ抽出

```bash
# ファイル内の全呼び出し関係を抽出
astro-sight calls --path src/main.rs

# 特定関数の呼び出し先のみ
astro-sight calls --path src/main.rs --function cmd_ast
```

出力例:
```json
{
  "version": "0.1.0",
  "language": "rust",
  "calls": [
    {
      "caller": { "name": "cmd_ast", "range": {...} },
      "callee": { "name": "read_file", "range": {...} },
      "call_site": { "line": 63, "column": 24 }
    }
  ]
}
```

### refs - クロスファイル参照検索

```bash
# シンボル名でワークスペース内を検索
astro-sight refs --name "extract_symbols" --dir src/

# glob パターンでファイルを絞り込み
astro-sight refs --name "AstgenResponse" --dir src/ --glob "**/*.rs"
```

出力例:
```json
{
  "version": "0.1.0",
  "symbol": "extract_symbols",
  "references": [
    { "path": "src/engine/symbols.rs", "line": 9, "column": 7, "context": "pub fn extract_symbols(...)", "kind": "definition" },
    { "path": "src/main.rs", "line": 156, "column": 24, "context": "let syms = symbols::extract_symbols(...)", "kind": "reference" }
  ]
}
```

### context - スマートコンテキスト（diff → 影響分析）

unified diff を受け取り、変更の影響範囲を分析する。AI コードレビュー支援機能。

```bash
# git diff から影響分析（stdin）
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
  "version": "0.1.0",
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

### doctor - 対応言語チェック

```bash
astro-sight doctor
```

### session - NDJSON ストリーミング

```bash
echo '{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

stdin から NDJSON リクエストを受け取り、stdout に NDJSON レスポンスを返す。複数リクエストの連続処理に対応。全コマンド（ast, symbols, doctor, calls, refs, context）をサポート。

```bash
# calls コマンド
echo '{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session

# refs コマンド
echo '{"command":"refs","name":"AstgenResponse","dir":"src/"}' | astro-sight session

# context コマンド（diff を直接渡す）
echo '{"command":"context","dir":".","diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n+use new_mod;"}' | astro-sight session
```

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

個別ファイルのエラーは行内 JSON エラーとして出力される（プロセスは成功終了）:
```jsonl
{"version":"0.1.0","location":{"path":"src/lib.rs"},"language":"rust","symbols":[...]}
{"error":{"code":"FILE_NOT_FOUND","message":"File not found: nonexistent.rs"}}
```

### mcp - MCP サーバーモード

stdio 上で JSON-RPC 2.0 (Model Context Protocol) サーバーとして動作。Claude Desktop, Cursor 等から利用可能。

```bash
astro-sight mcp
```

公開ツール:
- `ast_extract` - AST 断片抽出
- `symbols_extract` - シンボル抽出
- `calls_extract` - コールグラフ抽出
- `refs_search` - クロスファイル参照検索
- `context_analyze` - diff 影響分析
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

## Supported Languages (14)

| Language | Extension | Crate | Version |
|----------|-----------|-------|---------|
| Rust | `.rs` | `tree-sitter-rust` | 0.24 |
| C | `.c`, `.h` | `tree-sitter-c` | 0.24 |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx` | `tree-sitter-cpp` | 0.23 |
| Python | `.py`, `.pyi` | `tree-sitter-python` | 0.25 |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | `tree-sitter-javascript` | 0.25 |
| TypeScript | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` | 0.23 |
| TSX | `.tsx` | `tree-sitter-typescript` | 0.23 |
| Go | `.go` | `tree-sitter-go` | 0.25 |
| PHP | `.php`, `.phtml` | `tree-sitter-php` | 0.24 |
| Java | `.java` | `tree-sitter-java` | 0.23 |
| Kotlin | `.kt`, `.kts` | `tree-sitter-kotlin` | =0.3.5 * |
| Swift | `.swift` | `tree-sitter-swift` | 0.7 |
| C# | `.cs` | `tree-sitter-c-sharp` | 0.23 |
| Bash | `.sh`, `.bash`, `.zsh` | `tree-sitter-bash` | 0.25 |

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

## tree-sitter Grammar 一覧（crates.io）

### 未対応（tree-sitter 0.26 互換・追加可能）

| Crate | Version | 備考 |
|-------|---------|------|
| `tree-sitter-ruby` | 0.23.1 | |
| `tree-sitter-scala` | 0.24.0 | |
| `tree-sitter-haskell` | 0.23.1 | |
| `tree-sitter-lua` | 0.5.0 | |
| `tree-sitter-elixir` | 0.3.4 | |
| `tree-sitter-elm` | 5.9.0 | |
| `tree-sitter-ocaml` | 0.24.2 | `LANGUAGE_OCAML` / `LANGUAGE_OCAML_INTERFACE` |
| `tree-sitter-r` | 1.2.0 | |
| `tree-sitter-yaml` | 0.7.2 | |
| `tree-sitter-zig` | 1.1.2 | |
| `tree-sitter-html` | 0.23.2 | シンボル抽出の効果薄 |
| `tree-sitter-css` | 0.25.0 | シンボル抽出の効果薄 |
| `tree-sitter-json` | 0.24.8 | シンボル抽出の効果薄 |
| `tree-sitter-md` | 0.5.3 | シンボル抽出の効果薄 |
| `tree-sitter-regex` | 0.25.0 | 単体ファイルの需要なし |
| `tree-sitter-sequel` | 0.3.11 | SQL。シンボル抽出の効果薄 |

### 未対応（非互換）

| Crate | Version | 問題 |
|-------|---------|------|
| `tree-sitter-toml` | 0.20 | Legacy API（extern C ブリッジ必要） |
| `tree-sitter-kotlin` | >=0.3.8 | `links = "tree-sitter"` 競合 |
| `tree-sitter-dart` | 0.0.4 | 旧 API、メンテナンス停止 |

## Output Format

全コマンドの出力はデフォルト compact JSON（`--pretty` で整形）。主要フィールド:

```json
{
  "version": "0.1.0",
  "location": { "path": "src/main.rs", "line": 10, "column": 0 },
  "language": "rust",
  "hash": "blake3-content-hash",
  "ast": [...],
  "symbols": [...],
  "snippet": ">10 | fn main() {\n 11 |     ...",
  "diagnostics": []
}
```

## Cache

`~/.cache/astro-sight/` に BLAKE3 コンテンツハッシュベースのキャッシュを保存。`--no-cache` で無効化可能。

## Claude Code との連携

### スキル（自動）

`cargo install --path .` でインストール済みであれば、[Claude Code のスキル](https://docs.anthropic.com/en/docs/claude-code/skills)として自動登録されます（`~/.claude/skills/astro-sight/SKILL.md`）。
「コールグラフを調べて」「この関数の呼び出し元は？」「diff の影響範囲は？」等の質問で自動的に起動します。

### CLAUDE.md に追記して確実に使わせる

スキルだけでは Claude Code が Grep/Read にフォールバックすることがあります。
プロジェクトの `CLAUDE.md` またはグローバルの `~/.claude/CLAUDE.md` に以下を追記すると、構造分析時に astro-sight を優先的に使用するようになります:

```markdown
# コード構造分析について
以下のような構造的なコード分析が必要な場合は、Grep/Read ではなく astro-sight スキルを使用すること:
- コールグラフ / 呼び出し元・呼び出し先の調査
- クロスファイルのシンボル参照検索（definition/reference の区別付き）
- git diff の影響分析（影響を受けるシンボル、シグネチャ変更、影響を受ける呼び出し元）
- リファクタリング前の安全性確認（関数のシグネチャ変更が他にどう影響するか）
astro-sight は tree-sitter ベースの AST 解析ツールで、テキスト検索では不可能な構造的関係を取得できる
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
