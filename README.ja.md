<p align="center">
  <img src="docs/images/app.png" width="128" alt="ASTro-sight">
</p>

<h1 align="center"><b>AST</b>ro-sight</h1>

<p align="center">
  AI エージェント向けの AST 情報生成 CLI。tree-sitter で 16 言語のコードを解析し、シンボルの定義と参照、diff の影響範囲、API の差分、デッドコードを JSON か TOON で返す
</p>

<!-- standard:badges:start -->
<h3 align="center">対応プラットフォーム</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-FCC624?logo=linux&amp;logoColor=black" alt="Linux">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/astro-sight/actions/workflows/ci.yml"><img src="https://github.com/owayo/astro-sight/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/astro-sight/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/astro-sight" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/astro-sight" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

astro-sight は、AI エージェントがコードを編集しながら確かめたい構造の情報を返します。シンボルがどこで定義されどこで使われているか、diff が何を壊すか、公開 API のどこが変わったか、どこからも呼ばれなくなった export はどれか、に答えます。

tree-sitter の構文木の識別子ノードだけを照合するので、`refs --name new` は `grep new` のようにコメントや文字列の中の同じ綴りを拾いません。出力の既定は compact JSON で、トークンを減らしたいときは TOON か自動選択に切り替えられます。同じ問い合わせを CLI、NDJSON の session、MCP サーバーのどれからでも実行できます。

## 機能

- **識別子単位の参照検索**: `refs` はディレクトリ全体からシンボルの定義と参照を探し、`refs --names` は複数の名前を 1 回の走査で調べます
- **diff の影響分析**: `context` は unified diff から、変わったシンボル、シグネチャの変更、呼び出し元を洗い出します。`impact` は diff の外に未解決の呼び出し元が残っていれば exit 1 を返します (Stop hook 向け)
- **構造化レビュー**: `review` は影響範囲、git blame から見て一緒に変わりやすいファイル (`cochange`)、公開 API の差分、死蔵シンボルを 1 回でまとめて返します
- **ファイルの構造**: `symbols`・`calls`・`imports`・`ast` がファイルの輪郭、コールグラフ、依存、構文ノードを返し、`sequence` は呼び出しの流れを Mermaid のシーケンス図にします
- **デッドコードとルール**: `dead-code` はどこからも参照されない export を報告し、テストランナーやフレームワークが実行時に呼ぶものは除きます。`lint` は YAML で書いた AST パターンのルールで検査します
- **トークンを意識した出力**: compact JSON・TOON・`--format auto` から選べます。頻出する名前の結果は既定で 100 件か約 3,000 トークンで打ち切り、省いた分の内訳を `result_summary` に出します
- **AI エージェントとの連携**: Claude Code と Codex のスキル、MCP サーバー、問い合わせをまとめて流せる NDJSON の `session` があります

対応言語 (拡張子とパーサの版は [docs/languages.ja.md](docs/languages.ja.md) にあります):

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?logo=rust&amp;logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/C-A8B9CC?logo=c&amp;logoColor=white" alt="C">
  <img src="https://img.shields.io/badge/C++-00599C?logo=cplusplus&amp;logoColor=white" alt="C++">
  <img src="https://img.shields.io/badge/Python-3776AB?logo=python&amp;logoColor=white" alt="Python">
  <img src="https://img.shields.io/badge/JavaScript-F7DF1E?logo=javascript&amp;logoColor=black" alt="JavaScript">
  <img src="https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&amp;logoColor=white" alt="TypeScript">
  <img src="https://img.shields.io/badge/TSX-61DAFB?logo=react&amp;logoColor=black" alt="TSX">
  <img src="https://img.shields.io/badge/Go-00ADD8?logo=go&amp;logoColor=white" alt="Go">
  <img src="https://img.shields.io/badge/PHP-777BB4?logo=php&amp;logoColor=white" alt="PHP">
  <img src="https://img.shields.io/badge/Java-ED8B00?logo=openjdk&amp;logoColor=white" alt="Java">
  <img src="https://img.shields.io/badge/Kotlin-7F52FF?logo=kotlin&amp;logoColor=white" alt="Kotlin">
  <img src="https://img.shields.io/badge/Swift-F05138?logo=swift&amp;logoColor=white" alt="Swift">
  <img src="https://img.shields.io/badge/C%23-512BD4?logo=dotnet&amp;logoColor=white" alt="C#">
  <img src="https://img.shields.io/badge/Bash-4EAA25?logo=gnubash&amp;logoColor=white" alt="Bash">
  <img src="https://img.shields.io/badge/Ruby-CC342D?logo=ruby&amp;logoColor=white" alt="Ruby">
  <img src="https://img.shields.io/badge/Zig-F7A41D?logo=zig&amp;logoColor=white" alt="Zig">
