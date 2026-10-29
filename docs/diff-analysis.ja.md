# diff の解析

unified diff (`--git` なら作業ツリーの `git diff`) を受け取り、変更の影響を調べるコマンドをまとめる。`context` が影響範囲を返し、`impact` は未解決の影響があれば exit 1 で止め、`review` は影響・共変更・公開 API の差分・死蔵シンボルを 1 回で返す。`dead-code` と `cochange` は diff が無くても使える。

Rust で LF/CRLF の変換に巻き込まれただけの宣言は、旧新 AST で宣言全体・所属・属性が同じと確認できれば影響起点から外す。物理的な CRLF だけを LF と同一視し、文字列内の空白・空行・`\r` / `\n` のエスケープなどの実変更は残す。同名複数、別パス、復元・解析の失敗、大きい入力や深い所属は従来の保守的な判定を残す。通常・staged・外部 diff と、review の影響分析に適用する。

## context - スマートコンテキスト（diff → 影響分析）

unified diff を受け取り、変更の影響範囲を分析する。AI コードレビュー支援機能。関数シグネチャ変更は識別子境界で照合するため、`foo` と `foo_bar` のように名前の先頭だけが一致する別関数を混同しない。

```bash
# git diff を自動取得して影響分析（推奨）
astro-sight context --dir . --git

# ステージ済み変更を分析
astro-sight context --dir . --git --staged

# カスタムベースを指定
astro-sight context --dir . --git --base HEAD~3

# stdin からパイプ
git diff HEAD~1 | astro-sight context --dir .

# インライン diff 文字列
astro-sight context --dir . --diff "$(git diff HEAD~1)"

# diff ファイルから読み込み
git diff HEAD~1 > /tmp/changes.diff
astro-sight context --dir . --diff-file /tmp/changes.diff
```

`context` / `impact` / `review` の `--base` は `git diff` / `git show` / `git blame` にそのまま渡るため、`-` で始まる値・NUL を含む値・空文字を `INVALID_REQUEST` で拒否する（`--output=/path` などのオプション誤認識を防ぐ）。

出力例:
```json
{
  "changes": [
    {
      "path": "src/engine/symbols/mod.rs",
      "hunks": [{ "old_start": 10, "old_count": 5, "new_start": 10, "new_count": 8 }],
      "affected_symbols": [
        { "name": "extract_symbols", "kind": "function", "change_type": "modified" }
      ],
      "signature_changes": [
        { "name": "extract_symbols", "old_signature": "fn extract_symbols(...)", "new_signature": "fn extract_symbols(..., include_refs: bool)" }
      ],
      "impacted_callers": [
        { "path": "src/commands.rs", "name": "cmd_symbols", "line": 166 }
      ]
    }
  ]
}
```

Rust の `affected_symbols[].kind` は、impl ヘッダの変更・メソッドのない impl の追加・メソッド宣言を削除した可能性のある差分では `impl` になる。行削除を伴わないメソッド追加はメソッドだけを報告し、所有型の宣言変更としては報告しない。削除行がある場合は変更後の構文木だけではメソッド削除を否定できないため、`impl` の記録を残す。属性だけの変更もメソッドシンボルの範囲外なので `impl` として残す。`symbols` コマンドには、メソッドの所有元を解決するための impl owner を引き続き含める。

Rust の署名の帰属は、AST の関数宣言の名前で確認する。`apply(text: &str)` の引数名 `text` を、別の `text` メソッドの署名として扱わない。削除と追加が混ざる hunk でも、ファイル直下の inherent impl に直接属する一意な callable の全行が追加され、旧 AST に同名の callable・関連項目・import が無いと確認できれば、新規メソッドを `added` にする。同名の旧項目、trait / nested impl、生成可能な macro / attribute、Unicode 名の同一性が不明な場合、解析・旧ソース復元の失敗、他ファイルでの削除の可能性があれば保守的な分類を維持する。本体だけの変更・互換性・private メソッドの既存の証明条件は変えない。

呼び出し元は、確信度と破壊性によって 3 系統に分かれる。ここでの blocking 対象とは、`impact` や `review --hook` が exit 1 を返す原因になる検出を指す。

- `impacted_callers`: 実際の呼び出し位置。diff 外に残れば `impact` の blocking 対象になる。引数個数が変わったシンボルへの参照や、判定不能な参照もここに残す
- `low_confidence_callers`: owner（メソッドが属するクラスや型）を確定できない汎用名や、直接 import の証拠がない TS/Rust の同名参照
- `informational_callers`: 名前と引数個数が変わっていないシンボルへの関数値参照や、名前が変わっていない modified シンボルの import 行。blocking 対象にしない

### JS / TS のシグネチャに依存しない参照

JS / TS / TSX のトップレベル関数について、別ファイルの同名の出現がシグネチャに依存しないと確認できれば、`modified_closed_in_diff` (`api.mod_closed`) の判定から外す。旧新とも一意なトップレベルの直接関数宣言がある変更に限り、宣言種別が変わる変更には適用しない。対象は、非 computed な `property_signature` の名前と、用途が真偽判定だけの引数である。プロパティ型は、直接書いた `boolean`・`string`・`number` などの非 callable な組み込み型、リテラル型、その union に限る。型の別名・関数型・method signature・モックのキーは除外しない。

引数の証拠には、ファイル全体が strict mode だと確認できることが必要である。static import/export 文、またはファイル先頭の正しい `"use strict"` directive prologue で確認する。動的 import、関数内の directive、class 内だけの strict mode は証拠に使わない。sloppy mode や strictness が不明な入力は、関数の外からの反射的な引数取得も否定できないため保留する。プロパティ宣言の証拠には strict mode を要求しない。

引数は単純な識別子か、object pattern 直下の shorthand に限る。同じ関数内の入れ子の closure も含め、すべての同名の用途を調べる。括弧を除き、三項演算子や `if`・`while`・`do`・`for` の条件、または前置 `!` の対象だけなら除外できる。呼び出し・値の受け渡し・返却・代入・同名の再宣言・型への言及が一つでもあれば、引数全体を未証明のまま残す。default・rest・名前を変える分割代入・constructor parameter property も対象外である。`arguments` を参照する関数も、引数への別経路を否定できないため対象外とする。

解析エラー、`with`、`eval` の手掛かり、ambient 宣言、宣言ファイルではこの証拠を使わない。識別子系のいずれかの AST kind に Unicode escape があれば、名前の同一性を推測せず保留する。API の降格には、対象関数の実呼び出しを一つ以上確認する必要がある。同じ差分で引数の個数と変更行が追随していることも確かめる。別名 import は後述の独立した証拠で判定する。単一・バッチの `refs` と dead-code の件数は変えない。

`context` と `impact` も、この証拠を持つ別ファイルの参照を `low_confidence_callers` に分ける。対象は旧新とも一意な直接関数宣言で、変更された宣言自身がトップレベル関数だと確認できるシグネチャ変更に限る。同名の変更対象にメソッドや別種の宣言が混ざれば適用しない。削除・追加・宣言種別の変更、import の位置も対象外とする。確信度フィルタを無効にした場合は使わない。直接の `impact --staged` や、review の staged snapshot を作れなかった場合も適用しない。`review --staged` が index 全体の snapshot を使う場合は、その中の参照だけで判定する。

### TypeScript の named import の別名

TS/TSX の一意な通常直下関数では、直接の相対 `import { total as sum } from './pricing'` を通した呼び出しも `api.mod_closed` に数えられる。解決先は変更した宣言ファイル1つに限り、競合する候補があれば証明しない。別名の全利用が同じ import に束縛される直接 call であることを求める。内側で同名の引数やローカルを宣言した利用は別束縛として除く。元の名前と別名の両方について、変更後の引数数と既存の実変更行の条件を照合する。複数行の call も扱い、`review --staged` は完全な index snapshot の中の証拠だけを使う。

再 export、type-only import、関数値の持ち出し、`typeof`、値をさらに別名へ束縛する形、未対応の call、不確定な解決は blocking に残す。さらに同じ範囲の JS/TS/TSX を監査し、元名の索引に現れない escaped identifier と module object 経由の輸送を確かめる。namespace/default import、export star、直接の require・動的 import は、別の相対ファイルへ一意に到達する場合だけ許す。無関係でも解決不能な輸送があれば閉じない。rootDirs/moduleSuffixes/extends、config の escape、読取・解析失敗、リンク、動的コードの手掛かりでも保留する。名前は ASCII の単純な識別子に限り、型メンバー変更の義務は従来の方針を維持する。

inventory はソース10,000件・1件2MiB・合計64MiB・100,000 ASTノード・100,000 walk entry に限る。別名参照の走査には別途、別名64件と100,000ノードの上限がある。config は1件1MiB、ディレクトリ10,000 entry、祖先128階層まで調べ、上限を超えれば blocking に残す。ファイルの選択は既存の ignore・生成物の方針に従い、外部利用者や任意の runtime reflection は証明しない。`refs`・dead-code の件数と通常の出現一覧は変えない。

### TypeScript の関数値の受け渡し

export した関数の明示戻り型と、その関数を受け取る interface の callback 戻り型を同じ差分で変えた場合、無変更の `execute({ clone: cloneText })` を証明して両 API 変更を `mod_closed` にできる。三者は直接 export した一意な root 宣言に限り、interface は必須の function-type property を一つだけ持ち、consumer は同じファイルの interface を唯一の必須引数として受け取る。旧新それぞれで callback への代入を照合し、producer の引数、consumer の明示戻り契約など、それ以外のヘッダは不変であることを求める。

型は void・primitive・mutable/readonly 配列に限る。consumer 本体は callback 結果を捨てる呼び出し、primitive literal の return、配列 callback 結果の `.length` を number として返す形だけを証明する。producer の直接呼び出し、受取 interface の未更新、callback の変更・抽出・外部への受け渡し、consumer の推論戻り型、未知の用途は保留する。三者の全出現と module 輸送を旧新で確認してから両方を閉じる。standalone `impact --git` も同じ差分限定証拠を使い、通常の refs と context の出現は維持する。

inventory の上限は TS/TSX 512ファイル、旧新合計8MiB、AST 100,000ノード、深さ128、候補照合2,048回である。別名/default/namespace import、再 export、動的 loader、JS・埋込 source、JSDoc、解析・読取失敗、リンク、不確定な設定 (extends/rootDirs/moduleSuffixes/paths/baseUrl) があれば証明しない。観測した内部の手渡しと限定した consumer の依存を確認するもので、一般的な API 互換性や TypeScript 全体の型検査を保証しない。

### TypeScript の型への必須プロパティ追加

export した TS / TSX の interface か object 型の type alias へ、必須プロパティを足した変更を扱う。その型の値を作る箇所と受け取る関数が同じ差分で追随していれば、`modified_closed_in_diff` (`api.mod_closed`) に分ける。型への参照は、それぞれ次のどれかでなければならない。

