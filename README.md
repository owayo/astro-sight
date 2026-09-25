<p align="center">
  <img src="docs/images/app.png" width="128" alt="astro-sight">
</p>

<h1 align="center"><b>AST</b>ro-sight</h1>

<p align="center">
  AI エージェント向けの AST 情報生成 CLI。tree-sitter で 16 言語のコードを解析し、シンボルの定義と参照、diff の影響範囲、API の差分、デッドコードを JSON か TOON で返す。
</p>

<h3 align="center">対応プラットフォーム</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-FCC624?logo=linux&amp;logoColor=black" alt="Linux">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/astro-sight/actions/workflows/release.yml"><img src="https://github.com/owayo/astro-sight/actions/workflows/release.yml/badge.svg?branch=main" alt="Release"></a>
  <a href="https://github.com/owayo/astro-sight/actions/workflows/ci.yml"><img src="https://github.com/owayo/astro-sight/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/astro-sight/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/astro-sight" alt="Version"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/astro-sight" alt="License"></a>
</p>

<h3 align="center">対応言語</h3>

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

## Install

### Homebrew (macOS/Linux)

```bash
brew install owayo/astro-sight/astro-sight
```

### winget (Windows)

```powershell
winget install owayo.astro-sight
```

portable パッケージとしてインストールすると winget が PATH を書き換えるため、インストール後は新しいターミナルを開くこと。

### From GitHub Releases

