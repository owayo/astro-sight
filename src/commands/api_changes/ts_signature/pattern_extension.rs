//! 型へ今回足した optional property に対応する、末尾の単純な分割代入束縛だけを証明する。

use super::*;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use std::ops::ControlFlow;
use tree_sitter::Node;

pub(super) struct AddedBinding {
    name: String,
    node_id: usize,
}

fn object_branch(ty: Node<'_>) -> Option<Node<'_>> {
    match ty.kind() {
        "object_type" => Some(ty),
        "union_type" => {
            let parts = ts_union_type_parts(ty);
            let mut object = None;
            for part in parts {
                if part.kind() == "object_type" {
                    if object.replace(part).is_some() {
                        return None;
                    }
                } else if part.kind() != "literal_type"
                    || part
                        .named_child(0)
                        .is_none_or(|child| child.kind() != "undefined")
                {
                    // 他の枝に追加キーがあるとは証明できない。型だけの比較には適用しない。
                    return None;
                }
            }
            object
        }
        _ => None,
    }
}

fn plain_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.utf8_text(source).ok()?;
    // escape の展開による同名束縛は、この局所証明では扱わない。
    (!name.is_empty() && !name.contains('\\')).then(|| name.to_owned())
}

fn property_names(ty: Node<'_>, source: &[u8]) -> Option<HashSet<String>> {
    let mut names = HashSet::new();
    let mut cursor = ty.walk();
    for member in ty.named_children(&mut cursor) {
        if member.kind() == "comment" {
            continue;
        }
        if member.kind() != "property_signature" {
            return None;
        }
        let name = member.child_by_field_name("name")?;
        if name.kind() != "property_identifier" || !names.insert(plain_name(name, source)?) {
            return None;
        }
    }
    Some(names)
}

/// 呼出し元の従来 bool 証明が、既存メンバーと他 union 枝の不変を先に確認する。
pub(super) fn added_optional_keys<'new>(
    old_ty: Node<'_>,
    old_source: &[u8],
    new_ty: Node<'new>,
    new_source: &[u8],
) -> Option<HashMap<String, Node<'new>>> {
    let old_object = object_branch(old_ty)?;
    let new_object = object_branch(new_ty)?;
    let old_names = property_names(old_object, old_source)?;
    let new_names = property_names(new_object, new_source)?;
    let added: HashSet<_> = new_names.difference(&old_names).cloned().collect();
    if added.is_empty() {
        return None;
    }
    let mut types = HashMap::new();
    let mut cursor = new_object.walk();
    for member in new_object.named_children(&mut cursor) {
        if member.kind() == "comment" {
            continue;
        }
        let name = plain_name(member.child_by_field_name("name")?, new_source)?;
        if added.contains(&name) {
            if !ts_property_signature_is_optional(member) {
                return None;
            }
            let ty = member.child_by_field_name("type")?.named_child(0)?;
            types.insert(name, ty);
        }
    }
    Some(types)
}

struct Entry<'tree> {
    node: Node<'tree>,
    binding: Node<'tree>,
    key: String,
    default: Option<Node<'tree>>,
}

fn entries<'tree>(pattern: Node<'tree>, source: &[u8]) -> Option<Vec<Entry<'tree>>> {
    if pattern.kind() != "object_pattern" || pattern.has_error() {
        return None;
    }
    let mut result = Vec::new();
    let mut keys = HashSet::new();
    let mut bindings = HashSet::new();
    let mut cursor = pattern.walk();
    for node in pattern.named_children(&mut cursor) {
        let (key, binding, default) = match node.kind() {
            "comment" => continue,
            "shorthand_property_identifier_pattern" => (plain_name(node, source)?, node, None),
            "object_assignment_pattern" => {
                let left = node.child_by_field_name("left")?;
                if left.kind() != "shorthand_property_identifier_pattern" {
                    return None;
                }
                (
                    plain_name(left, source)?,
                    left,
                    Some(node.child_by_field_name("right")?),
                )
            }
            "pair_pattern" => {
                let key = node.child_by_field_name("key")?;
                if key.kind() != "property_identifier" {
                    return None;
                }
                let value = node.child_by_field_name("value")?;
                let (binding, default) = if value.kind() == "assignment_pattern" {
                    (
                        value.child_by_field_name("left")?,
                        Some(value.child_by_field_name("right")?),
                    )
                } else {
                    (value, None)
                };
                if binding.kind() != "identifier" {
                    return None;
                }
                (plain_name(key, source)?, binding, default)
            }
            _ => return None,
        };
        if !keys.insert(key.clone()) || !bindings.insert(plain_name(binding, source)?) {
            return None;
        }
        result.push(Entry {
            node,
            binding,
            key,
            default,
        });
    }
    Some(result)
}