- 関数・メソッドの戻り値の型 (型注釈がちょうどその型)。すべての `return` が、後述の「証明できる値」を返すこと。async 関数は次の限定した形だけを扱い、generator は対象外
- ファイル直下の通常関数宣言の戻り値が `Row[]`・`Array<Row>`・`readonly Row[]`、または async 関数宣言の戻り値が `Promise<Row[]>`・`Promise<Array<Row>>`・`Promise<readonly Row[]>`・`Promise<Row>`。配列を返す全 `return` は、配列型の必須引数か配列 literal に対する `.map` で、引数1つの無注釈 inline arrow が証明できる値だけを返すこと。callback の式本体とブロック本体を扱い、callback と外側の関数の両方で fallthrough がないことも確かめる。`Promise<Row>` は証明できる値を直接返す形だけを扱う。receiver の別名・書き換え・持ち出し、callback 引数の書き換え、任意オブジェクトの同名メソッド、`filter`・`flatMap`・spread・型アサーション・`await` は証明しない。`Array` / `ReadonlyArray` / `Promise` の宣言・import・prototype への参照や別名への持ち出しが解析範囲内にあっても証明しない。外部コードによる prototype の変更は解決しない
- `const x: Row = <値>` と `<値> satisfies Row`。値が証明できること
- ファイル直下の関数宣言の、既定値の無い必須引数 (`function label(row: Row)`)。本体でその引数へ代入せず、値を別の名前へ束縛したりオブジェクト・配列へ入れたりせず、`typeof` で型を取り出さないこと。メンバーの読み出し (`row.id`) と呼び出しへの受け渡しは妨げない。関数名で別に検索し、直接の呼び出しが 1 件以上あり、すべての参照が import・別名なしの export・その位置へ証明できる値を渡す直接の呼び出しのどれかであること
- 別名なしの `export { Row }`。値の名前空間の出現 (同名の変数・JSX のコンポーネント・プロパティ名) は型を指さないので数えない

証明できる値は次の 3 つ。追加した各プロパティを 1 つずつ持ち、そのプロパティが変更行にあり、値の型が追加したメンバーの型に合う object literal。ファイル直下で宣言した関数のうち、戻り値の型がちょうど対象の型であるものの呼び出し (名前空間を通した `rows.toRow()` は数えない)。型注釈がちょうど対象の型の必須引数と変数。object literal のキーに escape を含む名前 (`n\u006fte`) があれば証明しない。表記と違う名前のキーを作るためである。値の型は `string`・`number`・`boolean`・`bigint`・`null`・`undefined`・リテラル型 (bigint のリテラルを含む)・それらの union と配列・`any` / `unknown` で照合する。プロパティの値に使う型注釈の無い変数は、primitive のリテラルで初期化した `const` だけを型付けする。object や配列のリテラルは型が広がる (widening) ため。プロパティの値に `satisfies` や `as const` (primitive のリテラルへのものを除く) を書くと、リテラルの型付けが変わるので証明しない。ファイルの中で `undefined` という名前を束縛し直していれば、値の `undefined` を組み込みの値とみなさない。

必須追加で `const x: Row = <値>` が関数・ブロック内のローカル変数なら、字句束縛がその宣言へ解決する利用だけを調べる。別ファイルや別スコープの同名の変数・引数は含めない。同じ束縛への `typeof`・別名への代入・入れ物への格納と、同一スコープの重複宣言は証明しない。内側の別束縛を受け取る関数へ渡す場合は、その値を独立に確かめる。型縮小・プロパティ削除の監査は従来の範囲を維持する。

必須追加では `const picked = makeRow(); label(picked)` も、TS/TSX の単純な const で、宣言が参照より前にあり、初期化子が明示戻り型を持つ一意なファイル直下の関数の直接呼び出しなら証明できる。関数の戻り型はちょうど対象の型でなければならず、別名なしの相対 import も既存の解決条件で確かめる。変数の利用を同じ字句束縛で監査し、`typeof`・別名への代入・格納・重複・書込みを認めない。ローカル変数と module 内の非公開な直下変数を扱い、変数の export/import、JS、script global、`let`・分割代入・alias 連鎖・型アサーション・generic/overload・明示型引数・spread・名前空間経由・async/nullable 戻り型は証明しない。括弧で包んだ呼び出しは同じ値として扱う。

必須追加は `{ ...snapshot, title }` のようなコピーも扱う。すべての spread 元が、対象の型と証明できる必須引数・変数 (上記の直接呼び出し結果の const を含む)、または対象の型を明示して返す一意な通常直下関数の直接呼び出しでなければならない。コピーされた追加プロパティに変更行は求めず、後ろの明示値で上書きする場合は、その値の型と変更行を従来どおり確かめる。最後の確実な spread が前の明示値を上書きする順序も照合する。不明な spread は前後の位置によらず保留し、`any`・`Partial`・型アサーション・generic/overload・明示型引数・spread 引数・配列/Promise の元は扱わない。computed/escape key、重複した明示追加キー、メソッドを持つコピーも保留する。

引数・変数の値を spread で格納する際の例外は、コピー先が明示した対象型の戻り値を持つ直下の通常関数、`const x: Row` の初期化子、または `satisfies Row` の位置に限る。その型の所属・コピー先の値・caller・束縛名の `typeof` と別名利用は、他の型参照と同様に全て確認する。無注釈のコピー用変数や入れ物は例外にしない。型縮小・プロパティ削除はこのコピーの証明を使わない。

別ファイルの参照は、別名なしの named import で、相対 specifier の解決先として存在するファイルがちょうど宣言ファイル 1 つのときだけ数える。参照元から解析 root までの `tsconfig*.json` / `jsconfig*.json` に `rootDirs` か `moduleSuffixes` があれば証明しない。作った値を束縛する名前 (関数・変数・`satisfies` で初期化した変数と、その値を入れた外側のオブジェクトや配列) も検索する。次のどれかがあれば証明しない。

- `typeof` の利用。`ReturnType<typeof toRow>` のように、型の名前を出さずに型を写す
- その名前の別名での import や再 export
- 値の別名への束縛や、オブジェクト・配列への格納 (`const make = toRow`・`{ toRow }`)。条件式・`&&` / `||` / `??`・カンマ式を挟む場合も含む
- 入れ物から中身を取り出して別の名前へ束縛すること (`const it = box.item`・`const [first] = list`)。識別子で書いていないキーでの分割代入 (`const { "item": picked } = box`・`{ [key]: picked }`・`{ 0: picked }`) も含む
- 名前の無い入れ物から中身を取り出す値 (`[<値> satisfies Row][0]`)。行き先を名前で追えない
- これらの名前や受け取る関数の名前を、値の添字・分割代入のキー・型の添字に文字列で書いた箇所 (`rows["toRow"]`・`(typeof rows)["toRow"]`)。名前空間オブジェクトから、名前を出さずに取り出す。`as const`・括弧・条件式・computed key (`{ ["toRow"]: make }`)・型の union (`["toRow" | "makeRow"]`) を挟む場合も含み、無関係な辞書の添字 (`labels["toRow"]`) でも止める
- `.vue` などの埋め込みソースへの出現
- JS 系のファイルの JSDoc (`/** ... */`) への、型の名前やこれらの名前の出現。checkJs では JSDoc も型として検査されるが、コメントなので参照に現れない。型は `{...}` で囲まなくても読まれる。型として読まれない普通のコメントは見ない

値を作る関数・変数を default export や別名で公開している場合と、値を作るのがクラスのメンバーの場合も、利用者を名前で探せないので証明しない。クラスのメンバーの型は、クラスの型 (`Repo["find"]` や `implements Repo`) を通じて写る。変数に入れた文字列や関数に渡した文字列での取り出し (`rows[key]`・`get(rows, "toRow")`) と、コールバックとして渡したとき (`ids.map(toRow)`) に推論で伝わる型は追わない。

この種の変更では、参照の行が変更行にあることを証拠にしない。次の形の必須追加は、参照の行がすべて変わっていても blocking に残す。

- generic な宣言、`extends` や index signature のある宣言
- 必須メソッド、call / construct signature の追加
- 上の一覧にないメンバーの型、文字列名のプロパティ
- メンバー名の衝突 (同時に足した optional メンバーとの衝突を含む)、表記で比べられない名前 (数値の `1` と `0x1`、escape)、自分自身を参照する型

ほかの位置の参照 (上の戻り値以外の `Row[]`・`Promise<Row>`・ほかの型のプロパティ型・`implements`・`as Row`・型引数・callback・メソッド・arrow 関数・optional 引数) も blocking に残す。この証明を入れる前は、参照の行がすべて変わっていれば、位置によらず閉じていた。

証明が及ぶのは、型の名前の参照と、値を作る関数・変数・入れ物の名前の参照を、上に挙げた構文の範囲で確かめるところまでである。上の限定した const の証明以外では、呼び出しの結果・引数・return を通って推論で伝わる型は追わない。たとえば `note` の型が衝突する別の型で注釈した変数に `toRow()` を代入する場合や、`const r = toRow("a")` の `typeof r` である。

### TypeScript の既存プロパティ型の縮小

ファイル直下で export した interface / object 型の type alias で、既存の必須プロパティの scalar union から型を除く変更も、限定した証明によって `api.mod_closed` にする。たとえば `string | null` から `string`、`string | undefined` から `string`、`"ready" | "done"` から `"ready"` である。型の要素を表記のまま比較し、新しい要素の集合が元の集合の真部分集合であることを求める。型全体のプロパティが単純なscalar型で、名前・宣言ヘッダ・ほかのメンバーが不変である場合だけを扱う。generic・継承・index signature・関数値や未知の型のメンバー・名前の曖昧性・optional の型変更・同時のメンバー追加・引用名や数値名の変更は証明しない。未対応の既存メンバー変更や、一意性・解析を確かめられないobject型を、参照行が変更行にあるだけで閉じない。

値を作る位置はファイル直下の通常関数の `Row` 戻り値と `const x: Row` に限り、全ての値が変更後の型に合い、対象プロパティが変更行にあることを求める。関数のfallthrough、メソッドやgetterを持つobject literal、`any`・`unknown`・型アサーション・非nullアサーションに依存する値を証明しない。明示した型注釈付きのconstも、直接の初期化子の型が注釈に合うことを確認して先行constの連鎖を16段まで確かめ、循環・後方参照・再代入可能な変数を保留する。通常の型付き引数は、その本体で再代入されていなければ注釈を使う。引数に渡されたprimitive値の由来までは追わない。配列・Promiseのproducer、`satisfies`、arrowやクラスメンバーは、この縮小の証明には含めない。

受け取る関数には明示した戻り値型を求め、型の注釈が対象型の変数・引数とproducerの直接呼出し結果を調べる。縮小対象プロパティへの代入・複合代入・update・delete・分割代入・forの代入を保留する。**縮小後の型と重ならないリテラルとの比較を保留する方針**も適用する。これは比較に対するTypeScriptの診断を再現する処理ではない。対象値を渡せるのは、同じ証明で呼出し側と本体を確かめる関数だけである。未知の呼出し、値や対象プロパティの別名への束縛、文字列キーによる利用、未証明のメンバー呼出しや戻り値の推論は保留する。既存の `typeof`・別名import/export・JSDoc等のガードも維持する。

これは全プログラムの型検査や実行時の書込みの解決ではない。型やproducerの名前が現れない推論、外部コード・reflectionによる変更、別の型として渡された値の由来は追わない。`refs` とdead-codeの出現契約は変えない。

### TypeScript のプロパティ削除

ファイル直下でexportしたinterface/object型のtype aliasからproperty signatureだけを削除した変更は、観測できるproducerとconsumerの追随を全て証明できれば`api.mod_closed`にする。宣言ヘッダと残るメンバーが不変で、旧新の全メンバーが一意な識別子名を持つpropertyであることを求める。scalar型だけの変更は、従来の値の型照合で証明する。必須・optionalのどちらも削除できる。削除するメンバー自身はscalar型に限る。generic・継承・index signature・メソッド・関数値や対応外の型・自己参照・宣言の曖昧性や解析不能・追加や型変更との混在は、全参照行が変わっていてもblockingに残す。

