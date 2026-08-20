<p align="center">
  <img src="docs/images/app.png" width="128" alt="astro-sight">
</p>

<h1 align="center"><bold>AST</bold>ro-sight</h1>

<p align="center">
  AI エージェント向け AST 情報生成 CLI。tree-sitter ベースの高速構文解析で、AST 断片・シンボル定義・スニペットを JSON で返す。
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
  <img src="https://img.shields.io/badge/Zig-F7A41D?logo=zig&logoColor=white" alt="Zig">
  <img src="https://img.shields.io/badge/Xojo_(lexer--only)-5A9E42" alt="Xojo (lexer-only)">
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

# 4. exact node が欲しいときだけ ast へ上げる
astro-sight ast --path src/main.rs --line 10 --col 0

# 5. 繰り返しの構造ルールは lint、順序が重要または 3 段以上の呼び出しは sequence で確認する
astro-sight lint --path src/main.rs --rules rules.yaml
astro-sight sequence --path src/main.rs --function main

# 6. 2 個以上の mixed query は session にまとめる
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

# 旧来の完全出力（hash, range, doc 全て含む）
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
| `cx` | 循環的複雑度。関数/メソッドのみ付与（ベース 1 + 分岐ノード数、ネスト関数/クロージャの分岐は除外） |
| `cn` | enclosing container 名。`impl Default for AppService` の中のメソッドなら `AppService`。同名メソッドの見分けに使う |
| `doc` | docstring（`--doc` 指定時のみ） |

#### 生成ファイルの除外と申告

`refs --dir` と `symbols --dir` は、minified/bundle/IDE helper のファイル名と、
ファイル先頭 4KiB・40 行以内にある生成宣言コメント（`@generated`、
`Code generated by ...`、`DO NOT EDIT THIS FILE` など）を既定で走査から除外する。
マーカーはコメント行頭の宣言形だけを認識するため、文字列リテラルや
「automatically generated comments」のような通常コメント中の説明では発火しない。

除外が 1 件以上あれば、stdout に必ず機械可読な `skipped` を出す。不在は除外 0 件を表す。
`paths` は決定的にソートした先頭 50 件で、全件数は常に `generated`、省略の有無は
`truncated` で確認できる。

```json
{"symbol":"foo","refs":[],"skipped":{"generated":2,"paths":["gen/a.rs","gen/b.rs"]}}
```

`symbols --dir` は NDJSON なので、同じ `skipped` object を持つ control record を末尾に
1 行追加する。複数名の `refs --names` は既存の「1 シンボル 1 レコード」を維持し、共有の
`skipped` を先頭レコードへ 1 回だけ追加する。session / MCP の batch 応答も従来どおり
ルート配列を維持する。

除外せず走査する場合はグローバルオプションを指定する。

```bash
astro-sight --include-generated refs --name foo --dir .
astro-sight --include-generated symbols --dir src
```

設定ファイルでは `skip_generated = false` で同じ動作になる。後方互換の環境変数
`ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1` も引き続き利用できる。`**/parser.c` のように
glob の最終セグメントで具体的なファイル名を指定した場合は、明示指定を尊重してその
ファイルを走査する。`**/*.c` のような通常の filtered scan は既定除外を維持する。

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

### imports - import 依存抽出

```bash
# ファイルが参照するモジュールを抽出
astro-sight imports --path src/main.ts

# 複数ファイルを入力順に処理
astro-sight imports --paths src/main.ts,src/worker.ts
```

Xojo を除く16言語の import / use / include / require を tree-sitter AST から抽出し、`src`、`ln`、`kind`、`ctx` を返す。JavaScript / TypeScript / TSX は通常の import 文と `require()` に加えて、`import("./module")` および置換を含まない `` import(`./module`) `` も認識する。`${expr}` を含む template literal は依存先を静的に確定できないため除外し、呼び出し形式では第1引数だけを依存先として扱う。

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

単一検索と複数シンボル検索はいずれも worker local の fold/reduce で結果を直接統合し、per-file の中間 `Vec` を全ファイル分保持しない。非常に多くの参照が返るシンボルでは出力自体が大きくなるため、`--glob` で対象言語を絞るか、必要に応じて `ASTRO_SIGHT_BATCH_WORKERS` で並列ワーカー数を下げる（既定は論理コア数）。複数シンボル検索（`refs --names`）はディレクトリ走査・Aho-Corasick 走査・parse をすべて名前数に依らずファイル毎 1 回に集約し、パターンは原則 1 個の AC オートマトンに載せる（実測 5 万パターン ≈ 8MB とパターン数にほぼ線形）。`ASTRO_SIGHT_REFS_BATCH_CHUNK`（既定 100,000）を超える大規模入力だけ AC を分割するが、その場合もファイル走査と parse は 1 回のままで、分割サイズに依らず結果は一致する。

Angular テンプレートと Android XML の補助参照スキャンは、各ファイルをそれぞれ 2MB / 1MB に制限する。metadata 確認後にファイルが拡大した場合も、上限 + 1 byte で読み込みを止めてスキップする。

C/C++ の `struct` / `class` / `union` / `enum` tag 名は、本体付き定義だけを Definition とし、`struct X *p`、`sizeof(struct X)`、cast、引数型・メンバ宣言内の `struct X` は Reference として数える。単独 forward declaration は ref/def のどちらにも含めないため、dead-code でも使用中の型 tag を誤って dead にしにくい。