[Releases](https://github.com/owayo/astro-sight/releases) から最新のバイナリをダウンロードする。

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

#### Linux (x86_64, musl)

glibc を使わない静的リンク版。Alpine のように glibc が無い環境や、glibc が古い Docker イメージで使う。

```bash
curl -L https://github.com/owayo/astro-sight/releases/latest/download/astro-sight-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo mv astro-sight /usr/local/bin/
```

#### Linux (ARM64)

```bash
curl -L https://github.com/owayo/astro-sight/releases/latest/download/astro-sight-aarch64-unknown-linux-gnu.tar.gz | tar xz
sudo mv astro-sight /usr/local/bin/
```

#### Windows

[Releases](https://github.com/owayo/astro-sight/releases) から `astro-sight-x86_64-pc-windows-msvc.zip` をダウンロードして展開し、中の `astro-sight.exe` を PATH の通ったディレクトリに置く。

### From Source

ビルドには [mise](https://mise.jdx.dev/) と C コンパイラ（macOS なら Xcode Command Line Tools）が要る。Rust の版は `mise.toml` で固定していて、`make` が mise 経由でその版の `cargo` を呼ぶ。

```bash
git clone https://github.com/owayo/astro-sight.git
cd astro-sight
make install
```

`make install` はリリース版をビルドして `INSTALL_PATH`（既定は `/usr/local/bin`）に置き、続けて `astro-sight skill-install` で Claude Code と Codex のスキルを書き込む。スキルの書き込み先は `INSTALL_PATH` の外の `~/.claude/skills/astro-sight/` と `~/.codex/skills/astro-sight/` になる（→ [スキルインストール](#スキルインストール)）。バイナリの置き場所は `INSTALL_PATH` で、スキルを入れるエージェントは `SKILL_TARGETS` で変えられる。

```bash
# 書き込み権限のあるディレクトリに入れる
make install INSTALL_PATH="$HOME/.local/bin"

# スキルを入れない / Claude Code のスキルだけ入れる
make install SKILL_TARGETS=
make install SKILL_TARGETS=claude

# mise を使わず、PATH 上の cargo でビルドする（Rust の版は mise.toml とずれることがある）
make install SYSTEM_TOOLS=1
```

`make uninstall` はバイナリだけを消し、書き込んだスキルは残す。

## Usage

### グローバルオプション

```bash
# 既定は compact JSON（1 行出力、AI エージェント向け）
astro-sight symbols --path src/main.rs

# 人間向け整形出力（JSON のみ）
astro-sight symbols --pretty --path src/main.rs

# TOON 出力（同じ内容をより少ないトークンで）
astro-sight symbols --path src/main.rs --format toon

# json / toon のうち推定トークン数が少ない方を自動選択
astro-sight symbols --path src/main.rs --format auto
```

`--format json|toon|auto` で出力形式を切り替える。既定は `json` で、`config.toml` の `format` でも既定値を変えられる（優先順位は **CLI `--format` > `config.toml` > `json`**）。詳細は [Output Format](#output-format)。

### エージェント向けレビュー手順

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

`text` と `snippet` は 256 文字上限で切り詰められるため、minified/生成コードの巨大行でも応答サイズが暴れにくい。

### symbols - シンボル抽出

```bash
# ファイル内の関数・構造体・クラス等を一覧（compact 出力）
astro-sight symbols --path src/main.rs

# docstring 付き compact 出力
astro-sight symbols --path src/main.rs --doc

# 旧来の完全出力（hash, range, doc をすべて含む）
astro-sight symbols --path src/main.rs --full

# ディレクトリ内の全ソースファイルのシンボルを NDJSON で出力
astro-sight symbols --dir src/

# glob でフィルタ
astro-sight symbols --dir src/ --glob "**/*.rs"
```

compact 出力例:
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

| フィールド | 意味 |
|---|---|
| `name` | シンボル名 |
| `kind` | 種別の短縮形（`fn` / `method` / `class` / `struct` / `enum` / `iface` / `trait` / `var` / `const` / `mod` / `import` / `type` / `field` / `param`） |
| `ln` | 定義行（0-indexed） |
| `cx` | 循環的複雑度。関数/メソッドのみ付与（ベース 1 + 分岐ノード数）。計上規約は表の下を参照 |
| `cn` | そのシンボルを囲む container の名前。`impl Default for AppService` の中のメソッドなら `AppService`。同名メソッドの見分けに使う |
| `doc` | docstring（`--doc` 指定時のみ） |

`cx` の計上規約は McCabe に揃えてあり、同じロジックなら言語をまたいで同じ値になる。ただし `default:` や `_ =>` のような catch-all の arm を数えるかどうかは文法によって違い、±1 ずれることがある。

- ネスト関数/クロージャ・ローカル関数・`async` ブロックの中の分岐は数えない
- switch/match は arm だけを数え、構文本体は数えない
- 単独の `else`（`else if` を除く）は数えない
- 三項演算子は数え、null 合体演算子は数えない
- 式形の分岐（C# の switch 式、PHP の `match`）や Ruby の修飾子形ガード節（`return 0 if x`）も、文形と同じ値になる

#### 抽出する宣言

関数・メソッド・クラス・構造体・列挙・型などの宣言に加え、次の形も 1 シンボルとして抽出する。抽出したシンボルは公開 API 差分（`review` の api.add / api.rm / api.mod）と dead-code の対象にもなる（例外は各項目に記す）。

- JavaScript / TypeScript の分割代入（`export const { auth, signOut } = NextAuth()` / `const [first, second] = pair()`）は束縛名ごとに 1 シンボル。プロパティキー（`{ key: renamed }` の `key`）は含めず、`ln` は各名前の行を返す。関数内のローカルな分割代入も通常の `const` と同じく抽出する
- `var` 宣言、generator 関数（`function*`）、`abstract class`
- Java / C# の `record`（`class` として扱う）、Go の型エイリアス（`type A = B`）
- Rust の trait メソッド（本体のない必須メソッドを含む）。trait の可視性を継承し、`pub trait` のメソッドの削除・シグネチャ変更は api.rm / api.mod になる。dead-code の対象にはしない

TypeScript の interface / abstract メソッドと Go の interface メソッドは現状抽出しない。

#### 生成ファイルの除外と申告

`refs --dir` と `symbols --dir` は、次のどちらかに当てはまるファイルを既定で走査から除外する。

- ファイル名が minified / bundle / IDE helper のもの
- ファイル先頭 4KiB・40 行以内に生成宣言コメント（`@generated`、`Code generated by ...`、`DO NOT EDIT THIS FILE` など）があるもの

マーカーはコメント行頭の宣言形だけを認識するため、文字列リテラルや「automatically generated comments」のような通常コメント中の説明では発火しない。

除外が 1 件以上あれば、stdout に必ず機械可読な `skipped` を出す。`skipped` がなければ除外は 0 件である。`paths` には決定的にソートした先頭 50 件が入る。全件数は常に `generated` に入り、省略の有無は `truncated` で確認できる。

```json
{"symbol":"foo","refs":[],"skipped":{"generated":2,"paths":["gen/a.rs","gen/b.rs"]}}
```

`symbols --dir` は NDJSON なので、同じ `skipped` object を持つ制御用のレコード（control record）を末尾に 1 行追加する。複数名の `refs --names` は既存の「1 シンボル 1 レコード」を維持し、共有の `skipped` を先頭レコードへ 1 回だけ追加する。session / MCP の batch 応答も従来どおりルート配列を維持する。

除外せず走査する場合はグローバルオプション `--include-generated` を指定する。

```bash
astro-sight --include-generated refs --name foo --dir .
astro-sight --include-generated symbols --dir src
```

設定ファイルでは `skip_generated = false` で同じ動作になる。後方互換の環境変数 `ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1` も引き続き利用できる。`**/parser.c` のように glob の最終セグメントで具体的なファイル名を指定した場合は、明示指定を尊重してそのファイルを走査する。`**/*.c` のような通常の絞り込み走査では、既定の除外を維持する。

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

`--pretty` を付けると、caller と callee をオブジェクトで持ち call_site も含む完全な形式で出力する。

`--function <name>` は `<name>` の**中から出ていく呼び出し**（callee）に絞る。「誰が `<name>` を呼んでいるか」（caller）は `calls` ではなく `refs --name <name>` で調べる（他ファイルからの呼び出しも含み、`ctx` に呼び出し行が入る）。

### imports - import 依存抽出

```bash
# ファイルが参照するモジュールを抽出
astro-sight imports --path src/main.ts

# 複数ファイルを入力順に処理
astro-sight imports --paths src/main.ts,src/worker.ts
```

16 言語の import / use / include / require を tree-sitter AST から抽出し、`src`、`ln`、`kind`、`ctx` を返す。JavaScript / TypeScript / TSX は通常の import 文と `require()` に加えて、`import("./module")` および置換を含まない `` import(`./module`) `` も認識する。`${expr}` を含む template literal は依存先を静的に確定できないため除外し、呼び出し形式では第 1 引数だけを依存先として扱う。

### refs - クロスファイル参照検索

```bash
# シンボル名でワークスペース内を検索
astro-sight refs --name "extract_symbols" --dir src/

# glob パターンでファイルを絞り込み
astro-sight refs --name "AstgenResponse" --dir src/ --glob "**/*.rs"

# 複数シンボルを一括検索（NDJSON 出力、1 シンボル 1 行）
astro-sight refs --names "AppService,AstgenResponse" --dir src/

# 出力件数の上限を変える（既定は 100 件 / 3,000 トークン）
astro-sight refs --name "new" --dir . --max-results 500
astro-sight refs --name "new" --dir . --max-results unlimited --token-budget unlimited
```

出力例（`astro-sight refs --name extract_symbols --dir .`）:
```json
{
  "symbol": "extract_symbols",
  "refs": [
    { "path": "src/engine/symbols/mod.rs", "ln": 107, "col": 7, "ctx": "pub fn extract_symbols(...)", "kind": "def" },
    { "path": "src/commands/api_changes/exported.rs", "ln": 45, "col": 39, "ctx": "let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;", "kind": "ref" }
  ]
}
```

`path` は `--dir` からの相対パスで返す。`--name` は空文字を受け付けない。`--names` も空要素のみ（例: `",,,"`）の場合は `INVALID_REQUEST` を返す。`--dir` にはディレクトリのみ指定でき、ファイルパスを渡した場合も `INVALID_REQUEST` を返す。

#### 出力件数の上限と `result_summary`

高頻度な識別子は 1 回の呼び出しで数千件返り、表現をいくら最適化してもトークン消費が膨れ上がる（導入時点の実測: 自リポジトリの `refs --name new --dir .` が 1,846 件 ≈ 68,000 トークン）。エージェントは呼ぶ前にその識別子が高頻度だと知りようがないため、既定で **100 件 / 推定 3,000 トークン**の上限を課す（同条件で約 2,600 トークンに収まる）。

- **解析は止めない。** 全件解析して `total` を正確に出し、出力だけを絞る。件数で走査を打ち切ると、正確な総数も省略分の内訳も取れなくなる
- 省略が 1 件でも起きたときだけ `result_summary` を出す。上限に当たらない通常の問い合わせでは `result_summary` は付かず、出力は上限を設ける前とバイト単位で同じ
- 上限が効くのは出力だけ。`dead-code` / API 差分 / hook の判定は、全件を見た内部結果で行う
- `--max-results` / `--token-budget` はいずれも `unlimited` を受ける。`--token-budget` の下限は 256（それ未満だとサマリ自体が収まらない）
- `refs --names` では、呼び出し全体で 1 つの予算を round-robin で配分する。名前ごとに上限を課すと全体が名前数に比例して膨らみ、先頭から詰めると高頻度な 1 名が予算を食い尽くして後続が 0 件になる
- 同じ上限は `session` の `max_results` / `token_budget`（数値または `"unlimited"`）と MCP の `refs_search` / `refs_batch_search` にも適用される
- **守れなかった予算は申告する。** サマリ自体に固定コストがあるため、名前数が多く予算が小さいと、表示件数を 0 にしても予算を超える。その場合は `result_summary.budget_exceeded: true` を出す（予算を上げる / 名前を減らす / `--glob` で絞る、のいずれかが要ることを示す）。`refs --names` では、名前ごとの省略分の内訳（rollup）を「呼び出し全体の予算 ÷ 名前数」に収めるので、名前を増やしてもサマリが多重に膨らむことはない

```json
{
  "symbol": "new",
  "refs": [ /* 上限内の件数 */ ],
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

`by_kind` / `by_lang` / `files` は **省略された分だけ**の分布（本体込みの総分布にすると省略分を引き算で復元できない）。`by_lang` は、複数の言語が混在する（polyglot）リポジトリで効く。名前だけの一致（bare name）は言語をまたいで大量に出る（実測: ある polyglot リポジトリで、名前 `search` の参照 2,522 件のうち 2,521 件が別言語）。言語構成が見えれば、`--glob` で絞り直すかどうかを判断できる。`files` 自体も上限を持ち、超過分は `other_files` へ畳んで `rollup_truncated` で申告する（サマリが第二の出力爆発を起こさないため）。

`complete_input` は、解析対象になり得た入力を `total` がすべて数えているかどうかを表す。生成物として走査から外したファイルや、読み込み・parse に失敗したファイルがあれば false になり、`total` がリポジトリ全体の真の総数ではないことを示す。

単一検索と複数シンボル検索はいずれも、ワーカーごとの fold/reduce で結果を直接統合し、ファイルごとの中間 `Vec` を全ファイル分保持しない。非常に多くの参照が返るシンボルでは出力自体が大きくなるため、`--glob` で対象言語を絞るか、必要に応じて `ASTRO_SIGHT_BATCH_WORKERS` で並列ワーカー数を下げる（既定は利用可能 CPU 数）。

複数シンボル検索（`refs --names`）は、ディレクトリ走査・Aho-Corasick（AC）走査・parse のすべてを、名前数に依らずファイルごとに 1 回へ集約する。パターンは原則 1 個の AC オートマトンに載せる（オートマトンのサイズはパターン数にほぼ線形で、実測 5 万パターン ≈ 8MB）。`ASTRO_SIGHT_REFS_BATCH_CHUNK`（既定 100,000）を超える大規模入力だけは AC を分割するが、その場合もファイル走査と parse は 1 回のままで、分割サイズに依らず結果は一致する。

Angular テンプレートと Android XML の補助参照スキャンは、各ファイルをそれぞれ 2MB / 1MB に制限する。metadata 確認後にファイルが拡大した場合も、上限 + 1 byte で読み込みを止めてスキップする。

C/C++ の `struct` / `class` / `union` / `enum` tag 名は、本体付き定義だけを Definition とし、`struct X *p`、`sizeof(struct X)`、cast、引数型・メンバ宣言内の `struct X` は Reference として数える。単独の前方宣言（forward declaration）は ref / def のどちらにも含めないため、dead-code でも使用中の型 tag を誤って dead にしにくい。

### context - スマートコンテキスト（diff → 影響分析）

unified diff を受け取り、変更の影響範囲を分析する。AI コードレビュー支援機能。関数シグネチャ変更は識別子境界で照合するため、`foo` と `foo_bar` のように名前の先頭だけが一致する別関数を混同しない。

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

`context` / `impact` / `review` の `--base` は `git diff` / `git show` / `git blame` にそのまま渡るため、`-` で始まる値・NUL を含む値・空文字を `INVALID_REQUEST` で拒否する（`--output=/path` などのオプション誤認識を防ぐ）。

出力例:
```json
{
  "changes": [
    {
      "path": "src/engine/symbols/mod.rs",
      "hunks": [{ "old_start": 10, "old_count": 5, "new_start": 10, "new_count": 8 }],
      "affected_symbols": [
        { "name": "extract_symbols", "kind": "function", "change_type": "modified" }
      ],
      "signature_changes": [
        { "name": "extract_symbols", "old_signature": "fn extract_symbols(...)", "new_signature": "fn extract_symbols(..., include_refs: bool)" }
      ],
      "impacted_callers": [
        { "path": "src/commands.rs", "name": "cmd_symbols", "line": 166 }
      ]
    }
  ]
}
```

呼び出し元は、確信度と破壊性によって 3 系統に分かれる。ここでの blocking 対象とは、`impact` や `review --hook` が exit 1 を返す原因になる検出を指す。

- `impacted_callers`: 実際の呼び出し位置。diff 外に残れば `impact` の blocking 対象になる。引数個数が変わったシンボルへの参照や、判定不能な参照もここに残す
- `low_confidence_callers`: owner（メソッドが属するクラスや型）を確定できない汎用名や、直接 import の証拠がない TS/Rust の同名参照
- `informational_callers`: 名前と引数個数が変わっていないシンボルへの関数値参照や、名前が変わっていない modified シンボルの import 行。blocking 対象にしない

### impact - 未解決の影響検出（Stop hook 用）

`context` の結果から、diff に含まれないファイルへの影響を「未解決」と判定する。AI エージェントの Stop hook で使用し、未対応の影響先があればブロックして続行を促す。`impact` が追うのは変更後のツリーに残っているシンボルだけで、削除したシンボルは検出しない（呼び出しが残っていても exit 0）。削除まで止めたい場合は、公開 API の削除を `api.rm` として検出する `review --dir . --git --hook` を使う。

シグネチャ変更の判定は `context` と同じく識別子境界での一致を使うため、テストヘルパーや派生名の変更が基底名の関数変更として波及しない。関数内で宣言されたローカルシンボルはファイル間影響の起点から除外する。TypeScript/JavaScript、Rust、Python、Go、Java、Kotlin に対応し、Kotlin のネスト関数もトップレベルの同名関数と区別する。

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
- `--dir` が git 管理外 → exit 0（出力なし。`--hook` の有無は問わない。下記「git 管理外ディレクトリでのスキップ」参照）

出力例（exit 1 時）:
```
Unresolved impacts found:

src/engine/symbols/mod.rs changed [extract_symbols]:
  → src/service.rs:284 [extract_symbols]
  → src/commands/api_changes/exported.rs:45 [extract_symbols]
```

テキスト出力の行番号は、エディタでそのまま開けるよう 1 始まりで表示する（JSON 出力の `line` / `ln` は 0 始まり）。

claw-hooks との連携例（グローバル設定の `~/.config/claw-hooks/config.toml` に書く。プロジェクトの `.claw-hooks.toml` に書いた `stop_hooks` は claw-hooks が無視する）:
```toml
[[stop_hooks]]
commands = ["astro-sight impact --git --dir ."]
condition = { command_exists = "astro-sight" }
```

検出時の exit 1 は、Claude Code の hooks 設定に直接登録すると非ブロッキングなエラーとして扱われ、Claude は止まらない。上の例のように `condition` を付けた `stop_hooks` では、コマンドの失敗を claw-hooks が block として Claude に返すので止まる。

#### git 管理外ディレクトリでのスキップ

`--git` を受け付けるコマンド（`context` / `impact` / `review` / `dead-code` / `cochange`）を git 管理外ディレクトリで実行した場合は、内部の `git diff` の失敗をエラーにしない。「解析対象なし」としてスキップし、**exit 0** で正常終了する。`~/.config` のような git 管理外ディレクトリで編集しているときに、Claude Code の Stop hook をブロックしないための挙動である。

- `--hook`（`review` / `impact`）→ stdout / stderr ともに無出力で exit 0
- 通常 CLI → 空の正常結果に機械可読な `skipped` フィールドを付けて exit 0。「差分なし」と「git 管理外」を区別できる（構造化出力を持たない `impact` は無出力）

```json
{ "...": "...", "skipped": { "reason": "not_git_repository", "source": "git", "message": "--git was requested but --dir is not inside a git worktree" } }
```

判定には `git rev-parse --is-inside-work-tree`（`LC_ALL=C`）を使うので、worktree / submodule / bare repo でも正しく判定できる。**真のエラー**（`--base` 不正・git 実行不能・壊れた repo・権限不足）は従来どおり `exit 1` を維持する。`--diff` / `--diff-file` / stdin で diff を渡す経路はこの判定を通らないので、挙動は変わらない。

#### 未追跡ファイルの取り込み上限

`--git`（非 `--staged`）は未追跡のソースファイルを「新規ファイル」として解析対象に含める（同一作業で作った未追跡ファイルへの参照が「diff 外の未解決影響」と誤報されるのを防ぐため）。ただし **1 ファイル 256KB または 5,000 行を超える未追跡ファイルは対象外**にする。上限を設けるのは、コード生成器の出力や巨大 fixture のような生成物を取り込むと、その全 exported symbol が API 差分の候補になるため。そうなると `review` に数十分かかり、Stop hook がタイムアウトする。実測では、未追跡ファイルがなければ 1.75 秒で終わる `review` が、`pub fn` を計 22,000 個持つ未追跡ファイルを置いただけで 10 分を超えても終わらなかった。

追跡済み（tracked）のファイルには、この上限を適用しない。commit / add 済みのファイルは、意図的にレビュー対象に入れたものと見なせる。一方、未追跡のファイルは「まだ add していない」ものなので、コミット対象かどうかが分からず、巨大なら生成物の可能性が高い。

対象外にしたファイルは黙って落とさず `truncations` に出力する（「レビュー済み」と誤読させないため）:

```json
{ "...": "...", "truncations": [{ "path": "generated.rs", "reason": "untracked_file_too_large", "message": "untracked file excluded from --git analysis: lines 80000 exceeds limit 5000" }] }
```

`--hook` では `trunc: [{"f": "generated.rs", "r": "untracked_file_too_large"}]` として出力する（検出ではなく解析範囲の申告なので exit 1 にはしない）。`impact` は構造化 JSON を持たないため stderr の `note:` 行で出す。`--staged` / `--diff` / `--diff-file` は明示された範囲を尊重するため未追跡の取り込み自体を行わない。

#### 解析できないソースの申告

`dead-code` / `review` は、ディレクトリ内に存在するが**どのバックエンドでも解析できなかったソースファイル**も同じ `truncations` に出す。読めないファイル内の参照を数えないまま dead と断定すると、生きているシンボルを dead と報告してしまう（`.vue` の `<script>` からしか使われていない TypeScript 関数など）。そこで、「参照がない」のか「観測できなかった」のかを利用者が区別できるようにしている。

```json
{ "...": "...", "truncations": [{ "reason": "unanalyzable_source", "message": "1 \".vue\" file(s) were not analyzed (no parser for this language); references inside them are not counted (e.g. src/App.vue)" }] }
```

対象は**プログラム / テンプレート言語だと確実に言える拡張子**に限る（`.vue` / `.svelte` / `.astro` / `.erb` / `.razor` / `.scala` / `.dart` / `.lua` など）。走査対象外のファイルには画像・アーカイブ・データも含まれるため、全件を申告すると本当に見落としているソースがノイズに埋もれる。出力は拡張子単位に 1 件へ畳み、拡張子 10 種 / 代表パス 3 件を上限とする。該当ファイルがなければ `truncations` 自体を出力しない。

`dead-code` は参照を数えた範囲（ディレクトリ全体）の解析できないソースを申告する。`--glob` や `--git` で dead の候補を絞っても参照はディレクトリ全体から数えるので、申告の範囲も狭めない。

#### 既定の除外

`context` / `impact` / `review` の影響分析は、ファイル間の参照検索でサードパーティ依存と build artifact を既定で除外する。`new` / `save` / `find` / `update` などの汎用メソッド名がサードパーティや生成コードから大量に流入し、影響先を万件単位の偽陽性で埋めるのを防ぐ。

- vendor / package manager: `vendor`, `node_modules`, `bower_components`, `.venv`, `venv`, `.tox`, `Pods`, `Carthage`
- build artifact: `target`, `build`, `dist`, `out`, `.build`, `DerivedData`, `bin`, `obj`, `coverage`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `CMakeFiles`
  - `bin` のうち Cargo パッケージの `src/bin/`（直上が `src` で、その親に `Cargo.toml` がある）はバイナリターゲットのソースなので除外しない。`--exclude-dir bin` を明示した場合はすべての `bin` を除外する

この既定の除外を解除する場合:

```bash
ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1 astro-sight impact --dir . --git
```

`.gitignore` と隠しファイルの除外、および生成ファイルの判定（`refs::collect_files` 経由）は、この既定の除外とは別の仕組みで動く。このうち生成ファイルの除外だけは、`--include-generated` または `skip_generated = false` で解除できる。

#### ユーザー指定の追加除外（v26.5.117+）

固定リストに含まれない名前のディレクトリ（`pjproject-2.15`, `openssl_64_1.1.1c`, `third_party` など）を除外したい場合や、より細かい glob パターンで impact 解析の対象を絞りたい場合は、`--exclude-dir` / `--exclude-glob` を使う。`context` / `impact` / `review` で同じオプションが利用でき、固定リストに**追加**される（既定の除外を上書きするものではない）。

```bash
# vendored C library を除外
astro-sight impact --dir . --git \
  --exclude-dir pjproject-2.15 \
  --exclude-dir openssl_64_1.1.1c

# glob で複数バージョンをまとめて
astro-sight impact --dir . --git \
  --exclude-glob '**/openssl_*1.1.1*/**'

# review でも同じオプションが impact + dead_symbols 両方に作用
astro-sight review --dir . --git \
  --exclude-dir pjproject-2.15 \
  --exclude-glob '**/openssl_*/**'
```

`--exclude-glob` は `ignore::overrides` の negative pattern として扱う（先頭の `!` は不要、ワークスペース相対）。不正な glob 構文は実行前に `INVALID_REQUEST` で弾く。

### review - 構造化 diff レビュー

`context` の影響分析に加えて、`cochange` による変更漏れ候補、公開 API 差分、死蔵シンボルを 1 回の実行でまとめて返す。PR レビューや pre-merge チェック向け。

`--git --base <rev>` を指定した場合、`missing_cochanges` の blame 解析にも同じ base を使う。複数コミット分の PR をまとめてレビューするときも、diff と共変更候補の解析範囲が揃う。

`missing_cochanges` は、共変更が 3 回以上あるペアだけを候補にする（`--cochange-min-samples`、既定 3）。変更行 blame では証拠コミットが 2 件だけの起点がよく現れ、「1 回だけ一緒に変わった」ペアが confidence 1.0 として上位に並ぶため。探索的に小標本まで見たい場合は `--cochange-min-samples 2` を指定する（単体の `cochange` コマンドは既定 2 のまま）。候補の重複排除と上位 10 件の選択には、単体コマンドと同じ平滑化済みの `score` を使う。3/3 の小標本が 30/40 のような十分な標本より機械的に上位へ来るのを防ぐためで、raw confidence は証拠の表示と閾値判定に引き続き使う。

ロックファイルと、ソースに対応する依存宣言ファイル（`Cargo.toml` / `package.json` / `pyproject.toml` など）は `missing_cochanges` の候補にしない。依存を追加するコミットでは、これらとソースが必ず一緒に変わるので、履歴相関は 100% になる。しかしその相関は「依存を追加したとき」に限ったもので、import を 1 行も増減させない本体変更とは因果関係がない。依存宣言ファイルを候補から外すのは、ソースと同じエコシステムで、そのソースから見て最も近いものとの組だけに限る（`Cargo.toml` と Python スクリプトのような別エコシステムの組は候補に残る）。単体の `cochange` コマンドは、「過去に一緒に変更された」事実として依存宣言ファイルを出し続ける（ロックファイルは生成物なので両方で除外）。

**外部 snapshot と生成元テストの関係は方向付きで扱う。** snapshot を更新しただけの差分に対して、「生成元テストも変更漏れでは」とは出さない。snapshot は被テスト対象の出力が変わったときにも更新されるので、「テストを変えたら snapshot も変わる」という期待は成り立っても、その逆は成り立たないためである。逆方向は維持する。テストを変更したのに snapshot が欠けている場合は、従来どおり候補に出る。抑制するのは次をすべて満たすペアだけで、1 つでも確認できなければ従来どおり候補に残す:

- snapshot の直上ディレクトリが正確に `__snapshots__` で、ファイル名末尾の `.snap` を 1 回だけ除いたパスが欠落候補と完全一致する（Jest / Vitest / Bun が共有する標準規約。`tests/__snapshots__/widget.test.tsx.snap` → `tests/widget.test.tsx`）
- 生成元テストが実在する通常ファイルである
- snapshot の**先頭行が既知のランナーヘッダと完全一致**する（`// Vitest Snapshot v1, …` など）。パス規約だけでは手書き fixture や別用途の `.snap` を巻き込むため、生成出力であることをファイル自身で確認する

`.gitattributes` の `linguist-generated` とは判定経路が独立している（あちらは「生成物一般」の宣言で、指定すると両方向とも候補から消える）。カスタム snapshot resolver、inline snapshot、`.snap` 以外の形式は対象外で、いずれも従来どおり履歴相関の情報提供を維持する。グローバルの `--include-generated` を付けるとこの方向付けも無効化する。単体の `cochange` コマンドは探索的な用途なので方向付けしない。

なお、これは「テスト変更が不要だと証明した」ものではない（期待値だけ更新して必要なテストロジックの変更を忘れることはある）。標準の生成関係にあるペアについて、履歴相関だけを根拠に逆方向の変更を要求しないという推薦方針。

`api_changes.compatible_modified` には、シグネチャ文字列は変わるが既存の呼び出しとの互換性を保つ変更を出力する。次の変更は informational として扱い、`--hook` の blocking 対象にしない。

- React component の HOC ラップ
- 未参照の object member の削除
- TS/TSX のトップレベル関数の末尾への optional / default 引数の追加（`trailing_optional_params`）
- Python のトップレベル関数 / モジュール直下のクラスメソッドの末尾への、kwonly+default 引数または positional default 引数の追加（`trailing_optional_params`）。デコレータの差分がある場合や同名関数が複数定義されている場合は、保守的に blocking を維持する

同じシンボルに紐づく `impacts` も破壊的影響としては出さず、`mod_compat` の情報提供だけに留める。未参照 object member の判定では、削除キーを 1 個ずつ全リポジトリで検索せずに Aho-Corasick で一括して事前抽出し、各 JS/TS ファイルを最大 1 回だけ parse する。ファイルの収集・読み込み・parse に失敗したときは互換扱いへ降格せず、従来どおり blocking を維持する。

`export const` のような値バインディングは宣言全体（初期化子を含む）を比較するが、**値そのものが関数の場合は本体を比較から外す**（`export function` の本体変更が api.mod にならないのと揃える）。本体を比較から外すのは、次の関数の本体に限る。

- 値そのものであるアロー関数 / 関数式（括弧・`as`・`satisfies` 付きも含む）
- React の `memo` / `forwardRef` に包んだ関数
- オブジェクトリテラルのメンバーの関数（メソッド・`key: () => ...`）

引数・型注釈・キーの追加削除は引き続き比較する。それ以外の呼び出しに渡すコールバック（`create((set) => ({ ... }))` など）は、中身がストアの形や値そのものを決めるので本体も比較する。分割代入の束縛（`export const { a, b } = obj`）は「その名前へ至る経路 + 初期化子」で比較する（配列は位置を保つ）。同じ分割代入のほかの束縛を追加・削除しても、残った束縛が api.mod にならないようにするためである。default 値・computed key・rest を含むパターンは宣言全体で比較する。

Python の公開型契約を方向付きで分類できる変更には、`api_changes.modified[].contract_change`（hook では `api.mod[].contract`）として `{kind, breaks}` を付ける。対象は `TypedDict` の必須性変更と、モジュール直下の直接的な `Literal` 型エイリアスの値集合の変更である。`Literal` では、値集合の縮小を `literal_values_narrowed`（producer 側が壊れる）、拡大を `literal_values_widened`（consumer 側が壊れる）として報告する。`Literal` は `typing` / `typing_extensions` 由来と証明でき、値が escape / prefix を含まない文字列・10 進整数・真偽値・`None` だけの場合に限る。値の置換、動的な `__all__`、名前の shadow、star import など意味を静的に確定できない場合は方向を推測せず、通常の blocking な `api.mod` に残す。テストファイルは既存の公開 API 面規約どおり検出対象外。型エイリアスの項目は `kind = "type"` の疑似シンボルであり、`symbols` / `refs` / `dead-code` の解析対象には追加しない。

実行時に暗黙呼び出しされるシンボルの除外範囲は API 差分と dead-code で異なる。PHPUnit 規約、TS/JS の constructor、Flyway migration はどちらの公開面からも除外する。一方、Laravel relation や Angular lifecycle hook は dead-code では除外するが、外部公開シグネチャの変更を見逃さないよう API 差分には残す。`--framework` は dead-code 規約の選択であり、この API 差分境界を一律には変更しない。

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

### dead-code - デッドコード検出

エクスポートされているが参照されていないシンボルを検出する。diff 指定時は変更関連ファイルのみ、指定なしはプロジェクト全体をスキャン。

```bash
# プロジェクト全体をスキャン
astro-sight dead-code --dir .

# Rust ファイルのみスキャン
astro-sight dead-code --dir . --glob "**/*.rs"

# git diff に関連するファイルのみスキャン
astro-sight dead-code --dir . --git

# ステージ済み変更に関連するファイルのみ
astro-sight dead-code --dir . --git --staged
```

出力例:
```json
{
  "dir": "/path/to/project",
  "scanned_files": 48,
  "dead_symbols": [
    { "name": "unused_helper", "kind": "function", "file": "src/utils.rs", "line": 12 },
    { "name": "OldConfig", "kind": "struct", "file": "src/config.rs", "line": 40 }
  ]
}
```

`line` は宣言行（0 始まり）。テストからしか参照されないシンボルは `dead_symbols` に含めず、`test_only_symbols` に分けて出す。

同名シンボルが複数ファイルに存在する場合は誤判定防止のためスキップされる。ただし TS/JS と PHP の class member は、owner を安全に一意推定できる場合だけ例外的に判定する。PHP では、`Owner::method()` と同一クラス内の `self::method()` を確定参照として扱う。`$obj->method()` や callable 文字列のように owner を確定できない参照がある場合は、従来どおりスキップする。`static::` は遅延静的束縛によりサブクラスの override に到達し得るため、確定参照としては解決しない。trait を `use` する class / trait / enum 経由の静的呼び出しは、一意に到達する trait method に限り参照として数える。ただし合成先が同名の具象メソッドを持つ場合は、PHP の解決順に従って trait 側へは辿らない。

#### 実行時規約の自動除外

フレームワークやテストランナーが名前規約・リフレクションで動的に呼び出すシンボルは、識別子レベルのファイル間参照では呼び出し元を追跡できず誤検出になる。そのため、以下の規約は自動的に dead-code から除外される:

- **PHPUnit**: `*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest` クラスと `testXxx` / `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` メソッド
- **Python unittest**: `unittest.TestCase`（および `unittest.IsolatedAsyncioTestCase`）を継承するクラス（同一ファイル内の間接継承も fixed-point で解決）と、その `test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` メソッド
- **Python pytest**: `test_*.py` / `*_test.py` ファイルのトップレベル `test_*` 関数と `conftest.py` 内のすべての関数
- **Python フレームワーク登録デコレータ**: Typer / Click / FastAPI / Flask / Django / Celery / pytest などの登録デコレータが付いた関数・メソッド・クラス
- **Python 動的プロトコルメソッド**: `urllib.request.BaseHandler` 系の `*_open` / `*_request` / `*_response` / `http_error_*` と、watchdog の `FileSystemEventHandler` 系 `on_*` callback。いずれも既知の基底クラスを直接継承するメソッドだけを除外する
- **Angular**: `@Component` / `@Directive` 装飾クラスのライフサイクルフック（`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`）は Angular ランタイムが change detection サイクルで自動呼び出しするため除外。`@Pipe` 装飾クラスの `transform`（テンプレートの `| name` から呼ばれる）と、`@Injectable` / `@Pipe` 装飾クラスの `ngOnDestroy`（service / pipe の破棄時に呼ばれる）も除外する。service / pipe の他のフック（`ngOnInit` など）は Angular が呼ばないので除外しない
- **プログラムのエントリポイント**: 次を除外する。API 差分には残す
  - C / C++: グローバルスコープの `main`
  - Kotlin: トップレベルの `fun main`、`object` / `companion object` 直下の `@JvmStatic fun main`
  - Java: private でない `void main`（引数なしか `String[]` 1 個。Java 25 の instance main を含む）
  - C#: `static Main`（引数なしか `string[]` 1 個）
  - Java / C# / Kotlin では、エントリポイントを宣言する型（ネストした型の外側の型を含む）も除外する

#### 生成ファイルの扱い

生成ファイルとして検出したファイル（先頭の生成宣言コメント、`.gitattributes` の `linguist-generated` など）は、既定で dead 判定の**候補**から外す。ただし生成ファイルの中の参照は常に数える。gRPC の生成ハンドラ（`*_grpc.pb.go`）が手書きのサーバー実装を呼ぶ構成のように、生成コードは実行時に手書きコードを呼ぶため、その参照を捨てると生きているシンボルを dead と報告してしまう。

候補から外したファイルは `generated_candidates_skipped` に `refs` の `skipped` と同じ形で出す（0 件なら出力しない）。`--include-generated`（または `skip_generated = false`）を指定すると生成ファイルのシンボルも候補にする。

```json
{ "...": "...", "generated_candidates_skipped": { "generated": 1, "paths": ["api/greeter_grpc.pb.go"] } }
```

#### フレームワーク自動検出（v26.5.120+）

`--framework` 未指定時でも、`<dir>` 直下またはモノレポ配下の `package.json` の `dependencies` / `devDependencies` に `next` キーがあれば、自動で `nextjs` プリセットを適用する。自動検出した規約 glob は各 Next.js workspace からの相対パスへ限定するため、非 Next.js の兄弟 workspace にある `app/**/page.tsx` は除外しない。`node_modules`・生成物・symlink は探索対象外で、`peerDependencies` / `optionalDependencies` 経由も誤って適用されやすいため対象外。明示指定（`--framework laravel` など）は常に自動検出より優先される。

```bash
# ルートまたは配下 workspace の package.json に `next` があれば自動適用される
astro-sight dead-code --dir .
astro-sight review --dir . --git
```

#### bin-only Rust crate の API 差分除外

`review` の `api_changes`（`added` / `removed` / `modified`）は、bin-only Rust crate（`src/lib.rs` がなく `Cargo.toml` に `[lib]` セクションもない）の `pub fn` 変更を自動的に除外する。bin-only crate の `pub fn` は crate 外から到達できないため、追加・削除・シグネチャ変更いずれも外部公開 API の互換性問題にはならない。`src/lib.rs` を削除する変更が同じ diff に含まれていても、base リビジョン側で library crate だった場合は、旧公開 API の削除を正しく `removed` に残す。

### cochange - 共変更パターン検出

git blame と diff-tree から、指定ファイルと一緒に変更されやすいファイルを検出する。`review --git --base <rev>` の `missing_cochanges` でも同じ解析を使う。

```bash
# git diff から起点ファイルを自動取得
astro-sight cochange --dir . --git --base HEAD~5

# 起点ファイルを明示
astro-sight cochange --dir . --paths src/service.rs

# rename / copy を追跡
astro-sight cochange --dir . --git --base HEAD~10 --rename --copy
```

`--paths-file` は 100MB 上限付きで読み込まれ、空リストは `INVALID_REQUEST` を返す。`--min-confidence` は有限な `0.0..=1.0`、`--smoothing-alpha` / `--smoothing-beta` は有限な非負値のみ受け付ける。`--paths` / `--paths-file` で渡すソースファイルは `--dir` 配下の相対パスである必要があり、`..` を含むパス・絶対パス・Windows のドライブ修飾パスは `PATH_OUT_OF_BOUNDS` で拒否される。

**生成物は起点・候補の両方から除外する。** バッチ処理・コード生成・ビルドが同時に書き出すファイル群は履歴上ほぼ必ず同一コミットに乗るため、そのままでは「機械的な同時更新」が高い confidence の共変更として提示されてしまう。判定では `.gitattributes` の `linguist-generated` を最優先する（`set` / `true` なら除外し、`unset` / `false` ならヘッダマーカーを見ずに残す）。指定がなければ、ファイル先頭の生成マーカー（`@generated` / `DO NOT EDIT` など）を見る。どちらでも決まらなければ候補に残す。拡張子や更新頻度では判定しないので、コメントを書けない生成 JSON / CSV は `.gitattributes` で宣言する。

```gitattributes
data/*.json linguist-generated=true
fixtures/hand-maintained.yaml -linguist-generated
```

除外しても**残った起点の分母は変わらない**（生成物が同居していたコミットを分母から抜くと `1/2` が `1/1` に化けて選択バイアスになるため）。除外件数は `diagnostics` の `excluded_generated_sources` / `filtered_generated_candidates` に出る。`git check-attr` を実行できない場合は除外を一切行わず（`GeneratedAttrLookupFailed` を申告）、判定できないことを理由に候補を消さない。グローバルの `--include-generated`（`config.toml` の `skip_generated = false` も同義）で、生成物も対象にできる。この指定は単体の `cochange` だけでなく `review` の `missing_cochanges` にも効く。

### doctor - 対応言語チェック

```bash
astro-sight doctor
```

`doctor` は対応言語の可用性を確認し、tree-sitter 言語には ABI バージョンも返す。

### session - NDJSON ストリーミング

```bash
echo '{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

stdin から NDJSON リクエストを受け取り、stdout に NDJSON レスポンスを返す。複数リクエストの連続処理に対応。`ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, `cochange` をサポートする。1 行あたり 100MB（改行を除く生入力サイズ）を上限としている。`ASTRO_SIGHT_WORKSPACE` を指定した場合はそのディレクトリ配下だけを扱い、リクエスト内の相対 `path` / `dir` はワークスペースルート基準で解決する。空文字・非 UTF-8・存在しないパスなどの不正なワークスペース値は `INVALID_REQUEST` で終了する。

```bash
# calls コマンド
echo '{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session

# refs コマンド
echo '{"command":"refs","name":"AstgenResponse","dir":"src/"}' | astro-sight session

# context コマンド（diff を直接渡す。zsh の echo は \n を展開して JSON が割れるので printf を使う）
printf '%s\n' '{"command":"context","dir":".","diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n+use new_mod;"}' | astro-sight session
```

`refs` を session で使う場合も `name` または `names` の指定が必須（空文字不可）。

### バッチ処理（ast, symbols, calls, imports, lint, sequence）

複数ファイルを一度に処理し、NDJSON（1 ファイル 1 行）で出力。専用の rayon pool で並列処理しつつ入力順を維持する。ワーカー数は既定で、利用可能 CPU 数と 4 の小さい方に制限する。tree-sitter の Parser が巨大ファイルの作業領域を thread-local に保持しても、ピーク RSS が CPU 数に比例して増えないようにするため。`ASTRO_SIGHT_BATCH_WORKERS` に正の整数を指定すれば、利用可能 CPU 数を上限に並列度を変更できる。まだ出力していない結果はワーカー数の 8 倍までの窓に収めるので、入力件数に比例してピーク RSS が増えることはない。stdout が閉じた場合は現在の窓で停止し、残りのファイルを解析しない。

```bash
# カンマ区切りで複数ファイルを指定
astro-sight symbols --paths src/lib.rs,src/cli.rs,src/main.rs

# ファイルリストから読み込み
find src -name '*.rs' > /tmp/files.txt
astro-sight symbols --paths-file /tmp/files.txt

# バッチ ast / calls / imports / lint / sequence も同様
astro-sight ast --paths src/lib.rs,src/main.rs --depth 2
astro-sight calls --paths src/lib.rs,src/main.rs
astro-sight imports --paths src/lib.rs,src/main.rs
astro-sight sequence --paths src/lib.rs,src/main.rs --function main
```

`--paths` / `--paths-file` は 1 件以上の有効なパスが必要。空リストは `INVALID_REQUEST` を返す。`--paths-file` は 100MB 上限付きで読み込まれる。

個別ファイルのエラーは行内 JSON エラーとして出力される（プロセスは成功終了）:
```jsonl
{"path":"src/lib.rs","lang":"rust","symbols":[...]}
{"error":{"code":"FILE_NOT_FOUND","message":"File not found: nonexistent.rs"}}
```

### mcp - MCP サーバーモード

stdio 上で JSON-RPC 2.0（Model Context Protocol）サーバーとして動作。Claude Desktop や Cursor などから利用可能。起動したときのカレントディレクトリをワークスペースとして扱い、その外のファイルは `PATH_OUT_OF_BOUNDS` で拒否する。相対パスもそこを基準に解決するので、解析したいリポジトリをカレントディレクトリにして起動する。

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

`astro-sight symbols --dir src | head` のように、下流コマンドが先に終了して stdout pipe が閉じた場合は panic を表示せず exit 0 で終了する。これは CLI 利用時の通常のページング・サンプリングを壊さないための挙動で、実際の解析エラーは従来どおり JSON エラー + exit code 1 で返す。

## Supported Languages

| Language | Extension | Crate | Version |
|----------|-----------|-------|---------|
| <img src="https://img.shields.io/badge/-000000?logo=rust&amp;logoColor=white" height="16"> Rust | `.rs` | `tree-sitter-rust` | 0.24 |
| <img src="https://img.shields.io/badge/-A8B9CC?logo=c&amp;logoColor=white" height="16"> C | `.c`, `.h`（既定） | `tree-sitter-c` | 0.24 |
| <img src="https://img.shields.io/badge/-00599C?logo=cplusplus&amp;logoColor=white" height="16"> C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.h`（C++ 構文検出時） | `tree-sitter-cpp` | 0.23 |
| <img src="https://img.shields.io/badge/-3776AB?logo=python&amp;logoColor=white" height="16"> Python | `.py`, `.pyi` | `tree-sitter-python` | 0.25 |
| <img src="https://img.shields.io/badge/-F7DF1E?logo=javascript&amp;logoColor=black" height="16"> JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | `tree-sitter-javascript` | 0.25 |
| <img src="https://img.shields.io/badge/-3178C6?logo=typescript&amp;logoColor=white" height="16"> TypeScript | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-61DAFB?logo=react&amp;logoColor=black" height="16"> TSX | `.tsx` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-00ADD8?logo=go&amp;logoColor=white" height="16"> Go | `.go` | `tree-sitter-go` | 0.25 |
| <img src="https://img.shields.io/badge/-777BB4?logo=php&amp;logoColor=white" height="16"> PHP | `.php`, `.phtml` | `tree-sitter-php` | 0.24 |
| <img src="https://img.shields.io/badge/-ED8B00?logo=openjdk&amp;logoColor=white" height="16"> Java | `.java` | `tree-sitter-java` | 0.23 |
| <img src="https://img.shields.io/badge/-7F52FF?logo=kotlin&amp;logoColor=white" height="16"> Kotlin | `.kt`, `.kts` | `tree-sitter-kotlin` | 0.3.5 * |
| <img src="https://img.shields.io/badge/-F05138?logo=swift&amp;logoColor=white" height="16"> Swift | `.swift` | `tree-sitter-swift` | 0.7 |
| <img src="https://img.shields.io/badge/-512BD4?logo=dotnet&amp;logoColor=white" height="16"> C# | `.cs` | `tree-sitter-c-sharp` | 0.23 |
| <img src="https://img.shields.io/badge/-4EAA25?logo=gnubash&amp;logoColor=white" height="16"> Bash | `.sh`, `.bash`, `.zsh` | `tree-sitter-bash` | 0.25 |
| <img src="https://img.shields.io/badge/-CC342D?logo=ruby&amp;logoColor=white" height="16"> Ruby | `.rb`, `.rake`, `.gemspec` | `tree-sitter-ruby` | [owayo/tree-sitter-ruby](https://github.com/owayo/tree-sitter-ruby) |
| <img src="https://img.shields.io/badge/-F7A41D?logo=zig&amp;logoColor=white" height="16"> Zig | `.zig`, `.zon` | `tree-sitter-zig` | 1.1 |

上記 16 言語は tree-sitter クエリによる精密なシンボル抽出に対応。Ruby は Unicode 識別子に対応し、simple case folding の対象となる `ſ` / `K` などを含むメソッド名も欠落なく抽出する。空白を挟む添字代入、括弧なし lambda 仮引数直後の `{}`、空正規表現、括弧なし呼び出し直後の block、空識別子 heredoc も構文エラーなく解析できる。

`.h` は既定では C ヘッダとして扱う。C++ 専用構文のマーカーがあり、C++ parser の方が明確に parse error が少ない場合だけ C++ として解析する。C ヘッダを不用意に C++ 扱いせずに、`class Foo { public: ... }` や `struct X : Base<X> {}` のような C++ ヘッダでの `symbols` / `review` / `dead-code` の取りこぼしを抑える。

> **\* Kotlin バージョンについて:** `tree-sitter-kotlin` 0.3.8 以降は `tree-sitter` の `>=0.21, <0.23` に依存する。この範囲の `tree-sitter` は、本体が使う 0.27 と同じく `links = "tree-sitter"` を宣言している。Cargo は同じ `links` を持つパッケージを依存グラフに 1 つしか置けないため、依存解決の段階で失敗する。0.3.5 が依存する `tree-sitter` 0.20 は `links` を宣言していないので 0.27 と共存でき、現在は 0.3.5 に固定している。
>
> ```
> error: failed to select a version for `tree-sitter`.
>     ... required by package `tree-sitter-kotlin v0.3.8`
> versions that meet the requirements `>=0.21, <0.23` are: 0.22.6, 0.22.5, 0.22.4, 0.22.3, 0.22.2, 0.22.1, 0.21.0
>
> package `tree-sitter` links to the native library `tree-sitter`, but it conflicts with a previous package which links to `tree-sitter` as well:
> package `tree-sitter v0.27.0`
> ```

## JSON Compact Keys

JSON 出力は既定で compact（`--pretty` で整形）。compact モードではトークン削減のためキー名を短縮:

- `language` → `lang`（calls, imports, lint, sequence, compact ast/symbols）
- `location` → `path`（compact ast/symbols）
- `references` → `refs`、`line` → `ln`、`column` → `col`、`context` → `ctx`（refs）
- `source` → `src`（imports）
- `kind`: `"definition"` → `"def"`、`"reference"` → `"ref"`（refs）
- `SymbolKind`: `"function"` → `"fn"`、`"interface"` → `"iface"`、`"variable"` → `"var"` など（compact symbols）
- `calls`: caller でグルーピング、callee は `{name, ln, col}` に簡略化

compact 出力例（ast/symbols）:
```json
{"path":"src/main.rs","lang":"rust","schema":{"range":"[startLine,startCol,endLine,endCol]"},"ast":[...]}
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"main","kind":"fn","ln":20}]}
```

`ast` / `symbols` に `--full` を付けると、キーを短縮しない完全な形式（`location`, `language`, `hash`, `range` など）で出力する。`--pretty` は字下げするだけで、キーは短縮したまま変わらない（`calls` だけは `--pretty` で完全な形式になる）。`version` フィールドを含むのは `doctor` と MCP の `initialize` 応答だけ。

## Output Format

`--format json|toon|auto` で出力形式を切り替える。既定は `json`。

| | JSON | TOON | auto |
|---|---|---|---|
| 既定 | ✅ | | |
| 仕様 | RFC 8259 | [TOON v3](https://github.com/toon-format/toon-rust) | 推定トークン数が小さい方 |
| `--pretty` | 有効 | 無視（TOON は元からインデント構造） | JSON が選ばれた場合のみ有効 |
| キャッシュ | compact のみ利用 | 利用しない | 利用しない |

### TOON とは

[TOON](https://toonformat.dev/)（Token-Oriented Object Notation）は JSON と同じデータモデルを、インデントと表形式で表現するフォーマット。同じ内容をより少ないトークンで LLM に渡せる。

```bash
astro-sight symbols --path src/main.rs --format toon
```

```toon
path: src/main.rs
lang: rust
symbols[3]{name,kind,ln,cx}:
  MAX,const,0,null
  alpha,fn,1,2
  beta,fn,4,1
```

同じ内容の JSON は次のようになる。キー名が要素ごとに繰り返される分が削減される。

```json
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"MAX","kind":"const","ln":0},{"name":"alpha","kind":"fn","ln":1,"cx":2},{"name":"beta","kind":"fn","ln":4,"cx":1}]}
```

変換には [`toon-format` 0.5.0](https://github.com/toon-format/toon-rust) を使用する。CLI/TUI 用の依存を避けるため `default-features = false` とし、comma 区切り・2 スペースの既定設定で符号化する。対応仕様は **TOON v3**。空配列は `[0]:`、名前付きの空配列は `items[0]:` となる。

単一出力・バッチ出力は同ライブラリの strict decoder で検証している。文書末尾に改行は付けない（JSON / NDJSON の改行終端は従来どおり）。削減率は内容によって異なり、形式を自動選択する場合は `--format auto` を使う。

### auto - トークン数が少ない方を自動選択

`--format auto` は、その出力について **compact JSON と TOON を両方エンコードし、推定トークン数が小さい方**を選ぶ。同点なら JSON（既定フォーマットで消費側の互換性が高いため）。

```bash
astro-sight symbols --path src/main.rs --format auto
```

#### なぜ文字数そのままではないか

BPE トークナイザでは**改行とインデントが 1 行あたりおよそ 1 トークンを消費する**ため、素の文字数で比べると行数の多い TOON を過大評価する。実際、次は文字数と実トークン数で勝者が逆転する（`o200k_base` / `cl100k_base` の両方で同じ）:

| | 文字数 | トークン数 |
|---|---:|---:|
| `{"a":1,"b":2,"c":3,"d":4}` | 25 | **17** |
| `a: 1` `b: 2` `c: 3` `d: 4`（4 行） | **19** | 19 |

そこで判定には `文字数 + 4 × 改行数` を使う。TOON v3 への移行時（2026-09-24）に、単一出力・バッチ・件数制限付き refs の **63 ペア**を tiktoken で再測定した。既存の係数 4 を維持する。

| トークナイザ | 係数 3 の合計損失 / 最大損失 | 係数 4 の合計損失 / 最大損失 |
|---|---:|---:|
| `o200k_base` | 6 / 6 tokens | 0 / 0 tokens |
| `cl100k_base` | 12 / 9 tokens | 3 / 3 tokens |

損失は JSON と TOON の実トークン数が少ない方との差。これは上記標本での測定値で、任意の出力についての上限保証ではない。係数は出力形が変わるたびに再測定する。実トークナイザを本体に含めないことで、追加データの容量とモデルごとの tokenizer 差を避け、同じ入力に対する選択を一定に保つ。

なお `--token-budget`（[出力件数の上限](#出力件数の上限と-result_summary)）は、**この指標をそのまま使わない**。指標は形式間の相対比較に使うものなので、係数の絶対値は問わない。一方、予算は利用者が「N トークンまで」と絶対値で指定する。両者の桁を合わせないと、「3,000 と指定したのに 900 しか出ない」という乖離が起きる。実測（252 サンプル × 2 トークナイザ）の `指標 / 実トークン` は p05=3.00 / p50=3.42 / min=2.73 なので、予算判定では指標を 3 で割る（予算を超えない側に倒した値）。

#### 性質

- **推定トークン数は常に両候補以下**（小さい方を選ぶだけなので、どちらの候補よりも悪くなることはない）。ただし[出力件数の上限](#出力件数の上限と-result_summary)が効く場合は、予算内に収まる**件数**が形式ごとに変わる。実測では `refs --name new` が JSON で 64 件 / 2,591 トークン、TOON で 83 件 / 2,548 トークンになり、auto は TOON を選んで「同じ予算でより多くの情報」を返す
- 選択は入力内容だけで決まるので**決定的**。同じ入力・同じバージョンなら常に同じ形式になる
- 実際に両方が選ばれる。係数が 3 だった時点で astro-sight の出力 1,127 サンプルを測ったところ、**564 件で JSON、563 件で TOON** が選ばれた（`ast` は JSON、`symbols` / `refs` / `calls` は TOON が勝ちやすい）
- `--pretty` は「選ばれた JSON をどう描画するか」だけを決める。比較そのものは常に compact JSON と TOON で行うため、TOON が勝った場合は `--pretty` の指定は効かない
- 空 object（`{}`）は TOON では空ドキュメント＝無出力になるため、auto は JSON を選ぶ。「結果が空」と「何も出力されなかった」を利用者が区別できなくなるのを避けるため（明示的な `--format toon` は仕様どおり空ドキュメントを出す）
- 下記「常に JSON のままの出力」に挙げた出力面では、`auto` はエラーにならず JSON になる。「TOON で出せ」という満たせない要求ではなく、JSON を選ぶことも auto の正当な結果のため

**バッチでの近似**: `--paths` / `--paths-file` / `--dir` は解析結果を全件バッファしない設計のため、全レコードを見てから勝者を決められない。**出力順で先頭 32 件を標本として両形式で描画し、その実測値で勝者を決めて残りに適用する**。標本の件数は並列度（CPU 数 / `ASTRO_SIGHT_BATCH_WORKERS`）に依らない定数なので、同じ入力なら並列度を変えても同じ形式が選ばれる。二重エンコードのコストは標本ぶんだけで、解析自体はどの経路でもパス 1 回きり。出力が途中で混ざることはない。

### 常に JSON のままの出力

次の 3 つは相手側が JSON を前提とする契約のため、`--format json|toon` の対象外（`auto` は JSON を選ぶだけなのでエラーにならない）。

| 出力面 | 理由 |
|---|---|
| `session` | 「1 行 = 1 リクエスト / 1 レスポンス」の NDJSON プロトコル |
| `review --hook` | Claude Code の Stop hook が消費する JSON 契約（compact JSON を stderr に出す） |
| エラー出力 `{"error":{...}}` | 既存スクリプトが parse する機械可読契約 |

`impact` は構造化出力を持たず、`--hook` の有無にかかわらず stderr にテキストを出すだけなので、同じく `--format` の対象外になる。

CLI で明示的に `--format toon` を渡した場合は「満たせない要求」としてエラーにする。`config.toml` の `format = "toon"` は全コマンドの既定表示形式でしかないため、これらの出力面では黙って JSON に倒す（設定しただけで hook や session が壊れないようにするため）。

### バッチ出力の形

`--paths` / `--paths-file` / `--dir` は JSON では NDJSON（1 行 1 レコード）、TOON では**ルート配列 1 個のドキュメント**になる。

```toon
[2]:
  - path: a.rs
    lang: rust
    symbols[3]{name,kind,ln}:
      MAX,const,1
      alpha,fn,2
      beta,fn,5
  - path: b.rs
    lang: rust
    symbols[1]{name,kind,ln}:
      gamma,fn,1