値を作る箇所は、ちょうど`Row`を返す直下の通常関数と、同ファイルの`const x: Row`に限る。残る必須プロパティがあり、値の型が合うことを確かめる。追加・縮小と違い、そのプロパティが変更行にある必要はない。any・unknown・assertion・不適切な初期化子に依存する値、spread・computed key・メソッド・getterやfallthroughは保留し、先行constの初期化子を追う既存の上限付き検査を使う。consumerには明示戻り型と直接呼出しの証明を求める。型付きRowの値とproducerの直接呼出し結果も調べ、`const key = "extra"; row[key]`等の動的/文字列キー、分割代入、別名、未知の呼出しや持出しはblockingに残す。satisfies・arrowによるproducer・クラスメンバー・importしたRow定数・戻り型の推論は、この削除の証明には含めない。追加・縮小の既存契約は維持する。

残るメンバーがscalar型だけの削除では、明示した戻り型が`Row[]`・`Array<Row>`・`readonly Row[]`の一意な直下の通常関数も扱う。async関数の`Promise<Row[]>`にも対応する。returnは配列の`.map`を直接呼び、callbackはobject literalを式本体で返す形に限る。既存の配列・組み込み名の監査を通し、値の型と削除キーの不在を確かめる。callback引数を値に使えるのは、入力配列が型付きの必須引数であり、書換えや持出しがない場合だけ。

配列の利用は`toRows(...).map(label)`、Promiseは`(await toRows(...)).map(label)`に限る。生成関数の宣言と配列・Promiseの形を記録し、consumerの参照と生成結果の参照を同じ検証器で確認する。consumerは必須の`Row`引数1つと明示したprimitive/void戻り型を持つ通常関数とし、型引数・overload・async/generatorは保留する。別名・`typeof`・配列の持出し、thisArg・spread・optional chain・型引数、callbackのブロック本体、転送arrow、`.then`や非直結のawaitは扱わない。非scalarメンバーの不変証明との組合せと、追加・縮小の既存判定は広げない。

残るメンバーにscalarの配列型、`readonly T[]`、`Array<T>`、`ReadonlyArray<T>`、`Record<string, scalar>`があれば、旧新の値が変わらないことを別の証拠にする。残る型注釈のgeneric表記に現れる組み込み型名だけを監査し、同名宣言・import・別名への持出し・書換えがあれば証明しない。`Array.isArray`などの通常の値参照は許し、`T[]`と`readonly T[]`だけならこの監査は行わない。値を作る位置は一意な直下の通常関数のreturnと、直下の`const x: Row`のobject literalに限る。対象の型宣言と削除キーの項目だけを除き、ファイル全体のトークン列が旧新で一致することを求める。これにより、引数・束縛・残る全キーの値の変更も保留する。optionalなキーも、値を書いてあれば比較する。値はliteral、配列・object literal、型と束縛を確かめたproducer引数に限り、call・外側の変数・名前付き型・assertionは扱わない。引数の型はscalarか`T[]`・`readonly T[]`に限り、generic表記は保留する。新しいproducerやliteral引数、ローカルconst、map、satisfiesもこの証拠には含めない。1ファイル2MiB・100,000ノード・値の入れ子8段の上限、対応付けの失敗、同じファイルでのほかの変更でも保留する。scalarだけの経路と追加・縮小の判定は変えない。

別の走査で、削除名がプロパティ/メンバー名、objectや分割代入のキー、文字列の添字、文字列literal型に残っていないことを求める。`Pick<Row, "extra">`・`Row["extra"]`も含み、無関係なobjectや型の同名キーでもblockingに残す。通常の文字列値やコメントはキーとしないが、型を供給するJSのJSDocは保留する。生の削除名が無くてもescapeを含むファイルを解析し、escapeで書いた識別子やキーは証明しない。削除名かescapeを含む未解析の埋め込みソース、読取失敗、関係する解析失敗、keyof Row、typeof・別名や隠れたimport/exportも保留する。stagedはindex snapshotを使う。この限定検査は一般の推論・外部コード・reflectionを追わず、refs/dead-codeの出現契約も変えない。

### TypeScript の文字列 union の要素削除

ファイル直下で直接 export した、文字列 literal だけの type alias から要素を削除した変更も、観測した値の流れを証明できれば `api.mod_closed` にする。宣言ヘッダと残る要素の順序・表記を保持し、重複・escape・型引数・別名の展開・追加との混在は扱わない。対象と判断した形を証明できないときは、参照行が変更されていても blocking に残す。削除のない追加・並べ替えと、旧側が純粋な文字列型でない変更は既存の判定に返す。旧文字列要素が失われて `string` 等の一般型になる変更も、この証明では保留する。同一パス・同名の候補だけを扱い、rename は既存判定に任せる。旧入力の上限超過や ERROR に包まれた alias など、旧宣言そのものを認識できない場合も、この追加証明は適用しない。

型の参照は、直下の `const X: Record<Mode, primitive>`、`const X: Mode[]` / `readonly Mode[]`、明示した primitive/void 戻り型を持つ通常関数の必須 `Mode` 引数に限る。primitive は `string`・`number`・`boolean`・`bigint` の keyword とし、戻り型だけ `void` も扱う。ほかの引数も型付きの必須 primitive 引数に限り、optional・既定値・rest・分割代入・generic は扱わない。Record のキーは残る集合と一致し、配列の要素は残る文字列 literal だけとする。直接の相対 named import は別名なしで一意に対象へ到達する場合だけ扱う。同名の別型は束縛で区別し、解析や所属が不明なら保留する。許可した受け口と値の実利用が少なくとも1件必要で、無参照・型位置だけの利用では閉じない。

配列は単一引数の inline arrow による `.filter(...)` の連鎖と終端 `.map(...)` に限る。filter は既知の boolean consumer 呼出しか残る literal との比較、map はこの比較・既知の primitive consumer 呼出し・Record の添字読出しのいずれかを証明する。consumer 引数と添字へ入る値は、残る文字列 literal、証明済み consumer 引数、配列 callback 引数だけ。Mode の値の利用も、その呼出し・添字・既知 literal との比較・switch の判別に限る。別名・持出し・書込み・typeof/keyof・再 export・未知の引数・assertion・optional/spread/type-argument/thisArg は保留する。

補助の残存走査では、削除文字列と同じ cooked 値の string・静的 template・型 literal・property key を調べる。無関係な同一値も blocking とし、部分一致や無関係な置換付き template は妨げない。通常の JS escape と Unicode escape を解釈する。削除値が数値としても読める場合は生の名前の事前フィルタを使わず、数値・computed の静的キーを保留する。JavaScript の数値から文字列への変換は再実装しない。対象の名前や削除値を含む JSDoc・埋め込みソース・JSON も監査する。ignore・hidden・生成物の扱いは既存の参照走査にそろえる。この削除候補を認めた後の読取・解析・走査失敗、ソースを隠し得るリンク、走査上限では保留する。上限は100,000 entry・10,000ファイル・1件2MiB・合計64MiB・解析512ファイル・100,000 ASTノード。解析数とノード数は同一の API 変更検出全体で共有する。

相対 import の解決前には、caller の論理上・実体上の祖先をファイルシステムの root まで監査する。ignore に関係なく tsconfig / jsconfig の解決設定を読み、不明な設定・project references・読取失敗は保留する。directory import の先に package.json がある場合も保留する。候補列挙より先に解決され得る別ソースも監査する。明示した宣言ファイルより優先される実装ソースや、古い解決方式の JS suffix に付く宣言・directory entry も含む。config の entry・読取量・深さにも共有上限を設ける。祖先でない config をコマンドラインで指定した場合や、外部 bundler 独自の platform suffix などの解決規則は証明対象外とする。

`createRequire`・member の `require`・`require.context`・`import.meta.glob` 等の既知loaderも保留する。JSX の HTML entity は補助走査で復元せず、対象型の JSX 利用は未対応として保留する。生成物として選択から外れたソースや、任意の loader 別名・動的な名前合成は証明対象に含めない。

保証は観測範囲内の限定した静的な流れまで。外部利用者・runtime データ・reflection は証明しない。Record 定数の `const_value` と strict const の方針、standalone impact・refs・dead-code の出現契約は変えない。

### Rust の引数パターン

Rust の型付き通常引数では、名前や分解パターンだけを変えても呼び出し方は変わりません。`fn double(_n: i32)` → `fn double(n: i32)` や、同じタプル型の分解パターンの名前変更は、新旧の宣言と import 環境を確認できれば `api.mod` と関数シグネチャの影響から外します。本体を同時に変更した場合も対象です。表示用のシグネチャには引数名を残します。

型・数・順序・戻り値・receiver・ジェネリクス・境界・ライフタイム・修飾子・ABI・可視性・外側の宣言ヘッダ・許可した属性の値は引き続き比較します。限定した組み込み属性の許可リストに含まれない属性、`cfg_attr`、`unsafe(...)` 属性、引数属性、ヘッダ内のマクロ、解析エラー、曖昧な宣言があれば従来の判定を保ちます。別ファイルの `mod` 宣言の属性と外部マクロの展開は解決しません。型名は構文上の表記で比較し、同時に変更したローカルな型定義や推移的な型エイリアスの実体までは解決しません。

この証明の対象は関数項目です。容器は従来の規則で判定するため、1 行で書いた trait は trait 自体の `api.mod` が残り、impl owner の影響も残る場合があります。`refs` と dead-code の出現件数は変わりません。

### Python の相対 import 束縛

別名のない、無条件のモジュール直下の `from .cli import main` で読み込んだ `main()` は、無関係な `legacy.main` の変更による影響から除外する。除外するのは、相対モジュールを workspace 内の一意なファイルへ解決でき、import した関数が無装飾・直接定義で各1件だけ存在する場合に限る。実際の定義元の変更、型スタブ `.pyi` の変更、import 名の出現位置は従来の扱いを維持する。`refs` の出現一覧と dead-code の参照件数も変えない。

package の初期化はコメント・通常の文字列リテラル・`pass` だけを許す。再束縛、重複 import、star import、動的な名前空間操作（`__dict__` や frame の名前空間へのアクセスを含む）、解析失敗、モジュールの `__getattr__` があれば証明しない。絶対 import、別名、namespace package、クラス本体、型位置、lambda、内包表記は保守的な判定に残す。別ファイルからの任意の実行時差し替え、独自 import hook、文字列による monkey patch はこの静的な証明の対象外。package と定義元の証拠は1回の解析内で再利用し、次の問い合わせには持ち越さない。変更シンボルと無関係な import 名は解決しない。

### Python の省略可能引数の削除

無装飾で一意なモジュール直下関数から、省略可能なキーワード専用引数、または末尾の既定値付き位置引数だけを削除した変更は、`api.mod_closed` として扱える。残存引数の順序・位置/keyword-only 区分・型注釈・既定値と、残りのヘッダを保持する。変更後の全観測参照が、新しい引数契約に適合する直接呼び出し・宣言・宣言元へ解決した別名なしの absolute import であることと、直接呼び出しが1件以上あることを求める。呼び出しの行が差分に含まれる必要はない。hook の `impacts` は blocking にせず、通常の impact と refs の出現一覧は保持する。

別名・module 輸送・再 export・関数値・曖昧な束縛・動的名前空間・非 ASCII 識別子・可変長/位置専用引数・解析失敗は保留する。参照走査と同じ ignore/hidden 範囲を監査し、他言語/未対応ソース・リンク/junction・読取/走査失敗、512ソース・1件2MiB・合計8MiB・100,000ノード/entry・128階層の上限でも証明しない。staged review は index snapshot で確かめる。解析したリポジトリ内の呼び出しについての証拠であり、外部利用者や任意の runtime reflection は保証しない。

### Python の同名直下関数と属性代入

