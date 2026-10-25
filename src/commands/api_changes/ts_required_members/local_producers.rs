//! 関数・ブロック内の const の監査は、その字句束縛の利用だけに限定する。

use super::*;

/// ファイル直下や変数以外は既存の名前による監査へ戻す。
pub(super) fn prove_local_variable(declaration: Node<'_>, parsed: &ParsedRefFile) -> Option<bool> {
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
        return None;
    }
    if !matches!(binding, ScopeBinding::Variable(found) if found.id() == declaration.id()) {
        return Some(false);
    }
    Some(!any_node(scope, Children::Named, |node| {
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
                is_binding_name(node) || !node_outside_type_query(node, source, false)
            }
            Some(_) => false,
            None => true,
        }
    }))
}
