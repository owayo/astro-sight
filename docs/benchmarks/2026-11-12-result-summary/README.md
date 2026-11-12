# 2026-11-12 省略サマリ集計の点検

## 環境と比較条件

- Windows 11 x64 (10.0.26300)、AMD Ryzen 7 5700X、8 core / 16 logical processor、Python 3.12.9。
- Rust 1.99.1、既定 feature、mimalloc 0.1.52。`cargo build --release --locked --example bench-result-summary` を両版で使った。release は opt-level 3、LTO、有効な codegen unit は 1。
- 旧実装は `34177814803d7c9f6f62005be0e07e137481ae37`。同一のベンチマーク例を追加してビルドした。新実装は `d738239` の `build_rollup`。測定に使った Cargo.lock は両版で同じで、後述の toml 更新は測定後に行った。
- ビルドと全テストが終わってから測定した。両実行ファイルを 1 回ウォームアップし、9 回ずつ順序を交互に変えた。入力はプロセス内で作り、ファイルキャッシュには依存しない。
- 時間は Rust 内で集計の繰り返しを測り、反復数で割った 1 集計あたりの値。パース、ファイル探索、JSON 描画、プロセス起動、入力作成は含めない。
- 全試行で種類・言語・パス別件数、残余件数、切り詰めの申告の全フィールドが一致した。

## 原因と変更

旧実装は各レコードでパス別件数とパスから言語への表をそれぞれ検索し、種類名・言語名を所有文字列にして件数表へ渡していた。パス別件数から言語別件数を求めれば、パスの索引は一つで済む。種類名・言語名は静的な文字列で集計し、出力するキーだけを所有文字列にする。

集計対象と順序の選択は変えていない。`BTreeMap` のパス順を件数の安定ソートに渡す方法、予算から載せるパス数を決める方法、拡張子不明のパスを言語別件数に入れない規則を維持した。言語判定も従来どおりパスごとに 1 回で、ファイルは開かない。

## 結果

予算は 3,000 tokens。パスの拡張子は rs / ts / php / unknown、種類は ref / def / None を混在させた。括弧内は最小〜最大。

| ケース | パス数 / レコード数 / 内部反復数 | 旧版 ms | 新版 ms | 時間の短縮率 |
|---|---:|---:|---:|---:|
| small | 4 / 100 / 10,000 | 0.00461 (0.00451〜0.00478) | 0.00217 (0.00213〜0.00242) | 52.8% |
| repeated | 16 / 100,000 / 30 | 6.564 (6.418〜6.646) | 2.680 (2.595〜2.756) | 59.2% |
| many_files | 10,000 / 100,000 / 10 | 24.658 (24.239〜25.420) | 11.698 (11.500〜11.956) | 52.6% |
| unique_files | 10,000 / 10,000 / 30 | 3.398 (3.298〜3.446) | 1.846 (1.797〜1.933) | 45.7% |
| empty | 0 / 0 / 10,000 | 0.0000241 | 0.0000390 | -61.4% |

非空入力はすべて試行のばらつきより大きく短縮した。空入力は約 24 ns から 39 ns へ増えた。実際の `build_summary` は省略件数が 0 なら集計前に戻るため、この空集計を呼ばない。空入力用の分岐を追加する効果は実際の呼び出し経路にはなく、採用しなかった。空集合の戻り値が同じことはテストと各試行で確認した。

Windows のプロセス全体の最大 peak working set は旧版 13,225,984 bytes、新版 13,271,040 bytes (+44 KiB、約 0.34%)。全ケース・入力・実行コードを含む値であり、ケース別のメモリや allocation 数ではない。索引を一つ削ったことによるメモリ削減量をこの値からは主張しない。macOS の `/usr/bin/time -l` はこの Windows 環境では使わず、既存ハーネスの GetProcessMemoryInfo を使用した。

これは集計だけの比較で、`refs` 全体が同じ割合で速くなることを示さない。生データは [samples.csv](samples.csv)、環境・実行ファイル SHA-256・中央値・範囲は [summary.json](summary.json)、一致した全フィールドは [results.json](results.json) に保存した。

## 再実行

旧版の作業場所にも同一の `examples/bench-result-summary.rs` を置き、旧・新の各版で次を実行して、生成物を別名で保存する。

```powershell
mise exec -- cargo build --release --locked --example bench-result-summary
```

```powershell
python tools/bench-result-summary.py --repeats 9 `
  --baseline C:/temp/rollup-before.exe --candidate C:/temp/rollup-after.exe `
  --output-directory C:/temp/astro-rollup-bench
```

