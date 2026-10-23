//! 削除した自由関数と無関係な、座標ごとの定義・値束縛を証明する。

use std::collections::HashSet;
use tree_sitter::Node;

use crate::engine::refs::is_rust_struct_field_non_callable;

pub(super) type Positions = HashSet<(usize, usize)>;

fn position(node: Node<'_>) -> (usize, usize) {
    let p = node.start_position();
    (p.row, p.column)
}

/// 再帰せず、同一解析木を一度だけ列挙する。
fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    loop {
        out.push(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

/// Python は一意な通常のモジュール直下 def と、確定した読み取り位置だけを証明する。
/// 未知の束縛、動的名前解決、構文エラーがあればファイル単位で証明を諦める。
pub(super) fn python_independent_positions(root: Node<'_>, source: &[u8], name: &str) -> Positions {
    if root.has_error() {
        return Positions::new();
    }
    let all = nodes(root);
    if all.iter().any(|n| {
        n.is_missing()
            || n.kind() == "wildcard_import"
            || (n.kind() == "identifier"
                && n.utf8_text(source).is_ok_and(|s| {
                    matches!(
                        s,
                        "globals"
                            | "vars"
                            | "exec"
                            | "eval"
                            | "__import__"
                            | "modules"
                            | "getattr"
                            | "setattr"
                            | "__builtins__"
                            | "__dict__"
                            | "import_module"
                    )
                }))
    }) {
        return Positions::new();
    }
    let definitions: Vec<_> = all
        .iter()
        .copied()
        .filter(|n| {
            n.kind() == "function_definition"
                && n.parent().is_some_and(|p| p.id() == root.id())
                && n.child(0).is_none_or(|c| c.kind() != "async")
                && n.child_by_field_name("name")
                    .and_then(|c| c.utf8_text(source).ok())
                    == Some(name)
        })
        .collect();
    if definitions.len() != 1 {
        return Positions::new();
    }
    let Some(def) = definitions[0].child_by_field_name("name") else {
        return Positions::new();
    };
    let mut out = Positions::new();
    for node in all
        .into_iter()
        .filter(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(name))
    {
        if node.id() == def.id() {
            out.insert(position(node));
            continue;
        }
        let Some(parent) = node.parent() else {
            return Positions::new();
        };
        if parent.kind() == "attribute"
            && parent
                .child_by_field_name("attribute")
                .is_some_and(|n| n.id() == node.id())
        {
            continue;
        }
        if !python_is_load(node) {
            return Positions::new();
        }
        out.insert(position(node));
    }
    out
}

/// tree-sitter に Load フラグは無いため、読み取りと確定できる親だけを許す。
pub(super) fn python_is_load(mut node: Node<'_>) -> bool {
    // 括弧・tuple の入れ子は入力次第で深くなるため、再帰せずループで親方向へ辿る。
    loop {
        let Some(parent) = node.parent() else {
            return false;
        };
        let in_field = |field| {
            parent
                .child_by_field_name(field)
                .is_some_and(|n| n.id() == node.id())
        };
        match parent.kind() {
            "call" => return in_field("function"),
            "attribute" => return in_field("object"),
            "assignment" | "augmented_assignment" => return in_field("right"),
            "named_expression" | "keyword_argument" => return in_field("value"),
            "argument_list"
            | "return_statement"
            | "expression_statement"
            | "binary_operator"
            | "unary_operator"
            | "boolean_operator"
            | "comparison_operator"
            | "not_operator"
            | "conditional_expression" => return true,
            // tuple/list は代入先にも現れるため、親の読み取り位置まで辿る。
            "parenthesized_expression" | "tuple" | "list" | "set" => node = parent,
            _ => return false,
        }
    }
}

fn has_attributes(node: Node<'_>) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(n) = previous {
        match n.kind() {
            "attribute_item" => return true,
            "line_comment" | "block_comment" => previous = n.prev_named_sibling(),
            _ => break,
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|n| n.kind() == "attribute_item")
}

/// Rust の top-level fn 定義と、通常の let / 引数の有効範囲を別々に返す。
/// refs の出現自体は変更せず、削除候補の帰属判定だけで用いる。
pub(super) fn rust_scope_positions(
    root: Node<'_>,
    source: &[u8],
    name: &str,
) -> (Positions, Positions) {
    if root.has_error() {
        return (Positions::new(), Positions::new());
    }
    let all = nodes(root);
    if all.iter().any(|n| n.is_missing()) {
        return (Positions::new(), Positions::new());
    }
    let named = |n: Node<'_>| {
        n.child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            == Some(name)
    };
    let defs: Vec<_> = all
        .iter()
        .copied()
        .filter(|n| n.kind() == "function_item" && named(*n))
        .collect();
    let mut independent = Positions::new();
    if defs.len() == 1
        && defs[0].parent().is_some_and(|p| p.id() == root.id())
        && !has_attributes(defs[0])
        && let Some(n) = defs[0].child_by_field_name("name")
    {
        independent.insert(position(n));
    }
    // Item macros may introduce declarations not present in this tree. Expression
    // macros cannot introduce bindings into the surrounding function's block.
    let opaque_item_macro = all.iter().any(|node| {
        node.kind() == "macro_invocation"
            && node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "source_file" | "declaration_list")
                    || (parent.kind() == "expression_statement"
                        && parent.parent().is_some_and(|p| {
                            matches!(p.kind(), "source_file" | "declaration_list")
                        }))
            })
    });
    let scope = crate::engine::rust_scope::RustScopeIndex::build(root, source);
    let mut local = Positions::new();
    for node in all {
        if node.utf8_text(source).ok() != Some(name) {
            continue;
        }
        if is_rust_struct_field_non_callable(node) {
            local.insert(position(node));
            continue;
        }
        // Attribute macros can rewrite the entire function, including bindings.
        let mut parent = node.parent();
        let mut attributed_function = false;
        while let Some(ancestor) = parent {
            if ancestor.kind().ends_with("_item") && has_attributes(ancestor) {
                attributed_function = true;
                break;
            }
            parent = ancestor.parent();
        }
        if !opaque_item_macro
            && !attributed_function
            && scope.as_ref().is_some_and(|index| {
                index.resolve(node, source)
                    == crate::engine::lexical_binding::LexicalBinding::RustValueBinding
            })
        {
            local.insert(position(node));
        }
    }
    (independent, local)
}

