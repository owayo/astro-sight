# astro-sight

AI エージェント向け AST 情報生成 CLI (Rust)

## Architecture

- **AppService 層** — CLI / Session / MCP の統一コアロジック（`src/service.rs`）
- **tree-sitter** ベースの構文解析エンジン（17言語対応、Xojo は case-insensitive 識別子）
- **BLAKE3** ベースのファイルキャッシュ（単一ファイル `ast` / `symbols` は内容ハッシュに canonical path を混ぜ、同一内容の別ファイルや別言語で `path` / `lang` が混線しない）
- **clap derive** による CLI 引数パーサー
- **NDJSON** ストリーミングセッション対応
- **スマートコンテキスト**（diff→影響分析、単一/バッチ参照検索は fold/reduce でピーク RSS を抑制、バッチ参照検索 O(N+S)）
- **AST 応答の巨大行抑制**（`ast` の `text` / `snippet` は 256 文字上限で切り詰め、巨大行で応答が肥大化しない）
- **MCP サーバーモード**（rmcp 1.7 による stdio JSON-RPC 2.0、ワークスペースサンドボックス、11 ツール）
- **デフォルト compact JSON** 出力（`--pretty` で整形出力）
- **バッチ処理**（`--paths` / `--paths-file` で複数ファイル NDJSON 出力）
- **JSON エラー出力**（`{"error":{"code":"...","message":"..."}}` を stdout に出力）
- **入力検証の強化**（`refs` の空 `name/names` を拒否、`--paths` / `--paths-file` の空リストを拒否、`--paths-file` は 100MB 上限付きで読み込み、`cochange` の `min_confidence` / smoothing prior は有限範囲に制限、`session` の空文字・非 UTF-8・不正パスの `ASTRO_SIGHT_WORKSPACE` を拒否、`--dir` にはディレクトリのみ許可、streaming `context` は JSON prefix 出力前に入力を検証）
- **セキュリティ** — パス境界チェック（Session/MCP のワークスペースサンドボックス、相対パスはワークスペースルート基準、非 UTF-8 パスは canonicalize 後に fail-closed で拒否、`review --diff` 経由の `detect_api_changes` / `filter_diff_files_for_dead_code` も diff の `new_path` / `old_path` を `is_safe_diff_path` でトラバーサル検証）、ファイル/入力サイズ 100MB 上限（`session` / `context` / `impact` の生入力、`--paths-file` を含む、`parser::read_file` は metadata 取得後に拡大する TOCTOU 経路も `take()` + 読み込み後再検証で阻止）、`git diff` / `git show` / `git blame` に渡す `--base` 等 revision は `-` プレフィクス / NUL / 空文字を拒否（オプション誤認識防止、`cochange --blame` の `--paths` / `--paths-file` 経由でも検証）
- **トークン最適化** — version フィールド省略（doctor/MCP のみ）、compact キー短縮（`lang`/`ln`/`col`/`ctx`/`refs`/`src`/`def`/`ref`/`fn`/`cx` 等）、calls を caller グルーピング、CompactAstEdge フラット化、refs/context で相対パス出力、symbols デフォルト compact 出力（`--doc` で docstring 付加、`--full` で旧来の完全出力）
- **dead-code 規約除外** — PHPUnit 規約 (`*Test` / `*TestCase` クラス、`testXxx` / `setUp` / `tearDown` 等) と Python unittest/pytest 規約 (`unittest.TestCase` 派生クラス + 同一ファイル内の間接継承を fixed-point で解決、`test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` メソッド、`test_*.py` / `*_test.py` のトップレベル `test_*` 関数、`conftest.py` 内の関数) を dead-code から除外（テストランナーが動的 discover するため）。Angular `@Component` / `@Directive` 装飾クラスのライフサイクルフック (`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`) も Angular ランタイムが change detection サイクルで自動呼び出しする暗黙エントリポイントのため dead-code から除外
- **フレームワーク自動検出** — `--framework` 未指定時でも `<dir>/package.json` の `dependencies` / `devDependencies` に `next` キーがあれば自動で `nextjs` プリセットを適用 (`peerDependencies` / `optionalDependencies` は誤爆しやすいため対象外)。明示指定は常に auto detect に優先する
- **API 差分検出 (api.add / api.rm / api.mod)** — bin-only Rust crate (`src/lib.rs` なし & `Cargo.toml` の `[lib]` セクションなし) の `pub fn` 追加・削除・シグネチャ変更は外部公開 API ではないため API 差分から除外。`api.rm` 経路は base リビジョン側の `Cargo.toml` / `src/lib.rs` を `git show` で取得して判定し、新ツリーで `src/lib.rs` を削除した同時 diff でも旧公開 API の削除は誤抑制しない
- **循環的複雑度** — symbols 出力に `cx`（cyclomatic complexity）を付加（関数/メソッドのみ、ベース1 + 分岐ノード数、ネスト関数/クロージャは除外）、言語別分岐ノード定義（Rust/JS/TS/Python/Go/Java/Kotlin/Ruby/PHP/C#等）
- **設定ファイル** — `~/.config/astro-sight/config.toml`（TOML 形式、`astro-sight init` で生成、`log_path` 未指定時は config ファイル隣の `logs/`、明示指定時は同じ値でも尊重）
- **ロギング** — logroller による日次ローテーション（ローカルタイムゾーン、3日保持）

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
- `src/engine/refs.rs` - クロスファイル参照検索（ignore + rayon 並列 + fold/reduce 集約 + memchr/memmem 事前フィルタ + Aho-Corasick バッチ検索 + `collect_files` pub ユーティリティ）、CI 言語（Xojo）では正規化キー衝突を `Vec<usize>` で吸収、行コンテキスト抽出は `memchr` で該当行のみ UTF-8 変換
- `src/engine/diff.rs` - unified diff パーサー
- `src/engine/impact/mod.rs` - 影響分析オーケストレーター（CI 言語のみの diff は Pass1 前に skip、通常は 3パス方式: collect affected → stream refs → assemble）、`ParsedFile` キャッシュは `SourceBuf` を直接保持し mmap ゼロコピー経路を維持
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
- `tools/usage-stats/src/main.rs` - Claude Code / Codex の利用ログ集計（`astro-sight` はシェル上の実行コマンドとして現れた場合のみ採用数に数え、パス文字列やプロンプト内の言及は除外。`--config <path>` や `/usr/bin/time -o <file>` 経由でも実サブコマンドを抽出）
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

