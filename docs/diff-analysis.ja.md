# diff の解析

unified diff (`--git` なら作業ツリーの `git diff`) を受け取り、変更の影響を調べるコマンドをまとめる。`context` が影響範囲を返し、`impact` は未解決の影響があれば exit 1 で止め、`review` は影響・共変更・公開 API の差分・死蔵シンボルを 1 回で返す。`dead-code` と `cochange` は diff が無くても使える。

## context - スマートコンテキスト（diff → 影響分析）

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

## impact - 未解決の影響検出（Stop hook 用）

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
```text
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

### git 管理外ディレクトリでのスキップ

`--git` を受け付けるコマンド（`context` / `impact` / `review` / `dead-code` / `cochange`）を git 管理外ディレクトリで実行した場合は、内部の `git diff` の失敗をエラーにしない。「解析対象なし」としてスキップし、**exit 0** で正常終了する。`~/.config` のような git 管理外ディレクトリで編集しているときに、Claude Code の Stop hook をブロックしないための挙動である。

- `--hook`（`review` / `impact`）→ stdout / stderr ともに無出力で exit 0
- 通常 CLI → 空の正常結果に機械可読な `skipped` フィールドを付けて exit 0。「差分なし」と「git 管理外」を区別できる（構造化出力を持たない `impact` は無出力）

```json
{ "...": "...", "skipped": { "reason": "not_git_repository", "source": "git", "message": "--git was requested but --dir is not inside a git worktree" } }
```

判定には `git rev-parse --is-inside-work-tree`（`LC_ALL=C`）を使うので、worktree / submodule / bare repo でも正しく判定できる。**真のエラー**（`--base` 不正・git 実行不能・壊れた repo・権限不足）は `exit 1` を返す。`--diff` / `--diff-file` / stdin で diff を渡す経路は、この判定を通らない。

### 未追跡ファイルの取り込み上限

`--git`（非 `--staged`）は未追跡のソースファイルを「新規ファイル」として解析対象に含める（同一作業で作った未追跡ファイルへの参照が「diff 外の未解決影響」と誤報されるのを防ぐため）。ただし **1 ファイル 256KB または 5,000 行を超える未追跡ファイルは対象外**にする。上限を設けるのは、コード生成器の出力や巨大 fixture のような生成物を取り込むと、その全 exported symbol が API 差分の候補になるため。そうなると `review` に数十分かかり、Stop hook がタイムアウトする。実測では、未追跡ファイルがなければ 1.75 秒で終わる `review` が、`pub fn` を計 22,000 個持つ未追跡ファイルを置いただけで 10 分を超えても終わらなかった。

追跡済み（tracked）のファイルには、この上限を適用しない。commit / add 済みのファイルは、意図的にレビュー対象に入れたものと見なせる。一方、未追跡のファイルは「まだ add していない」ものなので、コミット対象かどうかが分からず、巨大なら生成物の可能性が高い。

対象外にしたファイルは黙って落とさず `truncations` に出力する（「レビュー済み」と誤読させないため）:

```json
{ "...": "...", "truncations": [{ "path": "generated.rs", "reason": "untracked_file_too_large", "message": "untracked file excluded from --git analysis: lines 80000 exceeds limit 5000" }] }
```

`--hook` では `trunc: [{"f": "generated.rs", "r": "untracked_file_too_large"}]` として出力する（検出ではなく解析範囲の申告なので exit 1 にはしない）。`impact` は構造化 JSON を持たないため stderr の `note:` 行で出す。`--staged` / `--diff` / `--diff-file` は明示された範囲を尊重するため未追跡の取り込み自体を行わない。

### 解析できないソースの申告

`dead-code` / `review` は、ディレクトリ内に存在するが**どのバックエンドでも解析できなかったソースファイル**も同じ `truncations` に出す。読めないファイル内の参照を数えないまま dead と断定すると、生きているシンボルを dead と報告してしまう（`.vue` の `<script>` からしか使われていない TypeScript 関数など）。そこで、「参照がない」のか「観測できなかった」のかを利用者が区別できるようにしている。

```json
{ "...": "...", "truncations": [{ "reason": "unanalyzable_source", "message": "1 \".vue\" file(s) were not analyzed (no parser for this language); references inside them are not counted (e.g. src/App.vue)" }] }
```

対象は**プログラム / テンプレート言語だと確実に言える拡張子**に限る（`.vue` / `.svelte` / `.astro` / `.erb` / `.razor` / `.scala` / `.dart` / `.lua` など）。走査対象外のファイルには画像・アーカイブ・データも含まれるため、全件を申告すると本当に見落としているソースがノイズに埋もれる。出力は拡張子単位に 1 件へ畳み、拡張子 10 種 / 代表パス 3 件を上限とする。該当ファイルがなければ `truncations` 自体を出力しない。

`dead-code` は参照を数えた範囲（ディレクトリ全体）の解析できないソースを申告する。`--glob` や `--git` で dead の候補を絞っても参照はディレクトリ全体から数えるので、申告の範囲も狭めない。

### 既定の除外

`context` / `impact` / `review` の影響分析は、ファイル間の参照検索でサードパーティ依存と build artifact を既定で除外する。`new` / `save` / `find` / `update` などの汎用メソッド名がサードパーティや生成コードから大量に流入し、影響先を万件単位の偽陽性で埋めるのを防ぐ。

- vendor / package manager: `vendor`, `node_modules`, `bower_components`, `.venv`, `venv`, `.tox`, `Pods`, `Carthage`
- build artifact: `target`, `build`, `dist`, `out`, `.build`, `DerivedData`, `bin`, `obj`, `coverage`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `CMakeFiles`
  - `bin` のうち Cargo パッケージの `src/bin/`（直上が `src` で、その親に `Cargo.toml` がある）はバイナリターゲットのソースなので除外しない。`--exclude-dir bin` を明示した場合はすべての `bin` を除外する

この既定の除外を解除する場合:

```bash
ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1 astro-sight impact --dir . --git
```

`.gitignore` と隠しファイルの除外、および生成ファイルの判定（`refs::collect_files` 経由）は、この既定の除外とは別の仕組みで動く。このうち生成ファイルの除外だけは、`--include-generated` または `skip_generated = false` で解除できる。

### ユーザー指定の追加除外

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

## review - 構造化 diff レビュー

`context` の影響分析に加えて、`cochange` による変更漏れ候補、公開 API 差分、死蔵シンボルを 1 回の実行でまとめて返す。PR レビューや pre-merge チェック向け。

`--git --base <rev>` を指定した場合、`missing_cochanges` の blame 解析にも同じ base を使う。複数コミット分の PR をまとめてレビューするときも、diff と共変更候補の解析範囲が揃う。

`missing_cochanges` は、共変更が 3 回以上あるペアだけを候補にする（`--cochange-min-samples`、既定 3）。変更行 blame では証拠コミットが 2 件だけの起点がよく現れ、「1 回だけ一緒に変わった」ペアが confidence 1.0 として上位に並ぶため。探索的に小標本まで見たい場合は `--cochange-min-samples 2` を指定する（単体の `cochange` コマンドは既定 2 のまま）。候補の重複排除と上位 10 件の選択には、単体コマンドと同じ平滑化済みの `score` を使う。3/3 の小標本が 30/40 のような十分な標本より機械的に上位へ来るのを防ぐためで、raw confidence は証拠の表示と閾値判定に使う。

ロックファイルと、ソースに対応する依存宣言ファイル（`Cargo.toml` / `package.json` / `pyproject.toml` など）は `missing_cochanges` の候補にしない。依存を追加するコミットでは、これらとソースが必ず一緒に変わるので、履歴相関は 100% になる。しかしその相関は「依存を追加したとき」に限ったもので、import を 1 行も増減させない本体変更とは因果関係がない。依存宣言ファイルを候補から外すのは、ソースと同じエコシステムで、そのソースから見て最も近いものとの組だけに限る（`Cargo.toml` と Python スクリプトのような別エコシステムの組は候補に残る）。単体の `cochange` コマンドは、「過去に一緒に変更された」事実として依存宣言ファイルを出し続ける（ロックファイルは生成物なので両方で除外）。

**外部 snapshot と生成元テストの関係は方向付きで扱う。** snapshot を更新しただけの差分に対して、「生成元テストも変更漏れでは」とは出さない。snapshot は被テスト対象の出力が変わったときにも更新されるので、「テストを変えたら snapshot も変わる」という期待は成り立っても、その逆は成り立たないためである。逆方向は残す。テストを変更したのに snapshot が欠けている場合は、候補に出る。抑制するのは次をすべて満たすペアだけで、1 つでも確認できなければ候補に残す:

- snapshot の直上ディレクトリが正確に `__snapshots__` で、ファイル名末尾の `.snap` を 1 回だけ除いたパスが欠落候補と完全一致する（Jest / Vitest / Bun が共有する標準規約。`tests/__snapshots__/widget.test.tsx.snap` → `tests/widget.test.tsx`）
- 生成元テストが実在する通常ファイルである
- snapshot の**先頭行が既知のランナーヘッダと完全一致**する（`// Vitest Snapshot v1, …` など）。パス規約だけでは手書き fixture や別用途の `.snap` を巻き込むため、生成出力であることをファイル自身で確認する

