# astro-sight

AI エージェント向け AST 情報生成 CLI (Rust)

## Architecture

- **AppService 層** — CLI / Session / MCP の統一コアロジック（`src/service.rs`）
- **tree-sitter** ベースの構文解析エンジン（16言語対応）
- **手書き lexer** バックエンド（`src/engine/lexer.rs`）。tree-sitter で OOM する言語向けの fallback として v26.6 で導入。現状 Xojo を lexer-only でサポート（`symbols` / `refs` / `dead-code` のみ動作、`calls` / `imports` / `ast` / `lint` / `sequence` は `UNSUPPORTED_LANGUAGE` を返す）
- **BLAKE3** ベースのファイルキャッシュ（単一ファイル `ast` / `symbols` は内容ハッシュに canonical path を混ぜ、同一内容の別ファイルや別言語で `path` / `lang` が混線しない）
- **clap derive** による CLI 引数パーサー
- **NDJSON** ストリーミングセッション対応
- **スマートコンテキスト**（diff→影響分析、関数シグネチャ変更は識別子境界で照合して prefix 名の別関数を誤検出しない、単一/バッチ参照検索は fold/reduce でピーク RSS を抑制、バッチ参照検索 O(N+S)、CLI の `refs --names` は名前を chunk 単位でまとめて走査=既定 64・`ASTRO_SIGHT_REFS_BATCH_CHUNK` で調整、ディレクトリ走査が名前数倍に退化するのを回避）
- **AST 応答の巨大行抑制**（`ast` の `text` / `snippet` は 256 文字上限で切り詰め、巨大行で応答が肥大化しない）
- **MCP サーバーモード**（rmcp 1.7 による stdio JSON-RPC 2.0、ワークスペースサンドボックス、11 ツール）
- **デフォルト compact JSON** 出力（`--pretty` で整形出力）
- **バッチ処理**（`--paths` / `--paths-file` で複数ファイル NDJSON 出力）
- **JSON エラー出力**（`{"error":{"code":"...","message":"..."}}` を stdout に出力）
- **入力検証の強化**（`refs` の空 `name/names` を拒否、`--paths` / `--paths-file` の空リストを拒否、`--paths-file` は 100MB 上限付きで読み込み、`cochange` の `min_confidence` / smoothing prior は有限範囲に制限、`session` の空文字・非 UTF-8・不正パスの `ASTRO_SIGHT_WORKSPACE` を拒否、`--dir` にはディレクトリのみ許可、streaming `context` は JSON prefix 出力前に入力を検証）
- **セキュリティ** — パス境界チェック（Session/MCP のワークスペースサンドボックス、相対パスはワークスペースルート基準、非 UTF-8 パスは canonicalize 後に fail-closed で拒否、`review --diff` 経由の `detect_api_changes` / `filter_diff_files_for_dead_code` も diff の `new_path` / `old_path` を `is_safe_diff_path` でトラバーサル検証）、ファイル/入力サイズ 100MB 上限（`session` / `context` / `impact` の生入力、`--paths-file` を含む、`parser::read_file` は metadata 取得後に拡大する TOCTOU 経路も `take()` + 読み込み後再検証で阻止）、`git diff` / `git show` / `git blame` に渡す `--base` 等 revision は `-` プレフィクス / NUL / 空文字を拒否（オプション誤認識防止、`cochange --blame` の `--paths` / `--paths-file` 経由でも検証）
- **トークン最適化** — version フィールド省略（doctor/MCP のみ）、compact キー短縮（`lang`/`ln`/`col`/`ctx`/`refs`/`src`/`def`/`ref`/`fn`/`cx` 等）、calls を caller グルーピング、CompactAstEdge フラット化、refs/context で相対パス出力、symbols デフォルト compact 出力（`--doc` で docstring 付加、`--full` で旧来の完全出力）
- **dead-code 規約除外** — PHPUnit 規約 (`*Test` / `*TestCase` クラス、`testXxx` / `setUp` / `tearDown` 等) と Python unittest/pytest 規約 (`unittest.TestCase` 派生クラス + 同一ファイル内の間接継承を fixed-point で解決、`test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` メソッド、`test_*.py` / `*_test.py` のトップレベル `test_*` 関数、`conftest.py` 内の関数) を dead-code から除外（テストランナーが動的 discover するため）。Angular `@Component` / `@Directive` 装飾クラスのライフサイクルフック (`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`) も Angular ランタイムが change detection サイクルで自動呼び出しする暗黙エントリポイントのため dead-code から除外
- **C/C++ dead-code liveness 補正** — 本体を持たない前方宣言 / opaque tag (`typedef struct st_mysql MYSQL;` の `st_mysql` 等、外部ライブラリの不透明構造体タグ) は「定義」ではないため dead 候補から除外 (`symbols.rs::is_cpp_forward_declaration`)。enum は型名が直接使われなくても列挙子のいずれかが参照されていれば live、body あり typedef tag は alias 名経由の参照でも live と判定する (`symbols.rs::collect_cpp_dead_liveness_aliases` で enum→列挙子 / typedef tag→alias を集め、`refs.rs::cpp_typedef_enum_definition_context` で enumerator name / typedef alias 名のみ Definition 分類し宣言行を参照と二重計上しない。typedef の元型・enumerator value 式・配列長式内の識別子は Reference)。すべて `LangId::C | Cpp` で閉じ symbols 公開出力は不変
- **フレームワーク自動検出** — `--framework` 未指定時でも `<dir>/package.json` の `dependencies` / `devDependencies` に `next` キーがあれば自動で `nextjs` プリセットを適用 (`peerDependencies` / `optionalDependencies` は誤爆しやすいため対象外)。明示指定は常に auto detect に優先する
- **API 差分検出 (api.add / api.rm / api.mod)** — bin-only Rust crate (`src/lib.rs` なし & `Cargo.toml` の `[lib]` セクションなし) の `pub fn` 追加・削除・シグネチャ変更は外部公開 API ではないため API 差分から除外。`api.rm` 経路は base リビジョン側の `Cargo.toml` / `src/lib.rs` を `git show` で取得して判定し、新ツリーで `src/lib.rs` を削除した同時 diff でも旧公開 API の削除は誤抑制しない。C/C++ では tree-sitter が誤パースする実関数 body 内ネスト `function_definition` (`BOOST_FOREACH(...){}` 等のマクロ呼び出し) を exported から除外し (`symbols.rs::is_cpp_nested_function`)、同名シンボルが旧/新いずれかに複数ある場合 (overload / 誤パース) は別物同士の突き合わせを避けるため曖昧として api.mod から除外する
- **循環的複雑度** — symbols 出力に `cx`（cyclomatic complexity）を付加（関数/メソッドのみ、ベース1 + 分岐ノード数、ネスト関数/クロージャは除外）、言語別分岐ノード定義（Rust/JS/TS/Python/Go/Java/Kotlin/Ruby/PHP/C#等）
- **設定ファイル** — `~/.config/astro-sight/config.toml`（TOML 形式、`astro-sight init` で生成、`log_path` 未指定時は config ファイル隣の `logs/`、明示指定時は同じ値でも尊重）
- **ロギング** — logroller による日次ローテーション（ローカルタイムゾーン、3日保持）。debug 時のログ初期化失敗（書込不可ディレクトリ等）は警告に留めて解析本体は継続する（read-only 環境などへの堅牢化）
- **git 管理外ディレクトリの graceful skip** — `--git` を受けるコマンド（context / impact / review / dead-code / cochange）が git 管理外 dir で実行された場合、`git rev-parse --is-inside-work-tree`（`LC_ALL=C`）で事前判定し「解析対象なし」として **exit 0** で skip。`--hook`（review / impact）は完全無出力（silent skip）、通常 CLI は各結果型に `skipped: Option<SkipInfo>`（`reason="not_git_repository"` / `source="git"` / `message`）を付けた空結果を返し「差分なし」と「git 管理外」を区別可能にする。真のエラー（base 不正・git 実行不能・壊れた repo・権限）は従来どおり exit 1（fail-closed）。判定は `commands.rs` の `resolve_git_diff`（経路A: context/impact/review/dead-code）と `resolve_blame_source_files`→`BlameSourceResolution`（経路B: cochange、`--paths`/`--paths-file` 明示時は管理外でも明示分を尊重）に集約。impact は構造化 JSON 出力を持たないため skip も無出力 exit 0（既存の「差分なし」と一貫）