参照ファイルの直下で直接定義された、無装飾で一意な通常関数も、宣言後の裸の参照の所有元として証明する。再束縛、重複・条件付き定義、decorator、上記の名前空間の障壁があれば保留する。

変更対象が旧新とも通常のモジュール直下関数と証明できる場合は、属性への単純代入の左辺の属性名を caller から外す。属性呼び出し、関数値、複合代入、右辺の実参照、method/property の変更は従来の扱いを維持する。参照を作業ツリーから読む `context --staged`・`impact --staged` ではこの2種の証拠を適用しない。`review --staged` は index のスナップショット内で適用できる。`refs` の出現一覧と dead-code の件数は変えない。

## impact - 未解決の影響検出（Stop hook 用）

`context` の結果から、diff に含まれないファイルへの影響を「未解決」と判定する。AI エージェントの Stop hook で使用し、未対応の影響先があればブロックして続行を促す。`impact` が追うのは変更後のツリーに残っているシンボルだけで、削除したシンボルは検出しない（呼び出しが残っていても exit 0）。削除まで止めたい場合は、公開 API の削除を `api.rm` として検出する `review --dir . --git --hook` を使う。

シグネチャ変更の判定は `context` と同じく識別子境界での一致を使うため、テストヘルパーや派生名の変更が基底名の関数変更として波及しない。関数内で宣言されたローカルシンボルはファイル間影響の起点から除外する。TypeScript/JavaScript、Rust、Python、Go、Java、Kotlin に対応し、Kotlin のネスト関数もトップレベルの同名関数と区別する。

対象は、inline object 型の引数を一つ持つ TypeScript/TSX の一意な root 関数。旧新とも optional なプロパティの型変更だけなら、個々の JSX caller を情報提供に分ける。タグは直接の識別子で、変更した prop をすべて省略する必要がある。`undefined` を含む prop の指定、JSX spread、型引数、開始・終了タグの不一致、変更した `children` への暗黙の子要素は証明しない。対応するタグ間に byte があれば、空白・タブ・改行だけでも children として扱う。既存の低確信度の判定を優先する。

宣言側はヘッダ・pattern・明示した戻り値型が不変で、引数全体にも pattern 内にも既定値がないことを条件とする。prop 型と明示した戻り値型は、primitive・リテラル・静的な inline object に限る。戻り値型が無注釈なら、外側の全 return が直接 JSX か string・number・boolean・null のリテラルを返す必要がある。式の括弧は除いて判定する。本体末尾はトップレベルの return で終わり、旧新の戻り値カテゴリ集合も一致しなければならない。入れ子の関数・class は走査しない。ファイルに JSX pragma コメントがあれば、この無注釈の証明を使わない。名前付きの型など未知の形式、required な変更、overload、トップレベル prop の追加・削除は対象外。証拠は変更元ファイルと関数名ごとに保持する。その他の本体の意味は検証せず、全体の `api.mod` も降格しない。Rust は識別子を NFC で比較するため、別表記との同一性を否定できない非 ASCII の import・ブロック item の束縛は未証明のまま残す。`refs` と dead-code の出現一覧は変えない。

Rust ではファイル直下の private 宣言を別ファイルの子モジュールから使えるため、その参照を影響先に含める。外部の子モジュールを持たない葉モジュールでは、ソースの別ファイルへの展開がないことを確かめ、無関係なファイルの同名参照を除外する。子モジュールがある場合も、旧新の可視性と通常のモジュールパス・ソース配置を証明できれば、宣言モジュールの部分木の外にある参照を除外する。モジュール配置やソース生成マクロが曖昧なら候補を残す。`impl` 内の private メソッドは別の規則で扱う。旧新で同一所有型の一意な private inherent メソッドと証明できた場合、子孫モジュールからのメソッド呼び出しを影響先に含め、証明できる兄弟モジュールの同名参照は除外する。素の関数呼び出しと import は対象外。参照側に同名宣言がある場合、明示所有型が異なる場合、所有型への言及がなく型を推定できない場合、部分木を証明できない場合は低確信度に分ける。`--staged` の未証明参照も低確信度とする。

Rust ソースの変更がファイル直下の private 宣言への `pub(crate)` または `pub` の追加だけなら、旧 private 範囲内の未変更参照を情報提供に分ける。範囲外の参照は旧宣言を指せなかったため除外する。通常の Cargo モジュール配置が変わらず、package 内に glob import がない場合に限る。可視性を広げると glob 経由で同名の衝突が起こり得るため、glob がある場合や所有元が曖昧な場合、同じ diff に別の変更がある場合は従来の blocking 判定を保つ。

Rust の型と自由関数の参照では、通常の Cargo モジュール配置に限り、名前付き import・`pub use` の連鎖・glob import をたどって宣言元を確認する。参照が別ファイルの同名宣言を指し、解決に使った束縛が差分の前後で変わらない場合だけ影響候補から除外する。別名 import が利用・公開されるか、子モジュールからの利用があり得る場合は、元の名前が現れる import 行を blocking に残す。標準 crate または registry 依存と証明できる別名 import は情報提供に保つ。registry 依存と見なすのは、Cargo がそのパッケージで読む設定 (パッケージの祖先の `.cargo/config(.toml)` と、プロジェクトの置き場所に関係なく読む `$CARGO_HOME/config(.toml)`) と環境変数に、依存の取得元 (`[patch]`・`[source]`・`paths`)・コンパイラ (`rustc`・wrapper・`[env]` で rustc に渡す差し替え)・`--extern` を変えうるものが無く、どれも読めて解析できる場合に限る。`cargo --config` で渡す設定と、パッケージの根より下のディレクトリで cargo を動かしたときにだけ読む設定は見えない。その他の外部 import、条件付き束縛、曖昧な glob、束縛変更、特殊なソース配置、ステージ済みの解析では従来の保守的な判定を維持する。`refs` と dead-code の出力は変えない。

Cargo の依存で変更元の package に届かない別 package の同名参照も、到達性を証明できた場合だけ除外する。この走査では、修飾なしの標準の式マクロと組み込みの derive・属性を許す。ワークスペース全体のマクロ定義・import と、`std` / `core` / `alloc` の名前を差し替える宣言・依存・library 名を調べ、使った標準名を上書きし得る場合は証明しない。依存は `dev-dependencies` / `build-dependencies` と従来の `dev_dependencies` / `build_dependencies` を、target 別の表も含めて同じ条件で調べる。マクロの引数内は token tree なので、そこに `use`・`extern`・マクロ定義・属性があれば保留する。glob は標準クレート起点のものと、`crate` / `self` / `super` だけを通るものに限る。テスト内の `use super::*` は許すが、`use crate::facade::*` は `facade` が外部クレートの別名を何段でも経由し得るため保留する。

未知のマクロ・独自の derive・属性マクロ・修飾したマクロ呼び出し（`std::println!` も含む）は保留する。未知マクロの定義元クレートと依存の解決は行わない。`include!`・`path`、独自のビルド構成、ソースの読み取り・解析失敗、LFS のポインタなどの既存の障壁も維持する。この到達性の検査は削除した Rust 自由関数の API 判定と共有し、`refs` と dead-code の出現一覧を変えない。

```bash
# git diff を自動取得して未解決影響を検出（推奨）
astro-sight impact --dir . --git

# ステージ済み変更を検査
astro-sight impact --dir . --git --staged

# カスタムベースを指定
astro-sight impact --dir . --git --base HEAD~3

# stdin からパイプ
git diff HEAD~1 | astro-sight impact --dir .
```

- 未解決なし → exit 0（出力なし）
- 未解決あり → stderr にテキスト出力 + exit 1
- `--dir` が git 管理外 → exit 0（出力なし。`--hook` の有無は問わない。下記「git 管理外ディレクトリでのスキップ」参照）
- `--base` を省略していて、まだコミットが無い（HEAD が unborn）→ exit 0（出力なし。同じく下記参照）

出力例（exit 1 時）:
```text
Unresolved impacts found:

src/engine/symbols/mod.rs changed [extract_symbols]:
  → src/service.rs:284 [extract_symbols]
  → src/commands/api_changes/exported.rs:45 [extract_symbols]
```

テキスト出力の行番号は、エディタでそのまま開けるよう 1 始まりで表示する（JSON 出力の `line` / `ln` は 0 始まり）。

claw-hooks との連携例（グローバル設定の `~/.config/claw-hooks/config.toml` に書く。プロジェクトの `.claw-hooks.toml` に書いた `stop_hooks` は claw-hooks が無視する）:
```toml
[[stop_hooks]]
commands = ["astro-sight impact --git --dir ."]
condition = { command_exists = "astro-sight" }
```

検出時の exit 1 は、Claude Code の hooks 設定に直接登録すると非ブロッキングなエラーとして扱われ、Claude は止まらない。上の例のように `condition` を付けた `stop_hooks` では、コマンドの失敗を claw-hooks が block として Claude に返すので止まる。

### git 管理外ディレクトリでのスキップ

`--git` を受け付けるコマンド（`context` / `impact` / `review` / `dead-code` / `cochange`）を git 管理外ディレクトリで実行した場合は、内部の `git diff` の失敗をエラーにしない。「解析対象なし」としてスキップし、**exit 0** で正常終了する。`~/.config` のような git 管理外ディレクトリで編集しているときに、Claude Code の Stop hook をブロックしないための挙動である。

- `--hook`（`review` / `impact`）→ stdout / stderr ともに無出力で exit 0
- 通常 CLI → 空の正常結果に機械可読な `skipped` フィールドを付けて exit 0。「差分なし」と「git 管理外」を区別できる（構造化出力を持たない `impact` は無出力）

```json
{ "...": "...", "skipped": { "reason": "not_git_repository", "source": "git", "message": "--git was requested but --dir is not inside a git worktree" } }
```

判定には `git rev-parse --is-inside-work-tree`（`LC_ALL=C`）を使うので、worktree / submodule / bare repo でも正しく判定できる。**真のエラー**（`--base` 不正・base がコミットに解決できない・git 実行不能・壊れた repo・権限不足）は `exit 1` を返す。

コミットが 1 つも無いリポジトリでも、`--base` を省略した場合は同じくスキップする。`git init` の直後や `git checkout --orphan` の直後は HEAD がまだコミットを指しておらず、比較の基点が無いためである。`skipped.reason` は `unborn_head` になる。`--base` を明示した場合はスキップせず、その base との差分を解析する。base がコミットに解決できなければ、`--base HEAD` でも従来どおり exit 1 を返す。`cochange` で `--paths` / `--paths-file` を明示した場合は、git 管理外のときと同じく、明示したファイルだけで続行する。HEAD が unborn かどうかは `git show-ref --exists`（git 2.43 以降）で確かめるので、それより前の git ではスキップせず exit 1 のままになる。`--diff` / `--diff-file` / stdin で diff を渡す経路は、この判定を通らない。

`review --git` は解析中の入力変化を検出すると、最大 3 回まで試行する。対象は index・作業ツリーの変化と、`git diff` がファイルの取り込み中に返す short read。各試行で差分取得からやり直し、base をコミット ID に解決する。その ID を試行内の差分・cochange・API 解析で共有するため、HEAD の移動だけでは試行を無効にしない。失敗した結果と一時 snapshot は破棄する。

3 回とも入力が定まらなければ、通常の review と `--hook` は `REVIEW_INPUT_CHANGED` を返し、**exit 2** で終了する。stdout には JSON エラーを 1 件出力し、`--hook` では stderr に再実行を促す短い案内も出す。他のエラーと hook の blocking 指摘は exit 1 を維持する。明示した `--diff`・`--diff-file`・stdin の入力は再試行しない。

