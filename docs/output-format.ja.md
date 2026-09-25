# 出力形式

`--format json|toon|auto` で出力形式を切り替える。既定は `json`。

| | JSON | TOON | auto |
|---|---|---|---|
| 既定 | ✅ | | |
| 仕様 | RFC 8259 | [TOON v3](https://github.com/toon-format/toon-rust) | 推定トークン数が小さい方 |
| `--pretty` | 有効 | 無視（TOON は元からインデント構造） | JSON が選ばれた場合のみ有効 |
| キャッシュ | compact のみ利用 | 利用しない | 利用しない |

## JSON の短縮キー

JSON 出力は既定で compact（`--pretty` で整形）。compact モードではトークン削減のためキー名を短縮:

- `language` → `lang`（calls, imports, lint, sequence, compact ast/symbols）
- `location` → `path`（compact ast/symbols）
- `references` → `refs`、`line` → `ln`、`column` → `col`、`context` → `ctx`（refs）
- `source` → `src`（imports）
- `kind`: `"definition"` → `"def"`、`"reference"` → `"ref"`（refs）
- `SymbolKind`: `"function"` → `"fn"`、`"interface"` → `"iface"`、`"variable"` → `"var"` など（compact symbols）
- `calls`: caller でグルーピング、callee は `{name, ln, col}` に簡略化

compact 出力例（ast/symbols）:
```json
{"path":"src/main.rs","lang":"rust","schema":{"range":"[startLine,startCol,endLine,endCol]"},"ast":[...]}
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"main","kind":"fn","ln":20}]}
```

`ast` / `symbols` に `--full` を付けると、キーを短縮しない完全な形式（`location`, `language`, `hash`, `range` など）で出力する。`--pretty` は字下げするだけで、キーは短縮したまま変わらない（`calls` だけは `--pretty` で完全な形式になる）。`version` フィールドを含むのは `doctor` と MCP の `initialize` 応答だけ。

## TOON とは

[TOON](https://toonformat.dev/)（Token-Oriented Object Notation）は JSON と同じデータモデルを、インデントと表形式で表現するフォーマット。同じ内容をより少ないトークンで LLM に渡せる。

```bash
astro-sight symbols --path src/main.rs --format toon
```

```toon
path: src/main.rs
lang: rust
symbols[3]{name,kind,ln,cx}:
  MAX,const,0,null
  alpha,fn,1,2
  beta,fn,4,1
```

同じ内容の JSON は次のようになる。キー名が要素ごとに繰り返される分が削減される。

```json
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"MAX","kind":"const","ln":0},{"name":"alpha","kind":"fn","ln":1,"cx":2},{"name":"beta","kind":"fn","ln":4,"cx":1}]}
```

変換には [`toon-format` 0.5.0](https://github.com/toon-format/toon-rust) を使用する。CLI/TUI 用の依存を避けるため `default-features = false` とし、comma 区切り・2 スペースの既定設定で符号化する。対応仕様は **TOON v3**。空配列は `[0]:`、名前付きの空配列は `items[0]:` となる。

単一出力・バッチ出力は同ライブラリの strict decoder で検証している。文書末尾に改行は付けない（JSON / NDJSON は改行で終わる）。削減率は内容によって異なり、形式を自動選択する場合は `--format auto` を使う。

## auto - トークン数が少ない方を自動選択

`--format auto` は、その出力について **compact JSON と TOON を両方エンコードし、推定トークン数が小さい方**を選ぶ。同点なら JSON（既定フォーマットで消費側の互換性が高いため）。

```bash
astro-sight symbols --path src/main.rs --format auto
```

### なぜ文字数そのままではないか

BPE トークナイザでは**改行とインデントが 1 行あたりおよそ 1 トークンを消費する**ため、素の文字数で比べると行数の多い TOON を過大評価する。実際、次は文字数と実トークン数で勝者が逆転する（`o200k_base` / `cl100k_base` の両方で同じ）:

| | 文字数 | トークン数 |
|---|---:|---:|
| `{"a":1,"b":2,"c":3,"d":4}` | 25 | **17** |
| `a: 1` `b: 2` `c: 3` `d: 4`（4 行） | **19** | 19 |

そこで判定には `文字数 + 4 × 改行数` を使う。TOON v3 への移行時（2026-09-24）に、単一出力・バッチ・件数制限付き refs の **63 ペア**を tiktoken で再測定した。既存の係数 4 を維持する。

| トークナイザ | 係数 3 の合計損失 / 最大損失 | 係数 4 の合計損失 / 最大損失 |
|---|---:|---:|
| `o200k_base` | 6 / 6 tokens | 0 / 0 tokens |
| `cl100k_base` | 12 / 9 tokens | 3 / 3 tokens |

損失は JSON と TOON の実トークン数が少ない方との差。これは上記標本での測定値で、任意の出力についての上限保証ではない。係数は出力形が変わるたびに再測定する。実トークナイザを本体に含めないことで、追加データの容量とモデルごとの tokenizer 差を避け、同じ入力に対する選択を一定に保つ。

なお `--token-budget`（[出力件数の上限](usage.ja.md#出力件数の上限と-result_summary)）は、**この指標をそのまま使わない**。指標は形式間の相対比較に使うものなので、係数の絶対値は問わない。一方、予算は利用者が「N トークンまで」と絶対値で指定する。両者の桁を合わせないと、「3,000 と指定したのに 900 しか出ない」という乖離が起きる。実測（252 サンプル × 2 トークナイザ）の `指標 / 実トークン` は p05=3.00 / p50=3.42 / min=2.73 なので、予算判定では指標を 3 で割る（予算を超えない側に倒した値）。

### 性質

- **推定トークン数は常に両候補以下**（小さい方を選ぶだけなので、どちらの候補よりも悪くなることはない）。ただし[出力件数の上限](usage.ja.md#出力件数の上限と-result_summary)が効く場合は、予算内に収まる**件数**が形式ごとに変わる。実測では `refs --name new` が JSON で 64 件 / 2,591 トークン、TOON で 83 件 / 2,548 トークンになり、auto は TOON を選んで「同じ予算でより多くの情報」を返す
- 選択は入力内容だけで決まるので**決定的**。同じ入力・同じバージョンなら常に同じ形式になる
- 実際に両方が選ばれる。係数が 3 だった時点で astro-sight の出力 1,127 サンプルを測ったところ、**564 件で JSON、563 件で TOON** が選ばれた（`ast` は JSON、`symbols` / `refs` / `calls` は TOON が勝ちやすい）
- `--pretty` は「選ばれた JSON をどう描画するか」だけを決める。比較そのものは常に compact JSON と TOON で行うため、TOON が勝った場合は `--pretty` の指定は効かない
- 空 object（`{}`）は TOON では空ドキュメント＝無出力になるため、auto は JSON を選ぶ。「結果が空」と「何も出力されなかった」を利用者が区別できなくなるのを避けるため（明示的な `--format toon` は仕様どおり空ドキュメントを出す）
- 下記「常に JSON のままの出力」に挙げた出力面では、`auto` はエラーにならず JSON になる。「TOON で出せ」という満たせない要求ではなく、JSON を選ぶことも auto の正当な結果のため

**バッチでの近似**: `--paths` / `--paths-file` / `--dir` は解析結果を全件バッファしない設計のため、全レコードを見てから勝者を決められない。**出力順で先頭 32 件を標本として両形式で描画し、その実測値で勝者を決めて残りに適用する**。標本の件数は並列度（CPU 数 / `ASTRO_SIGHT_BATCH_WORKERS`）に依らない定数なので、同じ入力なら並列度を変えても同じ形式が選ばれる。二重エンコードのコストは標本ぶんだけで、解析自体はどの経路でもパス 1 回きり。出力が途中で混ざることはない。

## 常に JSON のままの出力

次の 3 つは相手側が JSON を前提とする契約のため、`--format json|toon` の対象外（`auto` は JSON を選ぶだけなのでエラーにならない）。

| 出力面 | 理由 |
|---|---|
| `session` | 「1 行 = 1 リクエスト / 1 レスポンス」の NDJSON プロトコル |
| `review --hook` | Claude Code の Stop hook が消費する JSON 契約（compact JSON を stderr に出す） |
| エラー出力 `{"error":{...}}` | 既存スクリプトが parse する機械可読契約 |

`impact` は構造化出力を持たず、`--hook` の有無にかかわらず stderr にテキストを出すだけなので、同じく `--format` の対象外になる。

CLI で明示的に `--format toon` を渡した場合は「満たせない要求」としてエラーにする。`config.toml` の `format = "toon"` は全コマンドの既定表示形式でしかないため、これらの出力面では黙って JSON に倒す（設定しただけで hook や session が壊れないようにするため）。

## バッチ出力の形

`--paths` / `--paths-file` / `--dir` は JSON では NDJSON（1 行 1 レコード）、TOON では**ルート配列 1 個のドキュメント**になる。

```toon
[2]:
  - path: a.rs
    lang: rust
    symbols[3]{name,kind,ln}:
      MAX,const,1
      alpha,fn,2
      beta,fn,5
  - path: b.rs
    lang: rust
    symbols[1]{name,kind,ln}:
      gamma,fn,1
```

外側の配列は list form（`- ` 項目）で、tabular form にはしない。tabular 化には、全要素を見終えて初めて決まる情報が要る。これは、解析結果を全件バッファしない（ピーク RSS を入力件数から独立させる）という設計要件と両立しない。要素数 `[N]` は入力パス数から先に分かるので、ヘッダだけは先出しできる。内側の配列は tabular form になり、削減量の大半はそちらから来る。

バッチでは、全要素を一括変換した場合に tabular form になる配列も list form で出す。要素の符号化はライブラリへ委譲し、strict decoder で復元した値が一致することをテストしている。解析失敗も 1 要素として数えるため、ヘッダの件数と出力件数は一致する。`refs --names` は元々全件を保持しているため、一括変換する。

## nullable 列の正規化

astro-sight の compact JSON は、`cx`（循環的複雑度）のようなフィールドを、値がない要素ではキーごと省略する。一方 TOON の tabular form は、配列内の全要素で**キー集合がそろっている**ことを要求する。素直にエンコードすると、symbols のような出力が list form へ落ちて **JSON より冗長になる**（実測 +33%）。

そのため astro-sight は、配列内の object どうしのキーの違いが「一部のキーが欠けているだけ」のとき、欠損キーを `null` で補って tabular 形を成立させる。適用は**厳密エンコードより短くなる場合だけ**（同点なら厳密側）なので、補完によって TOON がかえって長くなることはない。

- 補完対象は、全要素が非空 object で、すべての値がプリミティブの配列だけ。object を含む列は対象外
- 列順は要素を順に走査したときのキー初出順で固定する（決定的）
- **JSON 表現との構造的 round-trip は保証しない**。decode すると JSON が省略していたキーが `null` として現れる。DTO としての意味（`Option<T>` の `None`）は保存される

この正規化は astro-sight が自分の DTO について行う判断で、`--format toon` のエンコーダ自体は仕様どおりの純粋な実装のままにしている。任意の JSON に対して欠損キーを null 補完すると、「明示的な null」と「未設定」を区別できなくなるため。
