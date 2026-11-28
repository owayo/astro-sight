# metrics の到達数集計

依存先が1つの強連結成分では、自分のサイズに依存先の到達数を加えると、その先のグラフを辿り直す必要がない。依存元の件数も辺を逆にして同じ方法で求める。分岐点では複数の経路が同じ成分に合流するため、訪問済みの印を使って1回だけ数える。

## 測定結果

時間は `graph::analyze` 1回のミリ秒。括弧内は最小～最大、メモリはプロセスのピーク working set の中央値（MiB）。[全468試行の記録](samples.csv)には各試行の出力チェックサムも収録した。

| 入力 | 変更前の中央値（範囲） | 変更後の中央値（範囲） | ピークメモリ 前 → 後 |
|---|---:|---:|---:|
| 一本道、5,000頂点 | 206.624（189.985–221.750） | 2.409（1.961–3.181） | 6.941 → 6.945 |
| 菱形500個を直列に接続、1,501頂点 | 15.789（10.942–21.133） | 4.881（4.036–9.007） | 4.980 → 5.109 |
| astro-sight の `src`、458頂点・1,769辺 | 0.442（0.295–0.533） | 0.383（0.277–0.561） | 4.559 → 4.562 |
| 有向環、5,000頂点 | 1.216（0.821–1.454） | 0.975（0.792–1.386） | 5.191 → 5.195 |
| 中心から葉に向かう星形、5,000頂点 | 2.461（1.658–2.946） | 2.493（1.707–3.058） | 6.332 → 6.336 |
| 孤立した5,000頂点 | 1.427（0.985–1.695） | 1.409（0.870–1.839） | 5.105 → 5.105 |

一本道は98.8%、菱形の連鎖は69.1%短縮した。実データと残りの対照入力では時間の範囲が重なった。ピークメモリの中央値の増加は最大0.13 MiBだった。

## 測定条件

- 基準は `1bff730`。両版とも Windows 11 x64、Ryzen 7 5700X、Rust 1.99.1、`rustc --edition 2024 -C opt-level=3 -C debuginfo=0`、標準アロケータを使用した。
- `astro-sight metrics --dir src --format json` で基準版の `components` の出力順を頂点 ID に変換し、`edges` の始点・終点を読み込んだ。合成入力の頂点 ID も 0 始まりで、菱形1個の相対辺は (0,1)、(0,2)、(1,3)、(2,3)。終点を次の菱形の始点にした。
- 各入力を同じ `BTreeSet<(usize, usize)>` に読み込み、`Instant` で `graph::analyze` の呼び出しだけを測った。ファイル探索、構文解析、出力の整形とプロセスの起動は計測区間の外にある。
- 入力ごとに旧版と新版を9組実行し、組ごとに順番を反転した。各プロセスの最初の1回でウォームアップし、一本道は続く1回、他の入力は続く5回を記録した。ピーク working set は Windows の `GetProcessMemoryInfo` でプロセス単位に取得した。同じマシンで動く他のプロセスの負荷には変動がある。
- 同じ入力に対して、全試行で旧版と新版のチェックサムが一致した。対象は全頂点の fan-in、fan-out、依存先数、依存元数、循環成分の全要素。単体テストでは、自己ループを含まない4頂点の有向グラフ4,096通りについて、到達数を独立した推移閉包と照合する。
- CLI の確認では、変更後の同じ `src` を v26.11.111 と点検版で解析した。`metrics --dir src --format json` の出力は、ファイル単位・ディレクトリ単位ともに旧新版でバイト単位で一致した。

## 一本道の再測定

比較する各リビジョンで、次のコードをリポジトリ外の `bench.rs` に保存する。`graph.rs` を直接読み込むので、CLI の起動とソースのパースは測定に入らない。

```rust
mod graph {
    include!(env!("METRICS_GRAPH_RS"));
}

fn main() {
    let count = 5_000;
    let edges = (1..count).map(|i| (i - 1, i)).collect();
    for _ in 0..2 {
        let start = std::time::Instant::now();
        let result = graph::analyze(count, std::hint::black_box(&edges));
        let elapsed = start.elapsed();
        assert_eq!(result.depends_on[0], count);
        assert_eq!(result.used_by[count - 1], count);
        std::hint::black_box(result);
        println!("{elapsed:?}");
    }
}
```

PowerShell ではリポジトリ直下から次のようにビルドする。`$benchDir` は上のファイルを保存したディレクトリ。各版を同じ Rust と Cargo の依存でビルドし、実行ごとに2行目の時間を採る。

```powershell
$env:METRICS_GRAPH_RS = (Resolve-Path src/engine/metrics/graph.rs).Path
rustup run 1.99.1 cargo build --locked -p anyhow --target-dir "$benchDir/target"
$deps = Join-Path $benchDir 'target/debug/deps'
$anyhow = Get-ChildItem $deps -Filter 'libanyhow-*.rlib' | Select-Object -First 1
rustup run 1.99.1 rustc --edition 2024 -C opt-level=3 -C debuginfo=0 -A dead_code -L "dependency=$deps" --extern "anyhow=$($anyhow.FullName)" "$benchDir/bench.rs" -o "$benchDir/bench.exe"
& "$benchDir/bench.exe"
```