## Key Modules

- `src/service.rs` - **AppService**: 全コア操作の統一エントリポイント（extract_ast/symbols/calls/imports/lint/sequence/cochange, find_references/find_references_batch, analyze_context + パス検証 + cochange オプション検証 + tracing ログ）
- `src/skill.rs` - スキルインストール（`skill-install claude/codex` → ~/.claude/skills/ or ~/.codex/skills/）
- `src/config.rs` - 設定ファイル管理（ConfigService: load/generate、TOML 形式、`log_path` の未指定と明示指定を区別）
- `src/logger.rs` - ロギング（logroller 日次ローテーション、3日保持、tracing-subscriber）
- `src/cli.rs` - CLI サブコマンド定義（ast, symbols, calls, refs, context, impact, review, dead-code, imports, lint, sequence, cochange, doctor, session, mcp, init）
- `src/main.rs` - コマンドディスパッチ、キャッシュ層、バッチ処理（全て AppService 経由）、`batch_ndjson` はスコープドスレッド + 順序保持スロットで並列処理と低ピーク RSS を両立
- `src/mcp/mod.rs` - MCP サーバー（AstroSightServer + AppService::sandboxed(cwd) + 11 ツール、fail-closed: sandbox 生成失敗時はパニック）
- `src/engine/parser.rs` - tree-sitter パーサー管理（100MB ファイルサイズ上限、SourceBuf によるゼロコピー mmap）
- `src/engine/extractor.rs` - AST ノード抽出
- `src/engine/symbols.rs` - シンボル抽出（tree-sitter クエリ）+ スコープ判定（is_local_scope_symbol, is_symbol_exported）+ 循環的複雑度（calculate_complexity）+ Python フレームワーク entrypoint デコレータ判定（has_framework_entrypoint_decorator_python）+ Python クラス base 抽出（python_class_base_names）+ Angular ライフサイクルフック判定（is_js_ts_angular_lifecycle_hook: `@Component` / `@Directive` 装飾クラスのメソッドのみ true）
- `src/engine/calls.rs` - コールグラフ抽出（言語別 call expression クエリ）
- `src/engine/refs.rs` - クロスファイル参照検索（ignore + rayon 並列 + fold/reduce 集約 + memchr/memmem 事前フィルタ + Aho-Corasick バッチ検索 + `collect_files` pub ユーティリティ）、lexer-only 言語 (Xojo) は `find_refs_via_lexer` / `find_refs_batch_via_lexer` 経由で identifier 走査、行コンテキスト抽出は `memchr` で該当行のみ UTF-8 変換（tree-sitter 経路の `extract_line_context` と lexer 経路の `extract_line_context_bytes` はともに 256B 上限で巨大行を切り詰め）
- `src/engine/lexer.rs` - 手書き state machine による未サポート言語向け fallback。`LexerProfile` でキーワード/コメント/文字列区切り/定義 keyword を宣言し、`extract_symbols` と `find_identifier_refs` を提供する。Xojo profile を内蔵
- `src/engine/diff.rs` - unified diff パーサー（`HunkProgress` で hunk の old/new 行数を厳密追跡し、本体の `--- a/` / `+++ b/` 風コンテンツ行をファイルヘッダと誤認しない＝以降の本体脱落による false negative を防ぐ。`parse_hunk_header` / `HunkProgress` / `HunkBodyLine` は pub(crate) で signature.rs と共用）
- `src/engine/impact/mod.rs` - 影響分析オーケストレーター（CI 言語のみの diff は Pass1 前に skip、通常は 3パス方式: collect affected → stream refs → assemble）、`ParsedFile` キャッシュは `SourceBuf` を直接保持し mmap ゼロコピー経路を維持。affected 判定（`find_affected_symbols` / `symbol_overlaps_hunks`）は `range.end.line` を包含的に扱い、単一行シンボル（`start==end`）や複数行シンボルの最終行のみの変更も取りこぼさない（ゼロ幅 hunk = pure-delete は隣接行削除の過検出を避けるため半開区間を維持、pure-add の全体カバー判定は `hunk_end > sym_end`）
- `src/engine/impact/signature.rs` - diff の `+` / `-` 行から関数シグネチャ変更を検出（識別子境界で照合し、`foo` と `foo_bar` のような prefix 名を混同しない）。`detect_signature_changes` / `is_definition_header_in_changed_lines` / `is_symbol_in_changed_lines` はいずれも diff.rs の `HunkProgress` で hunk 行数を追跡し、本体の `+++ b/` 風コンテンツ行をヘッダ誤認しない
- `src/engine/sequence.rs` - コールグラフから Mermaid シーケンス図を生成
- `src/engine/imports.rs` - ファイル間の import/export 関係を抽出（言語別 tree-sitter クエリ）
- `src/engine/lint.rs` - YAML ルールによる AST パターンマッチ（tree-sitter クエリ + テキストパターン）
- `src/engine/cochange.rs` - git log / blame から共変更ファイルペアを検出（confidence スコア付き、100ファイル超のコミットはスキップ、`review --base` の missing_cochanges は同じ base を blame 解析にも渡す）
- `src/engine/snippet.rs` - コンテキストスニペット生成
- `src/models/` - Request/Response/AST ノード/Call/Reference/Impact/Sequence/Import/Lint/CoChange/DeadCode 型定義
- `src/error.rs` - AstroError + ErrorCode（PathOutOfBounds 含む）
- `src/cache/store.rs` - BLAKE3 ベースのキャッシュ保存（~/.cache/astro-sight/、CLI の `ast` / `symbols` は呼び出し側で path 込みハッシュを渡す）
- `src/session/mod.rs` - NDJSON セッション処理（生行サイズで100MB上限、相対パスは `ASTRO_SIGHT_WORKSPACE` 基準、空文字・非 UTF-8 を含む `ASTRO_SIGHT_WORKSPACE` の不正値は fail-closed）
- `src/language.rs` - 言語検出（拡張子/shebang）
- `tools/usage-stats/src/main.rs` - Claude Code / Codex の利用ログ集計（`astro-sight` はシェル上の実行コマンドとして現れ、かつ既知サブコマンドを抽出できた場合のみ採用数に数える。パス文字列・プロンプト内の言及・`--version` / `--help` のようなサブコマンドなしの確認起動は除外。`--config <path>` や `/usr/bin/time -o <file>` 経由でも実サブコマンドを抽出）
- `tests/fixtures/` - 多言語テストフィクスチャ（sample.rb, sample.py, sample.go, sample.ts）