```

外側の配列は list form（`- ` 項目）で、tabular form にはしない。tabular 化には、全要素を見終えて初めて決まる情報が要る。これは、解析結果を全件バッファしない（ピーク RSS を入力件数から独立させる）という設計要件と両立しない。要素数 `[N]` は入力パス数から先に分かるので、ヘッダだけは先出しできる。内側の配列は従来どおり tabular form になり、削減量の大半はそちらから来る。

バッチでは、全要素を一括変換した場合に tabular form になる配列も list form で出す。要素の符号化はライブラリへ委譲し、strict decoder で復元した値が一致することをテストしている。解析失敗も 1 要素として数えるため、ヘッダの件数と出力件数は一致する。`refs --names` は元々全件を保持しているため、一括変換する。

### nullable 列の正規化

astro-sight の compact JSON は、`cx`（循環的複雑度）のようなフィールドを、値がない要素ではキーごと省略する。一方 TOON の tabular form は、配列内の全要素で**キー集合がそろっている**ことを要求する。素直にエンコードすると、symbols のような出力が list form へ落ちて **JSON より冗長になる**（実測 +33%）。

そのため astro-sight は、配列内の object どうしのキーの違いが「一部のキーが欠けているだけ」のとき、欠損キーを `null` で補って tabular 形を成立させる。適用は**厳密エンコードより短くなる場合だけ**（同点なら厳密側）なので、補完によって TOON がかえって長くなることはない。

- 補完対象は、全要素が非空 object で、すべての値がプリミティブの配列だけ。object を含む列は対象外
- 列順は要素を順に走査したときのキー初出順で固定する（決定的）
- **JSON 表現との構造的 round-trip は保証しない**。decode すると JSON が省略していたキーが `null` として現れる。DTO としての意味（`Option<T>` の `None`）は保存される

この正規化は astro-sight が自分の DTO について行う判断で、`--format toon` のエンコーダ自体は仕様どおりの純粋な実装のままにしている。任意の JSON に対して欠損キーを null 補完すると、「明示的な null」と「未設定」を区別できなくなるため。

## Configuration

`astro-sight init` は TOML 形式の設定ファイルを生成する。既定の保存先は `~/.config/astro-sight/config.toml` で、`--path` で変えられる。同じパスにファイルがあると確認なしで上書きするので、既存の設定を残したい場合は先に退避する。

```toml
# デバッグログをファイルに出力する (デフォルト: false)
debug = false