`--staged` は index の内容を一時ディレクトリに書き出し、影響・API 差分・dead-code をすべてその内容で解析する。作業ツリーにある未ステージの編集は結果に入らない (大きなリポジトリの扱いは「staged review の書き出しの上限」を参照)。

### 未追跡ファイルの取り込み上限

`--git`（非 `--staged`）は未追跡のソースファイルを「新規ファイル」として解析対象に含める（同一作業で作った未追跡ファイルへの参照が「diff 外の未解決影響」と誤報されるのを防ぐため）。ただし **1 ファイル 256KB または 5,000 行を超える未追跡ファイルは対象外**にする。上限を設けるのは、コード生成器の出力や巨大 fixture のような生成物を取り込むと、その全 exported symbol が API 差分の候補になるため。そうなると `review` に数十分かかり、Stop hook がタイムアウトする。実測では、未追跡ファイルがなければ 1.75 秒で終わる `review` が、`pub fn` を計 22,000 個持つ未追跡ファイルを置いただけで 10 分を超えても終わらなかった。

追跡済み（tracked）のファイルには、この上限を適用しない。commit / add 済みのファイルは、意図的にレビュー対象に入れたものと見なせる。一方、未追跡のファイルは「まだ add していない」ものなので、コミット対象かどうかが分からず、巨大なら生成物の可能性が高い。

対象外にしたファイルは黙って落とさず `truncations` に出力する（「レビュー済み」と誤読させないため）:

```json
{ "...": "...", "truncations": [{ "path": "generated.rs", "reason": "untracked_file_too_large", "message": "untracked file excluded from --git analysis: lines 80000 exceeds limit 5000" }] }
```

`--hook` では `trunc: [{"f": "generated.rs", "r": "untracked_file_too_large"}]` として出力する（検出ではなく解析範囲の申告なので exit 1 にはしない）。`impact` は構造化 JSON を持たないため stderr の `note:` 行で出す。`--staged` / `--diff` / `--diff-file` は明示された範囲を尊重するため未追跡の取り込み自体を行わない。

### staged review の書き出しの上限

`review --git --staged` は、index にある通常ファイルをすべて一時ディレクトリに書き出してから解析する。ファイル数が 50,000、または合計が 512 MiB を超えるリポジトリでは書き出さず、作業ツリーのファイルを読む経路に切り替える (変更したファイルだけは index の内容を読む)。エラーにはせず、切り替えたことを `truncations` で申告する:

```json
{ "...": "...", "truncations": [{ "reason": "staged_snapshot_unavailable", "message": "staged snapshot was not created: 61234 files / 734003200 bytes exceed the limit (50000 files / 536870912 bytes); the working tree matched the index, so files were read from the working tree" }] }
```

切り替えた経路では、作業ツリーに未ステージの編集があると、参照の検索結果はコミットされる内容と一致しない。そこで、作業ツリーが index と違うときは、参照が無いことを根拠に非 blocking へ分ける判定 (`mod_closed` / `mod_compat` / `rm_dead`) を使わず、blocking のまま残す。`--hook` では `trunc: [{"r": "staged_snapshot_unavailable"}]` として出る (解析範囲の申告なので、これだけでは exit 1 にしない)。

### 解析できないソースの申告

`dead-code` / `review` は、ディレクトリ内に存在するが**どのバックエンドでも解析できなかったソースファイル**も同じ `truncations` に出す。読めないファイル内の参照を数えないまま dead と断定すると、生きているシンボルを dead と報告してしまう（`.vue` の `<script>` からしか使われていない TypeScript 関数など）。そこで、「参照がない」のか「観測できなかった」のかを利用者が区別できるようにしている。

```json
{ "...": "...", "truncations": [{ "reason": "unanalyzable_source", "message": "1 \".vue\" file(s) were not analyzed (no parser for this language); references inside them are not counted (e.g. src/App.vue)" }] }
```

対象は**プログラム / テンプレート言語だと確実に言える拡張子**に限る（`.vue` / `.svelte` / `.astro` / `.erb` / `.razor` / `.scala` / `.dart` / `.lua` など）。走査対象外のファイルには画像・アーカイブ・データも含まれるため、全件を申告すると本当に見落としているソースがノイズに埋もれる。出力は拡張子単位に 1 件へ畳み、拡張子 10 種 / 代表パス 3 件を上限とする。該当ファイルがなければ `truncations` 自体を出力しない。

`dead-code` は参照を数えた範囲（ディレクトリ全体）の解析できないソースを申告する。`--glob` や `--git` で dead の候補を絞っても参照はディレクトリ全体から数えるので、申告の範囲も狭めない。

### 既定の除外

`context` / `impact` / `review` の影響分析は、ファイル間の参照検索でサードパーティ依存と build artifact を既定で除外する。`new` / `save` / `find` / `update` などの汎用メソッド名がサードパーティや生成コードから大量に流入し、影響先を万件単位の偽陽性で埋めるのを防ぐ。

- vendor / package manager: `vendor`, `node_modules`, `bower_components`, `.venv`, `venv`, `.tox`, `Pods`, `Carthage`
- build artifact: `target`, `build`, `dist`, `out`, `.build`, `DerivedData`, `bin`, `obj`, `coverage`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `CMakeFiles`
  - `bin` のうち Cargo パッケージの `src/bin/`（直上が `src` で、その親に `Cargo.toml` がある）はバイナリターゲットのソースなので除外しない。`--exclude-dir bin` を明示した場合はすべての `bin` を除外する

この既定の除外を解除する場合:

```bash
ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1 astro-sight impact --dir . --git
```

`.gitignore` と隠しファイルの除外、および生成ファイルの判定（`refs::collect_files` 経由）は、この既定の除外とは別の仕組みで動く。このうち生成ファイルの除外だけは、`--include-generated` または `skip_generated = false` で解除できる。

### ユーザー指定の追加除外

固定リストに含まれない名前のディレクトリ（`pjproject-2.15`, `openssl_64_1.1.1c`, `third_party` など）を除外したい場合や、より細かい glob パターンで impact 解析の対象を絞りたい場合は、`--exclude-dir` / `--exclude-glob` を使う。`context` / `impact` / `review` で同じオプションが利用でき、固定リストに**追加**される（既定の除外を上書きするものではない）。

```bash
# vendored C library を除外
astro-sight impact --dir . --git \
  --exclude-dir pjproject-2.15 \
  --exclude-dir openssl_64_1.1.1c

# glob で複数バージョンをまとめて
astro-sight impact --dir . --git \
  --exclude-glob '**/openssl_*1.1.1*/**'

# review でも同じオプションが impact + dead_symbols 両方に作用
astro-sight review --dir . --git \
  --exclude-dir pjproject-2.15 \
  --exclude-glob '**/openssl_*/**'
```

`--exclude-glob` は `ignore::overrides` の negative pattern として扱う（先頭の `!` は不要、ワークスペース相対）。不正な glob 構文は実行前に `INVALID_REQUEST` で弾く。

## review - 構造化 diff レビュー

`review --hook` は、同じレビューの `test_only_symbols` にファイル・名前・種別が一致し、宣言行も分かる `api.mod` と `api.const_value` を停止対象から外す。`--strict-public-const-values` 指定時も同じで、API の表示と通常出力の分類は維持する。レビューまたは impact に解析範囲の欠落があれば、この根拠は使わない。本体の利用・再 export がある記号、無参照の export、別ファイルの同名記号はテスト専用として扱わず、同じ一覧に本体用の変更があれば停止する。削除・callback・型注釈の policy は変更しない。これは観測したリポジトリ内の参照に基づく停止方針であり、外部の利用者がいないという保証ではない。

`context` の影響分析に加えて、`cochange` による変更漏れ候補、公開 API 差分、死蔵シンボルを 1 回の実行でまとめて返す。PR レビューや pre-merge チェック向け。

### TypeScript の名前空間と同名の宣言

別の名前空間が、ファイル直下で直接exportされた関数・interface・type alias・単純な変数と同名の項目をexportしている場合、TS/TSXのAPI比較はAST上の位置で直下宣言の署名変更を回復する。旧新が同じTS/TSX文法で解析でき、直下の束縛が一意と確認できる場合に限る。新しい直下署名が旧名前空間の署名と一致するときも、変更された重複名を参照検索へ残す。名前空間だけの変更を直下APIの変更にはしない。

回復した候補には既存の互換性・引数個数・型メンバー検査を適用する。必須引数を足していない呼出しは、その行を整形していてもblockingに残す。既存の複数定義ガードにより、変数や型の利用箇所を変更してもblockingに残る場合がある。overload・宣言merge・再束縛・分割代入の変数・間接/default export・解析失敗は、この回復の対象外。他言語とrefs/dead-codeの出現契約は変えない。

### JavaScript / TypeScript のリテラル定数値変更

型注釈がなく、それ以外の宣言が不変の公開 `const` では、正規表現リテラル同士の変更も `api_changes.const_value_changes`（hook では `api.const_value`）に分類する。pattern・flags が変わっても種類は `RegExp` として比較する。置換を含まない template は、空の template や escape を含むものも既存の文字列リテラルと同じ種類にし、文字列との相互変更にもこの値変更ポリシーを適用する。JS / TS / TSX と staged review で同じ扱いになる。

既定の hook は情報提供にし、`--strict-public-const-values` 指定時は blocking にする。`g`・`y` などの flags は正規表現の使い方を変えうるため、動作が同じという証明ではない。種類の変更、置換あり・タグ付きの template、assertion、`satisfies` は従来の判定を維持する。宣言ヘッダの検査と、静的 object / array の scalar 葉の判定も変えない。

### Rust の内部モジュールの定数値変更

Rust の `pub const` / 非 `mut` の `pub static` は、同一パスの通常変更で名前・型・可視性・binding kind が変わらず、初期値だけが変わった場合、既存の `api_changes.const_value_changes`（hook では `api.const_value`）へ分類する。Cargo のbin-only crateやlibraryのprivate moduleにある場合も、外部API面から除く処理より先にこの値変更の情報を保持する。既定のhookでは、この分類だけを理由に未更新参照で停止しない。`--strict-public-const-values` を付けると同じ分類をblockingとして扱う。

これは値変更のポリシー分類であり、互換性の証明や外部への公開の証明ではない。`added` / `removed` / `modified` の公開面の判定は維持し、型・可視性・`static mut`・曖昧な対応はこの補正で降格しない。移動やrenameは従来の検査経路を使う。通常出力とstagedのhookにも同じ分類を使い、`refs`の出現契約は変えない。

### Rust の自由関数削除とパス付きの同名参照

未使用のモジュール自由関数を消したとき、残った同名の出現が別の項目を指すと証明できれば `removed_dead`（hook では `api.rm_dead`）へ分類する。同じモジュールが直接宣言する型の `Engine::run()`、`crate::engine::run()` などのモジュールパス、別の自由関数を同じモジュールで呼ぶ素の `run()` が対象になる。消した関数を指す `crate::launcher::run()` や、解決できない `use` が残れば blocking の `removed`（`api.rm`）を保つ。`refs` と dead-code の出現一覧は変えない。

ファイルをまたぐ証拠は、解析 root 内で発見した Cargo manifest と、workspace・member・path 依存の閉包を使う。任意の拡張子の明示 target、build script、標準の library/bin/test/example/bench root を列挙し、明示した `mod` 宣言ごとに親と子の探索ディレクトリを保持する。同じファイルに複数の所属があれば、すべてが同じ参照先を証明する場合だけ分類する。未知の所属があれば証明せず、条件付き・曖昧な mod の辺はすべて監査し、都合のよい一方を選ばない。

