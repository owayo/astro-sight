//! JS / TS / TSX の変更済み直接呼び出しが、変更後の引数個数を満たすか確認する。

use tree_sitter::Node;

use super::ref_index::{ApiClosureCaches, parsed_ref_file};
use crate::language::LangId;
use crate::models::reference::SymbolReference;

#[derive(Clone, Copy)]
pub(super) struct CallContract {
    min: usize,
    max: Option<usize>,
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(source.get(node.byte_range())?).ok()
}

fn contains_identifier(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(
            current.kind(),
            "identifier" | "property_identifier" | "shorthand_property_identifier_pattern"
        ) && text(current, source) == Some(name)
        {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

fn unique_function<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<Node<'a>> {
    let mut found = None;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let declaration = if statement.kind() == "export_statement" {
            statement
                .child_by_field_name("declaration")
                .unwrap_or(statement)
        } else {
            statement
        };
        match declaration.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature"
                if declaration
                    .child_by_field_name("name")
                    .and_then(|node| text(node, source))
                    == Some(name) =>
            {
                if declaration.kind() == "function_signature" || found.is_some() {
                    return None;
                }
                found = Some(declaration);
            }
            _ => {}
        }
    }
    found
}

fn import_binds_name(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let Some(clause) = node
        .named_child(0)
        .filter(|child| child.kind() == "import_clause")
    else {
        return false;
    };
    let mut cursor = clause.walk();
    clause
        .named_children(&mut cursor)
        .any(|part| match part.kind() {
            "identifier" | "namespace_import" => contains_identifier(part, source, name),
            "named_imports" => {
                let mut specifier_cursor = part.walk();
                part.named_children(&mut specifier_cursor).any(|specifier| {
                    if specifier.kind() != "import_specifier" {
                        return false;
                    }
                    specifier
                        .child_by_field_name("alias")
                        .or_else(|| specifier.child_by_field_name("name"))
                        .and_then(|binding| text(binding, source))
                        == Some(name)
                })
            }
            _ => false,
        })
}

fn has_other_binding(root: Node<'_>, function: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let bound = match node.kind() {
            "function_declaration" | "generator_function_declaration" | "class_declaration"
                if node.id() != function.id() =>
            {
                node.child_by_field_name("name")
                    .is_some_and(|child| text(child, source) == Some(name))
            }
            "variable_declarator" => node
                .child_by_field_name("name")
                .is_some_and(|child| contains_identifier(child, source, name)),
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                node.child_by_field_name("left")
                    .is_some_and(|child| contains_identifier(child, source, name))
            }
            "import_statement" => import_binds_name(node, source, name),
            "export_specifier" => {
                node.child_by_field_name("alias")
                    .and_then(|alias| text(alias, source))
                    == Some(name)
                    && node
                        .child_by_field_name("name")
                        .and_then(|original| text(original, source))
                        != Some(name)
            }
            _ => false,
        };
        if bound {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn uses_arguments(function: Node<'_>, source: &[u8]) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return true;
    };
    contains_identifier(body, source, "arguments")
}

