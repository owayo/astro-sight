//! 「呼び出し式は無変更だが、引数に渡している共有 `const` の定義側が同一 diff 内で
//! 更新されている」ケースを `is_modified_closed_in_diff` の追加証拠として判定する
//! (Issue 2026-08-05-api-mod-callers-updated-indirectly のパターン C)。
//!
//! 既存の閉包判定は「参照行、または enclosing call_expression の行範囲が実変更行と交差するか」
//! で見ており、`buildX(SHARED_DEPS)` のように**呼び出し式そのものが無変更**で、実際の追随が
//! `SHARED_DEPS` 定義側で行われているケースを未更新 caller と誤判定していた。
//!
//! 証拠の強さを既存判定と同程度に保つため、ガードは意図的に狭い。次をすべて満たす場合だけ
//! 「追随済み」とみなす (1 つでも判定不能なら false = blocking 維持):
//!
//! - 参照ファイルの言語が TypeScript / TSX (必須プロパティ追加という概念が型に依存するため)
//! - signature 変更が「object type literal 引数への必須プロパティ追加のみ」で、変更された
//!   引数が 1 つに特定できている (`ts_signature::detect_added_required_object_props`)
//! - 参照が call_expression の callee で、対応する実引数が**裸の identifier** 1 個
//!   (spread / object literal / メンバーアクセス / 関数呼び出しは対象外)
//! - その identifier が**同一ファイル内**で `const NAME = { ... }` (直接の object literal、
//!   `as const` / 冗長括弧の透過は許容) に一意に束縛されている。同名の binding が他に
//!   1 つでもあれば shadow の可能性があるため不成立
//! - 追加された必須プロパティが**すべて**その object literal に存在し、かつ各プロパティの
//!   行が同一 diff の**実変更行**に含まれる (キー集合の一致だけでは「元からあった」ケースを
//!   区別できないため、追加行であることまで要求する)
//!
//! import された定数・spread・alias・factory 呼び出しの追跡 (1 hop 以上) は対象外。
//! module resolution / re-export / 循環 import まで責務が広がるため別 Issue とする。

use std::collections::HashSet;

use crate::commands::api_changes::ts_signature::{
    AddedRequiredObjectProps, collect_flat_object_keys, object_key_text, unwrap_to_object_literal,
};
use crate::language::LangId;

/// `call_node` の第 `param_index` 実引数が裸の identifier なら、その名前を返す。
fn bare_identifier_argument<'a>(
    call_node: tree_sitter::Node<'a>,
    param_index: usize,
    source: &'a [u8],
) -> Option<&'a str> {
    let args = call_node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let children: Vec<tree_sitter::Node> = args.named_children(&mut cursor).collect();
    // spread が混ざると実引数と仮引数の対応が崩れるため不成立。
    if children.iter().any(|c| c.kind() == "spread_element") {
        return None;
    }
    let arg = children.get(param_index)?;
    if arg.kind() != "identifier" {
        return None;
    }
    arg.utf8_text(source).ok()
}

/// ファイル内で `name` を束縛している **binding コンテナ** を列挙する。
///
/// 走査の単位を「identifier とその親の field 名」ではなく「binding を導入する構文ノード」に
/// している理由: identifier の親 field だけを見ると bare arrow パラメータ (`X => ...` の
/// `parameter` field)・catch パラメータ・renamed destructuring (`{ deps: X }` の
/// `pair_pattern` value)・配列/rest destructuring (`[X]` / `...X`) を取りこぼし、
/// **shadow を見逃して blocking を解除する fail-open** になる。
///
/// パターン内部の再帰 (destructuring / rest / default) と import binding の判定は
/// `js_ts_shadow` の `pattern_binds_name` / `import_binds_name` をそのまま共有する
/// (同じロジックを 2 箇所に持つと片方だけ直る形でドリフトするため)。
///
/// 同じ binding が複数のコンテナ経由で二重に数えられることはある (`formal_parameters` と
/// その子 `required_parameter` など) が、過大側 = 一意性チェックで不成立 = blocking 維持
/// なので安全側に倒れる。本判定が成立してほしいケース (トップレベルの `const NAME = {...}`)
/// は `variable_declarator` 1 個だけで数えられ、二重計上は起きない。
fn collect_bindings_named<'tree>(
    root: tree_sitter::Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<tree_sitter::Node<'tree>> {
    use crate::commands::api_changes::js_ts_shadow::{import_binds_name, pattern_binds_name};

    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        let binds = match node.kind() {
            "variable_declarator" => node
                .child_by_field_name("name")
                .is_some_and(|n| pattern_binds_name(n, source, name)),
            // TS の型注釈付きパラメータ。
            "required_parameter" | "optional_parameter" => node
                .child_by_field_name("pattern")
                .is_some_and(|n| pattern_binds_name(n, source, name)),
            // `catch (X)` / bare arrow `X => ...` はどちらも `parameter` field。
            "catch_clause" | "arrow_function" => node
                .child_by_field_name("parameter")
                .is_some_and(|n| pattern_binds_name(n, source, name)),
            // JS 形式の formal_parameters 直下の bare pattern
            // (`function f(X)` / `function f({ deps: X })` / `function f(...X)`)。
            // TS の wrapper (`required_parameter` / `optional_parameter`) は上の arm が
            // pattern field だけを見るので、ここで重ねて全体を走査しない — 走査すると
            // default 値 (`function f(other = X)` の `value` field) まで binding と誤認して
            // 降格できるはずのケースを blocking に残す false positive になる。
            "formal_parameters" => {
                let mut c = node.walk();
                node.named_children(&mut c)
                    .filter(|child| {
                        !matches!(child.kind(), "required_parameter" | "optional_parameter")
                    })
                    .any(|child| pattern_binds_name(child, source, name))
            }
            // `for (const X of xs)` / `for (X of xs)` の loop 変数 (bare pattern)。
            "for_in_statement" => node
                .child_by_field_name("left")
                .is_some_and(|n| pattern_binds_name(n, source, name)),
            // `using X = acquire()` / `await using X = ...` は tree-sitter-typescript では
            // 匿名 `using` トークン付きの assignment_expression になり、専用ノードを持たない。
            // 素の再代入 (`X = v`) も左辺の値が静的に決まらなくなる点は同じなので、
            // どちらも binding 扱いにして不成立へ倒す (const への再代入は TS では起こらないため
            // 本判定を成立させたいケースには当たらない)。
            "assignment_expression" | "augmented_assignment_expression" => node
                .child_by_field_name("left")
                .is_some_and(|n| pattern_binds_name(n, source, name)),
            "import_statement" => import_binds_name(node, source, name),
            // `name` field を持たず**先頭 named child がローカル名**になる import 系:
            // `import X = Legacy.Value;` (import_alias) と
            // `import X = require("./m");` (import_require_clause)。
            // 右辺 (nested_identifier / string) まで名前照合すると別物を binding と誤認するため
            // 先頭 child だけを見る。
            "import_alias" | "import_require_clause" => {
                node.named_child(0)
                    .and_then(|n| leftmost_identifier_text(n, source))
                    == Some(name)
            }
            // 名前を導入するその他の構文は **`name` field を持つノード**として構造的に拾う。
            // kind の列挙 (`*_declaration` / `class` / `internal_module` / ...) を維持すると
            // 文法の増分に追随できず、`abstract_class_declaration` /
            // `function_signature` (`declare function X()`) のような取りこぼし
            // = shadow 見逃し = fail-open を繰り返す (実際に 3 度踏んだ)。
            // 型空間だけの宣言 (interface / type alias) や class member まで数える過剰検出に
            // なるが、過剰側は一意性チェックで不成立 = blocking 維持に倒れるだけで安全。
            //
            // 照合は `name` field 全体のテキストではなく**左端 identifier** で行う。
            // `namespace X.Legacy {}` の name は `nested_identifier` で、完全一致だと
            // ローカルに導入される `X` を取りこぼす。
            _ => {
                node.child_by_field_name("name")
                    .and_then(|n| leftmost_identifier_text(n, source))
                    == Some(name)
            }
        };
        if binds {
            found.push(node);
        }
    }
    found
}

