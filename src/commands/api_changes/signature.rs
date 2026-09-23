//! API シグネチャの抽出と正規化。言語別の binding shape と Tauri / TypeScript の正規化を含む。

use super::*;

/// dead_symbols のうち、宣言行が今回の diff の追加行 (`+` 行) と重なるもののみを残す。
///
/// `--dead-scope touched-symbols` の実装。`review --hook` のデフォルトとして使われ、
/// 「changed file 内に元からあった dead」がレビューノイズとして毎回出る UX 問題を
/// 解消する。
///
/// 注意: `HunkInfo` の `new_start` / `new_count` は context 行も含むため
/// hunk 範囲全体を「touched」と扱うと既存 dead まで残してしまう。ここでは
/// `extract_changed_new_lines` で **実際に追加された行** だけを set 化して照合する。
pub(crate) fn extract_symbol_lines(
    dir: &str,
    file_path: &str,
) -> Option<std::collections::HashMap<String, usize>> {
    use std::collections::HashMap;
    let full = std::path::Path::new(dir).join(file_path);
    let utf8 = camino::Utf8Path::new(full.to_str()?);
    let source = parser::read_file(utf8).ok()?;
    let lang_id = parser::detect_lang(utf8, &source).ok()?;

    let symbols = if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        crate::engine::lexer::extract_symbols(&source, lexer_lang)
    } else {
        let tree = parser::parse_source(&source, lang_id).ok()?;
        crate::engine::symbols::extract_symbols(tree.root_node(), &source, lang_id).ok()?
    };

    let mut map = HashMap::new();
    for s in symbols {
        // 同名シンボルが複数ある場合、最初に出現した行を保持する。
        // 宣言を共有する分割代入の束縛は、宣言の先頭ではなく名前の行を使う。
        let line = s.name_line();
        map.entry(s.name).or_insert(line);
    }
    Some(map)
}

/// シンボルの種類に応じた API シグネチャを抽出する。
/// 関数/メソッド → 宣言行、struct/enum/trait/interface/class → 宣言行のみ。
///
/// クラス/型は宣言行（`class Foo(Bar):` や `struct Foo {` など）のみをシグネチャとする。
/// 本体（メソッド本体や private フィールド）の変更でクラス全体の API 変更として
/// 再検出されるのを避けるため、メンバーの集約はしない。
/// メンバー個々の変更は method シンボル単独で検出される。
///
/// function / method の場合は tree-sitter ノードで「宣言開始から body 直前まで」を
/// 抽出し、whitespace を正規化して signature とする。これにより `where` 句や複数行
/// generics で先頭行が同一でも引数列が変わったケース (Issue
/// 2026-05-14-rename-and-multiline-signature) を検出できる。
/// 関数/メソッドノードの body 開始 byte を返す。tree-sitter の "body" フィールドを優先し、
/// 取得できない grammar (tree-sitter-kotlin 0.3.5 の `function_declaration` は
/// `fields: []` でフィールド名を持たず、body は `function_body` 型の直接子) では直接の
/// named child から既知の body ノード kind を fallback で探す。body を持たない宣言
/// (Swift protocol requirement / Rust trait fn / Kotlin abstract fun) では None を返し、
/// 呼び出し側が `end_byte()` (= 宣言全体 = 署名のみ) に倒す。
/// これを入れないと body フィールドを持たない言語で「関数全体」が署名になり、
/// body のみ変更が api.mod に誤検出される (Kotlin body-only 変更の false positive 対策)。
pub(crate) fn function_body_start_byte(node: tree_sitter::Node<'_>) -> Option<usize> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body.start_byte());
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "function_body" | "block" | "statement_block" | "compound_statement"
            )
        })
        .map(|child| child.start_byte())
}

