use tree_sitter::Node;

use crate::language::LangId;
use crate::models::location::Range;

use super::{is_js_function_body, node_for_symbol_range};

/// シンボルが関数/メソッド本体内のローカルスコープ定義かどうかを判定。
///
/// 関数内の `const`/`let`/`var` 等はファイル外への影響を持たないため、
/// impact 分析の cross-file 起点から除外できる。
/// 未対応言語では保守的に `false`（ローカルではない＝除外しない）を返す。
pub fn is_local_scope_symbol(
    root: Node,
    _source: &[u8],
    lang_id: LangId,
    symbol_range: &Range,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false; // 保守的: ノード未検出はローカルと判定しない
    };

    match lang_id {
        LangId::Typescript | LangId::Tsx | LangId::Javascript => {
            has_enclosing_function_body_js(node)
        }
        LangId::Rust => has_enclosing_function_body_rust(node),
        LangId::Python => has_enclosing_function_body_python(node),
        LangId::Go => has_enclosing_function_body_go(node),
        LangId::Java | LangId::Kotlin => has_enclosing_function_body_jvm(node),
        _ => false, // 未対応言語は保守的にローカルと判定しない
    }
}

/// JS/TS: 祖先に関数本体 (statement_block) があるかチェック。
fn has_enclosing_function_body_js(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if is_js_function_body(n) {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Rust: 祖先に function_item の block があるかチェック。
fn has_enclosing_function_body_rust(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block" && n.parent().is_some_and(|p| p.kind() == "function_item") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Python: 祖先に function_definition の block があるかチェック。
fn has_enclosing_function_body_python(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block"
            && n.parent()
                .is_some_and(|p| p.kind() == "function_definition")
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Go: 祖先に function/method の block があるかチェック。
fn has_enclosing_function_body_go(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block"
            && n.parent().is_some_and(|p| {
                p.kind() == "function_declaration" || p.kind() == "method_declaration"
            })
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Java/Kotlin: 祖先に method/constructor の block があるかチェック。
fn has_enclosing_function_body_jvm(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block"
            && n.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    "method_declaration" | "constructor_declaration" | "function_declaration"
                )
            })
        {
            return true;
        }
        current = n.parent();
    }
    false
}