/// ファイル直下に同名の自由関数を持つ Rust ファイルで、そのファイルのモジュールから修飾なしで
/// 名前を引く位置。こうした出現はその関数か、より内側の束縛に解決し、ほかのファイルの宣言を
/// 指さない。inline の `mod` の中、パスの一部、use、extern ブロック、マクロと属性の中、属性の
/// 付いた item の中は含めず、マクロを含むファイルでは何も返さない。
pub(super) fn rust_file_scope_positions(root: Node<'_>, source: &[u8], name: &str) -> Positions {
    let all = nodes(root);
    if all
        .iter()
        .any(|node| matches!(node.kind(), "macro_invocation" | "macro_definition"))
    {
        return Positions::new();
    }
    let mut positions = Positions::new();
    for node in all {
        if node.kind() != "identifier" || node.utf8_text(source).ok() != Some(name) {
            continue;
        }
        let mut ancestor = node.parent();
        let mut module_scope = true;
        while let Some(parent) = ancestor {
            // 属性マクロは item の本体ごと書き換えうる。
            if matches!(
                parent.kind(),
                "scoped_identifier"
                    | "scoped_type_identifier"
                    | "use_declaration"
                    | "mod_item"
                    | "foreign_mod_item"
                    | "token_tree"
                    | "attribute_item"
                    | "inner_attribute_item"
            ) || (parent.kind().ends_with("_item") && has_attributes(parent))
            {
                module_scope = false;
                break;
            }
            ancestor = parent.parent();
        }
        if module_scope {
            positions.insert(position(node));
        }
    }
    positions
}

/// 拡張子の違いと package/module の代替配置を同一のモジュールとして扱う。
pub(super) fn module_key(path: &str) -> String {
    let path = std::path::Path::new(path);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "__init__" | "mod") {
        path.parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .to_string_lossy()
            .into_owned()
    } else {
        path.with_extension("").to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn python_is_load_recursive(node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        let in_field = |field| {
            parent
                .child_by_field_name(field)
                .is_some_and(|n| n.id() == node.id())
        };
        match parent.kind() {
            "call" => in_field("function"),
            "attribute" => in_field("object"),
            "assignment" | "augmented_assignment" => in_field("right"),
            "named_expression" | "keyword_argument" => in_field("value"),
            "argument_list"
            | "return_statement"
            | "expression_statement"
            | "binary_operator"
            | "unary_operator"
            | "boolean_operator"
            | "comparison_operator"
            | "not_operator"
            | "conditional_expression" => true,
            "parenthesized_expression" | "tuple" | "list" | "set" => {
                python_is_load_recursive(parent)
            }
            _ => false,
        }
    }

    /// ループ版が、浅い入力のすべてのノードで再帰版と同じ判定を返す。
    #[test]
    fn python_is_load_matches_recursive_version() {
        let src = "a, (b, [c]) = ((d), e)\nf((g, [h]))\nx = {i, (j)}\nk.l = (m)\nn += (o, p)\n\
                   return_value = q if (r) else s\nfor t in (u, v): pass\ndel (w)\nprint(not (y))\n\
                   z = [aa for bb in (cc)]\n";
        let tree =
            crate::engine::parser::parse_source(src.as_bytes(), crate::language::LangId::Python)
                .unwrap();
        let mut loads = 0;
        let mut stores = 0;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                let expected = python_is_load_recursive(node);
                assert_eq!(python_is_load(node), expected, "{}", node.to_sexp());
                if node.kind() == "identifier" {
                    if expected {
                        loads += 1;
                    } else {
                        stores += 1;
                    }
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        assert!(
            loads >= 5 && stores >= 5,
            "読み取りと書き込みの両方を比べる"
        );
    }
}
