//! 関数・ブロック内の const の監査は、その字句束縛の利用だけに限定する。

use super::*;

/// ファイル直下や変数以外は既存の名前による監査へ戻す。
pub(super) fn prove_local_variable(
    declaration: Node<'_>,
    parsed: &ParsedRefFile,
    type_name: &str,
) -> Option<bool> {
    prove_variable(declaration, parsed, false, type_name)
}

/// 型注釈の無い const は、module 内の非公開の直下宣言も同じ束縛で監査する。
pub(super) fn prove_inferred_variable(
    declaration: Node<'_>,
    parsed: &ParsedRefFile,
    type_name: &str,
) -> bool {
    prove_variable(declaration, parsed, true, type_name) == Some(true)
}

fn prove_variable(
    declaration: Node<'_>,
    parsed: &ParsedRefFile,
    allow_module: bool,
    type_name: &str,
) -> Option<bool> {
    if declaration.kind() != "variable_declarator" {
        return None;
    }
    let name_node = declaration.child_by_field_name("name")?;
    if name_node.kind() != "identifier" {
        return None;
    }
    let source: &[u8] = &parsed.source;
    let name = name_node.utf8_text(source).ok()?;
    let (scope, binding) = resolve_binding_scope(name_node, source, name, Namespace::Value)?;
    if scope.kind() == "program" {
        if !allow_module {
            return None;
        }
        let mut cursor = scope.walk();
        let module = scope
            .named_children(&mut cursor)
            .any(|node| matches!(node.kind(), "import_statement" | "export_statement"));
        if !module
            || declaration
                .parent()
                .and_then(|node| node.parent())
                .is_some_and(|node| node.kind() == "export_statement")
        {
            return Some(false);
        }
    }
    if !matches!(binding, ScopeBinding::Variable(found) if found.id() == declaration.id()) {
        return Some(false);
    }
    Some(!any_node(scope, Children::Named, |node| {
        // 束縛の無い for-of / for-in と var は、loop の外の束縛へ書き込む。
        if node.kind() == "for_in_statement"
            && node
                .child_by_field_name("kind")
                .is_none_or(|kind| kind.kind() == "var")
            && node
                .child_by_field_name("left")
                .is_some_and(|left| pattern_binds_name(left, source, name))
            && matches!(resolve_binding_scope(node, source, name, Namespace::Value),
                Some((_, ScopeBinding::Variable(found))) if found.id() == declaration.id())
        {
            return true;
        }
        if !matches!(
            node.kind(),
            "identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) {
            return false;
        }
        let Ok(text) = node.utf8_text(source) else {
            return true;
        };
        // escape された名前の正規化はここでは証明しない。
        if text.contains('\\') {
            return true;
        }
        if text != name || node.id() == name_node.id() {
            return false;
        }
        match resolve_binding_scope(node, source, name, Namespace::Value) {
            Some((_, ScopeBinding::Variable(found))) if found.id() == declaration.id() => {
                // 同一スコープの重複宣言も、最初の束縛の利用と誤認しない。
                is_binding_name(node)
                    || node
                        .parent()
                        .is_some_and(|parent| parent.kind() == "export_specifier")
                    || reference_is_written(node)
                    || !node_outside_type_query(node, source, false, Some(type_name))
            }
            Some(_) => false,
            None => true,
        }
    }))
}

fn reference_is_written(node: Node<'_>) -> bool {
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        let target = match parent.kind() {
            "parenthesized_expression" => {
                current = parent;
                continue;
            }
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                "left"
            }
            "update_expression" => "argument",
            _ => return false,
        };
        return parent
            .child_by_field_name(target)
            .is_some_and(|target| target.id() == current.id());
    }
    true
}