/// 名前ノードの**左端 identifier** のテキストを返す。
///
/// `namespace X.Legacy {}` の `name` は `nested_identifier` で、ローカルに導入されるのは
/// 左端の `X` だけ。完全一致で照合すると取りこぼす (= shadow 見逃し = fail-open)。
/// identifier / type_identifier などの葉はそのまま返す。
fn leftmost_identifier_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut cur = node;
    loop {
        match cur.named_child(0) {
            Some(child) => cur = child,
            None => return cur.utf8_text(source).ok(),
        }
    }
}

/// `variable_declarator` が `const` 宣言かを判定する。`let` / `var` は再代入されうるため
/// 「定義側の更新 = 呼び出し時の値の更新」と言い切れず対象外。
fn declarator_is_const(declarator: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = declarator.parent() else {
        return false;
    };
    if parent.kind() != "lexical_declaration" {
        return false;
    }
    let mut cursor = parent.walk();
    parent.children(&mut cursor).any(|c| c.kind() == "const")
}

/// object literal の直下 property のうち、`name` に一致するキーの行 (0-indexed) を返す。
fn property_line_for_key(obj: tree_sitter::Node<'_>, source: &[u8], name: &str) -> Option<usize> {
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        let key_text = match child.kind() {
            "pair" => object_key_text(child.child_by_field_name("key")?, source),
            "shorthand_property_identifier" => child.utf8_text(source).ok().map(str::to_string),
            _ => None,
        };
        if key_text.as_deref() == Some(name) {
            return Some(child.start_position().row);
        }
    }
    None
}

/// 参照 (call の callee) が「共有 const 引数の定義側更新」で追随済みかを判定する。
///
/// `call_node` は参照を callee に持つ call_expression、`changed_lines` は参照ファイルの
/// 実変更行 (0-indexed)。判定不能・条件不成立はすべて false (= blocking 維持)。
pub(crate) fn closed_via_local_const_argument(
    lang: LangId,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    call_node: tree_sitter::Node<'_>,
    added: &AddedRequiredObjectProps,
    changed_lines: &HashSet<usize>,
) -> bool {
    if !matches!(lang, LangId::Typescript | LangId::Tsx) {
        return false;
    }
    let Some(const_name) = bare_identifier_argument(call_node, added.param_index, source) else {
        return false;
    };
    let bindings = collect_bindings_named(root, source, const_name);
    // 同名 binding が複数 = shadow の可能性があり、どの値が渡るか静的に決められない。
    // 唯一の binding が `const` の variable_declarator でなければ (パラメータ / import /
    // 関数宣言 / let・var) 対象外。
    let [declarator] = bindings.as_slice() else {
        return false;
    };
    let declarator = *declarator;
    if declarator.kind() != "variable_declarator" || !declarator_is_const(declarator) {
        return false;
    }
    let Some(obj) = declarator
        .child_by_field_name("value")
        .and_then(unwrap_to_object_literal)
    else {
        return false;
    };
    // spread / computed key を含む object は静的にキー集合を確定できない。
    let Some(keys) = collect_flat_object_keys(obj, source) else {
        return false;
    };
    added.names.iter().all(|name| {
        keys.contains(name)
            && property_line_for_key(obj, source, name)
                .is_some_and(|line| changed_lines.contains(&line))
    })
}
