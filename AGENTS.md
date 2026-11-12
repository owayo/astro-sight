# astro-sight

AI エージェント向けの AST 情報生成 CLI (Rust)。tree-sitter で 16 言語を解析し、識別子の参照検索 (`refs`)、シンボル一覧 (`symbols`)、diff の影響分析 (`context` / `impact`)、差分レビュー (`review`)、死蔵シンボル検出 (`dead-code`)、共変更の検出 (`cochange`) などの結果を JSON / TOON で返す。

## このファイルの扱い

AGENTS.md (CLAUDE.md はこのファイルへの symlink) は、セッションごとに全文が読み込まれる。長くなるほど毎回の作業が重くなるので、ここには作業のたびに必要になる全体像・規約・ビルド手順だけを書く。

修正ごとの設計メモや経緯 (判断の理由、対象外にした入力、回帰テストの名前) は、このファイルに書き足さず、コミットメッセージと PR / Issue に書く。このファイルを直すのは、モジュール構成・規約・ビルド手順・注意点が変わったときだけにする。編集は AGENTS.md に対して行い、CLAUDE.md は symlink のまま残す。2026-11-02 まで書き足していた設計メモ (約 24 万字) は `git show 3c16818:AGENTS.md` で読める。

## Architecture

CLI (`src/cli.rs` の clap derive と `src/main.rs` のディスパッチ)、`session` (NDJSON)、`mcp` (rmcp による stdio の MCP サーバー) は、どれも `src/service.rs` の `AppService` を通してコアの処理を呼ぶ。session と MCP はワークスペースの外のパスを拒否する。主なディレクトリの役割は次のとおり。

- `src/engine/` は解析エンジン。`parser.rs` が tree-sitter の Parser を言語ごとに再利用し、`query_cache.rs` がクエリのコンパイル結果を保持する。主なモジュールは `symbols/` (シンボル抽出・循環的複雑度)、`metrics/` (依存解決・複雑度・グラフ指標・循環経路・基準比較。ファイル／ディレクトリ単位)、`refs/` (参照検索。走査は `walker.rs` の `walk_refs` 1 本)、`impact/` (diff の影響分析)、`diff.rs` (unified diff のパーサ)、`calls.rs`、`imports.rs`、`cochange.rs`、`sequence.rs`、`lint.rs`
- `src/commands/` は各コマンドの実装で、`commands.rs` が入口。`api_changes/` は API 差分 (api.add / api.rm / api.mod) とその互換判定、`review/` は影響・共変更・API 差分・dead-code をまとめ、Stop hook 用の出力も作る (`--staged` は index のスナップショットを解析する)。dead-code は `dead_code.rs` と `dead_code_member_liveness/`、`--git` の差分入力は `git_input.rs`
- `src/output/` は出力。既定は compact JSON で、`--format toon` か `--format auto` で TOON も選べる (`toon/` は TOON v4.1 の自前エンコーダ)。`limit.rs` は `refs` の出力に件数とトークン予算の上限を掛け、省いたときだけ `result_summary` で申告する。session・hook・エラー行は常に JSON
- `src/models/` は入出力の型、`src/error.rs` はエラー (`{"error":{"code":"...","message":"..."}}` を stdout に出す)、`src/cache/` はキャッシュ (BLAKE3、`~/.cache/astro-sight/v<version>/`)、`src/config.rs` は設定 (`~/.config/astro-sight/config.toml`)、`src/git_support.rs` は git の共通処理
- `src/language.rs` は拡張子と shebang から言語を判定する。`vendor/` の Bash / Kotlin / Rust / Swift / TypeScript は独自修正を入れた文法で、修正の内容は各ディレクトリの `PATCHES.md` にある。Xojo は手書き lexer (`src/engine/lexer/`) で動くが、対応言語としては案内しない
- テストは、単体テストが各モジュールと `src/commands/tests/`、CLI の結合テストが `tests/integration/` (共有ヘルパーは `support.rs`)、フィクスチャが `tests/fixtures/` にある
- `tools/usage-stats/` は利用状況の集計ツールで、別の Cargo プロジェクト (`make ci` の対象外)

## Review Standards

判定と出力の規約:

