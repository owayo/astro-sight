# 2026-11-06 dead-code の大きいファイルの構文解析の予算 ([#253](https://github.com/owayo/astro-sight/issues/253))

dead-code の候補の抽出と参照の集計は、全ファイルを rayon で並列に構文解析する。構文木はソースの約 20〜30 倍 (数値の表のような密なソースでは 100 倍超) のメモリを使うので、大きいファイルがワーカーの数まで同時に解析され、ピークメモリが大きいファイルの数とスレッド数に比例して上限なく増えていた。#253 では、64 KiB 以上のファイルを同時に解析する大きさの合計を予算 (既定 32 MiB、`ASTRO_SIGHT_PARSE_BUDGET_MIB`) に収める。結果 (dead・test-only・保留・申告) は予算に依らない。

## 計測の条件

Windows 11 Home 10.0.26300、AMD Ryzen 7 5700X (8コア/16スレッド)、32 GiB RAM、Python 3.12.9、Rust 1.99.1 (LLVM 23.1.1、x86_64-pc-windows-msvc)。変更前は `d058134` (#252 のマージ後)、変更後は `d058134` に #253 の変更を入れたもので、どちらも `cargo build --release --locked` (LTO・単一 codegen unit) で作った。実行ファイルの SHA-256 は変更前 `948cc335f28ea44aaabc58845d0bc2f5b2b0f255aba9169927c0c8358f43da89`、変更後 `42fd6ce61b5e458329dc99c5dd3512a8b366d28a160703abbaa6efb2d0aaa7d7`。

[bench_variants.py](bench_variants.py) で、1回のウォームアップの後に条件を交互に実行し、順番を試行ごとに反転した (ABBA)。全試行で条件間の stdout・stderr・終了コードが一致した。working set は GetProcessMemoryInfo によるピーク。ファイルキャッシュは温まった状態。

計測の間、同じ機械で別の作業 (ほかのリポジトリのビルドとテスト) の負荷があった (CPU 使用率 40〜96%)。時間は負荷の影響を受け、直列に解析する部分が長い条件ほど伸びる。ピーク working set は負荷の影響を受けにくい。構文解析のタイムアウト (既定 30 秒) は全条件で外した (`ASTRO_SIGHT_PARSE_TIMEOUT_SEC=0`)。高い負荷の中で巨大なファイルを同時に解析すると 30 秒を超え、予算を無制限にした条件で出力が1回揺れたため。

## 1 ファイルの構文解析

`symbols --path <file> --no-cache` を各2回 ([parse-one-file.csv](parse-one-file.csv))。このリポジトリの vendor の生成された `parser.c` で、木はソース 1 MB あたり約 25〜30 MiB。

| ファイル | 大きさ | 時間 (中央値) | ピーク working set |
|---|---:|---:|---:|
| vendor/tree-sitter-kotlin/src/parser.c | 23.8 MB | 4.75 s | 653.7 MiB |
| vendor/tree-sitter-swift/src/parser.c | 23.2 MB | 4.56 s | 586.1 MiB |
| vendor/tree-sitter-bash/src/parser.c | 9.9 MB | 2.03 s | 256.9 MiB |
| vendor/tree-sitter-typescript/tsx/src/parser.c | 8.8 MB | 1.78 s | 250.3 MiB |
| vendor/tree-sitter-typescript/typescript/src/parser.c | 8.8 MB | 1.71 s | 249.9 MiB |
| vendor/tree-sitter-rust/src/parser.c | 6.5 MB | 1.36 s | 194.1 MiB |

数値の配列が並ぶ 1 MiB 弱の TS ファイル 1本 (下の合成コーパスの `below-t`) では 127.7 MiB (ソースの 100 倍超) になった ([parse-dense-ts.csv](parse-dense-ts.csv)、1回)。当初は予算の対象を 1 MiB 以上にしていたが、それ未満のファイルは予算の外でワーカーの数まで同時に解析されるので、`read_file` が mmap に切り替える 64 KiB に下げた (64 KiB 以上のファイルは通常のリポジトリでは少なく、既定の予算なら約500本が同時に入る)。

## このリポジトリの dead-code --dir .

5回 ([r4-astro.csv](r4-astro.csv))。参照の集計が vendor の `parser.c` 6本 (6.5〜23.8 MB) を解析する (生成物なので候補にはならない)。

| 条件 | 時間 中央値 [最小–最大] | ピーク working set 中央値 (最大) |
|---|---:|---:|
| 変更前 | 6.335 s [5.912–6.465] | 1429.2 MiB (1460.4) |
| 予算 32 MiB (既定) | 11.158 s [9.533–12.048] | 678.3 MiB (892.6) |
| 予算 48 MiB | 8.947 s [8.736–9.916] | 941.3 MiB (962.7) |
| 予算 無制限 (`100000`) | 6.808 s [6.404–7.000] | 1448.9 MiB (1462.1) |

既定の予算ではピークが最大のファイル1本の解析と同じ程度まで下がる (最大の 892.6 MiB は、Kotlin の `parser.c` と 8.8 MB の `parser.c` が同時に入った試行)。時間は約1.8倍になる。Kotlin と Swift の `parser.c` (合わせて 47 MB) が同時に入らないので、この2本を直列に解析する 9.3 秒が下限になる。48 MiB にするとこの2本が同時に入り、時間は約1.4倍、ピークは約 940 MiB (2本が重なる試行では 1.2 GiB 程度) になる。速度だけを理由に既定を上げず、速さを優先する環境は `ASTRO_SIGHT_PARSE_BUDGET_MIB` で上げる。

## 大きいファイルの少ないリポジトリ

5回 ([r4-rails.csv](r4-rails.csv)、[r4-ruby.csv](r4-ruby.csv))。rails は Ruby 3,481ファイル、ruby は C と Ruby で 64 KiB 以上のファイルを多く含み、1 MiB を超える `.rb` が3本ある。

| リポジトリ | 変更前 | 予算 32 MiB (既定) |
|---|---:|---:|
| rails `dead-code --dir .` | 3.672 s [3.421–3.984]、98.7 MiB | 3.416 s [3.363–3.693]、101.2 MiB |
| ruby `dead-code --dir .` | 8.478 s [7.922–8.721]、358.0 MiB | 8.124 s [8.046–8.424]、380.3 MiB |

どちらも差は変動の範囲内だった。

## 生成物でない大きいファイル

[make_corpora.py](make_corpora.py) の合成コーパスで各1回 ([r3-syn-many-large.csv](r3-syn-many-large.csv)、[r3-syn-ac-miss.csv](r3-syn-ac-miss.csv))。別のテスト一式が同時に動いていた非常に高い負荷の中での値で、時間は大きく伸びている。

| コーパス | 条件 | 時間 | ピーク working set |
|---|---|---:|---:|
| many-large (数値の表の C 4 MiB × 24本、候補で参照もある) | 変更前 | 100.5 s | 5837.8 MiB |
| | 予算 0 (1本ずつ) | 229.8 s | 393.2 MiB |
| | 予算 32 MiB (既定) | 114.8 s | 2634.1 MiB |
| ac-miss (数値の表の C 20 MiB × 8本、候補の名前を含まない) | 変更前 | 43.2 s | 14448.0 MiB |
| | 予算 0 | 142.4 s | 1850.0 MiB |
| | 予算 32 MiB (既定) | 177.0 s | 1849.5 MiB |

これらのファイルは生成物ではないので候補になり、候補の抽出がピークを占めていた。当初は参照の集計だけに予算を掛けていたが、その版では予算 0 でも many-large が 5570 MiB、ac-miss が 14311 MiB のままだったので、候補の抽出にも同じ予算を掛けた。ac-miss では 20 MiB のファイルが予算に2本入らないので直列になり、時間は約4倍になる (メモリは約8分の1)。予算はソースの大きさで数えるので、密なソースでは同じ予算でも木は大きくなる (many-large は既定の予算に 4 MiB のファイルが8本入り、2.6 GiB)。

## 見送った案

- 大きいファイルの同時の本数の上限: バイトの予算で木のメモリは抑えられ、本数の上限は中くらいのファイルが多いときに並列度を下げるだけになる
- 予算を待つワーカーを Condvar で止める、大きいファイル専用のスレッドプール、`rayon::yield_now` で待つ: それぞれワーカーが並んで止まる、スレッドが既存の設定を超える、入れ子の待ちでスタックが伸び予算を持ったまま自分を待ちうる
- 生成物の参照を構文木を作らずに数える: 文字列・コメントの中の名前も数えて dead の検出を弱め、`--include-generated` では候補になる生成物の前提も崩れる
- dead-code のほかの解析 (member liveness など)、refs の一括検索、impact の呼び出し元の検索への適用: それぞれ言語と名前で絞ったファイルだけを解析し、今回の計測ではピークを占めなかった。必要になれば `engine/parse_budget.rs` の部品を使う