impl CallContract {
    fn from_function(function: Node<'_>, source: &[u8], lang: LangId) -> Option<Self> {
        if uses_arguments(function, source) {
            return None;
        }
        let params = function.child_by_field_name("parameters")?;
        let mut min = 0;
        let mut max = 0;
        let mut rest = false;
        let mut saw_optional_marker = false;
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            if param.kind() == "comment" {
                continue;
            }
            if max >= 64 || rest {
                return None;
            }
            let (required, is_rest) = match lang {
                LangId::Javascript => match param.kind() {
                    "identifier" | "object_pattern" | "array_pattern" => (true, false),
                    "assignment_pattern" => (false, false),
                    "rest_pattern" => (false, true),
                    _ => return None,
                },
                LangId::Typescript | LangId::Tsx => match param.kind() {
                    "required_parameter" | "optional_parameter" => {
                        let pattern = param.child_by_field_name("pattern")?;
                        if text(pattern, source) == Some("this") {
                            return None;
                        }
                        let is_rest = pattern.kind() == "rest_pattern";
                        if is_rest {
                            let annotation = param.child_by_field_name("type")?;
                            if annotation.named_child(0)?.kind() != "array_type"
                                || param.child_by_field_name("value").is_some()
                                || param.kind() == "optional_parameter"
                            {
                                return None;
                            }
                        }
                        let required = param.kind() == "required_parameter"
                            && param.child_by_field_name("value").is_none()
                            && !is_rest;
                        if param.kind() == "optional_parameter" {
                            saw_optional_marker = true;
                        }
                        (required, is_rest)
                    }
                    _ => return None,
                },
                _ => return None,
            };
            if required && saw_optional_marker {
                return None;
            }
            if is_rest {
                rest = true;
            } else {
                max += 1;
                if required {
                    min = max;
                }
            }
        }
        Some(Self {
            min,
            max: (!rest).then_some(max),
        })
    }

    fn accepts_call(self, call: Node<'_>) -> bool {
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return false;
        };
        if arguments.kind() != "arguments" {
            return false;
        }
        let mut count = 0;
        let mut cursor = arguments.walk();
        for argument in arguments.named_children(&mut cursor) {
            if argument.kind() == "comment" {
                continue;
            }
            if argument.kind() == "spread_element" {
                return false;
            }
            count += 1;
        }
        count >= self.min && self.max.is_none_or(|max| count <= max)
    }
}

