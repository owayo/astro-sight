# 2026-10-30 定期点検と所属コンテナ判定の性能比較

開始 revision は `4aab94753dc082833c22c35333f0a132be25a7b9`。`main` と `origin/main` は一致し、未コミット・未送信の変更はなく、stash は作っていない。fetch と pull --rebase は成功した。開始時の open Issue は0件だった。

## 依存更新

`mise install` と `cargo fetch --locked` を実行した。`depup --install --include-pinned` の最初の実行は、既知の tree-sitter-kotlin 0.3.8 と tree-sitter 0.27.1 の links 競合で失敗した。depup が Cargo の更新を復元したことを確認した。nextest 0.9.149 は2026-10-20公開で14日未満のため、今回の mise 更新だけを戻した。

Kotlin と独自パッチを持つ Bash/Rust/Swift/TypeScript、および nextest を除外して再実行し、rmcp と rmcp-macros を3.6.0から3.7.0へ更新した。[公式リリースノート](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.7.0)の変更は認証とHTTP transportが中心で、使用中の stdio server に追従修正は不要だった。独立した usage-stats の依存更新はなかった。biome.jsonc はない。

その後のユーザー指定により、tree-sitter-ruby は branch=master の旧コミット `2b400b5d41cff73be1bceecd236abc6ab18a0bcb` から、[v26.10.105](https://github.com/owayo/tree-sitter-ruby/releases/tag/v26.10.105) のタグへ変更した。Cargo.lock は `bfea52ceccccecaf518ab11e03dc8e63328d35d5` に固定している。新しいタグを使う明示的な指定を優先し、この更新には depup の公開経過期間を適用していない。

## 構造・正確性の点検

`symbols --dir src` で全体の構造と複雑さを確認した。候補は Python の所有元解析 `parse` (複雑度55)、宣言変更補正 `reconcile_declaration_changes` (45)、CLI の `dispatch_command` (35)、シンボルの所属判定だった。前3件は言語別の保留条件やコマンドの分岐が主因で、同じ検証境界を保ったまま整理する効果が明確な所属判定を優先した。

`refs`・`calls`・`sequence` でシンボル抽出の流れを確認した。従来の `assign_enclosing_containers` は C 個の型等と F 個の関数の全組合せを比較し、16,000クラス・各1メソッドで256,000,000組を比較していた。公開できる合成入力で再現し、[Issue #198](https://github.com/owayo/astro-sight/issues/198) に記録した。

所属判定を `engine/symbols/containers.rs` へ分離した。終点を降順に処理し、条件を満たす候補を始点順の Fenwick tree に追加する。始点以下のprefixから最小の `(行数差, 元の抽出位置)` を取ることで、従来の行数差だけの順位と同順位の抽出順を維持する。列境界・交差/重複/反転範囲、確定済みの Go receiver 等の所属も維持する。大きい入力の所属判定は O((C+F) log(C+F))、追加領域は O(C+F)。候補32件以下または未確定の関数16件以下では直接比較する。新しい共有キャッシュや並列処理は加えていない。

Git revisionの先頭ハイフン・空文字・NULの拒否、serviceの正規化後のワークスペース境界、上限付きreaderのmetadata確認後の再検査も確認した。今回の変更でこれらの境界は変更していない。追加で断言できる正確性・セキュリティ上の不具合は見つからなかった。

## 性能測定

Windows 11 Home 10.0.26300、Ryzen 7 5700X (8コア/16スレッド)、32 GiB RAM、Python 3.12.9、Rust 1.99.1 (LLVM 23.1.1、x86_64-pc-windows-msvc)。全実行ファイルを `mise exec -- cargo build --release --locked` で作り、既定の mimalloc・LTO・単一 codegen unit・strip をそろえた。

- session の旧新は開始revisionと同じ依存を使う。新版だけに `symbols/mod.rs` と `symbols/containers.rs` の変更を適用し、依存更新から独立した比較にした。
- CLI の新版は今回の最終構成で、rmcpとRuby文法の更新も含む。この比較だけで依存と最適化の効果を切り分けない。
- 各版を1回ウォームアップ後、同じ入力で9回ずつ実行し、順番を試行ごとに反転した。ファイルキャッシュは温まった状態。測定中にビルド・テストを走らせていない。
- session はケースごとの常駐プロセス内の要求時間。CLI は起動込みで、`--no-cache` によって永続結果キャッシュを無効にした。session はこのキャッシュ層を通らない。CLI の最初のキャッシュ命中による試行は抽出性能の結果として採用していない。
- 全測定で旧新の stdout がバイト一致した。最終依存を含む実行ファイルでも、参照検索9ケースとキャッシュを無効にしたシンボル抽出6ケースの一致を別に確認した。[出力の件数・ハッシュ](contracts.json)を保存している。
- working set は Windows の GetProcessMemoryInfo によるピークで、割当回数ではない。session はウォームアップと先行要求のピークも含む。

```powershell
python tools/bench-refs.py --suite symbols --mode session --repeats 9 --baseline C:/temp/before.exe --candidate C:/temp/isolated-after.exe --output-directory C:/temp/symbols-session
python tools/bench-refs.py --suite symbols --mode cli --repeats 9 --baseline C:/temp/before.exe --candidate C:/temp/final-after.exe --output-directory C:/temp/symbols-cli
```

生成入力と出力は指定先に保存される。解析に混入させないため出力先はリポジトリ外にする。生データは [session-samples.csv](session-samples.csv) と [cli-samples.csv](cli-samples.csv)、環境・実行ファイルのSHA-256・要約は [session-summary.json](session-summary.json) と [cli-summary.json](cli-summary.json)。

### 同じ依存での session 比較

| ケース | 変更前 ms 中央値 [最小–最大] | 変更後 ms 中央値 [最小–最大] | 短縮率 | 最大 working set MiB 前→後 |
|---|---:|---:|---:|---:|
| classes-1000 | 20.86 [19.49–21.25] | 20.80 [19.78–25.72] | 0.3% | 19.66→20.11 |
| classes-4000 | 93.06 [90.47–101.61] | 88.95 [83.68–91.27] | 4.4% | 37.03→37.59 |
| classes-16000 | 507.72 [486.36–600.54] | 360.77 [352.93–465.18] | 28.9% | 84.39→84.50 |
| functions-control | 65.16 [62.43–98.67] | 68.77 [63.09–93.16] | -5.5% | 26.07→26.14 |
| real-rust | 42.34 [36.33–49.76] | 40.85 [37.53–63.26] | 3.5% | 20.18→20.18 |

### 最終依存を含む CLI 比較（永続結果キャッシュなし）

| ケース | 変更前 ms 中央値 [最小–最大] | 変更後 ms 中央値 [最小–最大] | 短縮率 | 最大 working set MiB 前→後 |
|---|---:|---:|---:|---:|
| classes-1000 | 57.52 [52.11–72.81] | 56.15 [53.96–61.40] | 2.4% | 18.96→18.93 |
| classes-4000 | 129.94 [123.85–145.70] | 119.39 [117.57–129.12] | 8.1% | 26.12→26.19 |
| classes-16000 | 523.49 [507.47–533.70] | 371.65 [365.10–375.36] | 29.0% | 62.07→62.32 |
| functions-control | 96.43 [93.04–97.17] | 96.49 [93.61–114.88] | -0.1% | 23.40→23.41 |
| real-rust | 72.61 [69.55–86.28] | 72.85 [70.09–76.05] | -0.3% | 20.43→20.37 |
| mixed-fixtures | 98.49 [92.90–113.89] | 98.69 [96.75–101.06] | -0.2% | 59.66→56.87 |

16,000クラスの session は中央値で28.9%短縮し、旧新の測定範囲は重ならなかった。最終構成のキャッシュなしCLIでも29.0%短縮した。大きい入力のピーク working set に大幅な増加はなかった。小さいケースと対照は測定範囲が重なるため、中央値だけの小差から改善・退行を断定しない。所属判定以外のパース・抽出・出力を含む処理をすべて線形化したという結果ではなく、通常のキャッシュ命中時の速度改善も主張しない。

## 検証

変更前は `mise exec -- cargo nextest run --locked --no-fail-fast` で3,359件成功、失敗・skipは0だった。最終のタグ参照・最適化を含む構成は同コマンドで3,362件成功、失敗・skipは0だった。

旧実装を独立した比較元として、列境界・同順位・明示所属、1,000個の生成範囲と逆順の結果を照合する2テストを追加した。実際にパースした Rust/Python/TS/TSX の64個ずつの宣言の所属を確かめるテストも追加した。時間制限を通常テストへ加えていない。

- `cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings` と同 `--all-features`、`cargo test --locked --doc`、release build が成功（すべてmise経由）。doctestは0件。
- usage-stats は fmt/clippy と19テストが成功。計測スクリプトは Ruff の format/check と CLI/session の実測が成功。
- 最終 release 実行ファイルの `context`・`impact`・`review --git`・`dead-code --git` が成功。impact は exit 0。既存APIの追加/削除/変更、死蔵・未検証シンボル、解析範囲の truncation は0。
- 共変更候補は10件。verification_target の diff-analysis/output-format 文書と tests/integration.rs は、出力形式・diff分類の契約が不変で新テストも単体モジュール内にあるため、追加修正は不要と確認した。履歴だけの候補は変更義務として扱っていない。
- タグ指定前の古い依存に対する release 単体テスト試行は途中で中止し、成功には数えていない。最終構成を上記の全3,362テストとrelease buildで確認した。Linux/macOSとクロスターゲットのローカル検証は行っていない。

## ドキュメント

AGENTS.md、英日READMEと開発ドキュメントへ所属索引と比較手順を追記した。言語一覧のRubyのタグを明示し、Cargo.tomlとずれていたPHP文法の版も0.25へ訂正した。

指定の macOS 用 `link_agent.md.sh` は存在せず、同等と確認できるスクリプトもないため未実行。`CLAUDE.md` は Git 上 mode 120000、Windows 上は AGENTS.md という内容の通常ファイルで、変更していない。