`.gitattributes` の `linguist-generated` とは判定経路が独立している（あちらは「生成物一般」の宣言で、指定すると両方向とも候補から消える）。カスタム snapshot resolver、inline snapshot、`.snap` 以外の形式は対象外で、いずれも履歴相関を情報として出す。グローバルの `--include-generated` を付けるとこの方向付けも無効化する。単体の `cochange` コマンドは探索的な用途なので方向付けしない。

なお、これは「テスト変更が不要だと証明した」ものではない（期待値だけ更新して必要なテストロジックの変更を忘れることはある）。標準の生成関係にあるペアについて、履歴相関だけを根拠に逆方向の変更を要求しないという推薦方針。

`api_changes.compatible_modified` には、シグネチャ文字列は変わるが既存の呼び出しとの互換性を保つ変更を出力する。次の変更は informational として扱い、`--hook` の blocking 対象にしない。

- React component の HOC ラップ
- 未参照の object member の削除
- TS/TSX のトップレベル関数の末尾への optional / default 引数の追加（`trailing_optional_params`）
- Python のトップレベル関数 / モジュール直下のクラスメソッドの末尾への、kwonly+default 引数または positional default 引数の追加（`trailing_optional_params`）。デコレータの差分がある場合や同名関数が複数定義されている場合は、保守的に blocking を維持する