## Review Standards

- 修正対象は再現可能で根拠を示せる不具合に限る。推測や好みに基づく変更は行わない
- セキュリティ境界は fail-open にしない。設定値やサンドボックスが不正なら明示的に失敗させる
- コード変更の前には `astro-sight context --dir . --git`、変更後には `astro-sight impact --dir . --git` を実行する
- diff / PR 全体のレビュー依頼では、個別コマンドを積み上げる前に `astro-sight review --dir . --git` で全体像を確認する
- diff 全体の一括レビューでは `astro-sight review --dir . --git` も併用し、影響・共変更・API 差分・死蔵シンボルをまとめて確認する
- 公開 API や export を触った変更では `astro-sight dead-code --dir . --git` も併用して死蔵シンボルを確認する
- 複数の `astro-sight` クエリを連続で投げる場合は `session` を優先し、プロセス起動コストを抑える
- 繰り返しの構造ルール確認には `astro-sight lint --path <file> --rules <rules.yaml>` を使い、アドホックなテキスト検索で代用しない
- 並列集約や大規模リポジトリ向けの変更では `/usr/bin/time -l` でピーク RSS を測定し、`par_iter().collect()` で不要な中間 Vec を全展開していないか確認する
- コードコメントは必要な箇所にだけ付け、付ける場合は日本語で簡潔に記述する