/// body が無い (interface method / abstract 等) や node 取得失敗時は先頭行を fallback。
pub(crate) fn extract_api_signature(
    sym: &crate::models::symbol::Symbol,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    lang_id: crate::language::LangId,
) -> String {
    use crate::models::symbol::SymbolKind;
    if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
        let start = tree_sitter::Point {
            row: sym.range.start.line,
            column: sym.range.start.column,
        };
        let end = tree_sitter::Point {
            row: sym.range.end.line,
            column: sym.range.end.column,
        };
        if let Some(node) = root.descendant_for_point_range(start, end) {
            let mut cur = node;
            loop {
                match cur.kind() {
                    "function_item"
                    | "function_declaration"
                    | "generator_function_declaration"
                    | "function_definition"
                    | "method_declaration"
                    | "method_definition"
                    | "function_signature_item"
                    // Swift protocol requirement (body なしの宣言)。複数行 requirement でも
                    // 先頭行 fallback でなく AST から signature 全体を抽出する (codex 指摘)。
                    | "protocol_function_declaration" => {
                        let s = cur.start_byte();
                        let e =
                            function_body_start_byte(cur).unwrap_or_else(|| cur.end_byte());
                        // TS/TSX の関数 destructured params (`function foo({ a, b }: T)`) は
                        // `{ ... }` 内の variable 列が変わっても呼び出し側契約 (`: T` 型注釈)
                        // に影響しないため、signature 比較から除外する。React の Props
                        // 拡張 (optional prop 追加 + destructure 受け取り追加) で api.mod に
                        // 出る false positive を防ぐ (Issue
                        // 2026-05-28-api-mod-optional-props-additive 対応)。
                        if matches!(
                            lang_id,
                            crate::language::LangId::Typescript | crate::language::LangId::Tsx
                        ) {
                            return normalize_typescript_destructure_signature(cur, source, s, e);
                        }
                        // Tauri command (`#[tauri::command]` / `#[command]`) の自動注入型引数
                        // (AppHandle / State / Window 等) は実行時に Tauri が注入し JS 側 invoke()
                        // の引数には現れないため、signature 比較から除外する
                        // (Issue 2026-05-29-swift-sidecar-api-mod パターンB)。
                        if lang_id == crate::language::LangId::Rust
                            && let Some(sig) =
                                normalize_rust_tauri_command_signature(cur, source, s, e)
                        {
                            return crate::engine::rust_signature::normalize_rust_signature_text(
                                &sig,
                            )
                            .unwrap_or(sig);
                        }
                        if lang_id == crate::language::LangId::Rust
                            && let Some(sig) = crate::engine::rust_signature::normalize_rust_parameter_binding_signature(
                                cur, source, s, e,
                            )
                        {
                            return sig;
                        }
                        if let Some(bytes) = source.get(s..e) {
                            let sig = normalize_signature_whitespace(bytes);
                            if lang_id == crate::language::LangId::Python {
                                return with_python_binding_decorators(cur, source, sig);
                            }
                            return sig;
                        }
                        break;
                    }
                    _ => {}
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
    }

    // 値バインディング (const / let / var / static) は **宣言全体**を signature にする。
    //
    // 先頭行 fallback だと、prettier が整形した
    // `export const config = {\n  google: {\n    bgColor: "..."\n  }\n};` の signature が
    // `export const config = {` になり、**中身をどう変えても signature が変わらない**。
    // `collect_modified_symbols` は `old_sig != new_sig` でしか api.mod 候補を作らないため、
    // 参照されている member の削除・改名・値の差し替えが api.mod にも
    // compatible_modified にも一切現れず、**完全に沈黙する** (実測: 利用中の
    // `providerConfig.google.bgColor` を `bgClass` に改名した diff が exit 0。同じ変更を
    // 1 行に畳むと blocking な api.mod になる = 整形の違いだけで検出が反転していた)。
    // object literal を複数行で書くのは prettier / biome の既定なので、実質
    // 「TS/JS の exported const は member 変更を検出できない」状態だった。
    //
    // 対象は JS/TS/TSX と Rust に限る (object member 互換判定と const 値変更判定が
    // 効くのはこの範囲。他言語の signature 出力は不変)。
    //
    // 単位は `lexical_declaration` 全体ではなく **declarator 1 個**にする。
    // `const a = 1, b = 2;` で宣言全体を使うと、`b` だけを変えても `a` の signature が
    // 変わって api.mod に出る (無関係なシンボルの誤検出)。宣言 keyword と `export` は
    // `const` → `let` のような可視性・可変性の変化を捉えるため prefix として付ける。
    if matches!(sym.kind, SymbolKind::Variable | SymbolKind::Constant)
        && matches!(
            lang_id,
            crate::language::LangId::Javascript
                | crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
                | crate::language::LangId::Rust
        )
        && let Some(sig) = value_binding_signature(sym, root, source)
    {
        return sig;
    }

    // Python の class は宣言ヘッダ全体 (`class X(` 〜 body 直前) を signature にする。
    //
    // 先頭行 fallback だと、black / ruff が折り返した
    // `class Payload(\n    TypedDict,\n    total=False,\n):` のヘッダが `class Payload(` に
    // なり、2 行目以降のキーワード引数が signature に現れない。`python_contract.rs` は
    // `old_sig` / `new_sig` に `total` の字面があるかで安価に前段フィルタしているため、
    // 折り返しヘッダでは `total=` の反転を分類できず contract ラベルが失われていた。
    //
    // 「丸括弧が閉じていない signature は TypedDict 候補扱いにする」という回避は採らない。
    // 3 値判定 (`PythonContractDetection`) では解析に失敗すると `PotentialBreakingChange` へ
    // 倒れるため、無関係な複数行 Python class が広く blocking 化する。signature 抽出側を
    // 正しくするのが本筋。
    //
    // 適用は **Python の class だけ**に閉じる (他言語の class signature 出力は不変)。
    if lang_id == crate::language::LangId::Python && sym.kind == SymbolKind::Class {
        let start = tree_sitter::Point {
            row: sym.range.start.line,
            column: sym.range.start.column,
        };
        let end = tree_sitter::Point {
            row: sym.range.end.line,
            column: sym.range.end.column,
        };
        if let Some(node) = root.descendant_for_point_range(start, end) {
            let mut cur = node;
            loop {
                if cur.kind() == "class_definition" {
                    let s = cur.start_byte();
                    let e = cur
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| cur.end_byte());
                    if let Some(bytes) = source.get(s..e) {
                        return normalize_signature_whitespace(bytes);
                    }
                    break;
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
    }

    // フォールバック: 先頭行のみ
    lines
        .get(sym.range.start.line)
        .unwrap_or(&"")
        .trim()
        .to_string()
}

/// Python の関数 signature (`def` 〜 body 直前) の前に、呼び出し方 (束縛) を変える
/// デコレータ (`@property` / `@cached_property` / `@staticmethod` / `@classmethod`) を
/// 外側から順に付ける。
///
/// `function_definition` は `def` から始まるため、デコレータだけの変更 (メソッドに
/// `@property` を付ける = 呼び出し側の `u.name()` が TypeError になる) が signature に
/// 現れず api.mod から沈黙していた。束縛を変えないデコレータ (`@lru_cache` 等) まで
/// 入れると、呼び出し互換な付け外しが blocking になるため対象を絞る。
/// 該当デコレータが無ければ従来の signature をそのまま返す。
fn with_python_binding_decorators(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    sig: String,
) -> String {
    let decorators = python_binding_decorators(fn_node, source);
    if decorators.is_empty() {
        return sig;
    }
    let mut out = String::new();
    for decorator in decorators {
        out.push_str(decorator.signature_token());
        out.push(' ');
    }
    out.push_str(&sig);
    out
}

/// 宣言テキストから comment トークンを取り除いたうえで空白正規化する。
///
/// 値バインディングの signature は初期化子を含む item 全体から作るため、素朴に
/// テキストを取るとコメントまで signature の一部になる。その結果 **コメントを直しただけで
/// `api.mod` に載り Stop hook が blocking する** (実測: コードを 1 文字も変えず
/// `/// 一覧表` と配列内の `// 最初の要素` を書き換えただけで `modified` 1 件 / exit 1)。
/// さらに [`normalize_signature_whitespace`] が改行を潰すので、行コメント以降が 1 行へ
/// 連なり後段の shape 抽出 (`extract_binding_shape`) が parse に失敗する。失敗は
/// fail-closed なので、本来なら非 blocking な `const_value_changes` へ降格できる
/// 純粋な値変更まで blocking 側に残っていた (Issue: 2026-09-16-const-array-append)。
///
/// **コメントは削るのではなく空白 1 個へ置換する。** 削るとトークンが連結して
/// `foo/*c*/bar` が `foobar` になり、別物の signature が一致してしまう
/// (`ts_signature.rs` の「トークン境界は保つ」と同じ規約)。
///
/// `keep_doc_comments` は **JS/TS 専用**。`/** @type {string} */ ("x")` のような JSDoc は
/// 型アサーションとして実際に型契約を持つため、落とすと型変更を見逃す fail-open になる。
/// Rust の `///` / `//!` は型契約を持たないので落として良い (そもそも tree-sitter-rust では
/// doc comment と属性は `const_item` の**外側**に出るため範囲に入らない)。
///
/// 文字列リテラル中の `//` は comment ノードではないので AST ベースの本判定では誤爆しない
/// (テキスト置換で実装すると `"https://example.test/a//b"` を壊す)。
fn normalize_signature_dropping_comments(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    keep_doc_comments: bool,
) -> Option<String> {
    normalize_signature_eliding(node, source, keep_doc_comments, &[])
}

/// [`normalize_signature_dropping_comments`] に加え、`elided` の各範囲 (関数本体) を
/// `{}` に置き換える。範囲内のコメントは本体ごと消える。
fn normalize_signature_eliding(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    keep_doc_comments: bool,
    elided: &[(usize, usize)],
) -> Option<String> {
    let (start, end) = (node.start_byte(), node.end_byte());
    // (開始, 終了, 置換文字列)。コメントは空白 1 個、省略する本体は `{}`。
    let mut spans: Vec<(usize, usize, &[u8])> = elided
        .iter()
        .map(|&(s, e)| (s, e, b"{}".as_slice()))
        .collect();
    let mut cursor = node.walk();
    let mut descend = true;
    loop {
        let current = cursor.node();
        let is_comment = matches!(current.kind(), "comment" | "line_comment" | "block_comment");
        if is_comment {
            let keep = keep_doc_comments
                && source
                    .get(current.start_byte()..current.end_byte())
                    .is_some_and(|b| b.starts_with(b"/**"));
            if !keep {
                spans.push((current.start_byte(), current.end_byte(), b" ".as_slice()));
            }
        }
        // comment ノードは葉なので潜らない。
        if descend && !is_comment && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() || cursor.node().id() == node.id() {
            break;
        }
        descend = false;
    }

    if spans.is_empty() {
        return source.get(start..end).map(normalize_signature_whitespace);
    }
    spans.sort_unstable();
    let mut out: Vec<u8> = Vec::with_capacity(end.saturating_sub(start));
    let mut pos = start;
    for (s, e, replacement) in spans {
        // 走査順の乱れや範囲外を拾っても壊れないよう、進行方向だけを信じる。
        // 省略した本体の内側にあるコメントは `s < pos` でここに落ちる。
        if s < pos || e > end {
            continue;
        }
        out.extend_from_slice(source.get(pos..s)?);
        out.extend_from_slice(replacement);
        pos = e;
    }
    out.extend_from_slice(source.get(pos..end)?);
    Some(normalize_signature_whitespace(&out))
}

/// 値バインディング 1 個 (`export const X = ...` / `const X: T = ...` / `static X: T = ...`)
/// の宣言テキストを正規化して返す。
///
/// JS/TS では declarator 単位で切り出し、宣言 keyword (`const` / `let` / `var`) と
/// `export` を prefix として補う。Rust は `const_item` / `static_item` が 1 名前 1 item
/// なので item 全体をそのまま使う。
///
/// どちらの経路でもコメントトークンは signature から落とす
/// ([`normalize_signature_dropping_comments`])。
fn value_binding_signature(
    sym: &crate::models::symbol::Symbol,
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<String> {
    let start = tree_sitter::Point {
        row: sym.range.start.line,
        column: sym.range.start.column,
    };
    let end = tree_sitter::Point {
        row: sym.range.end.line,
        column: sym.range.end.column,
    };
    let mut cur = root.descendant_for_point_range(start, end)?;
    loop {
        match cur.kind() {
            // Rust: 1 item = 1 名前なので item 全体で良い。
            // doc comment と属性は item の外側に出るため、落とすのは初期化子中の
            // 行コメント / ブロックコメントだけになる。
            "const_item" | "static_item" => {
                return normalize_signature_dropping_comments(cur, source, false);
            }
            "variable_declarator" => {
                // 分割代入の束縛は「その名前へ至る経路 + 初期化子」だけを signature にする
                // (`destructured_binding_body`)。単純でないパターンは None が返り、
                // declarator 全体へ倒す。
                // 関数値 (`= () => {..}` 等) は本体を省く (`js_function_value_body_spans`)。
                // JS/TS は JSDoc (`/** @type {...} */`) が型アサーションとして効くので残す。
                let body = match sym
                    .name_range
                    .as_ref()
                    .and_then(|name_range| destructured_binding_body(cur, name_range, source))
                {
                    Some(body) => body,
                    None => {
                        let mut elided = Vec::new();
                        if let Some(value) = cur.child_by_field_name("value") {
                            js_function_value_body_spans(value, source, &mut elided);
                        }
                        normalize_signature_eliding(cur, source, true, &elided)?
                    }
                };
                let mut prefix = String::new();
                // 宣言 keyword と export を辿って補う。
                let mut ancestor = cur.parent();
                while let Some(node) = ancestor {
                    match node.kind() {
                        "lexical_declaration" | "variable_declaration" => {
                            // 最初の匿名トークンが `const` / `let` / `var`。
                            if let Some(kw) = node.child(0)
                                && let Some(text) = source.get(kw.start_byte()..kw.end_byte())
                            {
                                prefix
                                    .insert_str(0, &format!("{} ", String::from_utf8_lossy(text)));
                            }
                        }
                        "export_statement" => prefix.insert_str(0, "export "),
                        // 宣言より外側は signature に関係しない。
                        "program" | "statement_block" => break,
                        _ => {}
                    }
                    ancestor = node.parent();
                }
                let mut sig = prefix;
                // body は normalize_signature_dropping_comments で正規化済み。
                sig.push_str(&body);
                return Some(sig);
            }
            _ => {}
        }
        cur = cur.parent()?;
    }
}

/// 値バインディングの初期化子のうち、**バインディングの値そのものである関数**の本体範囲を
/// 集める (signature から `{}` に置き換える)。
///
/// 値バインディングの signature は宣言全体なので、`export const Button = () => {..}` の
/// JSX を 1 文字直しただけで blocking な api.mod になっていた (同じ変更を
/// `export function Button` で書けば本体を除いた宣言だけが signature なので何も出ない)。
/// アロー関数コンポーネントが主流の React では本体編集のたびに Stop hook が止まる。
/// 関数宣言と揃えて、次の位置の関数本体だけを省く:
/// - 値そのものが関数 (括弧・`as`・`satisfies`・`!` で包まれていても辿る)
/// - React の HOC `memo` / `forwardRef` (`React.*` 含む) の引数の関数
///   (描画ロジックで、コンポーネントの契約は引数と型引数に現れる)
/// - オブジェクトリテラルのメンバーの関数 (メソッド・`key: () => ..`)。キーの追加・削除や
///   関数から値への差し替えは本体を省いても signature に残る
///
/// 任意の呼び出しの引数 (`create((set) => ({ .. }))` / `compute(() => 1)`) は辿らない。
/// コールバックの中身がストアの形や値そのものを決める (= 契約) ことがあるため。
fn js_function_value_body_spans(
    value: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut Vec<(usize, usize)>,
) {
    match value.kind() {
        "parenthesized_expression"
        | "as_expression"
        | "satisfies_expression"
        | "non_null_expression" => {
            if let Some(inner) = value.named_child(0) {
                js_function_value_body_spans(inner, source, out);
            }
        }
        "arrow_function" | "function_expression" | "generator_function" => {
            if let Some(body) = value.child_by_field_name("body") {
                out.push((body.start_byte(), body.end_byte()));
            }
        }
        "call_expression" => {
            if value
                .child_by_field_name("function")
                .is_some_and(|callee| is_react_component_hoc(callee, source))
                && let Some(args) = value.child_by_field_name("arguments")
            {
                let mut cursor = args.walk();
                for arg in args.named_children(&mut cursor) {
                    js_function_value_body_spans(arg, source, out);
                }
            }
        }
        "object" => {
            let mut cursor = value.walk();
            for member in value.named_children(&mut cursor) {
                match member.kind() {
                    "method_definition" => {
                        if let Some(body) = member.child_by_field_name("body") {
                            out.push((body.start_byte(), body.end_byte()));
                        }
                    }
                    "pair" => {
                        if let Some(member_value) = member.child_by_field_name("value") {
                            js_function_value_body_spans(member_value, source, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// 呼び出し先が React の `memo` / `forwardRef` (`React.memo` / `React.forwardRef` を含む) か。
fn is_react_component_hoc(callee: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let is_hoc_name =
        |node: tree_sitter::Node<'_>| matches!(node.utf8_text(source), Ok("memo" | "forwardRef"));
    match callee.kind() {
        "identifier" => is_hoc_name(callee),
        "member_expression" => {
            callee
                .child_by_field_name("object")
                .is_some_and(|object| object.utf8_text(source).ok() == Some("React"))
                && callee
                    .child_by_field_name("property")
                    .is_some_and(is_hoc_name)
        }
        _ => false,
    }
}

/// 分割代入の束縛 1 つ分の signature 本体 (`<経路パターン>[: 型] = <初期化子>`) を作る。
///
/// declarator 全体を signature にすると、兄弟の束縛を消しただけで生き残った束縛の
/// signature まで変わり、契約が変わっていない束縛が blocking な api.mod に載る
/// (`export const { auth, signOut } = NextAuth()` から `signOut` を消すと `auth` も
/// 「変更」になる)。そこで「その名前へ至る経路」だけを残したパターンへ正規化する。
/// 配列は位置が契約なので穴で添字を保つ (`[first, second]` → `[, second]` は同一、
/// `[second]` は添字 1 → 0 の変更として検出する)。
///
/// 兄弟の差分を捨ててよいのは、パターンが評価を伴わない単純な束縛だけでできている
/// ときに限る。default 値 (`{ a = f() }`)・computed key (`{ [k()]: v }`) は兄弟の評価が
/// 副作用を持ちうるうえ、rest (`{ a, ...rest }`) は兄弟の集合で中身が決まる。これらを
/// 1 つでも含むパターンは None を返し、呼び出し側が declarator 全体を signature にする
/// (= 兄弟の変更でも api.mod に倒す保守側)。
fn destructured_binding_body(
    declarator: tree_sitter::Node<'_>,
    name_range: &crate::models::location::Range,
    source: &[u8],
) -> Option<String> {
    let pattern = declarator.child_by_field_name("name")?;
    if !matches!(pattern.kind(), "object_pattern" | "array_pattern")
        || pattern_has_non_static_element(pattern)
    {
        return None;
    }
    let mut target = None;
    let _ = crate::engine::js_binding_pattern::visit_pattern_bindings(pattern, &mut |binding| {
        if crate::models::location::Range::from(binding.range()) == *name_range {
            target = Some(binding);
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    });
    let mut body = binding_path_text(pattern, target?, source)?;
    if let Some(ty) = declarator.child_by_field_name("type") {
        body.push_str(&normalize_signature_dropping_comments(ty, source, true)?);
    }
    if let Some(value) = declarator.child_by_field_name("value") {
        body.push_str(" = ");
        body.push_str(&normalize_signature_dropping_comments(value, source, true)?);
    }
    Some(body)
}

/// パターンに default 値・computed key・rest を含むか (= 束縛ごとの正規化が安全でない)。
fn pattern_has_non_static_element(pattern: tree_sitter::Node<'_>) -> bool {
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "assignment_pattern"
                | "object_assignment_pattern"
                | "computed_property_name"
                | "rest_pattern"
        ) {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// `node` (パターン) から束縛 `target` へ至る経路だけを残したパターン文字列を作る。
/// 例: `{ a, b: { c } }` の `c` → `{ b: { c } }`、`[x, , y]` の `y` → `[, , y]`。
fn binding_path_text(
    node: tree_sitter::Node<'_>,
    target: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<String> {
    let text = |n: tree_sitter::Node<'_>| n.utf8_text(source).ok().map(str::to_string);
    let contains = |outer: tree_sitter::Node<'_>| {
        outer.start_byte() <= target.start_byte() && target.end_byte() <= outer.end_byte()
    };
    if node.id() == target.id() {
        return text(node);
    }
    let mut cursor = node.walk();
    match node.kind() {
        "object_pattern" => {
            let child = node
                .named_children(&mut cursor)
                .find(|child| contains(*child))?;
            match child.kind() {
                "shorthand_property_identifier_pattern" => Some(format!("{{ {} }}", text(child)?)),
                "pair_pattern" => {
                    let key = text(child.child_by_field_name("key")?)?;
                    let inner =
                        binding_path_text(child.child_by_field_name("value")?, target, source)?;
                    Some(format!("{{ {key}: {inner} }}"))
                }
                _ => None,
            }
        }
        "array_pattern" => {
            // 添字 = 対象要素より前の `,` の数 (穴 `[, x]` も 1 要素として数える)。
            let mut index = 0usize;
            for child in node.children(&mut cursor) {
                if child.kind() == "," {
                    index += 1;
                } else if child.is_named() && contains(child) {
                    let inner = binding_path_text(child, target, source)?;
                    return Some(format!("[{}{inner}]", ", ".repeat(index)));
                }
            }
            None
        }
        _ => None,
    }
}

/// 値バインディング (const / static / export const) の宣言から抽出した shape 情報。
/// initializer (= 右辺) を除いた宣言の骨格と、value-only 変更を安全に判定するための補助情報。
pub(crate) struct BindingShape {
    /// initializer を除いた正規化済み宣言テキスト (名前・型・visibility・binding kind を含む)。
    shape: String,
    /// 不変バインディング (Rust `const` / 非 mut `static`、TS/JS `const`) なら true。
    /// mutable (`static mut` / `let` / `var`) は false。
    is_const_binding: bool,
    /// 型注釈を持つなら true (TS の型注釈なし initializer の安全判定に使う)。
    has_type_annotation: bool,
    /// initializer が scalar literal (数値 / 文字列 / 真偽値 / null 等) なら true。
    /// 関数 / object / array / call 等の複雑な式は false。
    initializer_is_scalar: bool,
}

/// `node` を起点に、指定 kind のいずれかに最初に一致する子孫ノードを深さ優先で探す。
/// signature 文字列は単一宣言なので export_statement 等のラップを潜るために使う。
pub(crate) fn find_first_descendant_of_kinds<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if kinds.contains(&node.kind()) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_first_descendant_of_kinds(child, kinds) {
            return Some(found);
        }
    }
    None
}

/// value 手前で切った宣言テキストを正規化する。末尾に残る `=` と前後・連続空白を畳む。
pub(crate) fn normalize_binding_shape_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim_end();
    // value の直前で切ると末尾に `= ` が残るため取り除く。
    let without_eq = trimmed
        .strip_suffix('=')
        .map(str::trim_end)
        .unwrap_or(trimmed);
    without_eq.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// signature 文字列を AST パースし、値バインディングなら initializer を除いた shape を返す。
/// 対象外 (関数 / 型 / バインディング以外) や抽出失敗時は None を返し、呼び出し側は保守的に
/// 従来どおり api.mod へ倒す (codex 設計合意: テキストの `=` 分割ではなく AST ベース)。
pub(crate) fn extract_binding_shape(
    sig: &str,
    lang_id: crate::language::LangId,
) -> Option<BindingShape> {
    // lexer-only 言語は tree-sitter を持たないため対象外。
    if lang_id.is_lexer_only() {
        return None;
    }
    let source = sig.as_bytes();
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    match lang_id {
        crate::language::LangId::Rust => {
            let decl = find_first_descendant_of_kinds(root, &["const_item", "static_item"])?;
            extract_rust_binding_shape(decl, source)
        }
        crate::language::LangId::Typescript
        | crate::language::LangId::Tsx
        | crate::language::LangId::Javascript => {
            let decl = find_first_descendant_of_kinds(
                root,
                &["lexical_declaration", "variable_declaration"],
            )?;
            extract_js_binding_shape(decl, source)
        }
        _ => None,
    }
}

/// Rust の const_item / static_item から shape を抽出する。
pub(crate) fn extract_rust_binding_shape(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BindingShape> {
    // static mut は mutable_specifier を子に持つ。const は常に不変。
    let mut cursor = node.walk();
    let is_mut = node
        .children(&mut cursor)
        .any(|c| c.kind() == "mutable_specifier");
    let value = node.child_by_field_name("value");
    let has_type_annotation = node.child_by_field_name("type").is_some();
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let shape_bytes = source.get(node.start_byte()..shape_end)?;
    let initializer_is_scalar = value.map(rust_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding: !is_mut,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// TS/JS の lexical_declaration / variable_declaration から shape を抽出する。
pub(crate) fn extract_js_binding_shape(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BindingShape> {
    // binding kind (`const` / `let` / `var`) を最初の anonymous child から判定する。
    let mut decl_cursor = node.walk();
    let binding_kw = node
        .children(&mut decl_cursor)
        .find(|c| matches!(c.kind(), "const" | "let" | "var"))
        .map(|c| c.kind());
    let is_const_binding = binding_kw == Some("const");

    // 複数 declarator (`const a = 1, b = 2;`) は shape 抽出が壊れるため対象外。
    let mut declarators = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            declarators.push(child);
        }
    }
    if declarators.len() != 1 {
        return None;
    }
    let declarator = declarators[0];
    // 分割代入 (`const { a } = obj`) は値の変更で各束縛の型も中身も変わりうるため、
    // 値のみの変更 (const_value_changes) へ降格させない。単純な束縛だけを対象にする。
    if declarator
        .child_by_field_name("name")
        .is_none_or(|name| name.kind() != "identifier")
    {
        return None;
    }
    let value = declarator.child_by_field_name("value");
    let has_type_annotation = declarator.child_by_field_name("type").is_some();

    // visibility (export) を shape に含めるため、親が export_statement なら起点を遡る。
    let shape_start = match node.parent() {
        Some(p) if p.kind() == "export_statement" => p.start_byte(),
        _ => node.start_byte(),
    };
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| declarator.end_byte());
    let shape_bytes = source.get(shape_start..shape_end)?;
    let initializer_is_scalar = value.map(js_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// Rust の値ノードが scalar literal かを判定する (型注釈なし経路の安全弁、誤検出側に倒す)。
pub(crate) fn rust_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "integer_literal"
            | "float_literal"
            | "string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "boolean_literal"
    )
}

/// JS/TS の値ノードが scalar literal かを判定する。関数 / object / array / call は false。
pub(crate) fn js_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "number" | "string" | "true" | "false" | "null" | "undefined"
    )
}

/// old/new signature が「const / 非 mut static / export const の値のみ変更 (shape 不変)」かを
/// 判定する。true なら api.mod ではなく const_value_changes (informational) に振り分ける。
///
/// gate: (1) kind が value binding (constant/variable)、(2) 言語が Rust/TS/TSX/JS、
/// (3) 両者が不変バインディング、(4) shape 一致、(5) TS で型注釈なしなら両者 scalar literal。
/// いずれか外れる / 抽出失敗時は false を返し、保守的に api.mod へ倒す。
pub(crate) fn is_const_value_only_change(
    old_sig: &str,
    new_sig: &str,
    kind: &str,
    lang_id: crate::language::LangId,
) -> bool {
    // 値バインディングの kind のみ (Rust const/static="constant"、TS/JS const="variable")。
    if !matches!(kind, "constant" | "variable") {
        return false;
    }
    if !matches!(
        lang_id,
        crate::language::LangId::Rust
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        return false;
    }
    let (Some(old), Some(new)) = (
        extract_binding_shape(old_sig, lang_id),
        extract_binding_shape(new_sig, lang_id),
    ) else {
        return false;
    };
    // mutable バインディング (static mut / let / var) は demote しない。
    if !old.is_const_binding || !new.is_const_binding {
        return false;
    }
    // shape (名前・型・visibility・binding kind) が変われば破壊的変更の可能性 → api.mod。
    if old.shape != new.shape {
        return false;
    }
    // TS/JS で型注釈がない場合、関数 / object / array / call initializer は shape 推定が
    // 危険なため scalar literal 同士のときだけ demote する (codex 指摘)。
    if matches!(
        lang_id,
        crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        let both_typed = old.has_type_annotation && new.has_type_annotation;
        let both_scalar = old.initializer_is_scalar && new.initializer_is_scalar;
        if !both_typed && !both_scalar {
            return false;
        }
    }
    true
}

/// Tauri command の自動注入型 (実行時に Tauri が注入し JS-facing な invoke() 引数に現れない型)。
/// `Channel<T>` は JS 側から渡す引数なので含めない (signature 差分の対象に残す)。
pub(crate) const TAURI_INJECTED_TYPES: &[&str] = &[
    "AppHandle",
    "Window",
    "Webview",
    "WebviewWindow",
    "State",
    "Request",
    "CommandScope",
    "GlobalScope",
];

/// Rust の型ノードから base 名 (パス・参照・ジェネリクスを剥がした末尾型名) を取り出す。
pub(crate) fn rust_type_base_name(ty: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match ty.kind() {
        "type_identifier" => ty.utf8_text(source).ok().map(str::to_string),
        // tauri::AppHandle → name 子 'AppHandle'
        "scoped_type_identifier" => ty
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_string),
        // State<'_, T> → base 'State'
        "generic_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        // &State<...> / &AppHandle → 内側の型
        "reference_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        _ => None,
    }
}

/// function_item が Tauri command 属性 (`#[tauri::command]` / `#[command]`) を持つか判定する。
/// Rust では属性は function_item の前方兄弟 (attribute_item) に並ぶ。
pub(crate) fn rust_fn_has_tauri_command_attr(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut sib = fn_node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {
                if let Ok(text) = s.utf8_text(source) {
                    let inner = text
                        .trim_start_matches("#[")
                        .trim_start_matches("#![")
                        .trim_end_matches(']')
                        .trim();
                    if inner == "tauri::command"
                        || inner.starts_with("tauri::command(")
                        || inner == "command"
                        || inner.starts_with("command(")
                    {
                        return true;
                    }
                }
            }
            // 属性とコメントは読み飛ばし、それ以外に到達したら属性列の終端
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    false
}

/// Tauri command 関数の signature から自動注入型引数を除外して返す。
/// Tauri command でなければ None を返し、呼び出し側で通常の signature 抽出にフォールバックする。
pub(crate) fn normalize_rust_tauri_command_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    s: usize,
    e: usize,
) -> Option<String> {
    if !rust_fn_has_tauri_command_attr(fn_node, source) {
        return None;
    }
    let params = fn_node.child_by_field_name("parameters")?;
    let mut kept: Vec<String> = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "parameter" => {
                let injected = child
                    .child_by_field_name("type")
                    .and_then(|t| rust_type_base_name(t, source))
                    .is_some_and(|n| TAURI_INJECTED_TYPES.contains(&n.as_str()));
                if !injected && let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            "self_parameter" => {
                if let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    let prefix = source.get(s..params.start_byte())?;
    let suffix = source.get(params.end_byte()..e)?;
    let rebuilt = format!(
        "{}({}){}",
        String::from_utf8_lossy(prefix),
        kept.join(", "),
        String::from_utf8_lossy(suffix)
    );
    Some(normalize_signature_whitespace(rebuilt.as_bytes()))
}

/// TS/TSX 関数の signature を抽出し、parameters 直下の `object_pattern`
/// (destructured params) を `{}` に正規化する。
///
/// `function foo({ a, b, c = 0 }: Props)` と `function foo({ a, b }: Props)` は
/// どちらも呼び出し側契約は `: Props` のみで、destructure 中身は内部 binding。
/// 正規化することで Props 拡張に伴う destructure 行の追加が api.mod に出ない。
///
/// 型注釈側の inline object type (`function foo({x}: {x: string, y: number})` の
/// `{x: string, y: number}`) は `type_annotation` 子なので置換対象外。
///
/// 「引数なし `()` から省略可能な destructured 引数追加」の互換性判定は、
/// signature 単独では行わない (型注釈変更だけ起きるケースを誤って互換扱いする
/// リスクがあるため)。両側 signature を見て判定するロジックは
/// [`is_ts_no_arg_to_optional_destructured_compatible`] が detect_api_changes
/// 経路で行う。
pub(crate) fn normalize_typescript_destructure_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    start_byte: usize,
    end_byte: usize,
) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(params) = fn_node.child_by_field_name("parameters") {
        collect_parameter_object_pattern_ranges(params, &mut ranges);
    }
    if ranges.is_empty() {
        if let Some(bytes) = source.get(start_byte..end_byte) {
            return normalize_signature_whitespace(bytes);
        }
        return String::new();
    }
    ranges.sort_by_key(|r| r.0);

    let mut buf: Vec<u8> = Vec::with_capacity(end_byte - start_byte);
    let mut cursor = start_byte;
    for (op_start, op_end) in &ranges {
        if *op_start < cursor || *op_end > end_byte {
            continue;
        }
        if let Some(bytes) = source.get(cursor..*op_start) {
            buf.extend_from_slice(bytes);
        }
        buf.extend_from_slice(b"{}");
        cursor = *op_end;
    }
    if let Some(bytes) = source.get(cursor..end_byte) {
        buf.extend_from_slice(bytes);
    }
    normalize_signature_whitespace(&buf)
}

/// TS/TSX の formal_parameters 直下にある `object_pattern` のバイト範囲を集める。
///
/// パラメータの `type_annotation` (inline object type など) には踏み込まないため、
/// 型注釈側の object type は影響を受けない。required_parameter / optional_parameter の
/// `pattern` フィールドを直接見て object_pattern かを判定する。
pub(crate) fn collect_parameter_object_pattern_ranges(
    params: tree_sitter::Node<'_>,
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" => {
                if let Some(pattern) = child.child_by_field_name("pattern")
                    && pattern.kind() == "object_pattern"
                {
                    ranges.push((pattern.start_byte(), pattern.end_byte()));
                }
            }
            // 無型 JS スタイル: parameter ノードがなく object_pattern が直接子に来る
            // ケース。安全側に倒して同様に正規化する (TS/TSX に限定済み)。
            "object_pattern" => {
                ranges.push((child.start_byte(), child.end_byte()));
            }
            _ => {}
        }
    }
}

/// signature bytes を whitespace で分割して 1 つの space で結合し正規化する。
/// 改行・タブ・連続スペース・末尾の `{` 直前空白を一括で潰す。
pub(crate) fn normalize_signature_whitespace(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