## その他の点検

- 開始時の main は origin/main と一致し、未コミット変更はなかった。既存 open Issue は 0 件だった。
- `depup --install --include-pinned` は既知の Kotlin 0.3.8 / tree-sitter 0.27 の links 競合で Cargo 更新を取り消した。Kotlin は 0.3.5 の固定を維持した。当初は nextest 用の公開後 14 日条件を Cargo にも適用していたため、最終点検で Kotlin を除外して depup の通常条件で再確認した。
- 通常条件で更新対象になった toml を 1.1.8 から 1.2.0 へ更新し、ロック内の toml_writer も 1.1.3 から 1.2.0 へ更新した。OSV の検査で問題はなく、要求される Rust 1.85 は使用中の 1.99.1 で満たす。他の Cargo パッケージと usage-stats の依存は変更していない。既存ロック内の encoding_rs / gix-macros / jiff / jiff-core / jiff-static への警告は、今回の追加・更新ではない。
- nextest は公開後 14 日以上の 0.9.150 (2026-10-26 公開) を採用した。0.9.151 (2026-11-03 公開) は見送った。mise の 5 platform の lock と nextest の required version をそろえた。
- `symbols` で src 全体の構造・複雑度を確認し、サマリ集計の呼び出し元・呼び出し順を追った。Rust の export 収集も調べたが、言語のスコープ・名前空間をまとめて扱う責務があり、複雑度の数値だけを根拠に分割しなかった。
- サンドボックスのパス検証、入力読み込みの上限、設定の読み込み、session の処理を確認した。再現可能な追加の不具合は確認していない。
- ドキュメント更新用の `/Users/owa/GitHub/mac-shell/shell/link_agent.md.sh` と確認できる Windows の同等スクリプトは無く、実行していない。CLAUDE.md は Git では symlink、ローカルではリンク先のパスを持つ通常ファイルだった。置き換えていない。AGENTS.md の全体規約は変更不要と判断した。
- `review --git`、`impact --git`、`dead-code --git` で API 差分・未解決影響・死蔵シンボルは無かった。共変更候補は歴史上の関連として確認し、mise を参照する CI・Makefile、AGENTS.md、差分解析の文書の追加変更は不要と判断した。
- 点検中に追加された [Issue #354](https://github.com/owayo/astro-sight/issues/354) は、C/C++ 共用ヘッダーの条件付き `extern "C"` 包装による誤った構文エラーを Windows でも再現し、独立した [PR #355](https://github.com/owayo/astro-sight/pull/355) で修正した。対になる純粋な C++ 条件の包装だけを C パースの対象から外し、位置・参照・本来の構文エラーを保持する。C/C++ の symbols が本体のないプロトタイプを列挙しない既存仕様は維持し、言語別ドキュメントへ明記した。
- PR #355 の同一 SHA に対する CI は全 16 ジョブが成功した。macOS の 1 ジョブはテスト失敗なしでジョブの 20 分制限に達したため、そのジョブだけ再実行して成功を確認した。`a32eedc` として main に squash マージし、Issue #354 のクローズを確認した。最終取得時の open Issue は 0 件。

## 検証

- 変更前: fmt / clippy と、全 4,335 テストが成功 (nextest 0.9.149、1081.722 s)。
- 集計変更時: nextest 0.9.150 で集計・出力制限・CLI の refs と出力形式・宣言サンプル予算・session・MCP の重点テスト 145 件が成功 (8.552 s)。この時点では全体 4,336 件のうち残り 4,191 件を選択していない。
- 最終状態: Issue #354 の修正と toml 1.2.0 を含め、Windows で全 4,340 テストが成功 (nextest 0.9.150、769.273 s、skip 0 件)。点検全体でサマリの境界条件 1 件、C ヘッダーの単体 3 件と CLI 回帰 1 件を追加した。テスト全体の所要時間は実行中の負荷が異なり、性能比較には使わない。
- 最終状態の fmt、既定 feature と全 feature の clippy (all-targets、warnings as errors)、doctest が成功。doctest は 0 件。
- 新しい Python ハーネスは ruff check / format check が成功。
- 旧新版の release ビルドが成功。比較ハーネスは 5 ケース × (ウォームアップ 1 回 + 計測 9 回) × 2 版で結果が一致。
- 途中の追加テストの実行は、depup の一時的な Kotlin 更新中の解決競合、その後は実行中の Windows テストバイナリのリンク時のロックで失敗した。依存復元と変更前の実行完了後に再実行し、上記 145 件が成功した。