## Build & Test

```bash
make build    # or: cargo build
make test     # or: cargo test
make check    # clippy + fmt check
make help     # 全ターゲット表示
```

## Notes

- tree-sitter-kotlin 0.3.5 は旧 tree-sitter API のため、extern C ブリッジ（`src/language.rs` の `ffi_kotlin`）で対応
- tree-sitter 0.26 では Point/Range のフィールドがメソッドではなくパブリックフィールド
- QueryMatches は StreamingIterator（標準 Iterator ではない）

## Lexer-Only バックエンド (v26.6 で導入)

### 背景
tree-sitter で実用的に parse できない言語向けに、手書き lexer による fallback バックエンドを持つ。
case-insensitive な GLR バックトラッキング系の tree-sitter grammar は parse 中に 1GB/秒級でメモリが線形に膨張する事案 (Xojo 4 行 diff で 30GB OOMKilled の経緯あり) のため、tree-sitter-xojo は v26.6 で Cargo 依存ごと削除した。

### 言語モデルの分離
`src/language.rs` で型を 3 つに分けている:
- `TreeSitterLang` — `ts_language()` を持つのはこの型だけ。Rust/Python/JS 等 16 言語
- `LexerLang` — 手書き lexer で解析する言語 (現状 `Xojo`)
- `DetectedLang` — `TreeSitter(TreeSitterLang) | LexerOnly(LexerLang)` の判定結果型