`.h` は既定では C ヘッダとして扱うが、C++ 専用構文のマーカーがあり、C++ parser の方が明確に parse error が少ない場合は C++ として解析する。C ヘッダを不用意に C++ 扱いせず、`class Foo { public: ... }` や `struct X : Base<X> {}` のような C++ ヘッダの `symbols` / `review` / `dead-code` 取りこぼしを抑える。

`context` / `impact` / `review` の `--base` は `git diff` / `git show` / `git blame` にそのまま渡るため、`-` で始まる値・NUL を含む値・空文字を `INVALID_REQUEST` で拒否する（`--output=/path` 等のオプション誤認識を防ぐ）。

出力例:
```json
{
  "symbol": "extract_symbols",
  "refs": [
    { "path": "src/engine/symbols/mod.rs", "ln": 98, "col": 7, "ctx": "pub fn extract_symbols(...)", "kind": "def" },
    { "path": "src/service.rs", "ln": 284, "col": 11, "ctx": "pub fn extract_symbols(&self, path: &str) -> Result<AstgenResponse> {", "kind": "def" }
  ]
}
```

### context - スマートコンテキスト（diff → 影響分析）

unified diff を受け取り、変更の影響範囲を分析する。AI コードレビュー支援機能。
関数シグネチャ変更は識別子境界で照合するため、`foo` と `foo_bar` のような prefix 名の別関数を混同しない。

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

呼び出し元は確信度と破壊性で3系統に分かれる。`impacted_callers` は実際の呼び出し位置で、diff 外に残れば `impact` の blocking 対象になる。owner を確定できない汎用名や、直接 import の証拠がない TS/Rust の同名参照は `low_confidence_callers` に分離する。名前と引数個数を保った関数値参照や、名前を保った modified シンボルの import 行は `informational_callers` に分離し、blocking 対象にしない。削除、引数個数の変更、判定不能な参照は通常側へ残す。

### impact - 未解決の影響検出（stop hook 用）

`context` の結果から、diff に含まれないファイルへの影響を「未解決」と判定する。AI エージェントの stop hook で使用し、未対応の影響先があればブロックして続行を促す。
シグネチャ変更の判定は `context` と同じく識別子境界一致を使うため、テストヘルパーや派生名の変更が基底名の関数変更として波及しない。
関数内で宣言されたローカルシンボルはファイル間影響の起点から除外する。TypeScript/JavaScript、Rust、Python、Go、Java、Kotlin に対応し、Kotlin のネスト関数もトップレベルの同名関数と区別する。

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
- `--dir` が git 管理外 → exit 0（出力なし。`--hook` 有無を問わず silent skip。下記「git 管理外ディレクトリでの graceful skip」参照）

出力例（exit 1 時）:
```
Unresolved impacts found:

src/engine/symbols/mod.rs changed [extract_symbols]:
  → src/service.rs:284
  → src/commands/api_changes/exported.rs:45
```

claw-hooks との連携例（`.claw-hooks.toml`）:
```toml
[[stop_hooks]]
commands = ["astro-sight impact --git --dir ."]
condition = { command_exists = "astro-sight" }
```

#### git 管理外ディレクトリでの graceful skip

`--git` を受け付けるコマンド（`context` / `impact` / `review` / `dead-code` / `cochange`）を git 管理外ディレクトリで実行した場合、内部の `git diff` 失敗をエラーにせず「解析対象なし」として graceful に skip し、**exit 0** で正常終了する。`~/.config` のような git 管理外ディレクトリで編集する際に Claude Code の Stop hook をブロックしないための挙動。

- `--hook`（`review` / `impact`）→ stdout / stderr ともに無出力で exit 0（silent skip）
- 通常 CLI → 空の正常結果に機械可読な `skipped` フィールドを付けて exit 0。「差分なし」と「git 管理外」を区別できる

```json
{ "...": "...", "skipped": { "reason": "not_git_repository", "source": "git", "message": "--git was requested but --dir is not inside a git worktree" } }
```

判定は `git rev-parse --is-inside-work-tree`（`LC_ALL=C`）で行い、worktree / submodule / bare repo に堅牢。**真のエラー**（`--base` 不正・git 実行不能・壊れた repo・権限不足）は従来どおり `exit 1` を維持する。`--diff` / `--diff-file` / stdin 経路は判定を通らず無影響。

#### 未追跡ファイルの取り込み上限

`--git`（非 `--staged`）は未追跡のソースファイルを「新規ファイル」として解析対象に含める（同一作業で作った未追跡ファイルへの参照が「diff 外の未解決影響」と誤報されるのを防ぐため）。ただし **1 ファイル 256KB または 5,000 行を超える未追跡ファイルは対象外**にする。コード生成器の出力や巨大 fixture のような生成物を取り込むと、その全 exported symbol が API 差分の候補になり `review` が数十分かかって Stop hook がタイムアウトするため（実測: 未追跡 22,000 個の `pub fn` で 1.75 秒 → 10 分超）。

tracked ファイルには適用しない。commit / add 済みは意図的にレビュー対象に入れたものと見なせる一方、未追跡は「まだ add していない」= コミット対象か不明で、巨大なら生成物の可能性が高い。

対象外にしたファイルは黙って落とさず `truncations` に出力する（「レビュー済み」と誤読させないため）:

```json
{ "...": "...", "truncations": [{ "path": "generated.rs", "reason": "untracked_file_too_large", "message": "untracked file excluded from --git analysis: lines 80000 exceeds limit 5000" }] }
```

`--hook` では `trunc: [{"f": "generated.rs", "r": "untracked_file_too_large"}]` として出力する（検出ではなく解析範囲の申告なので exit 1 にはしない）。`impact` は構造化 JSON を持たないため stderr の `note:` 行で出す。`--staged` / `--diff` / `--diff-file` は明示された範囲を尊重するため未追跡の取り込み自体を行わない。

#### デフォルト除外

`context` / `impact` / `review` の影響分析は、cross-file 参照検索時にサードパーティ依存と build artifact をデフォルトで除外する。`new` / `save` / `find` / `update` などの汎用メソッド名が 3rd-party / generated コードから大量に流入し、影響先を万件単位の偽陽性で埋めるのを防ぐ。

- vendor / package manager: `vendor`, `node_modules`, `bower_components`, `.venv`, `venv`, `.tox`, `Pods`, `Carthage`
- build artifact: `target`, `build`, `dist`, `out`, `.build`, `DerivedData`, `bin`, `obj`, `coverage`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `CMakeFiles`

追加除外を解除する場合:

```bash
ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1 astro-sight impact --dir . --git
```

`.gitignore` / hidden file の除外と generated file 判定（`refs::collect_files` 経由）は別系統。
generated file だけは `--include-generated` または `skip_generated = false` で解除できる。

#### ユーザー指定の追加除外（v26.5.117+）

固定リストに含まれない命名のディレクトリ (`pjproject-2.15`, `openssl_64_1.1.1c`, `third_party` など) や、より細かい glob パターンで impact 解析対象を絞りたい場合は `--exclude-dir` / `--exclude-glob` を使う。`context` / `impact` / `review` で同じオプションが利用でき、固定リストに**追加**される (デフォルト除外を上書きするものではない)。

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

`--exclude-glob` は `ignore::overrides` の negative pattern として扱う (先頭の `!` は不要、ワークスペース相対)。不正な glob 構文は実行前に `INVALID_REQUEST` で弾く。

### review - 構造化 diff レビュー

`context` の影響分析に加えて、`cochange` による変更漏れ候補、公開 API 差分、死蔵シンボルを 1 回の実行でまとめて返す。PR レビューや pre-merge チェック向け。

`--git --base <rev>` を指定した場合、`missing_cochanges` の blame 解析にも同じ base を使う。複数コミット分の PR をまとめてレビューするときも、diff と共変更候補の解析範囲が揃う。

`missing_cochanges` は共変更が 3 回以上あるペアだけを候補にする (`--cochange-min-samples`、既定 3)。変更行 blame では証拠コミットが 2 件しかない起点が珍しくなく、「1 回だけ一緒に変わった」ペアが confidence 1.0 として上位に並ぶため。探索的に小標本まで見たい場合は `--cochange-min-samples 2` を指定する (単体の `cochange` コマンドは従来どおり既定 2)。候補の重複排除と上位 10 件の選択には、単体コマンドと同じ平滑化済み `score` を使う。raw confidence は証拠の表示・閾値判定に残しつつ、3/3 の小標本が 30/40 のような十分な標本より機械的に上位へ来ることを防ぐ。

依存宣言ファイル (`Cargo.toml` / `package.json` / `pyproject.toml` 等) とロックファイルは `missing_cochanges` の候補にしない。依存を追加するコミットではこれらとソースが必ず一緒に変わるため履歴相関が 100% になるが、その相関は「依存を追加したとき」限定で、import を 1 行も増減させない本体変更には因果が無い。単体の `cochange` コマンドは「過去に一緒に変更された」事実としてマニフェストを出し続ける (ロックファイルは生成物なので両方で除外)。

全 changed file が Xojo などの lexer-only 言語だけの場合、`review` は `impact` / `api_changes` / `dead_symbols` をすべて空結果で返す。lexer 経路の cross-file 解析は汎用名ノイズが多いため、`symbols` / `refs` / `dead-code` の単体コマンドで確認する。

