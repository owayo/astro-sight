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
- **MCP サーバーモード**（rmcp 1.6 による stdio JSON-RPC 2.0、ワークスペースサンドボックス、11 ツール）
- **デフォルト compact JSON** 出力（`--pretty` で整形出力）
- **バッチ処理**（`--paths` / `--paths-file` で複数ファイル NDJSON 出力）
- **JSON エラー出力**（`{"error":{"code":"...","message":"..."}}` を stdout に出力）
- **入力検証の強化**（`refs` の空 `name/names` を拒否、`--paths` / `--paths-file` の空リストを拒否、`session` の空文字・非 UTF-8・不正パスの `ASTRO_SIGHT_WORKSPACE` を拒否、`--dir` にはディレクトリのみ許可、streaming `context` は JSON prefix 出力前に入力を検証）
- **セキュリティ** — パス境界チェック（Session/MCP のワークスペースサンドボックス、相対パスはワークスペースルート基準、非 UTF-8 パスは canonicalize 後に fail-closed で拒否）、ファイル/入力サイズ 100MB 上限（`session` / `context` / `impact` の生入力を含む）、`git diff` / `git show` / `git blame` に渡す `--base` 等 revision は `-` プレフィクス / NUL / 空文字を拒否（オプション誤認識防止、`cochange --blame` の `--paths` / `--paths-file` 経由でも検証）
- **トークン最適化** — version フィールド省略（doctor/MCP のみ）、compact キー短縮（`lang`/`ln`/`col`/`ctx`/`refs`/`src`/`def`/`ref`/`fn`/`cx` 等）、calls を caller グルーピング、CompactAstEdge フラット化、refs/context で相対パス出力、symbols デフォルト compact 出力（`--doc` で docstring 付加、`--full` で旧来の完全出力）
- **dead-code 規約除外** — PHPUnit 規約 (`*Test` / `*TestCase` クラス、`testXxx` / `setUp` / `tearDown` 等) と Python unittest/pytest 規約 (`unittest.TestCase` 派生クラス + 同一ファイル内の間接継承を fixed-point で解決、`test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` メソッド、`test_*.py` / `*_test.py` のトップレベル `test_*` 関数、`conftest.py` 内の関数) を dead-code から除外（テストランナーが動的 discover するため）
- **循環的複雑度** — symbols 出力に `cx`（cyclomatic complexity）を付加（関数/メソッドのみ、ベース1 + 分岐ノード数、ネスト関数/クロージャは除外）、言語別分岐ノード定義（Rust/JS/TS/Python/Go/Java/Kotlin/Ruby/PHP/C#等）
- **設定ファイル** — `~/.config/astro-sight/config.toml`（TOML 形式、`astro-sight init` で生成）
- **ロギング** — logroller による日次ローテーション（ローカルタイムゾーン、3日保持）

## Key Modules

- `src/service.rs` - **AppService**: 全コア操作の統一エントリポイント（extract_ast/symbols/calls/imports/lint/sequence/cochange, find_references/find_references_batch, analyze_context + パス検証 + tracing ログ）
- `src/skill.rs` - スキルインストール（`skill-install claude/codex` → ~/.claude/skills/ or ~/.codex/skills/）
- `src/config.rs` - 設定ファイル管理（ConfigService: load/generate、TOML 形式）
- `src/logger.rs` - ロギング（logroller 日次ローテーション、3日保持、tracing-subscriber）
- `src/cli.rs` - CLI サブコマンド定義（ast, symbols, calls, refs, context, impact, review, dead-code, imports, lint, sequence, cochange, doctor, session, mcp, init）
- `src/main.rs` - コマンドディスパッチ、キャッシュ層、バッチ処理（全て AppService 経由）、`batch_ndjson` はスコープドスレッド + 順序保持スロットで並列処理と低ピーク RSS を両立
- `src/mcp/mod.rs` - MCP サーバー（AstroSightServer + AppService::sandboxed(cwd) + 11 ツール、fail-closed: sandbox 生成失敗時はパニック）
- `src/engine/parser.rs` - tree-sitter パーサー管理（100MB ファイルサイズ上限、SourceBuf によるゼロコピー mmap）
- `src/engine/extractor.rs` - AST ノード抽出
- `src/engine/symbols.rs` - シンボル抽出（tree-sitter クエリ）+ スコープ判定（is_local_scope_symbol, is_symbol_exported）+ 循環的複雑度（calculate_complexity）+ Python フレームワーク entrypoint デコレータ判定（has_framework_entrypoint_decorator_python）+ Python クラス base 抽出（python_class_base_names）
- `src/engine/calls.rs` - コールグラフ抽出（言語別 call expression クエリ）
- `src/engine/refs.rs` - クロスファイル参照検索（ignore + rayon 並列 + fold/reduce 集約 + memchr/memmem 事前フィルタ + Aho-Corasick バッチ検索 + `collect_files` pub ユーティリティ）、CI 言語（Xojo）では正規化キー衝突を `Vec<usize>` で吸収、行コンテキスト抽出は `memchr` で該当行のみ UTF-8 変換
- `src/engine/diff.rs` - unified diff パーサー
- `src/engine/impact/mod.rs` - 影響分析オーケストレーター（2パス方式: collect affected → batch refs）、`ParsedFile` キャッシュは `SourceBuf` を直接保持し mmap ゼロコピー経路を維持
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