`beta::items::run()`・`::beta::items::run()` などの外部パスも、通常の非 optional なローカル path 依存の残存する直接関数を証明できる。依存の改名、lib 名、workspace 継承、明示 lib root を同じ Cargo 入力閉包で確認し、すべての所属で結果が一致する必要がある。パスは入力監査で記録した package の実体との対応を使い、大文字小文字を区別するディレクトリ名を一律に変換しない。型の名前空間に非 ASCII の宣言があれば、NFC 同値のローカル名を否定できないため依存への fallback を保留するが、既存の file-local 証明と明示した `::` パスは保つ。build script の所属、条件付き inner cfg を持つ依存 root、依存先の競合、再公開、不明な名前の隠蔽や生成 extern alias、未対応の依存フィールド、コンパイラ・取得元の差し替えがあれば blocking を保つ。この証明では registry/git 依存を解決せず、曖昧な移動の対応付けも変えない。

到達したソースの取り込み（`#[path]`・`include!`）、配置変更を否定できない宣言マクロのテンプレート、解析 root 自身や配下の symlink、読取・解析失敗、資源上限の超過があればファイル間の証拠を使わない。モジュール位置の未展開マクロは、その子の探索ディレクトリ以下を ignore なしで再帰的に監査する。祖先ディレクトリの alias は実体パスへ正規化して配置グラフを作るが、Cargo 入力は論理・実体の両祖先を監査する。Cargo の実行場所を解析 root として、root・祖先・有効な `CARGO_HOME` の config を読み、ソース差し替えや未対応の config include があれば証明しない。入力閉包外の無視された独立 package、解析 root 外の独立 crate、別の実行場所でだけ使う config、未展開 proc macro や解析入力外のマクロ定義が生成する配置変更は保証対象外とする。ファイル内で完結する証拠は、この配置証明に依存しない。

### Python の受け手が未解決なメンバー削除

`api_changes.removed_unresolved`（hook では `api.rm_unresolved`）は、削除が確定していても、残った属性参照の受け手を特定できず、使用位置の型要求だけが分かる候補を報告する。`confidence: "low"` の情報提供として扱い、hook を止めない。削除メンバーへの参照の有無は未確定で、`removed_dead` の証明条件は変えない。シェルの解析エラーで削除自体を確定できない `uncertain_removals` / `rm_unverified` も別区分のまま維持する。

対象はデコレータのない通常のメソッドで、旧側に同名メンバーの再束縛や属性書き込みの手掛かりがなく、旧パスに所有クラスが残るものに限る。静的な参照入力が完全で、別クラスの同名フィールド宣言が少なくとも1件あることも必要になる。全出現が、既存の別フィールドへの帰属証明、または対応する型要求の位置での未解決な属性読み取りでなければならない。フィールド宣言は名前の衝突を説明する根拠であり、受け手の型の証明には使わない。別クラスのフィールド読み取りには、削除元の所有クラスについて、継承・別名化などを通じた利用がないと確認する既存の条件も適用する。対応付けが曖昧な移動は対象外とする。

型要求として扱うのは、単項 `+`・`-`・`~`、相手が整数・浮動小数点リテラルの算術・ビット演算（`%` を除く）と順序比較、再束縛のない `int`・`float`・`abs`・`round`・`range`・`len` への単一引数である。`sum` は生成式またはリスト内包1個を受け取り、start が無いか数値リテラルの場合に限る。値の伝播は括弧、条件式の分岐、`for` が1個の内包の本体だけを扱う。すべての外側の生成式は、上限付きの括弧だけを挟んで、再束縛のない`sum`へ直接渡されなければならない。外へ渡る生成式はblockingに残し、その場で評価されるリスト内包とは区別する。

受け手も注釈のない通常の引数と、その内包の変数に限定する。内包のiterableは、上限を設けて括弧を除いた後、注釈・再束縛のない引数か、呼び出しを含まず、再束縛のない関数引数を根とする属性連鎖でなければならない。属性連鎖の根となる引数に既定値がある場合は保留する。型付きの根は通常のクラスへ解決でき、最初のフィールドが注釈・再束縛のない`__init__`引数から一度だけ代入されている場合に限る。フィールドの明示型、型付きconstructor引数、装飾付きメンバー、不明な根の型は保留する。型付きiterable、局所の別名、明示的なconstructorやfactoryの結果は、既存のフィールド証明で読み取りを解決できる場合を除きblockingに残す。

呼び出し、所有クラスやインスタンスが分かる受け手、`self` / `cls`、外へ渡る bound method、property、staticmethod、classmethod は blocking に残す。真偽値・等値比較・文字列化、不明な消費先、`any`・`sorted`・`max`、対応する消費先のないコレクションも同様に扱う。外側に `try` / `with` がある場合、名前の再束縛、`builtins` へのアクセスの手掛かり、解析失敗、不完全な入力がある場合も、この区分へ移さない。単一・バッチの `refs` 出力は変えない。

この推論には、旧コードでその演算を評価した際、メソッド値による `TypeError` を意図して利用していなかったという前提がある。`sum`には通常の数値加算という前提も置き、旧候補ファイルか現在の解析範囲に`__add__`・`__radd__`の手掛かりがあれば`sum_element`へ分類しない。外部の独自数値型は判定できない。実行時の健全性は検証しない。関数をまたぐ例外捕捉、外部の利用者、観測した静的入力の外で行われる builtins やクラスの書き換えは未解決のままである。そのため、安全と断定せず、確認に使う型要求と位置を出力する。

```json
{"name":"Board.slot","kind":"method","file":"shop/board.py","refs_internal":0,"reason":"unresolved_receiver_type_demand","confidence":"low","references":[{"file":"shop/report.py","line":3,"column":20,"demand":"sum_element"}],"field_owners":[{"file":"shop/board.py","class":"Item","line":4,"column":4}]}
```

参照とフィールド宣言の位置は0始まり。hook では `n`・`f`・`r`、`refs: [{f,l,c,d}]`、`field_owners: [{f,n,l,c}]` を使い、`confidence: "low"` も残す。staged snapshot を作れず、作業ツリーが index と異なる場合は、ほかの参照に基づく降格とともに blocking の `removed` へ戻す。

`--git --base <rev>` を指定した場合、`missing_cochanges` の blame 解析にも同じ base を使う。複数コミット分の PR をまとめてレビューするときも、diff と共変更候補の解析範囲が揃う。

`missing_cochanges` は、共変更が 3 回以上あるペアだけを候補にする（`--cochange-min-samples`、既定 3）。変更行 blame では証拠コミットが 2 件だけの起点がよく現れ、「1 回だけ一緒に変わった」ペアが confidence 1.0 として上位に並ぶため。探索的に小標本まで見たい場合は `--cochange-min-samples 2` を指定する（単体の `cochange` コマンドは既定 2 のまま）。候補の重複排除と上位 10 件の選択には、単体コマンドと同じ平滑化済みの `score` を使う。3/3 の小標本が 30/40 のような十分な標本より機械的に上位へ来るのを防ぐためで、raw confidence は証拠の表示と閾値判定に使う。

ロックファイルと、ソースに対応する依存宣言ファイル（`Cargo.toml` / `package.json` / `pyproject.toml` など）は `missing_cochanges` の候補にしない。依存を追加するコミットでは、これらとソースが必ず一緒に変わるので、履歴相関は 100% になる。しかしその相関は「依存を追加したとき」に限ったもので、import を 1 行も増減させない本体変更とは因果関係がない。依存宣言ファイルを候補から外すのは、ソースと同じエコシステムで、そのソースから見て最も近いものとの組だけに限る（`Cargo.toml` と Python スクリプトのような別エコシステムの組は候補に残る）。単体の `cochange` コマンドは、「過去に一緒に変更された」事実として依存宣言ファイルを出し続ける（ロックファイルは生成物なので両方で除外）。

**外部 snapshot と生成元テストの関係は方向付きで扱う。** snapshot を更新しただけの差分に対して、「生成元テストも変更漏れでは」とは出さない。snapshot は被テスト対象の出力が変わったときにも更新されるので、「テストを変えたら snapshot も変わる」という期待は成り立っても、その逆は成り立たないためである。逆方向は残す。テストを変更したのに snapshot が欠けている場合は、候補に出る。抑制するのは次をすべて満たすペアだけで、1 つでも確認できなければ候補に残す:

- snapshot の直上ディレクトリが正確に `__snapshots__` で、ファイル名末尾の `.snap` を 1 回だけ除いたパスが欠落候補と完全一致する（Jest / Vitest / Bun が共有する標準規約。`tests/__snapshots__/widget.test.tsx.snap` → `tests/widget.test.tsx`）
- 生成元テストが実在する通常ファイルである
- snapshot の**先頭行が既知のランナーヘッダと完全一致**する（`// Vitest Snapshot v1, …` など）。パス規約だけでは手書き fixture や別用途の `.snap` を巻き込むため、生成出力であることをファイル自身で確認する

`.gitattributes` の `linguist-generated` とは判定経路が独立している（あちらは「生成物一般」の宣言で、指定すると両方向とも候補から消える）。カスタム snapshot resolver、inline snapshot、`.snap` 以外の形式は対象外で、いずれも履歴相関を情報として出す。グローバルの `--include-generated` を付けるとこの方向付けも無効化する。単体の `cochange` コマンドは探索的な用途なので方向付けしない。

なお、これは「テスト変更が不要だと証明した」ものではない（期待値だけ更新して必要なテストロジックの変更を忘れることはある）。標準の生成関係にあるペアについて、履歴相関だけを根拠に逆方向の変更を要求しないという推薦方針。

`api_changes.compatible_modified` には、シグネチャ文字列は変わるが既存の呼び出しとの互換性を保つ変更を出力する。次の変更は informational として扱い、`--hook` の blocking 対象にしない。

- React component の HOC ラップ
- Rust の一意な root 関数のヘッダを、非 generic・制限なしの `pub type` で書き直す変更と、その別名を展開した型へ戻す変更（`transparent_rust_type_alias`）。旧新それぞれのファイルの別名と import 環境を確かめる。
- 未参照の object member の削除
- TS/TSX のトップレベル関数の末尾への optional / default 引数の追加（`trailing_optional_params`）
- TypeScript の inline object 引数型への optional プロパティ追加。他の枝が変わらない union の、一意な object 枝も含む（`optional_object_props`）
- Python のトップレベル関数 / モジュール直下のクラスメソッドの末尾への、kwonly+default 引数または positional default 引数の追加（`trailing_optional_params`）。デコレータの差分がある場合や同名関数が複数定義されている場合は、保守的に blocking を維持する
- Rust の関数・inherent impl のメソッドへの `const` の追加（`rust_const_fn_added`）。const の文脈の外からの呼び出しは変わらないので、呼び出し元は `impact_info` に残す。違ってよいのは `const` だけで、ほかの宣言・属性・外側のモジュールや `impl` のヘッダ・ファイルの import と、宣言に現れる名前の同じファイルの宣言（型別名・`use` の別名・配列長の定数など。それらが使う名前もたどる）は変わらないことが条件。戻り値が `impl Trait` の関数は、隠れた型と外に漏れる auto trait が本体で決まるので、本体と、その本体が参照する同じファイルの宣言も変わらないことが条件。マクロの token tree は原文を保ち、空の impl を含む impl ヘッダと、入れ子の `impl`・マクロ呼び出しは囲む宣言と属性も含めて比べ、そこに現れる名前も依存先として調べる。参照先が旧新とも一意な関数なら、既存のヘッダ正規化の下で const を足しただけで本体のトークンが不変な変更も許す。関数自身の `const` だけは除くので、関数内の未変更の `assert!` は証明を妨げない。位置依存または未証明のマクロ呼び出しは、行と Unicode 文字の列も不変であることを求める。別名と入れ子のマクロトークンも保留する。単純な assert と、入れ子のマクロ・属性・呼び出しを含まない一意な先行 root マクロ定義だけは位置を問わないが、同一ファイル内のマクロ束縛が不明ならこの例外は使わない。ヘッダのマクロは未対応のまま保守的に残す。`const` を外す変更や、引数・戻り値・ジェネリクス・`unsafe`・`async`・`extern`・公開範囲の同時の変更は blocking を維持する。同じファイルに同名の宣言（別の `impl` のメソッド、`cfg` で分けた関数）があると呼び出し元を区別できないので、`impacts` は残る