</p>

## インストール

<!-- standard:install:start -->
### Homebrew (macOS/Linux)

```bash
brew install owayo/astro-sight/astro-sight
```

### winget (Windows)

```powershell
winget install owayo.astro-sight
```

### Cargo

Rust 1.98 以上が必要です。

```bash
cargo install --git https://github.com/owayo/astro-sight --locked
```

### GitHub Releases から

[Releases](https://github.com/owayo/astro-sight/releases/latest) から自分の環境のアーカイブを取得して展開し、`astro-sight` を `PATH` の通った場所に置きます。各リリースには、取得したファイルを確かめるための `SHA256SUMS` も添付しています。

| プラットフォーム | ファイル |
|---|---|
| Linux (x86_64) | `astro-sight-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (x86_64, musl) | `astro-sight-x86_64-unknown-linux-musl.tar.gz` |
| Linux (ARM64) | `astro-sight-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `astro-sight-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `astro-sight-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `astro-sight-x86_64-pc-windows-msvc.zip` |

macOS でブラウザから取得した場合は、実行の前に隔離属性を外します: `xattr -d com.apple.quarantine astro-sight`。

### ソースから

[mise](https://mise.jdx.dev/) が必要です (Rust のツールチェーンは `mise.toml` で固定しています)。

```bash
git clone https://github.com/owayo/astro-sight.git
cd astro-sight
make install
```

`make install` は `/usr/local/bin` に入れます。場所を変えるときは `INSTALL_PATH` を指定します (例: `make install INSTALL_PATH="$HOME/.local/bin"`)。
<!-- standard:install:end -->

`winget install` の後は新しいターミナルを開いてください。portable パッケージは PATH を書き換えて `astro-sight` を通すので、開いていたターミナルには反映されません。musl 版は glibc を使わない静的リンクのバイナリで、Alpine のように glibc が無い環境や、glibc が古い Docker イメージで使えます。

ソースからビルドするには、tree-sitter のパーサをビルドする C コンパイラ (macOS なら Xcode Command Line Tools) も要ります。`make install` はバイナリを置いた後に、[AI エージェントとの連携](#ai-エージェントとの連携) で説明する Claude Code と Codex のスキルも書き込みます。

## 使い方

結果の既定は 1 行の compact JSON です (バッチ処理ではファイルごとに 1 行)。`--pretty` を付けると JSON を字下げし、`--format toon` か `--format auto` を付けると TOON か、推定トークン数が少ない方の形式に切り替わります。TOON 出力は [v4.1 仕様](https://github.com/toon-format/spec/blob/main/SPEC.md) に従い、空配列は `[]` です。

### エージェント向けの手順

```bash
# 1. diff 全体は review から入る
astro-sight review --dir . --git

# 2. 編集前後は context / impact を対にする
astro-sight context --dir . --git
astro-sight impact --dir . --git

# 3. 構造把握は symbols、識別子参照は refs
astro-sight symbols --path src/main.rs
astro-sight refs --name "AppService" --dir src/

# 4. 構文ノードを正確に特定したいときだけ ast を使う
astro-sight ast --path src/main.rs --line 10 --col 0

# 5. 繰り返し確かめる構造ルールは lint、呼び出し順序が重要な場合や 3 段以上の呼び出しは sequence で確認する
astro-sight lint --path src/main.rs --rules rules.yaml
astro-sight sequence --path src/main.rs --function main

# 6. 種類の違う問い合わせが 2 個以上続くなら session にまとめる
printf '%s\n' \
  '{"command":"symbols","path":"src/main.rs"}' \
  '{"command":"refs","name":"AppService","dir":"src"}' \
  | astro-sight session
```

### 参照を探す

```bash
astro-sight refs --name extract_symbols --dir .
```

```json
{
  "symbol": "extract_symbols",
  "refs": [
    { "path": "src/engine/symbols/mod.rs", "ln": 107, "col": 7, "ctx": "pub fn extract_symbols(...)", "kind": "def" },
    { "path": "src/commands/api_changes/exported.rs", "ln": 45, "col": 39, "ctx": "let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;", "kind": "ref" }
  ]
}
```

`path` は `--dir` からの相対パスで、`ln` と `col` は 0 始まりです。参照が 100 件を超えるか、出力が約 3,000 トークンを超える名前では、そこで出力を打ち切り、省いた参照の内訳を `result_summary` に出します。上限は `--max-results` と `--token-budget` で変えられます (`unlimited` で上限なし)。

### 未解決の影響を確かめる

```bash
astro-sight impact --dir . --git
```

```text
Unresolved impacts found:

src/engine/symbols/mod.rs changed [extract_symbols]:
  → src/service.rs:284 [extract_symbols]
  → src/commands/api_changes/exported.rs:45 [extract_symbols]
```

呼び出し元をすべて直してあれば、`impact` は何も出力せずに exit 0 で終わります。未解決の影響が残っていれば、上の一覧を stderr に出して exit 1 を返します。このテキストの行番号は、エディタでそのまま開けるよう 1 始まりです。

詳しい説明は話題ごとに分けています。

- 全コマンドのオプション、バッチ処理、`session`、MCP サーバー: [docs/usage.ja.md](docs/usage.ja.md)
- `context`・`impact`・`review`・`dead-code`・`cochange` の詳細: [docs/diff-analysis.ja.md](docs/diff-analysis.ja.md)
- 短縮キー、TOON、`--format auto`: [docs/output-format.ja.md](docs/output-format.ja.md)

## 設定

`astro-sight init` は設定ファイルを `~/.config/astro-sight/config.toml` に書き出します (`--path` で別の場所に書けます。同じパスにファイルがあると確認なしで上書きします)。`--config <path>` を付けると、その実行だけ別の設定ファイルを読みます。

```toml
debug = false          # デバッグログをファイルに出力する
format = "json"        # 既定の出力形式: "json" | "toon" | "auto"
skip_generated = true  # ディレクトリの走査から生成ファイルを外す
```

コマンドラインの `--format` は、設定の `format` より優先されます。ログの置き場所、`~/.cache/astro-sight/` のキャッシュと `--no-cache` は [docs/configuration.ja.md](docs/configuration.ja.md) で説明しています。

## AI エージェントとの連携

スキルとして登録すると、Claude Code や Codex が「この関数の呼び出し元は？」「この diff の影響範囲は？」のような質問で astro-sight を使うようになります。

```bash
astro-sight skill-install claude   # ~/.claude/skills/astro-sight/SKILL.md
astro-sight skill-install codex    # ~/.codex/skills/astro-sight/SKILL.md
```

MCP サーバーとして stdio で動かすときは、解析したいリポジトリをカレントディレクトリにして `astro-sight mcp` を起動します。そのディレクトリの外のファイルは拒否します。

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

スキルを入れただけでは、エージェントが grep に戻ることがあります。astro-sight を優先させる `CLAUDE.md` / `AGENTS.md` の規則は [docs/integrations.ja.md](docs/integrations.ja.md) にあります。`impact` が未解決の呼び出し元を報告している間は作業を終えさせない Stop hook の設定も、同じ文書で説明しています。

## 開発

<!-- standard:dev:start -->
[mise](https://mise.jdx.dev/) が必要です。ツールの版は `mise.toml` で固定しています。

```bash
make setup   # ツールチェーン (mise) と依存を取得する
make ci      # CI と同じ検査 (書き換えない)
```

| コマンド | 説明 |
|---|---|
| `make setup` | ツールチェーン (mise) と依存を取得する |
| `make build` | デバッグ版をビルドする |
| `make release` | リリース版をビルドする |
| `make run` | デバッグ版を実行する (引数は ARGS="...") |
| `make test` | テストを実行する |
| `make lint` | clippy を警告ゼロで通す |
| `make fmt` | コードを整形する (書き換える) |
| `make fmt-check` | 整形済みかを確かめる (書き換えない) |
| `make check` | 整形と静的検査 (書き換えない) |
| `make ci` | CI と同じ検査 (書き換えない) |
| `make install` | リリース版を INSTALL_PATH (既定 /usr/local/bin) に入れる |
| `make uninstall` | INSTALL_PATH から取り除く |
| `make clean` | ビルド成果物を消す |

`make` でターゲットの一覧を表示します。リリースは GitHub Actions で行います (**Actions → Release → Run workflow**)。
<!-- standard:dev:end -->

mise のほかに、tree-sitter のパーサをビルドする C コンパイラと、テストが呼ぶ `git` が要ります。mise を使わないときは `SYSTEM_TOOLS=1` を付けると、PATH 上のツールで動きます。`.cargo/config.toml` の `[patch]` でローカルの tree-sitter 系を差し込むと `Cargo.lock` が手元でだけ変わり、`--locked` で止まります。そのときは `make ci CARGO_FLAGS=` で外してください。

`make install` で入れるスキルの選び方、ヒーププロファイル、利用状況の集計ツール、リリースの流れは [docs/development.ja.md](docs/development.ja.md) で説明しています。

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