`LangId::ts_language()` を呼ぶ前に `is_lexer_only()` で振り分ける義務がある (lexer-only に対する `ts_language()` は panic する)。

### コマンド別の挙動
lexer-only 言語に対する各コマンド:
- `symbols` / `refs` / `dead-code` — `src/engine/lexer.rs` 経由で動作 (定義ヘッダ列挙 + identifier 走査)
- `ast` / `calls` / `imports` / `lint` / `sequence` — `UNSUPPORTED_LANGUAGE` エラーを返す
- `context` / `impact` / `review` — 全 changed file が lexer-only のみの diff では cross-file 解析を skip する。`review` は `dead_symbols` も含めて空結果を返す (lexer 経路の cross-file refs は汎用名 noise が多いため将来 PR で本格対応)

### 強制無効化（デバッグ用）
context / impact / review / dead-code の skip 機構は次の env で解除できる (本番では設定しない):
- `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT=1` — context / impact / review 全体 skip / impact pass2 で skip 解除
- `ASTRO_SIGHT_FORCE_CI_LANG_DEAD_CODE=1` — dead_code で skip 解除 (v26.6 では dead-code 自体は lexer 経路で動くため deprecate 扱い)

### 言語追加時の手順
tree-sitter で動かない言語を追加するには:
1. `language.rs` の `LexerLang` に variant を追加
2. `LangId` も対応する variant を追加し `detected()` を更新
3. `src/engine/lexer.rs` に `LexerProfile` を新規追加 (keywords / コメント / 文字列区切り / 定義 keyword)
4. `profile_for()` の match arm に追加