Rust の root 自由関数・定数を移すのと同時に互換な宣言変更をした場合は、`moved` と `const_value_changes`（初期化式の変更）、または `compatible_modified`（透過的な型別名・自由関数への const 追加）を報告できる。const 追加は #113 の宣言依存の完全な比較に加え、別ファイルでの型の名前解決も確かめる。`impl Trait` 戻り値と rename 差分の const 追加は証明しない。名前・種別・宣言・移動先が一意であることを求める。追加証明は、edition を明示した通常の Cargo モジュールと非 optional の直接 path dependency（依存キーの改名を含む）を扱い、全参照が移動先を指す直接パスか移動先ファイル内の素の名前であることを確かめる。旧パスの残存、import・再公開の別名、曖昧な所有元、可視性縮小、非互換変更は従来の削除判定に残す。初期化式の値は評価せず、`const_value` は宣言した型が変わらないことを表す。

移動の型の証明では primitive 名も束縛を照合し、raw 識別子は索引で正規化する。glob・未知属性が primitive 名を隠す可能性も保留する。移動の証明では、std / core / alloc 起点の import も、旧新の両ファイルで先頭名が束縛されていない場合だけ型の証拠にする。依存名の証明では raw identifier を正規化し、参照ファイルに非 ASCII 識別子があれば NFC 同値を否定できないため証明しない。旧新のソース構成を一度ずつ調べ、未知のマクロ・属性、derive、外部ソース取り込み、symlink、build script、Cargo 設定、custom target、参照入力の不完全性があれば証明しない。上限は各側の列挙 10,000 entries と、旧新の Rust 合計 512 ファイル / 8 MiB。生成コードや解析 root 外のコンパイル構成は解決しない。従来の厳密なシグネチャ一致の移動、移動した型のメンバー変更、`refs` と dead-code の出現契約は変えない。

同じシンボルに紐づく `impacts` も破壊的影響としては出さず、`mod_compat` の情報提供だけに留める。`rust_const_fn_added` の `impacts` は影響分析の判定に従い、同じファイルに同名の宣言があれば残す。未参照 object member の判定では、削除キーを 1 個ずつ全リポジトリで検索せずに Aho-Corasick で一括して事前抽出し、各 JS/TS ファイルを最大 1 回だけ parse する。ファイルの収集・読み込み・parse に失敗したときは互換扱いへ降格せず、blocking のままにする。

`optional_object_props` では、同じ変更で型に足した optional プロパティを、分割代入の末尾にも追加できる。既存の要素は別名・既定値を含め、トークンが同じ順序で残ることが条件。追加できるのは shorthand 束縛と、リテラルの既定値を持つ shorthand 束縛だけである。既定値は文字列・数値・真偽値・null・置換のない template・負数に限る。引数の kind、引数全体の既定値、残りのシグネチャは変えない。

束縛を追加する場合、union の他の枝は `undefined` に限る。既定値は、`string`・`number`・`boolean`・`null`、またはこれらだけの union 型に適合する必要がある。別名型、これら以外のリテラル型、不明な型、BigInt の既定値、旧式の 8 進数、文字列や template の数字 escape は証明しない。

rest・computed key・入れ子・キーや束縛の重複・新しい別名は、この証明の対象外となる。旧型に既にあるキーの束縛追加も扱わない。新しい束縛名が後続引数の既定値や、プロパティ型・戻り値型の `typeof` などに現れる場合も blocking を維持する。direct eval、escape を含む識別子、未対応のメンバー形式は証明しない。型だけを変える従来の分類は維持する。

新しい束縛は、関数本体直下の lexical 宣言・関数宣言と、同じ関数 scope の `var` 束縛に衝突してはならない。未知の宣言形式は blocking を維持する。本体内の通常の参照が持つ意味は、この互換性の証明では検証しない。

`export const` のような値バインディングは宣言全体（初期化子を含む）を比較するが、**値そのものが関数の場合は本体を比較から外す**（`export function` の本体変更が api.mod にならないのと揃える）。本体を比較から外すのは、次の関数の本体に限る。

- 値そのものであるアロー関数 / 関数式（括弧・`as`・`satisfies` 付きも含む）
- React の `memo` / `forwardRef` に包んだ関数
- オブジェクトリテラルのメンバーの関数（メソッド・`key: () => ...`）

引数・型注釈・キーの追加削除は比較する。それ以外の呼び出しに渡すコールバック（`create((set) => ({ ... }))` など）は、中身がストアの形や値そのものを決めるので本体も比較する。分割代入の束縛（`export const { a, b } = obj`）は「その名前へ至る経路 + 初期化子」で比較する（配列は位置を保つ）。同じ分割代入のほかの束縛を追加・削除しても、残った束縛が api.mod にならないようにするためである。default 値・computed key・rest を含むパターンは宣言全体で比較する。

Python の公開型契約を方向付きで分類できる変更には、`api_changes.modified[].contract_change`（hook では `api.mod[].contract`）として `{kind, breaks}` を付ける。対象は `TypedDict` の必須性変更と、モジュール直下の直接的な `Literal` 型エイリアスの値集合の変更である。`Literal` では、値集合の縮小を `literal_values_narrowed`（producer 側が壊れる）、拡大を `literal_values_widened`（consumer 側が壊れる）として報告する。`Literal` は `typing` / `typing_extensions` 由来と証明でき、値が escape / prefix を含まない文字列・10 進整数・真偽値・`None` だけの場合に限る。値の置換、動的な `__all__`、名前の shadow、star import など意味を静的に確定できない場合は方向を推測せず、通常の blocking な `api.mod` に残す。テストファイルは既存の公開 API 面規約どおり検出対象外。型エイリアスの項目は `kind = "type"` の疑似シンボルであり、`symbols` / `refs` / `dead-code` の解析対象には追加しない。

実行時に暗黙呼び出しされるシンボルの除外範囲は API 差分と dead-code で異なる。PHPUnit 規約、TS/JS の constructor、Flyway migration はどちらの公開面からも除外する。一方、Laravel relation や Angular lifecycle hook は dead-code では除外するが、外部公開シグネチャの変更を見逃さないよう API 差分には残す。`--framework` は dead-code 規約の選択であり、この API 差分境界を一律には変更しない。

```bash
# git diff を自動取得してレビュー（推奨）
astro-sight review --dir . --git

# ステージ済み変更をレビュー
astro-sight review --dir . --git --staged

# カスタムベースを指定
astro-sight review --dir . --git --base HEAD~3

# 既に生成済みの patch / PR diff を使う
astro-sight review --dir . --diff-file /tmp/pr.patch
```

出力例:
```json
{
  "impact": { "changes": [...] },
  "missing_cochanges": [
    { "file": "src/service.rs", "expected_with": "src/commands.rs", "confidence": 0.75 }
  ],
  "api_changes": {
    "added": [],
    "removed": [],
    "modified": [
      {
        "name": "greet",
        "kind": "function",
        "file": "src/new.rs",
        "old_signature": "pub fn greet() -> i32 {",
        "new_signature": "pub fn greet(name: &str) -> i32 {"
      }
    ]
  },
  "dead_symbols": []
}
```

## dead-code - デッドコード検出

エクスポートされているが参照されていないシンボルを検出する。diff 指定時は変更関連ファイルのみ、指定なしはプロジェクト全体をスキャン。

```bash
# プロジェクト全体をスキャン
astro-sight dead-code --dir .

# Rust ファイルのみスキャン
astro-sight dead-code --dir . --glob "**/*.rs"

# git diff に関連するファイルのみスキャン
astro-sight dead-code --dir . --git

# ステージ済み変更に関連するファイルのみ
astro-sight dead-code --dir . --git --staged
```

出力例:
```json
{
  "dir": "/path/to/project",
  "scanned_files": 48,
  "dead_symbols": [
    { "name": "unused_helper", "kind": "function", "file": "src/utils.rs", "line": 12 },
    { "name": "OldConfig", "kind": "struct", "file": "src/config.rs", "line": 40 }
  ]
}
```

`line` は宣言行（0 始まり）。テストからしか参照されないシンボルは `dead_symbols` に含めず、`test_only_symbols` に分けて出す。

同名シンボルが複数ファイルに存在する場合は誤判定防止のためスキップされる。ただし TS/JS と PHP の class member は、owner を安全に一意推定できる場合だけ例外的に判定する。PHP では、`Owner::method()` と同一クラス内の `self::method()` を確定参照として扱う。`$obj->method()` や callable 文字列のように owner を確定できない参照がある場合は、スキップする。`static::` は遅延静的束縛によりサブクラスの override に到達し得るため、確定参照としては解決しない。trait を `use` する class / trait / enum 経由の静的呼び出しは、一意に到達する trait method に限り参照として数える。ただし合成先が同名の具象メソッドを持つ場合は、PHP の解決順に従って trait 側へは辿らない。

Ruby では `Builder.new`・`Demo::Builder.new`・`::Demo::Builder.new` のような静的な定数 receiver の呼び出しを、その class が直接定義する通常の instance `initialize` の潜在的な利用として加算する。字句的にネストした module と別ファイルの呼び出しを扱う。テストからしか呼ばれなければ `test_only_symbols` に分け、本番からの呼び出しがあれば両方の一覧から外す。候補から除外したテスト・生成ファイルも含む、全 Ruby 参照ファイルで所属を確認する。近い module や別の定数束縛がある場合、それを飛ばして外側の同名 class へ解決しない。

この証拠を使うには、通常の class と直接定義した instance initializer がそれぞれ一意である必要がある。継承・class の再オープン・修飾名での class/module 宣言・公開候補名の衝突・singleton の構築処理の上書き・refinement・attr 系の生成・alias/メソッド除去/可視性変更・動的な定数/メソッド変更・reflection・外部ソースの読み込みは証明しない。トップレベルの `Struct` のような既存の factory も通常の instance 構築として扱わない。`defined?` の被演算子内からは利用の証拠を返さない。同じファイルの即時呼び出しが initializer の定義より前にあれば、その呼び出しは加算せず、singleton method の receiver は定義時の評価位置で判定する。上限は参照ファイル10,000件、Ruby 1ファイル2MiB、合計64MiBで、構文の深さ・ノード数・保持する事実の量にも上限を設ける。読取/解析失敗や上限超過があれば、蓄積した構築呼び出しの証拠をすべて破棄する。その場合は既存の分類を維持するため、暗黙に使われる initializer の dead 誤検出が残り得る。証拠がないことは未使用の証明ではない。対応構文内の静的な潜在利用を調べるもので、実行順全般は解析しない。同名候補の扱い・公開 API の抽出・`refs` の出現一覧は変えず、未使用の initializer は候補に残す。

### 実行時規約の自動除外