同じシンボルに紐づく `impacts` も破壊的影響としては出さず、`mod_compat` の情報提供だけに留める。未参照 object member の判定では、削除キーを 1 個ずつ全リポジトリで検索せずに Aho-Corasick で一括して事前抽出し、各 JS/TS ファイルを最大 1 回だけ parse する。ファイルの収集・読み込み・parse に失敗したときは互換扱いへ降格せず、blocking のままにする。

`export const` のような値バインディングは宣言全体（初期化子を含む）を比較するが、**値そのものが関数の場合は本体を比較から外す**（`export function` の本体変更が api.mod にならないのと揃える）。本体を比較から外すのは、次の関数の本体に限る。

- 値そのものであるアロー関数 / 関数式（括弧・`as`・`satisfies` 付きも含む）
- React の `memo` / `forwardRef` に包んだ関数
- オブジェクトリテラルのメンバーの関数（メソッド・`key: () => ...`）

引数・型注釈・キーの追加削除は比較する。それ以外の呼び出しに渡すコールバック（`create((set) => ({ ... }))` など）は、中身がストアの形や値そのものを決めるので本体も比較する。分割代入の束縛（`export const { a, b } = obj`）は「その名前へ至る経路 + 初期化子」で比較する（配列は位置を保つ）。同じ分割代入のほかの束縛を追加・削除しても、残った束縛が api.mod にならないようにするためである。default 値・computed key・rest を含むパターンは宣言全体で比較する。

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

## dead-code - デッドコード検出

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

同名シンボルが複数ファイルに存在する場合は誤判定防止のためスキップされる。ただし TS/JS と PHP の class member は、owner を安全に一意推定できる場合だけ例外的に判定する。PHP では、`Owner::method()` と同一クラス内の `self::method()` を確定参照として扱う。`$obj->method()` や callable 文字列のように owner を確定できない参照がある場合は、スキップする。`static::` は遅延静的束縛によりサブクラスの override に到達し得るため、確定参照としては解決しない。trait を `use` する class / trait / enum 経由の静的呼び出しは、一意に到達する trait method に限り参照として数える。ただし合成先が同名の具象メソッドを持つ場合は、PHP の解決順に従って trait 側へは辿らない。

### 実行時規約の自動除外

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

### 生成ファイルの扱い

生成ファイルとして検出したファイル（先頭の生成宣言コメント、`.gitattributes` の `linguist-generated` など）は、既定で dead 判定の**候補**から外す。ただし生成ファイルの中の参照は常に数える。gRPC の生成ハンドラ（`*_grpc.pb.go`）が手書きのサーバー実装を呼ぶ構成のように、生成コードは実行時に手書きコードを呼ぶため、その参照を捨てると生きているシンボルを dead と報告してしまう。

候補から外したファイルは `generated_candidates_skipped` に `refs` の `skipped` と同じ形で出す（0 件なら出力しない）。`--include-generated`（または `skip_generated = false`）を指定すると生成ファイルのシンボルも候補にする。

```json
{ "...": "...", "generated_candidates_skipped": { "generated": 1, "paths": ["api/greeter_grpc.pb.go"] } }
```

### フレームワーク自動検出

`--framework` 未指定時でも、`<dir>` 直下またはモノレポ配下の `package.json` の `dependencies` / `devDependencies` に `next` キーがあれば、自動で `nextjs` プリセットを適用する。自動検出した規約 glob は各 Next.js workspace からの相対パスへ限定するため、非 Next.js の兄弟 workspace にある `app/**/page.tsx` は除外しない。`node_modules`・生成物・symlink は探索対象外で、`peerDependencies` / `optionalDependencies` 経由も誤って適用されやすいため対象外。明示指定（`--framework laravel` など）は常に自動検出より優先される。

```bash
# ルートまたは配下 workspace の package.json に `next` があれば自動適用される
astro-sight dead-code --dir .
astro-sight review --dir . --git
```

### bin-only Rust crate の API 差分除外

`review` の `api_changes`（`added` / `removed` / `modified`）は、bin-only Rust crate（`src/lib.rs` がなく `Cargo.toml` に `[lib]` セクションもない）の `pub fn` 変更を自動的に除外する。bin-only crate の `pub fn` は crate 外から到達できないため、追加・削除・シグネチャ変更いずれも外部公開 API の互換性問題にはならない。`src/lib.rs` を削除する変更が同じ diff に含まれていても、base リビジョン側で library crate だった場合は、旧公開 API の削除を正しく `removed` に残す。

## cochange - 共変更パターン検出

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
