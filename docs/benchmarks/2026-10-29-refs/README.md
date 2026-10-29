# 2026-10-29 定期点検と Rust refs の性能比較

対象は astro-sight。`origin/main` の `68512fba6cbe0083d27d4f5c9a68a49c699047ff` から開始し、開始時の未コミット変更と未送信コミットはなかった。今回作成した stash はない。fetch と pull --rebase は成功し、同期による更新はなかった。

## 依存更新

`mise install` 後に `mise exec -- depup --install --include-pinned` を実行した。一括更新は tree-sitter-kotlin 0.3.8 の runtime 制約 `>=0.21,<0.23` と tree-sitter 0.27.1 が競合し、depup がマニフェストを復元した。

Kotlin と独自パッチを使う Bash/Rust/Swift/TypeScript の5文法を `--exclude` で除外して再実行し、以下を更新した。Rust 1.99.1 と usage-stats の依存は変更不要だった。biome.jsonc はない。

| 直接依存 | 変更前 | 変更後 |
|---|---|---|
| lru | 0.18.5 | 0.19.0 |
| memmap2 | 0.9.11 | 0.9.12 |
| rmcp | 3.5.1 | 3.6.0 |
| tokio | 1.53.3 | 1.54.1 |

0.x の互換性変更として、[lru の公式 changelog](https://github.com/jeromefroe/lru-rs/blob/0.19.0/CHANGELOG.md) の共有 iterator の Send/Sync 制約を確認した。LruCache の利用箇所を refs で調べ、追従修正は不要だった。rmcp は[公式リリースノート](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.6.0)も確認した。ロックファイルには cc、find-msvc-tools、crc32fast、rmcp-macros などの推移依存更新も含む。

## 構造と正確性の点検

`symbols --dir src` で全体を調べ、`refs`・`calls`・`sequence` で参照検索の流れを確認した。主な候補は Python の所有元解析 `parse` (複雑度55)、宣言変更の補正 `reconcile_declaration_changes` (42)、CLI の `dispatch_command` (35)、Rust 参照ウォーカーだった。

Python の解析と宣言補正は、未対応構文・曖昧な束縛・旧ソース復元失敗を保留する条件が複雑さの主因で、既に証拠別の補助関数へ分かれている。今回は検証境界を変えずに整理する効果が明確な参照走査を優先した。CLI の大きな分岐はサブコマンドの列挙であり、外側に抽象化を追加する根拠はなかった。

参照ウォーカーに重複していた TreeCursor の前順走査を `tree_walk::walk_preorder` に統合した。Rust だけ祖先列を保持し、cfg/cfg_attr 判定が `Node::parent` を繰り返さず列を読むようにした。保持量は木の深さに比例し、部分木の外の祖先は一度だけ補う。条件の後ろの derive・通常マクロ引数・参照位置と順序・全4経路の契約は変えない。

性能不具合は公開の合成入力で再現し、[Issue #176](https://github.com/owayo/astro-sight/issues/176) に記録した。Git の revision 検証・ワークスペース境界・ファイルサイズ上限と読み込み時の再検査も確認した。追加で断言できる不具合は見つからなかった。

## 性能測定

- Windows 11 Home 10.0.26300、Ryzen 7 5700X (8コア/16スレッド)、32 GiB RAM、Python 3.12.9。
- 旧新とも `mise exec -- cargo build --release --locked`、Rust 1.99.1、release の LTO/単一 codegen unit/strip を使用。旧版は開始 revision の依存、新版は上記依存更新と最適化を含む。依存更新から完全に独立した比較ではない。
- 各版1回のウォームアップ後に9回ずつ、順番を試行ごとに反転。OS のファイルキャッシュは温まった状態。同じ端末で他のビルド・検証も実行されており、短い測定にはスケジューラ由来のばらつきがある。
- CLI は起動を含む。session はケースごとに旧新の常駐プロセスを作り、起動をウォームアップへ含めて要求の送信から応答までを測る。多言語ケースは main/greet/add の3要求を合計する。
- 全試行の stdout が旧版の出力とバイト一致。時間制限を通常の単体テストへ追加していない。
- working set は Windows の GetProcessMemoryInfo から取得。session のピークには同じケースのウォームアップと先行要求も含む。割当回数や private bytes ではない。

```powershell
python tools/bench-refs.py --baseline C:/temp/before.exe --candidate C:/temp/after.exe --output-directory C:/temp/refs-session --mode session --repeats 9
python tools/bench-refs.py --baseline C:/temp/before.exe --candidate C:/temp/after.exe --output-directory C:/temp/refs-cli --mode cli --repeats 9
```

生成入力と応答は指定した出力先に保存される。解析対象へ混入させないため、その出力先はリポジトリ外にする。保存した生データは [session-samples.csv](session-samples.csv) と [cli-samples.csv](cli-samples.csv)、環境・実行ファイル SHA-256・要約は [session-summary.json](session-summary.json) と [cli-summary.json](cli-summary.json)。

### session の測定値

| ケース | 変更前 ms 中央値 [最小–最大] | 変更後 ms 中央値 [最小–最大] | 短縮率 | 最大 working set MiB 前→後 |
|---|---:|---:|---:|---:|
| deep-500 | 17.70 [15.49–116.96] | 20.88 [12.24–105.26] | -17.9% | 13.75→13.46 |
| deep-1000 | 30.51 [23.48–79.05] | 18.66 [12.57–37.60] | 38.8% | 13.68→13.77 |
| deep-2000 | 72.40 [60.46–127.74] | 22.40 [14.84–62.22] | 69.1% | 14.00→14.24 |
| many-250 | 76.70 [72.49–85.17] | 21.26 [11.60–47.31] | 72.3% | 14.35→14.41 |
| many-500 | 505.94 [480.62–576.90] | 24.14 [20.80–59.37] | 95.2% | 14.50→14.27 |
| leaf-control | 17.11 [15.44–27.20] | 20.01 [15.25–30.14] | -16.9% | 13.94→14.20 |
| absent-control | 12.28 [10.52–18.35] | 13.43 [10.69–22.00] | -9.4% | 12.77→12.72 |
| ordinary | 39.96 [35.17–173.65] | 39.89 [32.15–56.64] | 0.2% | 16.46→16.57 |
| mixed-languages | 155.19 [126.10–205.69] | 147.80 [112.07–207.56] | 4.8% | 22.77→22.32 |

同名セグメント500個は前後の測定範囲が重ならず、中央値で95.2%短縮した。深さ2,000の名前は69.1%短縮した。深い名前と末尾の名前の差が小さくなったことも、祖先探索が寄与していた根拠になる。短い対照・通常関数・多言語の測定範囲は前後で重なり、改善・退行を断定しない。ピーク working set に大幅な増加はなかった。

起動込みの CLI でも同名セグメント500個は999.79→363.97 ms (63.6%短縮) だった。ただし他の短いケースは起動・同時負荷が支配し、中央値が悪化したケースもあるため、全ケースの高速化や起動時間の改善は主張しない。Rust の closure 束縛・参照役割などに残る親走査も対象外であり、全 Rust 解析の計算量を線形にした変更ではない。

## 検証とドキュメント

- 変更前: `mise exec -- cargo test --locked` で単体2,358件・統合928件成功 (失敗・ignored とも0)。
- 変更後: `mise exec -- cargo test --locked --lib -- --test-threads 8` で単体2,360件成功。新規2件は旧判定との全ノード比較と全部分木比較。
- 変更後: `mise exec -- cargo test --release --locked --test integration` で配布構成の統合928件成功 (失敗・ignored とも0)。debug の bins と doctest は各0件で正常終了。
- `cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings`、同 `--all-features`、`cargo check --locked`、debug/release の `cargo build --locked` が成功 (すべて mise 経由)。
- usage-stats は独立した manifest に対する fmt/clippy とテスト19件が成功。
- 測定スクリプトは Ruff の format/check と CLI/session の全測定を実行し、全出力一致を確認。
- 最終 `context`・`impact`・`review --git`・`dead-code --git` を実行。impact は exit 0、死蔵・未検証シンボル・未対応ソースによる truncation は0、既存 API の削除・変更は0だった。API 追加は測定補助の measured だけ。共変更候補10件のうち検証対象3件は、出力形式・diff 分類の規則が不変であることと、テストモジュールの登録先が既存の refs/tests/mod.rs であることを確認し、不要な変更は加えなかった。

変更前の統合テストと変更後の debug ビルドを並行させた最初の試行では、使用中の target/debug/astro-sight.exe の削除を Windows が拒否した (os error 5)。その試行を成功扱いせず、単体を debug・統合を release へ分け、変更前の終了後に debug ビルドと bins/doctest も再実行して成功した。Linux/macOS とクロスターゲットのローカル検証は未実施。

`AGENTS.md`、英日 README と開発ドキュメントに実装境界・測定方法を追記した。指定の `/Users/owa/GitHub/mac-shell/shell/link_agent.md.sh` は Windows 上に存在せず、同等と確認できるスクリプトもなかったため未実行。`CLAUDE.md` は Git 上は mode 120000、Windows 上では内容が AGENTS.md のリンク用通常ファイルだったため変更していない。