fn contract_for_target(
    dir: &str,
    target_path: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<CallContract> {
    let target = parsed_ref_file(dir, target_path, caches)?;
    if !matches!(
        target.lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) || target.tree.root_node().has_error()
    {
        return None;
    }
    let root = target.tree.root_node();
    let function = unique_function(root, &target.source, name)?;
    if has_other_binding(root, function, &target.source, name) {
        return None;
    }
    CallContract::from_function(function, &target.source, target.lang_id)
}

fn plain_member_chain(node: Node<'_>) -> bool {
    let mut current = node;
    loop {
        if current.kind() == "identifier" {
            return true;
        }
        if current.kind() != "member_expression"
            || current.child_by_field_name("optional_chain").is_some()
            || !current
                .child_by_field_name("property")
                .is_some_and(|property| property.kind() == "property_identifier")
        {
            return false;
        }
        let Some(object) = current.child_by_field_name("object") else {
            return false;
        };
        current = object;
    }
}

/// arity は閉包の必要条件だけ。JSX は既存の props 判定を維持する。
pub(super) fn changed_call_matches_signature(
    dir: &str,
    target_path: &str,
    name: &str,
    reference_name: &str,
    reference: &SymbolReference,
    caches: &mut ApiClosureCaches,
) -> bool {
    // JSX の props 契約は arity と別に扱う。従来の分類へそのまま渡す。
    let is_jsx = {
        let Some(caller) = parsed_ref_file(dir, &reference.path, caches) else {
            return false;
        };
        if caller.tree.root_node().has_error() {
            return false;
        }
        let point = tree_sitter::Point {
            row: reference.line,
            column: reference.column,
        };
        caller
            .tree
            .root_node()
            .descendant_for_point_range(point, point)
            .is_some_and(|node| {
                super::ref_index::jsx_name_opening_tag(node, &caller.source).is_some()
            })
    };
    if is_jsx {
        return true;
    }
    let key = (target_path.to_owned(), name.to_owned());
    let contract = if let Some(cached) = caches.js_call_contracts.get(&key) {
        *cached
    } else {
        let loaded = contract_for_target(dir, target_path, name, caches);
        caches.js_call_contracts.insert(key, loaded);
        loaded
    };
    let Some(contract) = contract else {
        return false;
    };
    let Some(caller) = parsed_ref_file(dir, &reference.path, caches) else {
        return false;
    };
    if !matches!(
        caller.lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) || caller.tree.root_node().has_error()
    {
        return false;
    }
    let point = tree_sitter::Point {
        row: reference.line,
        column: reference.column,
    };
    let Some(callee) = caller
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    else {
        return false;
    };
    if !matches!(callee.kind(), "identifier" | "property_identifier")
        || text(callee, &caller.source) != Some(reference_name)
    {
        return false;
    }
    let mut function = callee;
    if callee.kind() == "property_identifier" {
        let Some(member) = callee
            .parent()
            .filter(|parent| parent.kind() == "member_expression")
        else {
            return false;
        };
        if member.child_by_field_name("property").map(|node| node.id()) != Some(callee.id())
            || !plain_member_chain(member)
        {
            return false;
        }
        function = member;
    }
    while let Some(parent) = function.parent() {
        if parent.kind() != "parenthesized_expression" {
            break;
        }
        function = parent;
    }
    let Some(call) = function
        .parent()
        .filter(|parent| parent.kind() == "call_expression")
    else {
        return false;
    };
    if call.child_by_field_name("function").map(|node| node.id()) != Some(function.id()) {
        return false;
    }
    // 明示した型引数の個数・制約はここでは証明しない。
    if call.child_by_field_name("type_arguments").is_some() {
        return false;
    }
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return false;
    };
    if caller
        .source
        .get(function.end_byte()..arguments.start_byte())
        .is_some_and(|gap| gap.windows(2).any(|pair| pair == b"?."))
    {
        return false;
    }
    contract.accepts_call(call)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(source: &str, lang: LangId) -> Option<CallContract> {
        let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let function = unique_function(root, source.as_bytes(), "render")?;
        if has_other_binding(root, function, source.as_bytes(), "render") {
            return None;
        }
        CallContract::from_function(function, source.as_bytes(), lang)
    }

    #[test]
    fn js_ts_function_arity_contract_counts_required_optional_default_and_rest() {
        for (source, lang, expected) in [
            (
                "export function render(a: string, b: string) { return a + b; }",
                LangId::Typescript,
                Some((2, Some(2))),
            ),
            (
                "export function render(a: string, b?: string) { return a; }",
                LangId::Typescript,
                Some((1, Some(2))),
            ),
            (
                "export function render(a = 'x', b: string) { return b; }",
                LangId::Typescript,
                Some((2, Some(2))),
            ),
            (
                "export function render(a: string, ...rest: string[]) { return a; }",
                LangId::Tsx,
                Some((1, None)),
            ),
            (
                "export function render(a: string, ...rest: [string, number]) { return a; }",
                LangId::Typescript,
                None,
            ),
            (
                "export function render(this: object, a: string) { return a; }",
                LangId::Typescript,
                None,
            ),
            (
                "export function render(a?: string, b: string) { return b; }",
                LangId::Typescript,
                None,
            ),
            (
                "export function render(a, b = 'x') { return a; }",
                LangId::Javascript,
                Some((1, Some(2))),
            ),
            (
                "export function render({a}) { return a; }",
                LangId::Javascript,
                Some((1, Some(1))),
            ),
            (
                "export function render(a, ...rest) { return a; }",
                LangId::Javascript,
                Some((1, None)),
            ),
            (
                "export function render(a) { return arguments[0]; }",
                LangId::Javascript,
                None,
            ),
            (
                "export function render(a) { return a; }\nrender = wrap(render);",
                LangId::Javascript,
                None,
            ),
            (
                "export function render(a) { return a; }\nimport { render as other } from './x';",
                LangId::Javascript,
                Some((1, Some(1))),
            ),
            (
                "export function render(a) { return a; }\nimport { fast as render } from './x';",
                LangId::Javascript,
                None,
            ),
            (
                "export function render(a) { return a; }\nfunction render(b) { return b; }",
                LangId::Javascript,
                None,
            ),
            (
                "export function render(a: string): string;\nexport function render(a: string) { return a; }",
                LangId::Typescript,
                None,
            ),
        ] {
            let actual = contract(source, lang).map(|contract| (contract.min, contract.max));
            assert_eq!(actual, expected, "{source}");
        }
    }

    #[test]
    fn js_binding_scan_rejects_shorthand_loops_and_export_aliases() {
        for suffix in [
            "({ render } = impl);",
            "for (render of items) {}",
            "for (render in obj) {}",
            "export { fast as render };",
        ] {
            let source = format!("function render(a) {{ return a; }}\n{suffix}");
            let tree = crate::engine::parser::parse_source(source.as_bytes(), LangId::Javascript)
                .expect("JavaScript parse");
            let root = tree.root_node();
            assert!(!root.has_error(), "{source}");
            let function = unique_function(root, source.as_bytes(), "render").expect("function");
            assert!(
                has_other_binding(root, function, source.as_bytes(), "render"),
                "{source}"
            );
        }
    }
}
