# 開発

README の [開発](../README.ja.md#開発) の補足。`make setup` と `make ci` の流れはそちらにある。

## 前提

[mise](https://mise.jdx.dev/) を前提にしている。Rust とテストランナー [cargo-nextest](https://nexte.st/) の版は `mise.toml` で固定し、Makefile の各ターゲットが `mise exec --` 経由でその版の `cargo` を呼ぶ。シェルで mise を有効にしていなくても版はそろう。配布物の URL と checksum は `mise.lock` に固定している。版を変えたら `mise lock --platform linux-x64,linux-arm64,macos-x64,macos-arm64,windows-x64` で作り直す（CI は `--locked` でツールを入れるので、lock が古いとそこで止まる）。mise のほかに C コンパイラ（tree-sitter の各パーサをビルドする。macOS なら Xcode Command Line Tools）と git（結合テストが一時リポジトリを作って `git` コマンドを呼ぶ）が要る。

mise を使わずに PATH 上の `cargo` で動かすなら `SYSTEM_TOOLS=1` を付ける。版が `mise.toml` とずれることがあり、`make test` には PATH 上の `cargo-nextest` も要る。

```bash
make ci SYSTEM_TOOLS=1
make install SYSTEM_TOOLS=1
```

## CI と同じ検査

Linux と macOS の CI は `make setup` と、`make ci` を構成するターゲット（`make check` と `make test`）を呼ぶだけなので、手元の `make ci` がそのまま CI の検査になる。テストは配布物と同じ既定の feature で回す。clippy は既定の feature と `--all-features` の両方で、ヒーププロファイラ（`dhat-heap` feature）側のコードも検査する。

`make test` は単体テストと結合テストを cargo-nextest で 1 件ずつ別のプロセスとして走らせ（設定は `.config/nextest.toml`）、nextest が扱わない doctest を `cargo test --doc` で回す。一部のテストだけを走らせるときは `mise exec -- cargo nextest run <filter>` のように mise 経由で呼ぶ。

lint、テスト、6 ターゲットの配布用ビルドは独立して開始する。CI はテストを nextest の `--partition slice:<m>/<n>` で複数のランナーへ分ける。Linux と macOS は各 2 つ（`make test TEST_PARTITION=slice:<m>/2`）、Windows は 5 つで、Windows は `cargo nextest run` を直接呼ぶ。doctest は各 OS の 1 番目のランナーだけが回し、残りは `TEST_DOC=0` を渡す。macOS の 1 番目は `make ci` を呼び、macOS のランナーを増やさずに macOS の lint も兼ねる。ランナーの数は、どのランナーも配布用ビルド（約 7 分）と同じころに終わることを目安に決めている。macOS のランナーは速さのばらつきが大きく、lint も回す 1 番目は配布用ビルドより長くなることがある。

テストの並列数は既定（CPU 数）のまま。テストの時間の大半は `git` と `astro-sight` のプロセスの起動で、CPU 数より増やすと 4 コアの Linux と Windows のランナーではかえって遅くなった。CPU 律速のストレステスト（`deep_nesting::rust_impact_proofs_handle_deep_nesting`。各コマンドに 300 秒の期限がある）だけは最初に始め、2 件分の枠を使う。Windows では一時ファイルを作業ツリーと同じドライブにある `runner.temp` に置く。既定の `TEMP`（C:）では同じテストが 1.6 倍かかった。テストのステップでは git の本体（`git --exec-path` が示すディレクトリの `git.exe`）も PATH の先頭に置く。PATH で見つかる `git.exe` は本体を別のプロセスとして起動するラッパーで、ラッパーを通さないと同じランナーでテストが 25% 前後縮んだ。

ビルドのキャッシュを保存するのは main への push だけで、PR は復元するだけにする。PR ごとに保存すると上限の 10GB を使い切り、main のキャッシュが追い出される。キャッシュの鍵はジョブの種類ごとに分け（`lint`、`test`、`build-<target>`）、テストのランナーは OS ごとに 1 つの鍵を共有して 1 番目だけが保存する。

Cargo のコマンドには既定で `--locked` を付けている。`.cargo/config.toml` の `[patch]` でローカルの tree-sitter 系を差し込むと `Cargo.lock` が手元でだけ変わり、`--locked` で止まる。そのときは `make ci CARGO_FLAGS=` のように `CARGO_FLAGS` を空にする。

`make install` はバイナリを入れた後に、Claude Code と Codex のスキルも書き込む。書き込む先は `SKILL_TARGETS` で選べる（既定は `claude codex`）。

```bash
make install SKILL_TARGETS=          # スキルを入れない
make install SKILL_TARGETS=claude    # Claude Code のスキルだけ入れる
```

`make uninstall` はバイナリだけを消し、書き込んだスキルは残す。

インストール後は `command -v astro-sight` と `astro-sight --version` で実行先と版を確認する。mise の shim が選ばれる場合は `mise which astro-sight` で実体が分かる。既定の `/usr/local/bin` より別のコピーが優先されていたら、実体のあるディレクトリを `INSTALL_PATH` に指定する。たとえば `make install INSTALL_PATH="$HOME/.cargo/bin"` とする。shim 自体は置き換えない。

Bash・Swift・TypeScript の文法には独自修正がある。依存更新でその修正が外れないよう、依存の版を vendor 内のパッケージの版と完全一致させている。文法と回帰テストは一緒に更新する。Kotlin は新しい版が互換性のない tree-sitter を要求するため、0.3.5 を維持する。

## ヒーププロファイル

`dhat-heap` feature を付けて実行すると、ヒープの内訳を作業ディレクトリの `dhat-heap.json` に書き出す。巨大なリポジトリでメモリの内訳を測るときに使う。

```bash
mise exec -- cargo run --release --locked --features dhat-heap -- symbols --dir .
```

## 利用状況の集計ツール

`tools/usage-stats`（[利用状況の分析](integrations.ja.md#利用状況の分析)）はルートとは別の Cargo プロジェクトで、`make ci` の対象に入らない。コマンドとして入れるなら `make -C tools/usage-stats install` を実行する。

## リリース

GitHub Actions の **Actions > Release > Run workflow** から実行する。1 回の実行で次の順に進む。

1. `Cargo.toml` と `Cargo.lock` の版を書き換えてコミットし、`v<版>` のタグを付けて push する
2. 6 つのターゲット（Linux の x86_64 / x86_64 musl / ARM64、macOS の Intel / Apple Silicon、Windows の x86_64）をビルドし、添付と `SHA256SUMS` を載せた GitHub Release を作る
3. Homebrew tap（`owayo/homebrew-astro-sight`）の formula を、この版の添付を直接指す形に書き直し、winget-pkgs に更新のマニフェストを提出する

版の形式は `yy.m.counter`（例: `26.9.103`）。年月は日本時間で決め、counter は年月が変わると 100 に戻り、同じ年月の中ではリリースのたびに 1 ずつ増える。同じ版のタグが既にあれば、何もせずに止まる。

`dry_run` を有効にすると、次の版を計算してログに出すだけで、コミット・タグ・ビルド・リリースは行わない。6 つのターゲットのビルドは、CI の build ジョブが main への push と PR のたびに同じ設定で確かめている。

tap と winget-pkgs への提出に使う設定がリポジトリに無いときは、そのジョブが警告を出して飛ばし、リリースそのものは止めない。結果は実行の Summary に出る。

## 参照走査の性能

Python 3.9 以降では、同じ Rust ツールチェーンと release profile で作った2つのバイナリを比較できます。

```powershell
python tools/bench-refs.py `
  --baseline C:/temp/astro-before.exe `
  --candidate C:/temp/astro-after.exe `
  --output-directory C:/temp/astro-refs-bench --mode session --repeats 9
```

`--mode session` はケースごとに常駐するプロセス内の要求時間を測り、`--mode cli` (既定) は起動時間も含めます。多言語の session ケースは3つの名前を別々の要求で送ります。スクリプトは再現可能な入力を保存し、毎回の stdout 一致を確認します。各版を1回ウォームアップしてから実行順を交互に反転します。Rust の深い `use` パス内の名前、同名セグメントの反復、末尾の名前、不在の名前、浅い通常関数、多言語 fixture を測定します。全測定の時間とピーク working set は `samples.csv`、中央値・範囲・実行ファイルのハッシュ・環境は `summary.json` に出します。Windows のピーク値はCLI の終了後または session の各要求後に GetProcessMemoryInfo で取得し、mmap のページも含みます。割当回数ではありません。session のピークは同じケースのウォームアップと先行要求も含みます。他の OS ではこの指標を null として記録します。

生成入力はリポジトリの外に置き、参照検索や API 分析の対象ソースへ混入させないでください。結果にはソース revision、依存の版、ツールチェーン、ビルドコマンド、同時負荷、ファイルキャッシュの条件を記録します。改善幅は全測定の範囲と比較し、小さな差をスケジューラ由来の変動と区別します。Rust の closure 束縛と参照役割には別の祖先走査が残るため、全 Rust 参照検索が線形時間になるという測定ではありません。
