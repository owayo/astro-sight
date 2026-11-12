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

astro-sight は、AI エージェントがコードを編集するときに必要な構造情報を返す CLI です。識別子の定義・参照、差分の影響、公開 API の変更、デッドコード候補、複雑度と依存構造を調べられます。

tree-sitter の構文木を使い、識別子の検索ではコメントや文字列を除外します。出力は compact JSON が既定で、TOON も選べます。CLI・NDJSON session・MCP から利用できます。

## 機能

- **複雑度と設計指標**: `metrics` で関数の複雑度、静的なローカル依存・循環依存、CCD・ACD・伝搬コストを測定。ファイル／ディレクトリの粒度と階層を選べる。[使い方と解析範囲](docs/usage.ja.md#metrics---複雑度と依存構造の測定)を参照。

- **識別子単位の参照検索**: `refs` はディレクトリ全体からシンボルの定義と参照を探し、`refs --names` は複数の名前を 1 回の走査で調べます
- **diff の影響分析**: `context` は unified diff から、変わったシンボル、シグネチャの変更、呼び出し元を洗い出します。`impact` は diff の外に未解決の呼び出し元が残っていれば exit 1 を返します (Stop hook 向け)
- **構造化レビュー**: `review` は影響範囲、git blame から見て一緒に変わりやすいファイル (`cochange`)、公開 API の差分、死蔵シンボルを 1 回でまとめて返します
- **ファイルの構造**: `symbols`・`calls`・`imports`・`ast` がファイルの輪郭、コールグラフ、依存、構文ノードを返し、`sequence` は呼び出しの流れを Mermaid のシーケンス図にします
- **デッドコードとルール**: `dead-code` はどこからも参照されない export を報告し、テストランナーやフレームワークが実行時に呼ぶものは除きます。`lint` は YAML で書いた AST パターンのルールで検査します
- **トークンを意識した出力**: compact JSON・TOON・`--format auto` から選べます。頻出する名前の結果は既定で 100 件か約 3,000 トークンで打ち切り、省いた分の内訳を `result_summary` に出します。省略分を数える費用は[サマリ集計の性能確認](docs/development.ja.md#省略した参照のサマリ集計の性能)で比較できます
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

Rust 1.99 以上が必要です。

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

インストール後は `astro-sight --version` を確認します。`command -v astro-sight` が mise の shim を指す場合は、`mise which astro-sight` で更新対象の実体を確認できます。詳しくは [インストール後の確認](docs/development.ja.md#ci-と同じ検査) を参照してください。
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

### 複雑度と依存構造を比較する

```bash
astro-sight metrics --dir . --component directory --component-depth 2 --base HEAD
```

ファイル／ディレクトリ単位で循環的複雑度・依存関係・循環・CCD・ACD・伝搬コストを測定します。`--base` は基準コミットとの差分を返し、依存辺と代表循環経路にはソース上の根拠が付きます。

`impact` は未解決の呼び出し元が残ると exit 1、残っていなければ何も出さず exit 0 になります。JSON の位置は 0 始まり、`impact` の表示位置は 1 始まりです。解析できない入力や未解決の依存は診断に残るため、空の結果だけで影響や循環がないとは判断できません。

| 詳しく知りたいこと | ガイド |
| --- | --- |
| 各コマンド・依存解決の範囲・基準比較・循環経路の根拠 | [使い方](docs/usage.ja.md) |
| 差分判定・言語別の互換性判定・デッドコード・共変更 | [差分解析](docs/diff-analysis.ja.md) |
| JSON・TOON・表示件数とトークン予算 | [出力形式](docs/output-format.ja.md) |

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

`make help` で全コマンドを確認できます。
<!-- standard:dev:end -->

mise のほかに、tree-sitter のパーサをビルドする C コンパイラと、テストが呼ぶ `git` が要ります。テストは [cargo-nextest](https://nexte.st/) で走らせ、`make setup` が `mise.toml` の版を入れます。mise を使わないときは `SYSTEM_TOOLS=1` を付けると、PATH 上のツールで動きます（`make test` には PATH 上の `cargo-nextest` も要ります）。`.cargo/config.toml` の `[patch]` でローカルの tree-sitter 系を差し込むと `Cargo.lock` が手元でだけ変わり、`--locked` で止まります。そのときは `make ci CARGO_FLAGS=` で外してください。

`make install` で入れるスキルの選び方、ヒーププロファイル、利用状況の集計ツール、リリースの流れは [docs/development.ja.md](docs/development.ja.md) で説明しています。

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
