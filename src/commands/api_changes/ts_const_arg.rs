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

/// ファイル内で `name` を束縛している identifier ノードを列挙する。
///
/// 「binding かどうか」は親の field 名 (`name` / `pattern` / `left`) と親の kind
/// (import 系) で**過剰気味に**判定する。取りこぼす (= binding を見逃す) と shadow を
/// 見落として fail-open になるため、迷ったら binding 扱いにして候補数を増やし、
/// 一意性チェックで不成立に倒す。
fn collect_bindings_named<'tree>(
    root: tree_sitter::Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<tree_sitter::Node<'tree>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "identifier" && node.kind() != "shorthand_property_identifier_pattern" {
            continue;
        }
        if node.utf8_text(source).ok() != Some(name) {
            continue;
        }
        let Some(parent) = node.parent() else {
            continue;
        };
        let is_import_binding = parent.kind().starts_with("import")
            || parent.kind() == "namespace_import"
            || parent.kind() == "namespace_export";
        let is_field_binding = ["name", "pattern", "left"]
            .iter()
            .any(|field| parent.child_by_field_name(field) == Some(node));
        // destructuring pattern 内の shorthand (`const { X } = y`) は field を持たない。
        let is_pattern_shorthand = node.kind() == "shorthand_property_identifier_pattern";
        if is_import_binding || is_field_binding || is_pattern_shorthand {
            found.push(node);
        }
    }
    found
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
    let [binding] = bindings.as_slice() else {
        return false;
    };
    let Some(declarator) = binding.parent() else {
        return false;
    };
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
