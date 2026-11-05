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
    prove_variable_with_container(declaration, parsed, allow_module, type_name, false)
}

pub(super) fn prove_array_variable(declaration: Node<'_>, parsed: &ParsedRefFile) -> bool {
    prove_variable_with_container(declaration, parsed, true, "", true) == Some(true)
}

fn prove_variable_with_container(
    declaration: Node<'_>,
    parsed: &ParsedRefFile,
    allow_module: bool,
    type_name: &str,
    container: bool,
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
                    || (container && array_reference_is_unsafe(node, source))
                    || !node_outside_type_query(
                        node,
                        source,
                        container && !array_length_read(node, source),
                        (!container).then_some(type_name),
                    )
            }
            Some(_) => false,
            None => true,
        }
    }))
}

fn array_length_read(node: Node<'_>, source: &[u8]) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "member_expression"
            && parent
                .child_by_field_name("object")
                .is_some_and(|object| object.id() == node.id())
            && parent
                .child_by_field_name("property")
                .and_then(|key| key.utf8_text(source).ok())
                == Some("length")
    })
}

fn array_reference_is_unsafe(node: Node<'_>, source: &[u8]) -> bool {
    let length = array_length_read(node, source);
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        let field_is = |field| {
            parent
                .child_by_field_name(field)
                .is_some_and(|child| child.id() == current.id())
        };
        match parent.kind() {
            "parenthesized_expression" => current = parent,
            "member_expression" | "subscript_expression" if field_is("object") => current = parent,
            "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "non_null_expression"
            | "type_query" => return true,
            "assignment_expression" | "augmented_assignment_expression" if field_is("left") => {
                return true;
            }
            "update_expression" | "unary_expression" if field_is("argument") => {
                return parent.kind() == "update_expression"
                    || parent
                        .child_by_field_name("operator")
                        .is_some_and(|operator| operator.kind() == "delete");
            }
            "call_expression" | "new_expression"
                if field_is("function") || field_is("constructor") =>
            {
                return true;
            }
            "arguments" => return !length,
            "return_statement" | "arrow_function" => {
                return !length && current.kind() != "member_expression";
            }
            _ if value_flows_to_parent(current, parent) => current = parent,
            _ => return false,
        }
    }
    true
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