フレームワークやテストランナーが名前規約・リフレクションで動的に呼び出すシンボルは、識別子レベルのファイル間参照では呼び出し元を追跡できず誤検出になる。そのため、以下の規約は自動的に dead-code から除外される:

- **PHPUnit**: `*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest` クラスと `testXxx` / `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` メソッド
- **Python unittest**: `unittest.TestCase`（および `unittest.IsolatedAsyncioTestCase`）を継承するクラス（同一ファイル内の間接継承も fixed-point で解決）と、その `test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` メソッド
- **Python pytest**: `test_*.py` / `*_test.py` ファイルのトップレベル `test_*` 関数と `conftest.py` 内のすべての関数
- **Python フレームワーク登録デコレータ**: Typer / Click / FastAPI / Flask / Django / Celery / pytest などの登録デコレータが付いた関数・メソッド・クラス。別名で import したデコレータ（`from celery import shared_task as st`）も元の名前で照合する。Pydantic が検証とシリアライズのときに呼ぶ、クラス本体のメソッド（`@model_validator` / `@field_validator` / `@field_serializer` / `@model_serializer` / `@computed_field` / `@root_validator`）も除外する。v1 の `@validator` は、`pydantic` から import したと確かめられたときだけ除外する
- **Python 動的プロトコルメソッド**: `urllib.request.BaseHandler` 系の `*_open` / `*_request` / `*_response` / `http_error_*` と、watchdog の `FileSystemEventHandler` 系 `on_*` callback。いずれも既知の基底クラスを直接継承するメソッドだけを除外する
- **Angular**: `@Component` / `@Directive` 装飾クラスのライフサイクルフック（`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`）は Angular ランタイムが change detection サイクルで自動呼び出しするため除外。`@Pipe` 装飾クラスの `transform`（テンプレートの `| name` から呼ばれる）と、`@Injectable` / `@Pipe` 装飾クラスの `ngOnDestroy`（service / pipe の破棄時に呼ばれる）も除外する。service / pipe の他のフック（`ngOnInit` など）は Angular が呼ばないので除外しない
- **PHP のアノテーションと属性**: docblock に大文字で始まるアノテーション（`@Route` / `@Entity` など）か PHPUnit の `@test` / `@dataProvider` などを書いたメソッド・クラスと、同じ名前の PHP 8 の属性（`#[Route]` / `#[Test]` など）を付けたもの
- **構文から呼ばれるメソッド**: Kotlin の `operator fun` と Swift の `callAsFunction`。演算子や関数呼び出しの構文から呼ばれ、名前では参照されない。API 差分には残す
- **プログラムのエントリポイント**: 次を除外する。API 差分には残す
  - C / C++: グローバルスコープの `main`
  - Kotlin: トップレベルの `fun main`、`object` / `companion object` 直下の `@JvmStatic fun main`
  - Java: private でない `void main`（引数なしか `String[]` 1 個。Java 25 の instance main を含む）
  - C#: `static Main`（引数なしか `string[]` 1 個）
  - Java / C# / Kotlin では、エントリポイントを宣言する型（ネストした型の外側の型を含む）も除外する

### 生成ファイルの扱い

生成ファイルとして検出したファイル（先頭の生成宣言コメント、`.gitattributes` の `linguist-generated` など）は、既定で dead 判定の**候補**から外す。ただし生成ファイルの中の参照は常に数える。gRPC の生成ハンドラ（`*_grpc.pb.go`）が手書きのサーバー実装を呼ぶ構成のように、生成コードは実行時に手書きコードを呼ぶため、その参照を捨てると生きているシンボルを dead と報告してしまう。

候補から外したファイルは `generated_candidates_skipped` に `refs` の `skipped` と同じ形で出す（0 件なら出力しない）。`--include-generated`（または `skip_generated = false`）を指定すると生成ファイルのシンボルも候補にする。

```json
{ "...": "...", "generated_candidates_skipped": { "generated": 1, "paths": ["api/greeter_grpc.pb.go"] } }
```

### 参照を数えられなかったファイル

参照を数える対象のファイルを読めなかった場合（100MB の上限を超えた、権限が無いなど）や、構文解析に失敗した場合は、そのファイルに名前が現れる候補を dead と断定しない。該当する候補は `unverified_symbols` に出し、`dead_symbols` には参照が無いと確かめたものだけを残す。数えられなかったファイルは、`truncations` の `source_analysis_failed` で申告する（dead を保留したかどうかに関わらず出す）。

```json
{ "...": "...", "unverified_symbols": [{ "name": "load_fixture", "kind": "function", "file": "src/fixtures.rs", "line": 3 }], "truncations": [{ "path": "data/huge.rs", "reason": "source_analysis_failed", "message": "references in this file were not counted (the file exceeds the read limit (115343360 bytes > 104857600 bytes)); 1 candidate name(s) appear in it, so they are not reported as dead (1 candidate(s) in total were withheld from dead/test-only)" }] }
```

上限を超えたファイルは、構文木を作らずに中身を一定の大きさずつ読み、候補の名前が現れるかだけを調べる。開けないファイルや 1GiB を超えるファイルは、どの名前が現れるか分からないので、すべての候補を保留する。テストのファイルを数えられなかった場合は、`test_only_symbols` の判定は保留しない（本番のファイルからの参照が無いことは、テストのファイルを読めなくても変わらないため）。`review --hook` では `trunc` に出るだけで、これだけでは exit 1 にしない。

### フレームワーク自動検出

`--framework` 未指定時でも、`<dir>` 直下またはモノレポ配下の `package.json` の `dependencies` / `devDependencies` に `next` キーがあれば、自動で `nextjs` プリセットを適用する。自動検出した規約 glob は各 Next.js workspace からの相対パスへ限定するため、非 Next.js の兄弟 workspace にある `app/**/page.tsx` は除外しない。`node_modules`・生成物・symlink は探索対象外で、`peerDependencies` / `optionalDependencies` 経由も誤って適用されやすいため対象外。明示指定（`--framework laravel` など）は常に自動検出より優先される。

```bash
# ルートまたは配下 workspace の package.json に `next` があれば自動適用される
astro-sight dead-code --dir .
astro-sight review --dir . --git
```

### bin-only Rust crate の API 差分除外

`review` の `api_changes`（`added` / `removed` / `modified`）は、bin-only Rust crate（`src/lib.rs` がなく `Cargo.toml` に `[lib]` セクションもない）の `pub fn` 変更を自動的に除外する。bin-only crate の `pub fn` は crate 外から到達できないため、追加・削除・シグネチャ変更いずれも外部公開 API の互換性問題にはならない。`src/lib.rs` を削除する変更が同じ diff に含まれていても、base リビジョン側で library crate だった場合は、旧公開 API の削除を正しく `removed` に残す。

削除した Rust の自由関数では、既存の帰属証明を先に使い、未解決の同名の出現について、変更前と変更後の両方で Cargo の依存が削除元 package に届かないと証明できたものを除く。残りで既存の削除判定をやり直す。この証拠は起動時の環境、字句上と実体側の祖先、`CARGO_HOME` の設定も調べ、`--extern`・コンパイラの差し替えなどがあれば使わない。staged では元の Cargo 設定の場所を確定できないため、この証拠を使わない。標準マクロ・derive・属性の検査は上記の impact と共通で、`println!` や `#[derive(Debug)]` だけでは証明を止めない。属性付き・ABI 付きの削除関数や、変更後も削除元 package を指し得る参照は対象外。同じ差分で書き換えた参照ファイルでは、同じファイル直下の自由関数への修飾なしの束縛を証明できた出現だけを除く。

## cochange - 共変更パターン検出

git blame と diff-tree から、指定ファイルと一緒に変更されやすいファイルを検出する。`review --git --base <rev>` の `missing_cochanges` でも同じ解析を使う。

```bash
# git diff から起点ファイルを自動取得
astro-sight cochange --dir . --git --base HEAD~5

# 起点ファイルを明示
astro-sight cochange --dir . --paths src/service.rs

# rename / copy を追跡
astro-sight cochange --dir . --git --base HEAD~10 --rename --copy
```

`--paths-file` は 100MB 上限付きで読み込まれ、空リストは `INVALID_REQUEST` を返す。`--min-confidence` は有限な `0.0..=1.0`、`--smoothing-alpha` / `--smoothing-beta` は有限な非負値のみ受け付ける。`--paths` / `--paths-file` で渡すソースファイルは `--dir` 配下の相対パスである必要があり、`..` を含むパス・絶対パス・Windows のドライブ修飾パスは `PATH_OUT_OF_BOUNDS` で拒否される。

**生成物は起点・候補の両方から除外する。** バッチ処理・コード生成・ビルドが同時に書き出すファイル群は履歴上ほぼ必ず同一コミットに乗るため、そのままでは「機械的な同時更新」が高い confidence の共変更として提示されてしまう。判定では `.gitattributes` の `linguist-generated` を最優先する（`set` / `true` なら除外し、`unset` / `false` ならヘッダマーカーを見ずに残す）。指定がなければ、ファイル先頭の生成マーカー（`@generated` / `DO NOT EDIT` など）を見る。どちらでも決まらなければ候補に残す。拡張子や更新頻度では判定しないので、コメントを書けない生成 JSON / CSV は `.gitattributes` で宣言する。

```gitattributes
data/*.json linguist-generated=true
fixtures/hand-maintained.yaml -linguist-generated
```

除外しても**残った起点の分母は変わらない**（生成物が同居していたコミットを分母から抜くと `1/2` が `1/1` に化けて選択バイアスになるため）。除外件数は `diagnostics` の `excluded_generated_sources` / `filtered_generated_candidates` に出る。`git check-attr` を実行できない場合は除外を一切行わず（`GeneratedAttrLookupFailed` を申告）、判定できないことを理由に候補を消さない。グローバルの `--include-generated`（`config.toml` の `skip_generated = false` も同義）で、生成物も対象にできる。この指定は単体の `cochange` だけでなく `review` の `missing_cochanges` にも効く。

## シェルの解析エラー

この分類は変更前・変更後の双方が Bash 文法のときだけ適用する。別言語の API をシェルへ置き換えた場合は、同名の関数ヘッダが残っても旧 API の削除判定を維持する。拡張子なしファイルは変更前の shebang も確認する。

Bash / zsh の関数が抽出結果から消えても、`ERROR` 内に同名の宣言ヘッダが残る場合、`review` は削除と断定せず `api_changes.uncertain_removals` に分類する。名前・ファイル・0 始まりの行番号・旧シグネチャ・`reason: "parse_error_region"` を返す。hook では `api.rm_unverified` の `{n,f,l,r}` となる。この項目自体は情報提供であり、同じファイルの確定した削除は引き続き blocking になる。同名の複数定義や、文字列・heredoc か判別できない内容では、従来の保守的な削除判定を維持する。

解析エラーを含む変更後の Bash / zsh ファイルは、`truncations`（hook: `trunc`）にも `parse_error_region` として申告する。その範囲の宣言・参照・シグネチャ変更は未検証であり、exit 0 は互換性の保証ではない。推測したシンボルは追加せず、正常に解析できた領域の検証は続ける。

明示 diff で対応言語のソースファイルを削除し、その旧内容が指定 base に無い、食い違う、または完全に照合できない場合、`review` は `truncations`（hook: `trunc`）へ `diff_base_mismatch` を出す。hook は `api.diff_base_mismatch` で停止する。抽出できた削除シンボルは blocking の `api.rm` に残し、旧ソース自体が無ければ打ち切りの申告だけを返す。文書や Git サブモジュールの削除は対象外。`--git` は `diff.submodule` の設定にかかわらず gitlink の短い差分を使う。可能なら削除前のリビジョンを `--base` に指定する。
