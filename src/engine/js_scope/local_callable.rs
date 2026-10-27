//! 閉包判定で別の関数だと証明できる、関数内の直接関数値 (Issue #162)。

use tree_sitter::Node;

use super::pattern_binds_name;

pub(super) fn is_stable_local_callable(
    reference: Node<'_>,
    declaration: Node<'_>,
    source: &[u8],
    name: &str,
) -> bool {
    let Some(mut value) = declaration.child_by_field_name("value") else {
        return false;
    };
    while value.kind() == "parenthesized_expression" && value.named_child_count() == 1 {
        value = value.named_child(0).unwrap();
    }
    if !matches!(value.kind(), "arrow_function" | "function_expression") {
        return false;
    }
    let Some(statement) = declaration.parent().filter(|parent| {
        matches!(
            parent.kind(),
            "lexical_declaration" | "variable_declaration"
        )
    }) else {
        return false;
    };
    // ファイル直下・ループ初期化子・条件付き var の持出しは広げない。
    let Some(block) = statement.parent().filter(|n| n.kind() == "statement_block") else {
        return false;
    };
    if reference.start_byte() < declaration.end_byte() || reference.end_byte() > block.end_byte() {
        return false; // 初期化より前の参照や自己参照は証明しない。
    }
    let mut owner = block;
    loop {
        if matches!(
            owner.kind(),
            "function_declaration" | "function_expression" | "arrow_function" | "method_definition"
        ) {
            break;
        }
        let Some(parent) = owner.parent().filter(|n| n.kind() != "program") else {
            return false;
        };
        owner = parent;
    }
    // エスケープされた同名識別子による書換えを、表記の違いで見落とさない。
    if source[owner.byte_range()].contains(&b'\\') {
        return false;
    }
    let mut pending = vec![owner];
    let mut visited = 0;
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > 100_000 || node.kind() == "with_statement" {
            return false;
        }
        let binding = match node.kind() {
            "variable_declarator" if node.id() != declaration.id() => {
                node.child_by_field_name("name")
            }
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "enum_declaration"
            | "internal_module" => node.child_by_field_name("name"),
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                node.child_by_field_name("left")
            }
            "update_expression" => node.child_by_field_name("argument"),
            _ => None,
        };
        if binding.is_some_and(|binding| pattern_binds_name(binding, source, name)) {
            return false;
        }
        if node.kind() == "identifier" && node.utf8_text(source).ok() == Some("eval") {
            return false;
        }
        // 内側の closure からの書込みも対象。別束縛の書込みも安全側に保留する。
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::language::LangId;

    #[test]
    fn local_callable_node_kinds_exist_in_js_ts_and_tsx() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let language = lang.ts_language();
            for kind in [
                "arrow_function",
                "function_expression",
                "parenthesized_expression",
                "lexical_declaration",
                "variable_declaration",
                "statement_block",
                "function_declaration",
                "method_definition",
                "program",
                "with_statement",
                "variable_declarator",
                "generator_function_declaration",
                "class_declaration",
                "assignment_expression",
                "augmented_assignment_expression",
                "for_in_statement",
                "update_expression",
                "identifier",
            ] {
                tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
            }
            if lang != LangId::Javascript {
                for kind in ["enum_declaration", "internal_module"] {
                    tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
                }
            }
        }
    }
}