fn number_literal(node: Node<'_>, source: &[u8]) -> bool {
    node.kind() == "number"
        && node.utf8_text(source).is_ok_and(|text| {
            !text.ends_with('n')
                && !(text.starts_with('0')
                    && text
                        .as_bytes()
                        .get(1)
                        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_'))
        })
}

fn literal_escapes_are_safe(value: Node<'_>, source: &[u8]) -> bool {
    walk_preorder(value, Children::Named, |node, _| {
        if node.kind() == "escape_sequence" {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            if text.as_bytes().first() == Some(&b'\\')
                && text.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
            {
                return ControlFlow::Break(());
            }
            if let Some(hex) = text
                .strip_prefix("\\u{")
                .and_then(|text| text.strip_suffix('}'))
                && !u32::from_str_radix(hex, 16).is_ok_and(|value| value <= 0x10_FFFF)
            {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
}

fn inert_literal(node: Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "number" => number_literal(node, source),
        "string" => literal_escapes_are_safe(node, source),
        "true" | "false" | "null" => true,
        "template_string" => {
            !template_has_substitution(node) && literal_escapes_are_safe(node, source)
        }
        "unary_expression" => {
            node.child_by_field_name("operator")
                .is_some_and(|op| op.kind() == "-")
                && node
                    .child_by_field_name("argument")
                    .is_some_and(|value| number_literal(value, source))
        }
        _ => false,
    }
}

fn default_matches_type(value: Node<'_>, ty: Node<'_>, source: &[u8]) -> bool {
    if !inert_literal(value, source) {
        return false;
    }
    let expected = match value.kind() {
        "string" | "template_string" => "string",
        "number" | "unary_expression" => "number",
        "true" | "false" => "boolean",
        "null" => "null",
        _ => return false,
    };
    let parts = if ty.kind() == "union_type" {
        ts_union_type_parts(ty)
    } else {
        vec![ty]
    };
    let mut found = false;
    for part in parts {
        let actual = match part.kind() {
            "predefined_type" => match part.utf8_text(source).ok() {
                Some("string") => "string",
                Some("number") => "number",
                Some("boolean") => "boolean",
                _ => return false,
            },
            "literal_type"
                if part
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "null") =>
            {
                "null"
            }
            _ => return false,
        };
        found |= actual == expected;
    }
    found
}

pub(super) fn added_bindings(
    old_pattern: Node<'_>,
    old_source: &[u8],
    new_pattern: Node<'_>,
    new_source: &[u8],
    added_keys: &HashMap<String, Node<'_>>,
) -> Option<Vec<AddedBinding>> {
    let old = entries(old_pattern, old_source)?;
    let new = entries(new_pattern, new_source)?;
    if new.len() <= old.len() {
        return None;
    }
    for (old, new) in old.iter().zip(&new) {
        if node_signature_tokens(old.node, old_source)?
            != node_signature_tokens(new.node, new_source)?
        {
            return None;
        }
    }
    let mut result = Vec::new();
    for entry in &new[old.len()..] {
        let ty = *added_keys.get(&entry.key)?;
        if !matches!(
            entry.node.kind(),
            "shorthand_property_identifier_pattern" | "object_assignment_pattern"
        ) || matches!(
            entry.key.as_str(),
            "eval"
                | "arguments"
                | "implements"
                | "interface"
                | "let"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "static"
                | "yield"
                | "await"
        ) || entry
            .default
            .is_some_and(|value| !default_matches_type(value, ty, new_source))
        {
            return None;
        }
        result.push(AddedBinding {
            name: plain_name(entry.binding, new_source)?,
            node_id: entry.binding.id(),
        });
    }
    Some(result)
}

fn pure_key(node: Node<'_>) -> bool {
    if node.kind() != "property_identifier" {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    let field = match parent.kind() {
        "property_signature" => "name",
        "pair_pattern" | "pair" => "key",
        "member_expression" => "property",
        _ => return false,
    };
    parent
        .child_by_field_name(field)
        .is_some_and(|key| key.id() == node.id())
}

fn body_pattern_preserves_bindings(
    pattern: Node<'_>,
    source: &[u8],
    names: &HashSet<&str>,
) -> bool {
    let mut pending = vec![pattern];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                let Some(name) = plain_name(node, source) else {
                    return false;
                };
                if names.contains(name.as_str()) {
                    return false;
                }
            }
            "object_pattern" | "array_pattern" => {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            "pair_pattern" => {
                let Some(value) = node.child_by_field_name("value") else {
                    return false;
                };
                pending.push(value);
            }
            "object_assignment_pattern" | "assignment_pattern" => {
                let Some(left) = node.child_by_field_name("left") else {
                    return false;
                };
                pending.push(left);
            }
            "rest_pattern" => {
                let mut cursor = node.walk();
                let mut children = node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() != "comment");
                let Some(binding) = children.next() else {
                    return false;
                };
                if children.next().is_some() {
                    return false;
                }
                pending.push(binding);
            }
            "comment" => {}
            _ => return false,
        }
    }
    true
}

fn body_expression_has_no_unknown_declaration(statement: Node<'_>) -> bool {
    walk_preorder(statement, Children::Named, |node, _| {
        match node.kind() {
            "statement_block" | "class_body" => return ControlFlow::Continue(Walk::Skip),
            "internal_module" | "module" => return ControlFlow::Break(()),
            // この文法の using は assignment_expression の匿名 token で表される。
            "assignment_expression" => {
                let mut cursor = node.walk();
                if node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "using")
                {
                    return ControlFlow::Break(());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
}

fn declarators_preserve_bindings(
    declaration: Node<'_>,
    source: &[u8],
    names: &HashSet<&str>,
) -> bool {
    let mut cursor = declaration.walk();
    for declarator in declaration.named_children(&mut cursor) {
        if declarator.kind() == "comment" {
            continue;
        }
        let Some(pattern) = declarator.child_by_field_name("name") else {
            return false;
        };
        if declarator.kind() != "variable_declarator"
            || !body_pattern_preserves_bindings(pattern, source, names)
        {
            return false;
        }
    }
    true
}

fn var_scope_preserves_bindings(body: Node<'_>, source: &[u8], names: &HashSet<&str>) -> bool {
    walk_preorder(body, Children::Named, |node, _| {
        match node.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "class"
            | "class_declaration"
            | "abstract_class_declaration"
            | "class_body" => return ControlFlow::Continue(Walk::Skip),
            "variable_declaration" => {
                if !declarators_preserve_bindings(node, source, names) {
                    return ControlFlow::Break(());
                }
                return ControlFlow::Continue(Walk::Skip);
            }
            "for_in_statement"
                if node
                    .child_by_field_name("kind")
                    .is_some_and(|kind| kind.kind() == "var") =>
            {
                let Some(pattern) = node.child_by_field_name("left") else {
                    return ControlFlow::Break(());
                };
                if !body_pattern_preserves_bindings(pattern, source, names) {
                    return ControlFlow::Break(());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
}

fn body_declarations_preserve_bindings(
    body: Node<'_>,
    source: &[u8],
    names: &HashSet<&str>,
) -> bool {
    if body.kind() != "statement_block" {
        return true;
    }
    let mut cursor = body.walk();
    for statement in body.named_children(&mut cursor) {
        match statement.kind() {
            "lexical_declaration" => {
                if !declarators_preserve_bindings(statement, source, names) {
                    return false;
                }
            }
            "class_declaration"
            | "abstract_class_declaration"
            | "enum_declaration"
            | "function_declaration"
            | "generator_function_declaration" => {
                let Some(name) = statement
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(name, source))
                else {
                    return false;
                };
                if names.contains(name.as_str()) {
                    return false;
                }
            }
            "expression_statement" => {
                if !body_expression_has_no_unknown_declaration(statement) {
                    return false;
                }
            }
            // var は下の関数 scope 走査で検査し、type/interface は値を束縛しない。
            // 入れ子の block や loop の lexical binding は別 scope に属する。
            "variable_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "break_statement"
            | "continue_statement"
            | "debugger_statement"
            | "do_statement"
            | "empty_statement"
            | "for_in_statement"
            | "for_statement"
            | "if_statement"
            | "labeled_statement"
            | "return_statement"
            | "statement_block"
            | "switch_statement"
            | "throw_statement"
            | "try_statement"
            | "while_statement"
            | "with_statement"
            | "comment" => {}
            // ambient/import/module など、未対応の宣言は証明しない。
            _ => return false,
        }
    }
    // TS では var も引数と同じ scope に属し、推論型の違いで再宣言が不正になり得る。
    var_scope_preserves_bindings(body, source, names)
}

pub(super) fn signature_preserves_bindings(
    function: Node<'_>,
    source: &[u8],
    added: &[AddedBinding],
) -> bool {
    if added.is_empty() {
        return true;
    }
    let mut names = HashSet::new();
    let mut nodes = HashSet::new();
    for binding in added {
        if !names.insert(binding.name.as_str()) {
            return false;
        }
        nodes.insert(binding.node_id);
    }
    let body = function.child_by_field_name("body");
    if body.is_some_and(|body| !body_declarations_preserve_bindings(body, source, &names)) {
        return false;
    }
    let body = body.map(|body| body.id());
    walk_preorder(function, Children::Named, |node, _| {
        if Some(node.id()) == body
            || node.kind() == "comment"
            || nodes.contains(&node.id())
            || pure_key(node)
        {
            return ControlFlow::Continue(Walk::Skip);
        }
        if matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "undefined"
        ) {
            let Ok(name) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            // eval は括弧付きの direct call も含め、文字列から局所束縛を参照し得る。
            if name.contains('\\') || name == "eval" || names.contains(name) {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LangId;

    fn compatible(old_params: &str, new_params: &str, tail: &str, lang: LangId) -> bool {
        compatible_with_body(old_params, new_params, tail, "return '';", lang)
    }

    fn compatible_with_body(
        old_params: &str,
        new_params: &str,
        tail: &str,
        body: &str,
        lang: LangId,
    ) -> bool {
        let source = |params: &str| {
            format!("const hint = '';\nexport function Field({params}){tail} {{ {body} }}\n")
        };
        let old = source(old_params);
        let new = source(new_params);
        let old_tree = parser::parse_source(old.as_bytes(), lang).unwrap();
        let new_tree = parser::parse_source(new.as_bytes(), lang).unwrap();
        if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
            return false;
        }
        let old_fn =
            find_top_level_function_by_name(old_tree.root_node(), old.as_bytes(), "Field").unwrap();
        let new_fn =
            find_top_level_function_by_name(new_tree.root_node(), new.as_bytes(), "Field").unwrap();
        super::super::function_params::optional_object_props_extension(
            old_fn,
            old.as_bytes(),
            new_fn,
            new.as_bytes(),
        )
        .is_some()
    }

    #[test]
    fn destructured_optional_props_defaults_reject_legacy_literals() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (value, ty, expected) in [
                ("01", "number", false),
                ("-01", "number", false),
                (r"'\1'", "string", false),
                (r"`\1`", "string", false),
                (r"'\0'", "string", false),
                (r"'\\1'", "string", true),
                (r"`\\1`", "string", true),
                (r"'\n'", "string", true),
                (r"'\u{110000}'", "string", false),
                (r"`\u{110000}`", "string", false),
                (r"'\u{FFFFFFFFF}'", "string", false),
                (r"'\u{10FFFF}'", "string", true),
                (r"`\u{10FFFF}`", "string", true),
                (r"'\u{0000000041}'", "string", true),
                (r"`\u{0000000041}`", "string", true),
                (r"'\u{D800}'", "string", true),
                ("0", "number", true),
                ("0.5", "number", true),
                ("0x1", "number", true),
                ("0o1", "number", true),
                ("0b1", "number", true),
                ("1_000", "number", true),
            ] {
                let new = format!("{{ label, hint = {value} }}: {{ label: string; hint?: {ty} }}");
                let source = format!("export function Field({new}): string {{ return label; }}");
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{lang:?}/{value}");
                assert_eq!(
                    compatible("{ label }: { label: string }", &new, ": string", lang),
                    expected,
                    "{lang:?}/{value}"
                );
            }
            assert!(compatible(
                r"{ label = '\1' }: { label?: string }",
                r"{ label = '\1' }: { label?: string; hint?: string }",
                ": string",
                lang
            ));
        }
    }

    #[test]
    fn destructured_optional_props_body_bindings_preserve_the_parameter_scope() {
        let old = "{ label }: { label: string }";
        let new = "{ label, hint }: { label: string; hint?: string }";
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (body, expected) in [
                ("const hint = label; return hint;", false),
                ("let hint = label; return hint;", false),
                ("class hint {} return label;", false),
                ("abstract class hint {} return label;", false),
                ("enum hint { First } return label;", false),
                ("const { hint } = data; return label;", false),
                ("const { key: hint } = data; return label;", false),
                ("let { key: { hint = '' } } = data; return label;", false),
                ("const [first, [hint = '']] = data; return label;", false),
                ("let { ...hint } = data; return label;", false),
                ("const [first, ...hint] = data; return label;", false),
                ("using hint = acquire(); return label;", false),
                ("await using hint = acquire(); return label;", false),
                ("using local = acquire(); return label;", false),
                (
                    "using local = acquire(), extra = acquire(); return label;",
                    false,
                ),
                ("namespace Local {} return label;", false),
                ("module Local {} return label;", false),
                ("declare const local: string; return label;", false),
                ("const local = label; return local;", true),
                ("const { hint: local } = data; return label;", true),
                ("const { local = hint } = data; return label;", true),
                (
                    "const { label: caption = hint } = data; return label;",
                    true,
                ),
                ("class Local { hint = ''; } return label;", true),
                ("abstract class Local {} return label;", true),
                ("enum Local { hint } return label;", true),
                ("var hint = label; return hint;", false),
                ("function hint() {} return label;", false),
                ("function* hint() {} return label;", false),
                ("if (label) { var hint = label; } return label;", false),
                ("for (var hint in data) {} return label;", false),
                ("for (var hint of values) {} return label;", false),
                ("for (var { key: hint } of values) {} return label;", false),
                ("for (var [hint] of values) {} return label;", false),
                ("for (var hint = ''; label; ) {} return label;", false),
                ("var { key: { hint } } = data; return label;", false),
                ("var local = label; return local;", true),
                ("{ function hint() {} } return label;", true),
                ("{ function* hint() {} } return label;", true),
                (
                    "const f = function () { var hint = ''; }; return label;",
                    true,
                ),
                (
                    "const f = function* () { var hint = ''; }; return label;",
                    true,
                ),
                ("const f = () => { var hint = ''; }; return label;", true),
                (
                    "const o = { method() { var hint = ''; } }; return label;",
                    true,
                ),
                (
                    "class Local { method() { var hint = ''; } } return label;",
                    true,
                ),
                (
                    "class Local { static { var hint = ''; } } return label;",
                    true,
                ),
                (
                    "const C = class { method() { var hint = ''; } }; return label;",
                    true,
                ),
                ("{ const hint = label; } return label;", true),
                ("for (let hint of values) {} return label;", true),
                ("interface hint {} return label;", true),
                ("type hint = string; return label;", true),
                ("return hint || label;", true),
            ] {
                let source = format!("export function Field({new}): string {{ {body} }}");
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{lang:?}/{body}");
                assert_eq!(
                    compatible_with_body(old, new, ": string", body, lang),
                    expected,
                    "{lang:?}/{body}"
                );
            }
            assert!(compatible_with_body(
                old,
                "{ label }: { label: string; hint?: string }",
                ": string",
                "const hint = label; return hint;",
                lang
            ));
        }
    }

    #[test]
    fn destructured_optional_props_validate_append_only_bindings() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (name, old, new, expected) in [
                (
                    "type only",
                    "{ label }: { label: string }",
                    "{ label }: { label: string; hint?: string }",
                    true,
                ),
                (
                    "shorthand",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; hint?: string }",
                    true,
                ),
                (
                    "default string",
                    "{ label }: { label: string }",
                    "{ label, hint = '' }: { label: string; hint?: string }",
                    true,
                ),
                (
                    "default negative",
                    "{ label }: { label: string }",
                    "{ label, hint = -1 }: { label: string; hint?: number }",
                    true,
                ),
                (
                    "default template",
                    "{ label }: { label: string }",
                    "{ label, hint = `text` }: { label: string; hint?: string }",
                    true,
                ),
                (
                    "default null",
                    "{ label }: { label: string }",
                    "{ label, hint = null }: { label: string; hint?: string | null }",
                    true,
                ),
                (
                    "default true",
                    "{ label }: { label: string }",
                    "{ label, hint = true }: { label: string; hint?: boolean }",
                    true,
                ),
                (
                    "default false",
                    "{ label }: { label: string }",
                    "{ label, hint = false }: { label: string; hint?: boolean }",
                    true,
                ),
                (
                    "default number",
                    "{ label }: { label: string }",
                    "{ label, hint = 1 }: { label: string; hint?: number }",
                    true,
                ),
                (
                    "default primitive union",
                    "{ label }: { label: string }",
                    "{ label, hint = -1 }: { label: string; hint?: string | boolean | number | null }",
                    true,
                ),
                (
                    "default null type",
                    "{ label }: { label: string }",
                    "{ label, hint = null }: { label: string; hint?: null }",
                    true,
                ),
                (
                    "shorthand literal type",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; hint?: 'a' | 'b' }",
                    true,
                ),
                (
                    "type-only named union",
                    "{ label }: { label: string } | Base",
                    "{ label }: { label: string; hint?: string } | Base",
                    true,
                ),
                (
                    "binding named union",
                    "{ label }: { label: string } | Base",
                    "{ label, hint }: { label: string; hint?: string } | Base",
                    false,
                ),
                (
                    "binding default named union",
                    "{ label }: { label: string } | Base",
                    "{ label, hint = '' }: { label: string; hint?: string } | Base",
                    false,
                ),
                (
                    "binding null union",
                    "{ label }: { label: string } | null",
                    "{ label, hint }: { label: string; hint?: string } | null",
                    false,
                ),
                (
                    "default string versus number",
                    "{ label }: { label: string }",
                    "{ label, hint = '' }: { label: string; hint?: number }",
                    false,
                ),
                (
                    "default null versus string",
                    "{ label }: { label: string }",
                    "{ label, hint = null }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "default template versus literal union",
                    "{ label }: { label: string }",
                    "{ label, hint = `x` }: { label: string; hint?: 'a' | 'b' }",
                    false,
                ),
                (
                    "default numeric literal type",
                    "{ label }: { label: string }",
                    "{ label, hint = 1 }: { label: string; hint?: 1 }",
                    false,
                ),
                (
                    "default boolean literal type",
                    "{ label }: { label: string }",
                    "{ label, hint = true }: { label: string; hint?: true }",
                    false,
                ),
                (
                    "default alias type",
                    "{ label }: { label: string }",
                    "{ label, hint = '' }: { label: string; hint?: Text }",
                    false,
                ),
                (
                    "default any type",
                    "{ label }: { label: string }",
                    "{ label, hint = '' }: { label: string; hint?: any }",
                    false,
                ),
                (
                    "default mixed unknown union",
                    "{ label }: { label: string }",
                    "{ label, hint = '' }: { label: string; hint?: string | Text }",
                    false,
                ),
                (
                    "default bigint number node",
                    "{ label }: { label: string }",
                    "{ label, hint = 1n }: { label: string; hint?: number }",
                    false,
                ),
                (
                    "default negative bigint",
                    "{ label }: { label: string }",
                    "{ label, hint = -1n }: { label: string; hint?: number }",
                    false,
                ),
                (
                    "default bigint type",
                    "{ label }: { label: string }",
                    "{ label, hint = 1n }: { label: string; hint?: bigint }",
                    false,
                ),
                (
                    "multiple added",
                    "{ label }: { label: string }",
                    "{ label, hint, active = false }: { label: string; hint?: string; active?: boolean }",
                    true,
                ),
                (
                    "subset of added",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; hint?: string; active?: boolean }",
                    true,
                ),
                (
                    "old alias",
                    "{ label: caption }: { label: string }",
                    "{ label: caption, hint }: { label: string; hint?: string }",
                    true,
                ),
                (
                    "old alias default",
                    "{ label: caption = '' }: { label?: string }",
                    "{ label: caption = '', hint }: { label?: string; hint?: string }",
                    true,
                ),
                (
                    "old default",
                    "{ label = '' }: { label?: string }",
                    "{ label = '', hint }: { label?: string; hint?: string }",
                    true,
                ),
                (
                    "pattern comments",
                    "{ label }: { label: string }",
                    "{ label, /* extra */ hint }: { label: string; hint?: string }",
                    true,
                ),
                (
                    "default object",
                    "{ label = '' }: { label?: string } = {}",
                    "{ label = '', hint }: { label?: string; hint?: string } = {}",
                    true,
                ),
                (
                    "union default",
                    "{ label }: { label: string } | undefined = { label: '' }",
                    "{ label, hint }: { label: string; hint?: string } | undefined = { label: '' }",
                    true,
                ),
                (
                    "leading pipe",
                    "{ label }: { label: string } | undefined = { label: '' }",
                    "{ label, hint }:\n | { label: string; hint?: string }\n | undefined = { label: '' }",
                    true,
                ),
                ("untyped", "{ label }", "{ label, hint }", false),
                (
                    "no type delta",
                    "{ label }: { label: string; hint?: string }",
                    "{ label, hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "existing optional key",
                    "{ label }: { label: string; hint?: string }",
                    "{ label, hint }: { label: string; hint?: string; active?: boolean }",
                    false,
                ),
                (
                    "required added",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; hint: string }",
                    false,
                ),
                (
                    "old type changed",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: number; hint?: string }",
                    false,
                ),
                (
                    "pattern order",
                    "{ label, mode }: { label: string; mode: string }",
                    "{ mode, label, hint }: { label: string; mode: string; hint?: string }",
                    false,
                ),
                (
                    "inserted binding",
                    "{ label, mode }: { label: string; mode: string }",
                    "{ label, hint, mode }: { label: string; mode: string; hint?: string }",
                    false,
                ),
                (
                    "new alias",
                    "{ label }: { label: string }",
                    "{ label, hint: tip }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "new alias default",
                    "{ label }: { label: string }",
                    "{ label, hint: tip = '' }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "old alias changed",
                    "{ label: caption }: { label: string }",
                    "{ label: title, hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "old default changed",
                    "{ label = 'a' }: { label?: string }",
                    "{ label = 'b', hint }: { label?: string; hint?: string }",
                    false,
                ),
                (
                    "whole default changed",
                    "{ label = '' }: { label?: string } = {}",
                    "{ label = '', hint }: { label?: string; hint?: string } = { hint: '' }",
                    false,
                ),
                (
                    "old rest",
                    "{ label, ...rest }: { label: string }",
                    "{ label, hint, ...rest }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "new rest",
                    "{ label }: { label: string }",
                    "{ label, hint, ...rest }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "nested old",
                    "{ label: { text } }: { label: { text: string } }",
                    "{ label: { text }, hint }: { label: { text: string }; hint?: string }",
                    false,
                ),
                (
                    "nested new",
                    "{ label }: { label: string }",
                    "{ label, hint: { text } }: { label: string; hint?: { text: string } }",
                    false,
                ),
                (
                    "computed old",
                    "{ [key]: label }: { label: string }",
                    "{ [key]: label, hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "computed new",
                    "{ label }: { label: string }",
                    "{ label, [key]: hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "duplicate new",
                    "{ label }: { label: string }",
                    "{ label, hint, hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "duplicate old",
                    "{ label, label }: { label: string }",
                    "{ label, label, hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "alias collision",
                    "{ label: hint }: { label: string }",
                    "{ label: hint, hint }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "duplicate type key",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; hint?: string; hint?: number }",
                    false,
                ),
                (
                    "computed type key",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; [key]?: string }",
                    false,
                ),
                (
                    "untyped member",
                    "{ label }: { label: string }",
                    "{ label, hint }: { label: string; hint? }",
                    false,
                ),
                (
                    "default identifier",
                    "{ label }: { label: string }",
                    "{ label, hint = other }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "default undefined",
                    "{ label }: { label: string }",
                    "{ label, hint = undefined }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "default call",
                    "{ label }: { label: string }",
                    "{ label, hint = make() }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "default new",
                    "{ label }: { label: string }",
                    "{ label, hint = new Text() }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "default template expression",
                    "{ label }: { label: string }",
                    "{ label, hint = `${other}` }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "default array",
                    "{ label }: { label: string }",
                    "{ label, hint = [] }: { label: string; hint?: unknown }",
                    false,
                ),
                (
                    "default object expression",
                    "{ label }: { label: string }",
                    "{ label, hint = {} }: { label: string; hint?: unknown }",
                    false,
                ),
                (
                    "default parentheses",
                    "{ label }: { label: string }",
                    "{ label, hint = (-1) }: { label: string; hint?: number }",
                    false,
                ),
                (
                    "default plus",
                    "{ label }: { label: string }",
                    "{ label, hint = +1 }: { label: string; hint?: number }",
                    false,
                ),
                (
                    "other union branch",
                    "{ label }: { label: string } | undefined",
                    "{ label, hint }: { label: string; hint?: string } | null",
                    false,
                ),
                (
                    "multiple object branches",
                    "{ label }: { label: string } | { mode: string }",
                    "{ label, hint }: { label: string; hint?: string } | { mode: string }",
                    false,
                ),
                (
                    "escaped binding",
                    "{ label }: { label: string }",
                    "{ label, h\\u0069nt }: { label: string; hint?: string }",
                    false,
                ),
                (
                    "strict eval binding",
                    "{ label }: { label: string }",
                    "{ label, eval }: { label: string; eval?: string }",
                    false,
                ),
                (
                    "strict arguments binding",
                    "{ label }: { label: string }",
                    "{ label, arguments }: { label: string; arguments?: string }",
                    false,
                ),
            ] {
                assert_eq!(
                    compatible(old, new, ": string", lang),
                    expected,
                    "{lang:?}/{name}"
                );
            }
        }
    }

    #[test]
    fn destructured_optional_props_reject_strict_binding_names() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for name in [
                "eval",
                "arguments",
                "implements",
                "interface",
                "let",
                "package",
                "private",
                "protected",
                "public",
                "static",
                "yield",
                "await",
            ] {
                let new = format!("{{ label, {name} }}: {{ label: string; {name}?: string }}");
                if matches!(name, "package" | "yield") {
                    let source = format!("export function Field({new}): string {{ return ''; }}");
                    let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                    assert!(!tree.root_node().has_error(), "{lang:?}/{name}");
                }
                assert!(
                    !compatible("{ label }: { label: string }", &new, ": string", lang),
                    "{lang:?}/{name}"
                );
            }
        }
    }

    #[test]
    fn destructured_optional_props_reject_signature_capture_and_cross_parameter_duplicates() {
        let old = "{ label }: { label: string }";
        let new = "{ label, hint }: { label: string; hint?: string }";
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (name, before, after, tail) in [
                (
                    "later default",
                    format!("{old}, later = hint"),
                    format!("{new}, later = hint"),
                    ": string",
                ),
                (
                    "later typeof",
                    format!("{old}, later: typeof hint"),
                    format!("{new}, later: typeof hint"),
                    ": string",
                ),
                (
                    "earlier default",
                    format!("earlier = hint, {old}"),
                    format!("earlier = hint, {new}"),
                    ": string",
                ),
                ("return typeof", old.into(), new.into(), ": typeof hint"),
                (
                    "parenthesized direct eval",
                    "{ label = (eval)('hint') }: { label?: string }".into(),
                    "{ label = (eval)('hint'), hint }: { label?: string; hint?: string }".into(),
                    ": string",
                ),
                (
                    "undefined value capture",
                    format!("{old}, later = undefined"),
                    "{ label, undefined }: { label: string; undefined?: string }, later = undefined".into(),
                    ": string",
                ),
                (
                    "existing param name",
                    format!("{old}, hint: string"),
                    format!("{new}, hint: string"),
                    ": string",
                ),
                (
                    "other pattern",
                    format!("{old}, {{ hint }}: {{ hint: string }}"),
                    format!("{new}, {{ hint }}: {{ hint: string }}"),
                    ": string",
                ),
                (
                    "cross-param new",
                    format!("{old}, {{ mode }}: {{ mode: string }}"),
                    format!("{new}, {{ mode, hint }}: {{ mode: string; hint?: string }}"),
                    ": string",
                ),
                (
                    "added prop typeof",
                    old.into(),
                    "{ label, hint }: { label: string; hint?: typeof hint }".into(),
                    ": string",
                ),
                (
                    "existing prop typeof",
                    "{ label }: { label: typeof hint }".into(),
                    "{ label, hint }: { label: typeof hint; hint?: string }".into(),
                    ": string",
                ),
                (
                    "old default capture",
                    "{ label = hint }: { label?: string }".into(),
                    "{ label = hint, hint }: { label?: string; hint?: string }".into(),
                    ": string",
                ),
                (
                    "whole default capture",
                    format!("{old} = {{ label: hint }}"),
                    format!("{new} = {{ label: hint }}"),
                    ": string",
                ),
                (
                    "escaped later reference",
                    format!("{old}, later = h\\u0069nt"),
                    format!("{new}, later = h\\u0069nt"),
                    ": string",
                ),
                (
                    "direct eval",
                    "{ label = eval('hint') }: { label?: string }".into(),
                    "{ label = eval('hint'), hint }: { label?: string; hint?: string }".into(),
                    ": string",
                ),
            ] {
                assert!(!compatible(&before, &after, tail, lang), "{lang:?}/{name}");
            }
            assert!(compatible(
                &format!("{old}, later = {{ hint: '' }}"),
                &format!("{new}, later = {{ hint: '' }}"),
                ": string",
                lang
            ));
        }
    }

    #[test]
    fn destructured_optional_props_node_kinds_exist_in_both_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for kind in [
                "object_type",
                "predefined_type",
                "literal_type",
                "union_type",
                "property_signature",
                "property_identifier",
                "object_pattern",
                "shorthand_property_identifier_pattern",
                "object_assignment_pattern",
                "pair_pattern",
                "assignment_pattern",
                "identifier",
                "undefined",
                "type_identifier",
                "shorthand_property_identifier",
                "member_expression",
                "pair",
                "call_expression",
                "number",
                "string",
                "true",
                "false",
                "null",
                "template_string",
                "template_substitution",
                "escape_sequence",
                "unary_expression",
                "comment",
                "statement_block",
                "lexical_declaration",
                "variable_declarator",
                "variable_declaration",
                "array_pattern",
                "rest_pattern",
                "class_declaration",
                "abstract_class_declaration",
                "class_body",
                "enum_declaration",
                "function_declaration",
                "generator_function_declaration",
                "function_expression",
                "generator_function",
                "arrow_function",
                "method_definition",
                "class",
                "interface_declaration",
                "type_alias_declaration",
                "expression_statement",
                "assignment_expression",
                "internal_module",
                "module",
                "break_statement",
                "continue_statement",
                "debugger_statement",
                "do_statement",
                "empty_statement",
                "for_in_statement",
                "for_statement",
                "if_statement",
                "labeled_statement",
                "return_statement",
                "switch_statement",
                "throw_statement",
                "try_statement",
                "while_statement",
                "with_statement",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                    .unwrap_or_else(|error| panic!("{lang:?}/{kind}: {error}"));
            }
            tree_sitter::Query::new(&lang.ts_language(), "\"using\" @token").unwrap();
        }
    }
}
