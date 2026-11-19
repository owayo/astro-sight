use super::*;

/// 必須追加の証明で扱う、引数なしの void メソッド・関数型プロパティ。
pub(crate) fn is_required_callback(member: Node<'_>, source: &[u8]) -> bool {
    let signature = match member.kind() {
        "method_signature" if !accessor_or_generator(member) => Some(member),
        "property_signature" => member
            .child_by_field_name("type")
            .and_then(only_named_child)
            .filter(|ty| ty.kind() == "function_type"),
        _ => None,
    };
    signature.is_some_and(|signature| is_void_signature(signature, source))
}

fn is_void_signature(signature: Node<'_>, source: &[u8]) -> bool {
    let returns = signature.child_by_field_name("return_type");
    let returns = match returns {
        Some(annotation) if annotation.kind() == "type_annotation" => only_named_child(annotation),
        _ => returns,
    };
    has_no_parameters(signature)
        && returns.is_some_and(|ty| {
            ty.kind() == "predefined_type" && ty.utf8_text(source).ok() == Some("void")
        })
}

fn has_no_parameters(callable: Node<'_>) -> bool {
    callable.child_by_field_name("type_parameters").is_none()
        && callable
            .child_by_field_name("parameters")
            .is_some_and(|parameters| {
                parameters.kind() == "formal_parameters" && first_named_child(parameters).is_none()
            })
}

fn accessor_or_generator(node: Node<'_>) -> bool {
    ["get", "set", "*"]
        .iter()
        .any(|token| has_token(node, token))
}

fn is_callback_value(value: Node<'_>) -> bool {
    matches!(
        value.kind(),
        "arrow_function" | "function_expression" | "function_declaration" | "method_definition"
    ) && has_no_parameters(value)
        && value.child_by_field_name("body").is_some()
        && !accessor_or_generator(value)
}

pub(super) fn proves_callback(value: Node<'_>, parsed: &ParsedRefFile) -> bool {
    let Some(value) = unwrap_parens(value) else {
        return false;
    };
    is_callback_value(value)
        || (matches!(value.kind(), "identifier" | "shorthand_property_identifier")
            && callback_binding(value, &parsed.source).is_some_and(is_callback_value))
}

fn callback_binding<'tree>(value: Node<'tree>, source: &[u8]) -> Option<Node<'tree>> {
    let name = value.utf8_text(source).ok()?;
    let (scope, binding) = resolve_binding_scope(value, source, name, Namespace::Value)?;
    match binding {
        ScopeBinding::Variable(declaration)
            if super::super::ts_const_arg::declarator_is_const(declaration)
                && declaration.child_by_field_name("name")?.kind() == "identifier"
                && declaration.end_byte() < value.start_byte() =>
        {
            // 束縛の検索では識別子の Unicode escape を展開しない。
            if source.windows(2).any(|bytes| bytes == br"\u") {
                return None;
            }
            if let Some(annotation) = declaration.child_by_field_name("type") {
                let ty = only_named_child(annotation)?;
                if ty.kind() != "function_type" || !is_void_signature(ty, source) {
                    return None;
                }
            }
            unwrap_parens(declaration.child_by_field_name("value")?)
        }
        ScopeBinding::FunctionDeclaration if scope.kind() == "program" => {
            let mut cursor = scope.walk();
            let is_module = scope.named_children(&mut cursor).any(|statement| {
                matches!(statement.kind(), "import_statement" | "export_statement")
            });
            if !is_module
                || !crate::engine::js_occurrence::unique_direct_function(scope, source, name)
            {
                return None;
            }
            scope
                .named_children(&mut cursor)
                .map(|statement| {
                    if statement.kind() == "export_statement" {
                        statement
                            .child_by_field_name("declaration")
                            .unwrap_or(statement)
                    } else {
                        statement
                    }
                })
                .find(|declaration| {
                    declaration.kind() == "function_declaration"
                        && declaration
                            .child_by_field_name("name")
                            .and_then(|ident| ident.utf8_text(source).ok())
                            == Some(name)
                })
        }
        _ => None,
    }
}