- 修正対象は、再現でき根拠を示せる不具合に限る。推測や好みに基づく変更は行わない
- **判定を証明できないときは保守側に倒す**。impact と API 差分は blocking のまま残し、dead-code は dead と断定しない。セキュリティ境界も fail-open にしない。設定値やサンドボックスが不正なら、明示的に失敗させる
- **解析の対象を絞ったり出力を省いたりしたら申告する (No silent caps)**。`truncations` / `result_summary` などに残す。黙って落とすと、全部を見た結果と区別できない
- `refs` は識別子の出現をそのまま返す。impact や dead-code の判定のために除く出現は、それぞれの側で除き、`refs` の出力からは消さない。言語ごとの組み込み名の除外リストを `refs` / dead-code に入れない
- **出力順は決定論的に保つ**。`std::collections::HashMap` / `HashSet` は RandomState のため反復順が実行ごとに変わる。ユーザーに見える配列を組み立てるとき、`into_values()` / `iter()` の順をそのまま使わない。入力 Vec の順を保つ (index をバケットに積む) か、明示的な全順序でソートする。ソート後に `truncate(N)` する箇所は特に危険で、同値キーだけで比較すると実行ごとに違う部分集合が落ちる
- **言語別の tree-sitter ノード名テーブル**は、`astro-sight ast` でダンプした実ノード名と照合する。テーブルは `_` の catch-all を置かずに `LangId` を全列挙し、言語の追加時にコンパイルエラーで気付けるようにする。名前の実在はテストで確かめる (`complexity.rs` の `branch_node_kinds_exist_in_every_grammar` と同じ形)。存在しない名前は 1 つもマッチしないだけでエラーにならず、`cx = 1` のような無害に見える値を返す
- **dir 相対パスの文字列は `/` 区切りにそろえる**。`Path::to_string_lossy` は Windows で `\` を返すが、diff のパスは常に `/` 区切りなので、出力に載せるパスや diff のパスと突き合わせるパスは `git_support::normalize_workspace_separators` を通す

実装の規約:

- 構文木を再帰で辿らない。深い入れ子のファイル 1 つでスタックが溢れ、プロセスごと落ちる。`engine/tree_walk.rs` の `walk_preorder` / `any_node` か明示スタックを使う。走査中に `Node::parent` で祖先を辿ると、深さの 2 乗の時間がかかる
- 生成物を外す経路は `engine::generated::judge` (`.gitattributes` の `linguist-generated` → 名前 → ヘッダー) と `engine::gitattributes::AttributeResolver` (gix で Git と同じ規則で解決し、git を起動しない) を通す。経路ごとに `.gitattributes` を読んだり判定の順を変えたりしない
- ファイルは上限付きで読む (`parser::read_file`、`bounded_read::read_input_file`。上限 100MB)。上限の無い `std::fs::read` を新たに書かない
- 出力を解析する `git diff` には `git_support::GIT_DIFF_PARSEABLE_OUTPUT_ARGS` を付ける。付けないと、利用者の diff 設定次第で変更を 1 つも認識できなくなる。`git show` は `git_input.rs` の `git_show_blob` / `GitBlobBatch` を通し、パスの一覧 (`ls-tree` / `ls-files`) は `-z` で読む
- 回帰テストには、修正前のコードで落ちることの確認と、変えてはいけない挙動の対照ケースを入れる。対照が無いと、別の理由で通る空振りのテストになる
- 並列集約や大規模リポジトリ向けの変更では、`/usr/bin/time -l` でピーク RSS を測る。`par_iter().collect()` で不要な中間 Vec を全展開していないか確かめる
- 構文木はソースの 20〜30 倍のメモリを使う (24 MB の生成された `parser.c` 1 本で約 650 MiB)。全ファイルを並列に構文解析する経路で巨大なファイルを含みうるときは、`engine/parse_budget.rs` の予算で大きいファイルの同時解析を制限する (dead-code の候補の抽出と参照の集計が使う)
- コードコメントは必要な箇所にだけ付け、付ける場合は日本語で簡潔に書く

astro-sight 自身の使い方:

- Grep / `grep` / `rg` を呼ぶ直前に検索パターンを確認し、関数・クラス・変数・型・定数・メソッド名を 1 つでも含む場合は `astro-sight refs --name/--names` を使う。ファイル種別や周辺タスクだけで判断しない
- 関数・型を変更する前に `astro-sight refs --name <symbol> --dir .` で呼び出し箇所を列挙する (`context` / `impact --git` は既存の diff しか見ないため、クリーンな作業ツリーでは空を返す)。編集途中で他ファイルへ進む前には `astro-sight context --dir . --git`、変更後には `astro-sight impact --dir . --git` を実行する
- 「誰がこの関数を呼ぶか」は `refs --name <name>` で調べる。`calls --function <name>` は逆向き (`<name>` が呼ぶもの) しか返さない
- diff / PR のレビューでは、個別コマンドを積み上げる前に `astro-sight review --dir . --git` で影響・共変更・API 差分・死蔵シンボルをまとめて確認する。公開 API や export を触った変更では `astro-sight dead-code --dir . --git` も使う
- 構造確認が 2 手以上続く場合 (`symbols` → `imports` / `calls` / `sequence` など) は、最初から `session` にまとめる
- 呼び出し順序が重要な場合や、caller / callee の連鎖が 3 段以上の場合は、`astro-sight sequence --path <file> --function <name>` で流れを可視化する
- 構文境界・parse エラー・巨大行の扱いは、`astro-sight ast --path <file> --line <n> --col <n>` で exact node を確認する
- 関連ファイルの変更漏れは、`review --dir . --git` の `missing_cochanges` か `cochange --dir . --paths <file>` で先に確かめる。0 件だったときは `diagnostics` / `cochange_diagnostics` を読み、「共変更が無い」のか「証拠を作れなかった / 閾値で落ちた」のかを区別する
- 繰り返しの構造ルールの確認には `astro-sight lint --path <file> --rules <rules.yaml>` を使う

## Build & Test

開発コマンドは `make help` を参照する。ツールの版は `mise.toml` が正で、Makefile は `mise exec --` 経由でその版の `cargo` を呼ぶ (mise を使わない場合は `SYSTEM_TOOLS=1` を付ける)。版を変えたら `mise lock --platform linux-x64,linux-arm64,macos-x64,macos-arm64,windows-x64` で `mise.lock` を作り直す。

```bash
make setup    # mise.toml のツールチェーンを入れ、依存を取得する (初回と依存の更新後)
make build    # デバッグビルド
make test     # テスト (cargo-nextest で単体・結合テスト + cargo test --doc)
make check    # fmt-check + clippy (既定の feature と --all-features、--all-targets)
make ci       # check + test (CI と同じ検査)
```

- 一部のテストだけを走らせるときは `mise exec -- cargo nextest run <filter>` を使う
- テストは nextest で 1 件ずつ別のプロセスで走る。テスト間で static やプロセスの状態 (環境変数・カレントディレクトリ) を共有する前提で書かない
- CPU を長く使うテストを足したら、`.config/nextest.toml` の override (`priority` / `threads-required`) にも足す
- Cargo のコマンドには `CARGO_FLAGS` (既定 `--locked`) が付く。`.cargo/config.toml` の `[patch]` で Cargo.lock が手元でだけ変わるときは `make ci CARGO_FLAGS=` で外す
- CI のジョブ構成 (シャード分割・キャッシュ・Windows の設定) は `docs/development.ja.md` にある

## Notes

- tree-sitter (0.26 以降) の Point / Range の値はメソッドではなくフィールド。QueryMatches は StreamingIterator で、標準の Iterator ではない
- tree-sitter-kotlin は `=0.3.5` に固定し、`src/language.rs` の `ffi_kotlin` (extern C ブリッジ) で繋ぐ。0.3.8 以降は tree-sitter `>=0.21,<0.23` を要求し、今の tree-sitter 0.27 と組み合わせられない。後継の `tree-sitter-kotlin-ng` への移行は、ノード名が大きく変わるため保留している
- Windows のメインスレッドのスタックは、`build.rs` でリンカに渡して 8MB にしている。既定の 1MB では、debug ビルドが起動直後に stack overflow で落ちる。rayon のプールと MCP の tokio ランタイムのスタックも `engine::WORKER_STACK_SIZE` (8 MiB) にそろえている
- lexer-only 言語 (Xojo) に `LangId::ts_language()` を呼ぶと panic する。先に `is_lexer_only()` で振り分ける。lexer-only 言語では `ast` / `calls` / `imports` / `lint` / `sequence` が `UNSUPPORTED_LANGUAGE` を返し、`context` / `impact` / `review` は lexer-only のファイルだけを変えた diff を解析しない (デバッグ用に `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT=1` で解除できる)