`api_changes.compatible_modified` には、シグネチャ文字列は変わるが既存呼び出しの互換性を保つ変更を出力する。React component の HOC ラップ、未参照 object member 削除、TS/TSX トップレベル関数の末尾 optional/default 引数追加 (`trailing_optional_params`)、Python トップレベル関数 / モジュール直下クラスメソッドの末尾 kwonly+default / 末尾 positional default 引数追加 (`trailing_optional_params`、デコレータ差分や同名関数複数定義は保守的に blocking 維持) は informational として扱い、`--hook` の blocking 対象にしない。同じシンボルに紐づく `impacts` も破壊的影響としては出さず、`mod_compat` の情報提供だけに留める。未参照 object member の判定では削除キーを 1 個ずつ全リポジトリ検索せず、Aho-Corasick で一括事前抽出して各 JS/TS ファイルを最大 1 回だけ parse する。ファイル収集・読み込み・parse の失敗時は互換扱いへ降格せず、従来どおり blocking を維持する。

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
    { "name": "unused_helper", "kind": "function", "file": "src/utils.rs" },
    { "name": "OldConfig", "kind": "struct", "file": "src/config.rs" }
  ]
}
```

同名シンボルが複数ファイルに存在する場合は誤判定防止のためスキップされる。ただし TS/JS と PHP の class member は、owner を安全に一意推定できる場合だけ例外的に判定する。PHP では `Owner::method()` と同一クラス内の `self::method()` を確定参照として扱い、`$obj->method()` や callable 文字列など owner を確定できない参照がある場合は従来どおりスキップする（`static::` は遅延静的束縛でサブクラス override に到達し得るため確定解決しない）。trait を `use` する class / trait / enum 経由の静的呼び出しは、一意に到達する trait method に限り参照として数える（合成先が同名の具象メソッドを持つ場合は PHP の解決順により trait 側へ辿らない）。

#### 実行時規約の自動除外

フレームワークやテストランナーが名前規約・リフレクションで動的に呼び出すシンボルは、識別子レベルの cross-file refs では caller を追跡できず誤検出になるため、以下の規約は自動的に dead-code から除外される:

- **PHPUnit**: `*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest` クラスと `testXxx` / `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` メソッド
- **Python unittest**: `unittest.TestCase` (および `unittest.IsolatedAsyncioTestCase`) を継承するクラス（同一ファイル内の間接継承も fixed-point で解決）と、その `test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` メソッド
- **Python pytest**: `test_*.py` / `*_test.py` ファイルのトップレベル `test_*` 関数と `conftest.py` 内のすべての関数
- **Python フレームワーク登録デコレータ**: Typer / Click / FastAPI / Flask / Django / Celery / pytest 等の登録デコレータが付いた関数・メソッド・クラス
- **Python 動的プロトコルメソッド**: `urllib.request.BaseHandler` 系の `*_open` / `*_request` / `*_response` / `http_error_*` と、watchdog の `FileSystemEventHandler` 系 `on_*` callback。いずれも既知の基底クラスを直接継承するメソッドだけを除外する
- **Angular**: `@Component` / `@Directive` 装飾クラスのライフサイクルフック (`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`) は Angular ランタイムが change detection サイクルで自動呼び出しするため除外

#### フレームワーク自動検出 (v26.5.120+)

`--framework` 未指定時でも、`<dir>` 直下またはモノレポ配下の `package.json` の `dependencies` / `devDependencies` に `next` キーがあれば、自動で `nextjs` プリセットを適用する。自動検出した規約 glob は各 Next.js workspace からの相対パスへ限定するため、非 Next.js の兄弟 workspace にある `app/**/page.tsx` は除外しない。`node_modules`・生成物・symlink は探索対象外で、`peerDependencies` / `optionalDependencies` 経由も誤爆しやすいため対象外。明示指定 (`--framework laravel` 等) は常に auto detect より優先される。

```bash
# ルートまたは配下 workspace の package.json に `next` があれば自動適用される
astro-sight dead-code --dir .
astro-sight review --dir . --git
```

#### bin-only Rust crate の API 差分除外

`review` の `api_changes` (`added` / `removed` / `modified`) は、bin-only Rust crate (`src/lib.rs` が無く `Cargo.toml` に `[lib]` セクションも無い) の `pub fn` 変更を自動的に除外する。bin crate の `pub fn` は crate 外から到達できないため、追加・削除・シグネチャ変更いずれも外部公開 API の互換性問題にはならない。新ツリーで `src/lib.rs` を削除した同時 diff でも、base リビジョン側で library crate だった場合は旧公開 API の削除を正しく `removed` に残す。

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

### doctor - 対応言語チェック

```bash
astro-sight doctor
```

`doctor` は対応言語（tree-sitter 16 言語 + lexer-only の Xojo、計 17）の可用性を確認し、tree-sitter 言語には ABI version も返す。

### session - NDJSON ストリーミング

```bash
echo '{"command":"symbols","path":"src/main.rs"}' | astro-sight session
```

stdin から NDJSON リクエストを受け取り、stdout に NDJSON レスポンスを返す。複数リクエストの連続処理に対応。`ast`, `symbols`, `doctor`, `calls`, `refs`, `context`, `imports`, `lint`, `sequence`, `cochange` をサポートする。1行あたり 100MB（改行を除く生入力サイズ）を上限としている。`ASTRO_SIGHT_WORKSPACE` を指定した場合はそのディレクトリ配下だけを扱い、リクエスト内の相対 `path` / `dir` はワークスペースルート基準で解決する。空文字・非 UTF-8・存在しないパスなどの不正なワークスペース値は `INVALID_REQUEST` で終了する。

```bash
# calls コマンド
echo '{"command":"calls","path":"src/main.rs","function":"main"}' | astro-sight session

# refs コマンド
echo '{"command":"refs","name":"AstgenResponse","dir":"src/"}' | astro-sight session

