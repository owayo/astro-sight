//! 削除した自由関数と無関係な、座標ごとの定義・値束縛を証明する。

use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

use crate::engine::refs::{
    RustPatternBindingCache, is_rust_shadowable_value_identifier,
    is_rust_struct_field_non_callable, rust_pattern_binds_name,
};

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
fn python_is_load(node: Node<'_>) -> bool {
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
        // tuple/list は代入先にも現れるため、親の読み取り位置まで辿る。
        "parenthesized_expression" | "tuple" | "list" | "set" => python_is_load(parent),
        _ => false,
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

fn contains(container: Node<'_>, node: Node<'_>) -> bool {
    container.start_byte() <= node.start_byte() && node.end_byte() <= container.end_byte()
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
    let cache = RustPatternBindingCache::default();
    // 展開後の use / const / 値束縛はこの解析木には無い。マクロのあるファイルでは
    // パターンの名前解決を証明せず、従来の保守的な参照判定へ戻す。
    let has_macros = all
        .iter()
        .any(|n| matches!(n.kind(), "macro_invocation" | "macro_definition"));
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
    // 入れ子の同名 item は外側 let より優先され得る。解決できなければ値束縛を証明しない。
    let nested_item = all.iter().any(|n| {
        matches!(
            n.kind(),
            "function_item" | "struct_item" | "enum_item" | "mod_item"
        ) && named(*n)
            && n.parent().is_none_or(|p| p.id() != root.id())
    });
    let mut lets: HashMap<usize, Vec<(Node<'_>, Node<'_>)>> = HashMap::new();
    let mut parameters: HashMap<usize, Vec<Node<'_>>> = HashMap::new();
    if !nested_item && !has_macros {
        for &n in &all {
            if n.kind() == "let_declaration"
                && !has_attributes(n)
                && let Some(block) = n.parent().filter(|p| p.kind() == "block")
                && let Some(pattern) = n.child_by_field_name("pattern")
                && rust_pattern_binds_name(pattern, name, source, &cache)
            {
                lets.entry(block.id()).or_default().push((n, pattern));
            }
            if n.kind() == "parameter"
                && !has_attributes(n)
                && let Some(params) = n.parent().filter(|p| p.kind() == "parameters")
                && let Some(function) = params.parent().filter(|p| p.kind() == "function_item")
                && let Some(pattern) = n.child_by_field_name("pattern")
                && rust_pattern_binds_name(pattern, name, source, &cache)
            {
                parameters.entry(function.id()).or_default().push(pattern);
            }
        }
    }
    let mut local = Positions::new();
    for &node in &all {
        if node.utf8_text(source).ok() != Some(name) {
            continue;
        }
        if is_rust_struct_field_non_callable(node) {
            local.insert(position(node));
            continue;
        }
        if nested_item || has_macros || !is_rust_shadowable_value_identifier(node) {
            continue;
        }
        let mut current = node;
        let mut proven = false;
        while let Some(parent) = current.parent() {
            if matches!(
                parent.kind(),
                "token_tree" | "macro_invocation" | "macro_definition" | "attribute_item"
            ) {
                proven = false;
                break;
            }
            if parent.kind() == "block"
                && lets.get(&parent.id()).is_some_and(|bindings| {
                    bindings.iter().any(|(decl, pattern)| {
                        contains(*pattern, node) || node.start_byte() >= decl.end_byte()
                    })
                })
            {
                proven = true;
            }
            if parent.kind() == "function_item" {
                if let Some(body) = parent.child_by_field_name("body")
                    && parameters.get(&parent.id()).is_some_and(|bindings| {
                        bindings
                            .iter()
                            .any(|pattern| contains(*pattern, node) || contains(body, node))
                    })
                {
                    proven = true;
                }
                break;
            }
            if matches!(
                parent.kind(),
                "mod_item" | "impl_item" | "const_item" | "static_item"
            ) {
                break;
            }
            current = parent;
        }
        if proven {
            local.insert(position(node));
        }
    }
    (independent, local)
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
