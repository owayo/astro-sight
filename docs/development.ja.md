# 開発

README の [開発](../README.ja.md#開発) の補足。`make setup` と `make ci` の流れはそちらにある。

## 前提

[mise](https://mise.jdx.dev/) を前提にしている。Rust の版は `mise.toml` で固定し、Makefile の各ターゲットが `mise exec --` 経由でその版の `cargo` を呼ぶ。シェルで mise を有効にしていなくても版はそろう。mise のほかに C コンパイラ（tree-sitter の各パーサをビルドする。macOS なら Xcode Command Line Tools）と git（結合テストが一時リポジトリを作って `git` コマンドを呼ぶ）が要る。

mise を使わずに PATH 上の `cargo` で動かすなら `SYSTEM_TOOLS=1` を付ける。Rust の版が `mise.toml` とずれることがある。

```bash
make ci SYSTEM_TOOLS=1
make install SYSTEM_TOOLS=1
```

## CI と同じ検査

CI の quality ジョブ (Linux と macOS) は `make setup` と `make ci` を呼ぶだけなので、手元の `make ci` がそのまま CI の検査になる。テストは配布物と同じ既定の feature で回す。clippy だけは `--all-features` で、ヒーププロファイラ（`dhat-heap` feature）側のコードも検査する。Windows は build ジョブで `cargo test --locked` を直接回す。

Cargo のコマンドには既定で `--locked` を付けている。`.cargo/config.toml` の `[patch]` でローカルの tree-sitter 系を差し込むと `Cargo.lock` が手元でだけ変わり、`--locked` で止まる。そのときは `make ci CARGO_FLAGS=` のように `CARGO_FLAGS` を空にする。

`make install` はバイナリを入れた後に、Claude Code と Codex のスキルも書き込む。書き込む先は `SKILL_TARGETS` で選べる（既定は `claude codex`）。

```bash
make install SKILL_TARGETS=          # スキルを入れない
make install SKILL_TARGETS=claude    # Claude Code のスキルだけ入れる
```

`make uninstall` はバイナリだけを消し、書き込んだスキルは残す。

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