- tree-sitter-toml 0.20 は旧 API のため、extern C ブリッジで対応
- tree-sitter 0.26 では Point/Range のフィールドがメソッドではなくパブリックフィールド
- QueryMatches は StreamingIterator（標準 Iterator ではない）

## CI 言語 (Xojo 等) の OOM 対策（v26.5.108 で導入、load-bearing fix）

### 背景
case-insensitive な GLR バックトラッキング系言語（現状 Xojo のみ）の `parse_file` は、約 10MB の LR table をロードした瞬間から **約 1GB/秒で線形にメモリが膨張**する。実測 (実 Xojo project の `.xojo_code` 4 行 diff, 5GB watchdog 付き):

| 条件 | Peak RSS | 経過 | 結果 |
|---|---:|---:|---|
| skip ON (既定) | 12 MB | 0.1s | OK |
| `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT=1` で skip 解除 | 5,221 MB | 5.1s | **5GB watchdog で SIGKILL**（防がなければ 30GB+ で OOMKilled） |

CI 環境で Xojo 1 ファイル変更だけで 30GB OOMKilled が発生した経緯あり。

### 実装方針
review / context / impact / dead_code の各フェーズで、diff の **全 changed file が case-insensitive 言語のみ**の場合は parse を起動せずに空結果を返す。削除 diff は `new_path` が `/dev/null` になるため、CI 言語判定では `old_path` を使う:
- `engine/impact/mod.rs`: context / impact / MCP 経由の影響分析を Pass1 前に skip
- `commands.rs`: review 内の context / api_changes フェーズで同じ CI 言語判定を再利用
- `commands.rs` dead_code 検出: CI 言語のみの対象ファイルでは dead-code 検出を skip
- `engine/impact/pass2.rs`: デバッグ用に強制 skip 解除した場合でも cross-file 解析を最終防衛線として skip

### 強制無効化（デバッグ用）
従来挙動に戻したい場合のみ次の env を指定。本番 CI では絶対に設定しない:
- `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT=1` — context / impact / review api_changes / impact pass2 で skip 解除
- `ASTRO_SIGHT_FORCE_CI_LANG_DEAD_CODE=1` — dead_code で skip 解除

### tree-sitter-xojo 側の改善は補助
tree-sitter-xojo の grammar 削減（直近: e14beb6 で parser.c -3.10%, STATE -4.9%）は parse 時の絶対消費量を僅かに下げるだけで、**OOM 防止効果は無い**（実測で確認済み）。CI 復旧の load-bearing fix はあくまで本リポジトリの skip 機構。tree-sitter-xojo 側での解決追求は時間の無駄なので避ける。

### 言語追加時の注意
新たな case-insensitive GLR 系 grammar を `language.rs` の `LangId::is_case_insensitive` に追加すると自動で skip 対象になる。スキップ範囲が想定より広い場合は `is_case_insensitive` の判定境界を見直す。