# context コマンド（diff を直接渡す）
echo '{"command":"context","dir":".","diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n+use new_mod;"}' | astro-sight session
```

`refs` を session で使う場合も `name` または `names` の指定が必須（空文字不可）。

### バッチ処理（ast, symbols, calls, imports, lint, sequence）

複数ファイルを一度に処理し、NDJSON（1ファイル1行）で出力。専用の rayon pool で並列処理しつつ入力順を維持する。tree-sitter Parser が巨大ファイルの作業領域を thread-local に保持してもピーク RSS が論理 CPU 数に比例しないよう、ワーカー数は既定で利用可能 CPU 数と 4 の小さい方に制限する。`ASTRO_SIGHT_BATCH_WORKERS` に正の整数を指定すれば、利用可能 CPU 数を上限に並列度を変更できる。未排出結果はワーカー数の8倍までの窓に制限するため、入力件数に比例してピーク RSS が増えない。stdout が閉じた場合は現在の窓で停止し、残りのファイルを解析しない。

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

`astro-sight symbols --dir src | head` のように、下流コマンドが先に終了して stdout pipe が閉じた場合は panic を表示せず exit 0 で終了する。これは CLI 利用時の通常のページング・サンプリングを壊さないための挙動で、実際の解析エラーは従来どおり JSON エラー + exit code 1 で返す。

## Supported Languages (16 + 1 lexer-only)

| Language | Extension | Crate | Version |
|----------|-----------|-------|---------|
| <img src="https://img.shields.io/badge/-000000?logo=rust&logoColor=white" height="16"> Rust | `.rs` | `tree-sitter-rust` | 0.24 |
| <img src="https://img.shields.io/badge/-A8B9CC?logo=c&logoColor=white" height="16"> C | `.c`, `.h` (既定) | `tree-sitter-c` | 0.24 |
| <img src="https://img.shields.io/badge/-00599C?logo=cplusplus&logoColor=white" height="16"> C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.h` (C++ 構文検出時) | `tree-sitter-cpp` | 0.23 |
| <img src="https://img.shields.io/badge/-3776AB?logo=python&logoColor=white" height="16"> Python | `.py`, `.pyi` | `tree-sitter-python` | 0.25 |
| <img src="https://img.shields.io/badge/-F7DF1E?logo=javascript&logoColor=black" height="16"> JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | `tree-sitter-javascript` | 0.25 |
| <img src="https://img.shields.io/badge/-3178C6?logo=typescript&logoColor=white" height="16"> TypeScript | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-61DAFB?logo=react&logoColor=black" height="16"> TSX | `.tsx` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-00ADD8?logo=go&logoColor=white" height="16"> Go | `.go` | `tree-sitter-go` | 0.25 |
| <img src="https://img.shields.io/badge/-777BB4?logo=php&logoColor=white" height="16"> PHP | `.php`, `.phtml` | `tree-sitter-php` | 0.24 |
| <img src="https://img.shields.io/badge/-ED8B00?logo=openjdk&logoColor=white" height="16"> Java | `.java` | `tree-sitter-java` | 0.23 |
| <img src="https://img.shields.io/badge/-7F52FF?logo=kotlin&logoColor=white" height="16"> Kotlin | `.kt`, `.kts` | `tree-sitter-kotlin` | 0.3.5 * |
| <img src="https://img.shields.io/badge/-F05138?logo=swift&logoColor=white" height="16"> Swift | `.swift` | `tree-sitter-swift` | 0.7 |
| <img src="https://img.shields.io/badge/-512BD4?logo=dotnet&logoColor=white" height="16"> C# | `.cs` | `tree-sitter-c-sharp` | 0.23 |
| <img src="https://img.shields.io/badge/-4EAA25?logo=gnubash&logoColor=white" height="16"> Bash | `.sh`, `.bash`, `.zsh` | `tree-sitter-bash` | 0.25 |
| <img src="https://img.shields.io/badge/-CC342D?logo=ruby&logoColor=white" height="16"> Ruby | `.rb`, `.rake`, `.gemspec` | `tree-sitter-ruby` | [owayo/tree-sitter-ruby](https://github.com/owayo/tree-sitter-ruby) |
| <img src="https://img.shields.io/badge/-F7A41D?logo=zig&logoColor=white" height="16"> Zig | `.zig`, `.zon` | `tree-sitter-zig` | 1.1 |
| Xojo (lexer-only) | `.xojo_code`, `.xojo_window`, `.xojo_menu`, `.xojo_toolbar`, `.xojo_report`, `.rbbas` | 手書き lexer（built-in、v26.6 で導入） | - |

上記 16 言語は tree-sitter クエリによる精密なシンボル抽出に対応。Ruby は Unicode 識別子に対応し、simple case folding の対象となる `ſ` / `K` などを含むメソッド名も欠落なく抽出する。空白を挟む添字代入、括弧なし lambda 仮引数直後の `{}`、空正規表現、括弧なし呼び出し直後の block、空識別子 heredoc も構文エラーなく解析できる。Xojo は tree-sitter ではなく手書き lexer による限定サポート: `symbols` / `refs` / `dead-code` のみ動作し、`calls` / `imports` / `ast` / `lint` / `sequence` は `UNSUPPORTED_LANGUAGE` エラーを返す。`context` / `impact` / `review` は changed file が Xojo のみの diff では cross-file 解析を skip する。`doctor` は Xojo を含めて計 17 言語を報告する。

> **\* Kotlin バージョンについて:** `tree-sitter-kotlin` 0.3.8 以降は `links = "tree-sitter"` を宣言しており、コアクレート `tree-sitter` 0.26 と Cargo の native library リンク名が競合してビルドできない。現在は 0.3.5 系を利用している。
>
> ```
> error: failed to select a version for `tree-sitter`.
>     ... required by package `tree-sitter-kotlin v0.3.8`
> package `tree-sitter` links to the native library `tree-sitter`,
> but it conflicts with a previous package which links to `tree-sitter` as well:
> package `tree-sitter v0.26.6`
> Only one package in the dependency graph may specify the same links value.
> ```

## JSON Compact Keys

JSON 出力はデフォルト compact（`--pretty` で整形）。compact モードではトークン削減のためキー名を短縮:

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

## Output Format

`--format json|toon|auto` で出力形式を切り替える。既定は `json`。

| | JSON | TOON | auto |
|---|---|---|---|
| 既定 | ✅ | | |
| 仕様 | RFC 8259 | [TOON v4.1](https://toonformat.dev/) | 推定トークン数が小さい方 |
| `--pretty` | 有効 | 無視（TOON は元からインデント構造） | JSON が選ばれた場合のみ有効 |
| キャッシュ | compact のみ利用 | 利用しない | 利用しない |

### TOON とは

[TOON](https://toonformat.dev/) (Token-Oriented Object Notation) は JSON と同じデータモデルを、インデントと表形式で表現するフォーマット。同じ内容をより少ないトークンで LLM に渡せる。

```bash
astro-sight symbols --path src/main.rs --format toon
```

```toon
path: src/main.rs
lang: rust
symbols[19]{name,kind,ln,cx}:
  DELIMITER,const,8,null
  needs_quoting,fn,11,6
  is_numeric_like,fn,46,11
```

同じ内容の JSON は次のようになる。キー名が要素ごとに繰り返される分が削減される。

```json
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"DELIMITER","kind":"const","ln":8},{"name":"needs_quoting","kind":"fn","ln":11,"cx":6},...]}
```

astro-sight のリポジトリ (`src/` 全体) での実測値:

| コマンド | JSON | TOON | 削減 |
|---|---:|---:|---:|
| `symbols --dir src` | 223,435 B | 160,702 B | -28% |
| `symbols --path <file>` | 1,804 B | 1,124 B | -38% |
| `calls --path <file>` | 9,756 B | 6,887 B | -29% |
| `refs --name <sym> --dir .` | 1,924 B | 1,382 B | -28% |
| `imports --path <file>` | 3,878 B | 3,215 B | -17% |
| `dead-code --dir <dir>` | 330 B | 243 B | -26% |
| `cochange --git` | 11,342 B | 5,355 B | -53% |
| `review --git` | 2,727 B | 2,119 B | -22% |
| `doctor` | 1,435 B | 578 B | -60% |

出力はリファレンス実装 ([`@toon-format/toon`](https://www.npmjs.com/package/@toon-format/toon) v4.1) の **strict モード** で decode できることを確認している。canonical encoder の要件に合わせ、単一出力・バッチ出力・`auto` で選ばれた TOON のいずれも文書末尾に改行を付けない（JSON / NDJSON の改行終端は従来どおり）。

### auto — トークン数が少ない方を自動選択

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

そこで判定には `文字数 + 3 × 改行数` を使う。astro-sight の出力 1,127 サンプルを tiktoken で実測した結果:

| 判定指標 | 1 回の呼び出しの最悪損失 | 全体（真の最小との差） |
|---|---:|---:|
| 素の文字数 | +28 tokens (15%) | +65 |
| **文字数 + 3 × 行数** | **+1〜2 tokens** | **+10〜67 (0.0001〜0.0008%)** |

係数は 1〜4 が平坦域でどちらのトークナイザでも安定しており、その中央値を採っている（特定のトークナイザ版に賭けた値ではない）。実トークナイザを積まないのは、(1) BPE テーブルで数 MB 増える、(2) 消費側のモデルや tokenizer 版で結果が変わり出力の再現性が壊れる、(3) 上表のとおり推定値で十分近い、の 3 点による。

#### 性質

- **常に両候補以下**（どちらかを選ぶだけなので、片方より悪くなることはない）
- 選択は入力内容だけで決まるので**決定的**。同じ入力・同じバージョンなら常に同じ形式になる
- 実際に両方が選ばれる。上記 1,127 サンプルでは **564 件で JSON、563 件で TOON** が選ばれた（`ast` は JSON、`symbols` / `refs` / `calls` は TOON が勝ちやすい）
- `--pretty` は「選ばれた JSON をどう描画するか」だけを決める。比較そのものは常に compact JSON と TOON で行うため、TOON が勝った場合は `--pretty` の指定は効かない
- 空 object（`{}`）は TOON では空ドキュメント＝無出力になるため、auto は JSON を選ぶ。「結果が空」と「何も出力されなかった」を利用者が区別できなくなるのを避けるため（明示的な `--format toon` は仕様どおり空ドキュメントを出す）
- プロトコル面（下記）では `auto` はエラーにならず JSON になる。「TOON で出せ」という満たせない要求ではなく、JSON を選ぶことも auto の正当な結果のため

**バッチでの近似**: `--paths` / `--paths-file` / `--dir` は解析結果を全件バッファしない設計のため、全レコードを見てから勝者を決められない。**最初の window（既定でワーカー数 × 8 件）を両形式で描画し、その実測値で勝者を決めて以降の window に適用する**。二重エンコードのコストは先頭 window ぶんだけで、解析自体はどの経路でもパス 1 回きり。出力が途中で混ざることはない。

### 常に JSON のままの出力

次の 3 つは相手側が JSON を前提とする契約のため、`--format json|toon` の対象外（`auto` は JSON を選ぶだけなのでエラーにならない）。

| 出力面 | 理由 |
|---|---|
| `session` | 「1 行 = 1 リクエスト / 1 レスポンス」の NDJSON プロトコル |
| `review --hook` / `impact --hook` | Claude Code の Stop hook が消費する JSON 契約 |
| エラー出力 `{"error":{...}}` | 既存スクリプトが parse する機械可読契約 |

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

外側の配列は list form（`- ` 項目）で、tabular form にはしない。tabular 化には全要素を見てからでないと決まらない情報が要り、解析結果を全件バッファしない（ピーク RSS を入力件数から独立させる）という設計要件と両立しないため。要素数 `[N]` は入力パス数から先に分かるので、ヘッダだけは先出しできる。内側の配列は従来どおり tabular form になり、削減量の大半はそちらから来る。

### nullable 列の正規化

astro-sight の compact JSON は `cx`（循環的複雑度）のような値を持たないフィールドをキーごと省略する。一方 TOON の tabular form は配列内の全要素が**同じキー集合**を持つことを要求するため、素直にエンコードすると symbols のような出力が list form へ落ちて **JSON より冗長になる**（実測 +33%）。

そのため astro-sight は、配列内 object のキーが「欠けているだけ」で揃うとき、欠損キーを `null` で補って tabular 形を成立させる。適用は**厳密エンコードより短くなる場合だけ**（同点なら厳密側）なので、TOON が JSON より長くなることは構造的に起きない。

- 補完対象は全要素が非空 object で、全ての値がプリミティブの配列だけ。object を含む列は対象外。
- 列順は要素を順に走査したときのキー初出順で固定する（決定的）。
- **JSON 表現との構造的 round-trip は保証しない**。decode すると JSON が省略していたキーが `null` として現れる。DTO としての意味（`Option<T>` の `None`）は保存される。

この正規化は astro-sight が自分の DTO について行う判断で、`--format toon` のエンコーダ自体は仕様どおりの純粋な実装になっている（任意の JSON に対して欠損キーを null 補完すると、「明示的な null」と「未設定」を区別できなくしてしまうため）。

## Configuration

`astro-sight init` は TOML 形式の設定ファイルを生成する。デフォルトの保存先は `~/.config/astro-sight/config.toml`。

```toml
# デバッグログをファイルに出力する (デフォルト: false)
debug = false

# ログディレクトリのパス (デフォルト: ~/.config/astro-sight/logs)
# log_path = "~/.config/astro-sight/logs"

# 既定の出力フォーマット: "json" | "toon" | "auto" (デフォルト: json)
format = "json"
```

`log_path` を省略した場合は、読み込んだ config ファイルと同じディレクトリの `logs/` を使う。`--config /path/to/config.toml` でカスタム config を使う場合も同じ。`log_path` を明示した場合は、その値がデフォルトパスと同じでも明示指定として尊重する。

## Cache

単一ファイル `ast` / `symbols` の compact 出力を BLAKE3 ベースで保存するキャッシュ。ファイル内容または astro-sight のバージョンが変わればハッシュが変わるため自動的に無効化される（バージョン更新時は解析ロジック/出力スキーマの変更に追従し、内容不変でも結果が変わるケースで stale な結果を返さない）。

- **対象コマンド**: `ast`, `symbols`（単一ファイルモードのみ）
- **キャッシュキー**: `BLAKE3(astro-sight バージョン + canonical path + BLAKE3(ファイル内容))` + コマンド固有サフィックス（オプション組み合わせ別）
- **path/lang の分離**: `ast` / `symbols` の応答には `path` と `lang` が含まれるため、同じ内容でも別ファイル・別拡張子なら別キャッシュとして扱う
- **保存先**: `~/.cache/astro-sight/`
- **ディレクトリシャード**: ハッシュの先頭 2 文字でサブディレクトリを分割（例: `ab/cdef1234....symbols.json`）
- **`--pretty` 時はキャッシュをスキップ**（compact 出力のみキャッシュ）
- **`--no-cache`** で無効化可能

## AI エージェントとの連携

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
`grep` / `rg` を呼ぶ直前に検索パターン自体を確認し、関数名・型名・定数名などの識別子を 1 つでも含む場合は、`astro-sight refs --name <symbol> --dir .` または `refs --names` に置き換えてください。ファイル種別や周辺タスクだけで判断せず、コメントや文字列の偶然一致を避けます。
`symbols` だけで構造を読んだあとに import / caller / call flow を確認する流れでは、最初から `symbols` + `imports` / `calls` / `sequence` を `session` にまとめると、プロセス起動を減らしつつ手順漏れを防げます。
呼び出し順序が重要、または caller/callee の連鎖が 3 段以上になる場合は、`calls` の一覧に加えて `sequence --path <file> --function <name>` で分岐と受け渡し順を確認します。
レビュー観点が繰り返される場合は `lint` で AST/text ルール化し、関連ファイル漏れは `review` の `missing_cochanges` または `cochange --paths <file>` で先に確認します。

### 利用状況の分析

`tools/usage-stats` は Claude Code / Codex の利用ログから astro-sight 採用率とサブコマンド分布を集計する補助ツールです。
`astro-sight` はシェル上の実行コマンドとして現れ、かつ既知サブコマンドを抽出できた場合だけ採用数に数えます。`/skills/astro-sight/SKILL.md` のようなパス文字列、プロンプト内の言及、`astro-sight --version` / `astro-sight --help` のようなサブコマンドなしの確認起動は除外します。
`--pretty` / `--debug` / `--config <path>` などのグローバルフラグや、`/usr/bin/time -o <file> astro-sight ...` のようなラッパー経由でも、実際に実行されたサブコマンドを抽出します。
Codex の従来形式 (`function_call` / `exec_command`) と現行形式 (`custom_tool_call` / `exec` 内の `tools.exec_command`) の双方を解析します。JavaScript の文字列・コメント内に埋め込まれたコマンド例は実行として数えません。
自動継続に使われる `wait` はコード分析や編集の選択ではないため、採用率の分母とツール分布から除外します。

```bash
cargo run --manifest-path tools/usage-stats/Cargo.toml -- --json --days 1
```

### CLAUDE.md/AGENTS.md に追記して確実に使わせる

スキルだけでは Claude Code/Codex が Grep/Read にフォールバックすることがあります。
プロジェクトの `CLAUDE.md` またはグローバルの `~/.claude/CLAUDE.md` に以下を追記すると、構造分析時に astro-sight を優先的に使用するようになります:

````markdown
# Code Structure Analysis — astro-sight MANDATORY Rules

## STOP-AND-CHECK Rule (CRITICAL: Check BEFORE every Grep/grep/rg call)

**Immediately before every Grep, `grep`, or `rg` call, ask yourself**: "Does my search target contain code identifiers (function/class/variable/type/constant/method names)?" Classify the search pattern itself; do not infer from the file type or the surrounding task.
- **YES → Use `astro-sight refs`** (Grep, `grep`, `rg` ALL FORBIDDEN)
- **NO → Grep OK** (error messages, config values, TODOs, file paths, etc.)

This applies to EVERY supported language — including Xojo (`.xojo_code`), Zig, Swift, C#, Ruby. Never assume a language is unsupported and fall back to Grep.

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
| `Grep "TODO"` | ✅ Grep OK | Non-code search |
| `Grep "error message text"` | ✅ Grep OK | String literal search |
| `Grep "config_key"` | ✅ Grep OK | Config value search |

## Workflow Rules (MANDATORY for code changes)
- **Reviewing a diff / PR (START HERE)**: Run `astro-sight review --dir . --git` for impact + cochange + API diff + dead symbols before any piecemeal analysis
- **Before editing code**: Run `astro-sight context --dir . --git` to check impact
- **After editing code**: Run `astro-sight impact --dir . --git` to detect unresolved impacts
- **Understanding a file**: Run `astro-sight symbols --path <file>` to see structure
- **Understanding a directory**: Run `astro-sight symbols --dir <dir>` to see all symbols
- **Exact AST node / parse debug**: Run `astro-sight ast --path <file> --line <n> --col <n>`
- **Finding symbol usage**: Run `astro-sight refs` (Grep FORBIDDEN)
- **Finding multiple symbols**: Run `astro-sight refs --names sym1,sym2 --dir .`
- **Who calls this function?**: Run `astro-sight calls --path <file> --function <name>`
- **What does this file import?**: Run `astro-sight imports --path <file>`
- **Files that change together**: Run `astro-sight cochange --dir . --paths <file>` (or `--git --base <rev>` to derive from a diff)
- **Visualize call flow**: When execution order matters or the flow spans 3+ caller/callee interactions, run `astro-sight sequence --path <file> --function <name>`
- **Find dead code**: Run `astro-sight dead-code --dir .` or `--git` for diff-scoped
- **Enforce repeated structural rules**: Run `astro-sight lint --path <file> --rules rules.yaml`
- **Multiple mixed queries in one run**: If `symbols` will be followed by `imports` / `calls` / `sequence`, start with NDJSON `astro-sight session`

## Command Quick Reference

```
astro-sight refs --name <symbol> --dir .           # Symbol reference search (REPLACES Grep for identifiers)
astro-sight refs --names sym1,sym2 --dir .         # Batch symbol search (REPLACES Grep "FOO|Bar")
astro-sight symbols --path <file>                  # File structure overview
astro-sight symbols --dir <dir>                    # Directory structure overview (NDJSON)
astro-sight ast --path <file> --line <n> --col <n> # Exact AST node at cursor (parse debug)
astro-sight calls --path <file> --function <name>  # Caller/callee relationships
astro-sight context --dir . --git                  # Change impact analysis (run BEFORE editing code)
astro-sight impact --dir . --git                   # Detect unresolved impacts (run AFTER editing code)
astro-sight review --dir . --git                   # Structured diff review (impact + cochange + API + dead)
astro-sight dead-code --dir . --git                # Find dead/unreferenced exported symbols
astro-sight imports --path <file>                  # Import relationships
astro-sight sequence --path <file>                 # Call flow visualization
astro-sight cochange --dir .                       # Co-change patterns
astro-sight session                                # NDJSON multi-query batch (stdin→stdout)
```

## Efficiency Rules
- **`refs` results include `context` (source line)** → No need for additional Read/Grep
- **Batch multiple symbol searches with `refs --names`** (simpler than session)
- **For very common symbols, combine `--glob` with `ASTRO_SIGHT_BATCH_WORKERS`** to keep output size and peak RSS bounded
- **Use Read for surrounding context when editing** (astro-sight shows 1 line only)
````

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