# ログディレクトリのパス (デフォルト: ~/.config/astro-sight/logs)
# log_path = "~/.config/astro-sight/logs"

# 既定の出力フォーマット: "json" | "toon" | "auto" (デフォルト: json)
format = "json"

# 生成ファイルをディレクトリ走査から除外する (デフォルト: true)
skip_generated = true
```

`log_path` を省略した場合は、読み込んだ config ファイルと同じディレクトリの `logs/` を使う。`--config /path/to/config.toml` でカスタム config を使う場合も同じ。`log_path` を明示した場合は、その値が既定のパスと同じでも明示指定として尊重する。

## Cache

単一ファイル `ast` / `symbols` の compact 出力を BLAKE3 ベースで保存するキャッシュ。ファイル内容または astro-sight のバージョンが変わるとハッシュが変わり、キャッシュは自動的に無効になる。キーにバージョンを含めるのは、内容が同じでも解析ロジックや出力スキーマの変更で結果が変わる場合に、古い結果を返さないため。

- **対象コマンド**: `ast`, `symbols`（単一ファイルモードのみ）
- **キャッシュキー**: `BLAKE3(astro-sight バージョン + canonical path + BLAKE3(ファイル内容))` + コマンド固有サフィックス（オプション組み合わせ別）
- **path/lang の分離**: `ast` / `symbols` の応答には `path` と `lang` が含まれるため、同じ内容でも別ファイル・別拡張子なら別キャッシュとして扱う
- **保存先**: `~/.cache/astro-sight/v<バージョン>/`
- **ディレクトリシャード**: ハッシュの先頭 2 文字でサブディレクトリを分割（例: `v26.9.100/ab/cdef1234....symbols.json`）
- **世代 GC**: キャッシュキーにバージョンが混ざるため、リリースのたびに全エントリが失効する。失効しただけではエントリは消えないので、以前は更新を重ねるたびに死蔵データが積み上がっていた（実測: 開発機で 176MB、そのほぼ全量が到達不能）。現在は世代ごとにディレクトリを分け、新しい世代の初回起動で旧世代を削除する。削除するのは、旧バージョンの世代ディレクトリ（`v26.8.111`）と、世代分離前のフラットな 2 桁 hex シャード（`00`〜`ff`）だけ。**それ以外の名前は消さない**（利用者の `~/.cache` 配下なので、astro-sight が作ったと確証が持てないものは残す）。削除の失敗は無視する（掃除に失敗しても解析は続けられるべきなので）
- **`--pretty` 時はキャッシュをスキップ**（compact 出力のみキャッシュ）
- **`--no-cache`** で無効化可能

## AI エージェントとの連携

### スキルインストール

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

### 利用状況の分析

`tools/usage-stats` は、Claude Code / Codex の利用ログから astro-sight の採用率とサブコマンドの分布を集計する補助ツールである。

`astro-sight` がシェル上の実行コマンドとして現れ、既知のサブコマンドを抽出できた場合だけを採用として数える。`/skills/astro-sight/SKILL.md` のようなパス文字列、プロンプト内での言及、`astro-sight --version` / `astro-sight --help` のようなサブコマンドなしの確認起動は数えない。`--pretty` / `--debug` / `--config <path>` などのグローバルフラグや、`/usr/bin/time -o <file> astro-sight ...` のようなラッパーを挟んでも、実際に実行されたサブコマンドを抽出する。Codex のログは従来形式（`function_call` / `exec_command`）と現行形式（`custom_tool_call` / `exec` 内の `tools.exec_command`）の両方を解析し、JavaScript の文字列・コメントに埋め込まれたコマンド例は実行として数えない。自動継続に使われる `wait` はコード分析や編集の選択ではないため、採用率の分母とツール分布から除外する。

```bash
cargo run --manifest-path tools/usage-stats/Cargo.toml -- --json --days 1
```

### CLAUDE.md / AGENTS.md に追記して確実に使わせる

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

### MCP サーバーとして登録

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

## Development

[mise](https://mise.jdx.dev/) を前提にしている。Rust の版は `mise.toml` で固定し、Makefile の各ターゲットが `mise exec --` 経由でその版の `cargo` を呼ぶ。シェルで mise を有効にしていなくても版はそろう。mise のほかに C コンパイラ（tree-sitter の各パーサをビルドする）と git（結合テストが一時リポジトリを作って `git` コマンドを呼ぶ）が要る。

```bash
make setup   # mise.toml のツールチェーンを入れ、依存を取得する
make ci      # CI と同じ検査（整形・clippy・cargo check・テスト）
```

CI の quality ジョブは `make setup` と `make ci` を呼ぶだけなので、手元の `make ci` がそのまま CI の検査になる。テストは配布物と同じ既定の feature で回す。clippy だけは `--all-features` で、ヒーププロファイラ（`dhat-heap` feature）側のコードも検査する。

| Command | Description |
|---|---|
| `make setup` | Install the toolchain (mise.toml) and fetch dependencies |
| `make build` | Build debug version |
| `make release` | Build release version |
| `make run` | Run the debug build (pass arguments with ARGS="...") |
| `make test` | Run tests |
| `make lint` | Run clippy with warnings as errors |
| `make fmt` | Format code |
| `make fmt-check` | Check formatting (no rewrite) |
| `make check` | Run fmt check, clippy, and cargo check (no rewrite) |
| `make ci` | Run the same checks as the CI quality job (no rewrite) |
| `make install` | Build release, install the binary to INSTALL_PATH, and install skills (claude + codex) |
| `make uninstall` | Remove the binary from INSTALL_PATH (installed skills are kept) |
| `make clean` | Clean build artifacts |
| `make help` | Show this help message |

Cargo のコマンドには既定で `--locked` を付けている。`.cargo/config.toml` の `[patch]` でローカルの tree-sitter 系を差し込むと `Cargo.lock` が手元でだけ変わり、`--locked` で止まる。そのときは `make ci CARGO_FLAGS=` のように `CARGO_FLAGS` を空にする。

`tools/usage-stats`（[利用状況の分析](#利用状況の分析)）はルートとは別の Cargo プロジェクトで、`make ci` の対象に入らない。コマンドとして入れるなら `make -C tools/usage-stats install` を実行する。

## Release

GitHub Actions の **Actions > Release > Run workflow** から実行する。1 回の実行で次の順に進む。

1. `Cargo.toml` と `Cargo.lock` の版を書き換えてコミットし、`v<版>` のタグを付けて push する
2. 6 つのターゲット（Linux の x86_64 / x86_64 musl / ARM64、macOS の Intel / Apple Silicon、Windows の x86_64）をビルドし、GitHub Release を作る
3. Homebrew tap（`owayo/homebrew-astro-sight`）の Formula と bottle を更新し、winget-pkgs に更新のマニフェストを提出する

版の形式は `yy.m.counter`（例: `26.9.103`）。counter は年月が変わると 100 に戻り、同じ年月の中ではリリースのたびに 1 ずつ増える。

`dry_run` を有効にすると、次の版を計算してログに出すだけで、コミット・タグ・ビルド・リリースは行わない。6 つのターゲットのビルドは、CI の build ジョブが main への push と PR のたびに同じ設定で確かめている。

リポジトリに要る設定値は次のとおり。

| 名前 | 置き場所 | 用途 |
|---|---|---|
| `APP_CLIENT_ID` | Actions の Variables | Homebrew tap に push する GitHub App の Client ID |
| `PRIVATE_KEY` | Actions の Secrets | 同じ GitHub App の秘密鍵 |
| `WINGET_TOKEN` | Actions の Secrets | winget-pkgs に PR を出すトークン。未設定なら winget への提出だけを飛ばす |

## License

MIT
