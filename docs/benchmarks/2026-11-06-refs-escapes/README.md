# 2026-11-06 定期点検と参照検索の事前フィルタの性能比較

開始 revision は `2c09b2f`。fetch と pull --rebase で `9deda93` (Release v26.11.101) まで進めた。未コミット・未送信の変更はなく、stash は作っていない。開始時の open Issue と open PR は0件だった。Windows の作業ツリーで index と異なる CRLF になっていた212ファイルを LF に戻した (内容は HEAD と同じ)。変更前の基準として fmt・既定と全 feature の clippy・nextest 3,556件・doctest がすべて成功した。

## 依存更新

`depup --install --include-pinned` で globset・ignore・smallvec (patch)、rmcp 3.8.0、tree-sitter-ruby v26.10.107、usage-stats の simd-json 0.19.0 へ上げた。tree-sitter-kotlin は tree-sitter 0.27 との links 競合で =0.3.5 のまま。cargo-nextest は公開から14日以上の 0.9.149 にした (0.9.150 以降は14日未満)。

tree-sitter-swift は 0.7.4 の公開クレートから vendor を作り直した。上流が独自修正4件のうち2件 (#32 の複数行の nil 合体、#201 の Windows の `try!`) を取り込んだので、残る修正は空タプル (#41) と、try の await の代替の動的優先度 -1 (#200) の2件になった。上流のままでは `try await load().count` の try が `.count` を覆わなくなることを既存の回帰テストで検出し、-1 を残した。詳細は [PATCHES.md](../../../vendor/tree-sitter-swift/PATCHES.md)。

## 構造・正確性の点検

`symbols --dir src` (非テストの関数 4,965件) を複雑度と行数で並べ、直近1か月の変更頻度と突き合わせた。最も変更の多い `commands/api_changes.rs` の `detect_api_changes_with_renames` (435行) から、Phase 0 の前処理を `PreparedDiff` へ分け、削除と内容同一の rename の準備の重複をまとめた。MCP サーバーの `#[allow(dead_code)]` が隠していた読まれないフィールド (ToolRouter の二重構築) を取り除いた。CLI の分岐 `dispatch_command`、影響分析の `collect_affected_symbols` (536行) は、言語別の特例と呼び出し側の共有状態を伴うため、検証の費用に見合う分割が見つからず今回は見送った。

`unsafe` (起動時の環境変数、mmap、Kotlin の FFI)、git へ渡す引数 (`--` の区切り、`GIT_LITERAL_PATHSPECS`、`--base` の検証)、session / MCP のワークスペース境界を確認し、問題は見つからなかった。

## 参照検索の事前フィルタ ([#248](https://github.com/owayo/astro-sight/issues/248))

Stop hook と同じ `impact --dir . --git` を、このリポジトリで `2c09b2f` の差分を未コミットにした作業ツリーで測ると、Pass2 の約0.9秒の多くは、検索名を含まないのに全件解析されるファイルだった。事前フィルタは、エスケープの印 (`\x` / `\u{`) と書式マクロの名前を両方含むファイルを、名前に関係なく全部の名前の出現ありとしていた (エスケープで綴った暗黙キャプチャを落とさないため)。走査対象の Rust ファイル378件のうち43件 (1.9 MB) が該当し、41件は検索名を含まなかった。

同時に、行継続 (`\` と改行) で分けたキャプチャを落とす不具合 (#248) が見つかった。印に行継続を足すだけでは該当が88件 (4.8 MB) に増える。そこで、印が揃うファイルでは、エスケープを左から復号した写しでも名前を探し、どちらかで見つかった名前だけを出現ありとするようにした。

### 計測の条件

Windows 11 Home 10.0.26300、AMD Ryzen 7 5700X (8コア/16スレッド)、32 GiB RAM、Python 3.12.9、Rust 1.99.1 (LLVM 23.1.1、x86_64-pc-windows-msvc)。変更前 (`9deda93`) と変更後 (`9deda93` に #248 の変更) をそれぞれ `cargo build --release --locked` で作った (LTO・単一 codegen unit)。実行ファイルの SHA-256 は変更前 `72fe74bc…`、変更後 `cf87e604…` (全桁は summary.json)。各版を1回ウォームアップしてから交互に実行し、順番を試行ごとに反転した (ABBA)。ファイルキャッシュは温まった状態で、全試行で旧新の stdout・stderr・終了コードが一致した。working set は GetProcessMemoryInfo によるピークで、割当回数ではない。

### 生成入力

検索名を含まず `format!` と `\x41\u{42}` を持つ Rust ファイル240件 (各40関数) と、検索名を使う5件の `escapes`、同じ構成で印だけを外した `plain` を比べる。

```powershell
python tools/bench-refs.py --suite escapes --mode cli --repeats 9 `
  --baseline C:/temp/astro-before.exe --candidate C:/temp/astro-after.exe `
  --output-directory C:/temp/astro-escapes-cli
python tools/bench-refs.py --suite escapes --mode session --repeats 9 `
  --baseline C:/temp/astro-before.exe --candidate C:/temp/astro-after.exe `
  --output-directory C:/temp/astro-escapes-session
```

| ケース (CLI) | 変更前 ms 中央値 [最小–最大] | 変更後 ms 中央値 [最小–最大] | 短縮率 | 最大 working set MiB 前→後 |
|---|---:|---:|---:|---:|
| escapes-refs | 112.90 [102.22–124.22] | 54.44 [47.67–57.79] | 51.8% | 21.76→16.84 |
| escapes-refs-batch | 103.31 [100.30–114.90] | 51.25 [46.75–64.58] | 50.4% | 22.06→17.13 |
| plain-refs | 50.50 [45.68–65.27] | 49.93 [47.32–262.10] | 1.1% | 16.80→17.08 |
| plain-refs-batch | 49.98 [45.13–58.77] | 50.20 [45.83–275.76] | -0.4% | 16.93→17.10 |

| ケース (session) | 変更前 ms 中央値 [最小–最大] | 変更後 ms 中央値 [最小–最大] | 短縮率 | 最大 working set MiB 前→後 |
|---|---:|---:|---:|---:|
| escapes-refs | 75.16 [71.25–76.04] | 19.71 [18.34–22.33] | 73.8% | 23.42→17.36 |
| escapes-refs-batch | 151.02 [143.97–178.84] | 41.34 [39.53–45.07] | 72.6% | 24.21→18.18 |
| plain-refs | 20.47 [18.44–22.95] | 19.06 [18.87–21.70] | 6.9% | 17.45→17.24 |
| plain-refs-batch | 38.63 [36.11–44.34] | 37.76 [37.11–42.54] | 2.2% | 18.08→18.03 |

変更後は `escapes` が `plain` とほぼ同じ時間になる。`plain` の最大値の外れ (262 / 275 ms) は1回ずつで、中央値と他の試行は変わらない。生データは [cli-samples.csv](cli-samples.csv) と [session-samples.csv](session-samples.csv)、要約は [cli-summary.json](cli-summary.json) と [session-summary.json](session-summary.json)。

### 実リポジトリ (CLI)

`refs` と `dead-code` はこのリポジトリの `9deda93`、`impact` は `2c09b2f^` に `2c09b2f` の差分を未コミットで当てた作業ツリー (Rust 10ファイル、+479/-14行)、rails は `5dc287b` (Ruby 3,481ファイル)。

| ケース | 回数 | 変更前 ms 中央値 [最小–最大] | 変更後 ms 中央値 [最小–最大] | 短縮率 | 最大 working set MiB 前→後 |
|---|---:|---:|---:|---:|---:|
| `refs --name ApiSymbolChange` | 15 | 225.0 [209.1–444.6] | 168.6 [152.7–279.0] | 25.0% | 97.0→95.7 |
| `refs --names ApiSymbolChange,PreparedDiffFile,log_phase` | 15 | 294.1 [265.1–347.6] | 258.6 [251.1–327.4] | 12.1% | 133.3→92.5 |
| `impact --dir . --git` | 9 | 1196.5 [1160.3–1259.9] | 825.5 [809.5–1099.7] | 31.0% | 29.4→28.6 |
| rails `refs --name deep_transform_keys` | 9 | 677.2 [659.1–687.4] | 671.3 [655.1–701.1] | 0.9% | 29.4→29.3 |
| `dead-code --dir .` | 5 | 5161.0 [5099.5–5207.2] | 5186.2 [5119.3–5398.2] | -0.5% | 1499.6→1498.3 |

rails は印を持つファイルが少なく、`dead-code` は数千の名前をまとめて数えるためほぼ全ファイルが生のソースで一致し、どちらも差は変動の範囲内だった。生データは [repo-samples.csv](repo-samples.csv)。`dead-code` のピーク working set (約1.5 GiB) は変更前後で同じで、要因は次の節に記す。

## dead-code のピーク working set

上の変更後の実行ファイルで、このリポジトリの `4a81a9d` の作業ツリーと rails を各1〜3回測った。

| コマンド | 時間 (中央値) | ピーク working set |
|---|---:|---:|
| `dead-code --dir .` (rayon 既定の16スレッド) | 6.07 s | 1466.5 MiB |
| `dead-code --dir .` (`RAYON_NUM_THREADS=4`) | 7.46 s | 1327.0 MiB |
| `dead-code --dir .` (`RAYON_NUM_THREADS=1`) | 17.36 s | 644.9 MiB |
| `dead-code --dir . --exclude-dir vendor` | 6.07 s | 1467.2 MiB |
| `symbols --dir src --no-cache` | 0.55 s | 33.1 MiB |
| `symbols --path vendor/tree-sitter-swift/src/parser.c --no-cache` | 5.17 s | 586.1 MiB |
| `symbols --path vendor/tree-sitter-kotlin/src/parser.c --no-cache` | 5.12 s | 653.6 MiB |
| rails `dead-code --dir .` (Ruby 3,481ファイル、50 MB) | 3.05 s | 103.4 MiB |

`dhat-heap` feature のビルドでも同じ約1.5 GiB になり、dhat が数える Rust のヒープのピークは約1.2 MB だった。大半は tree-sitter (C) の構文木で、vendor の生成物 `parser.c` (6〜24 MB、`.gitattributes` で `linguist-generated`) の構文解析が時間とメモリを占める。refs の生成物の判定はファイル名と先頭 4 KB の印 (`@generated` など) だけを見て `.gitattributes` を見ないため、`refs` / `symbols --dir` / dead-code の参照の集計はこれらを解析する。dead-code は `--exclude-dir vendor` でも参照の集計で vendor を読む。

対象から外すと、出現をそのまま返す `refs` と、生成物の参照も数える dead-code の出力の契約が変わるため、今回は変えない。生成物の判定をコマンド間でそろえるか、巨大なファイルの同時解析を抑えるかは、別に設計を決める課題として残す。巨大なファイルの同時解析は [#253](https://github.com/owayo/astro-sight/issues/253) で予算を入れた ([記録](../2026-11-06-dead-code-parse-budget/README.md))。

## 未解消の候補

いずれも計測用のログを足した非 LTO の release ビルドで、全テストの実行中に取った予備計測で、上の比較とは条件が違う。

- `impact --git` の Pass1 (変更10ファイルで約0.6秒) の記録がある内訳は、シグネチャ検出 143 ms・構文解析 88 ms・呼び出しの抽出 88 ms・シンボル抽出 60 ms で、単独で大きいボトルネックは見つからなかった
- Pass2 の並列度 (`ASTRO_SIGHT_IMPACT_WORKERS`、既定1) を4・8に上げても明確な短縮は見られず、ピーク working set は 29→41 MiB に増えた (各5回)
- テストの debug ビルドで依存だけを `opt-level=3` にしても、結合テスト8件の合計 (A: 77.5 / 70.5 s、B: 69.6 / 94.6 s) に一貫した差は無かった。全テスト (約55〜65分) の遅さは、並列に起動する git と astro-sight の子プロセスの競合が主で、同じテストは単独なら4〜15秒で終わる
