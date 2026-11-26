//! 省略可能な props の追加で、本体の束縛や推論した戻り値が変わる形を除く。

use std::collections::{HashMap, HashSet};

use super::super::{
    OptionalPropsExtension, callable::unwrap_parentheses, module_root, pattern_extension,
    ts_property_signature_is_optional,
};
use crate::commands::api_changes::ts_required_members::only_named_child;
use crate::engine::jsx::has_jsx_pragma;
use crate::engine::tree_walk::{Children, any_node};
use tree_sitter::Node;

fn single_return<'tree>(body: Node<'tree>, source: &[u8]) -> Option<Node<'tree>> {
    let value = if body.kind() == "statement_block" {
        let statement = only_named_child(body)?;
        if statement.kind() != "return_statement" {
            return None;
        }
        let value = only_named_child(statement)?;
        let gap =
            std::str::from_utf8(source.get(statement.start_byte()..value.start_byte())?).ok()?;
        if gap.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
            return None;
        }
        value
    } else {
        body
    };
    unwrap_parentheses(value)
}

fn jsx(node: Node<'_>) -> bool {
    matches!(node.kind(), "jsx_element" | "jsx_self_closing_element")
}

fn value_name(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "undefined"
    )
}

/// string を返す既存の `return hint || label` で、追加した hint も string なら
/// 左辺の真値は string のまま。右辺に現れる同名の参照は別途検査する。
fn string_fallback(
    body: Node<'_>,
    source: &[u8],
    return_type: Node<'_>,
    extension: &OptionalPropsExtension<'_>,
    new_source: &[u8],
) -> Option<usize> {
    let ty = only_named_child(return_type)?;
    let value = single_return(body, source)?;
    if ty.kind() != "predefined_type"
        || ty.utf8_text(source).ok()? != "string"
        || value.kind() != "binary_expression"
        || !matches!(value.child_by_field_name("operator")?.kind(), "||" | "??")
    {
        return None;
    }
    let left = value.child_by_field_name("left")?;
    let name = left.utf8_text(source).ok()?;
    if left.kind() != "identifier" {
        return None;
    }
    let right = value.child_by_field_name("right")?;
    extension.parameters.iter().find_map(|p| {
        let string = |ty: Node<'_>| {
            ty.kind() == "predefined_type" && ty.utf8_text(new_source) == Ok("string")
        };
        if !string(*p.members.named.get(name)?) {
            return None;
        }
        let pattern = p.parameter.child_by_field_name("pattern")?;
        let shorthand = |key: &str| {
            pattern.named_children(&mut pattern.walk()).any(|binding| {
                binding.kind() == "shorthand_property_identifier_pattern"
                    && binding.utf8_text(new_source) == Ok(key)
            })
        };
        if !shorthand(name) {
            return None;
        }
        if right.kind() == "string" {
            return Some(left.id());
        }
        let fallback = right.utf8_text(source).ok()?;
        let object = only_named_child(p.parameter.child_by_field_name("type")?)?;
        (right.kind() == "identifier"
            && shorthand(fallback)
            && object.kind() == "object_type"
            && object.named_children(&mut object.walk()).any(|prop| {
                prop.kind() == "property_signature"
                    && prop
                        .child_by_field_name("name")
                        .is_some_and(|n| n.utf8_text(new_source) == Ok(fallback))
                    && !ts_property_signature_is_optional(prop)
                    && prop
                        .child_by_field_name("type")
                        .and_then(only_named_child)
                        .is_some_and(string)
            }))
        .then_some(left.id())
    })
}

/// 旧本体での同名参照が、その本体内のスコープに束縛されていれば捕捉は起きない。
fn captures_outer(
    body: Node<'_>,
    source: &[u8],
    names: &HashSet<&str>,
    allowed: Option<usize>,
) -> bool {
    if names.is_empty() {
        return false;
    }
    let mut local_ends = HashMap::new();
    any_node(body, Children::Named, |node| {
        // 前順走査では、内側の束縛が終わっても外側の同名束縛は有効。
        let mut protect = |binding: Node<'_>| {
            if let Ok(text) = binding.utf8_text(source)
                && names.contains(text)
            {
                let end = local_ends.entry(text).or_insert(0);
                *end = (*end).max(node.end_byte());
            }
        };
        if node.kind() == "statement_block" {
            for statement in node.named_children(&mut node.walk()) {
                match statement.kind() {
                    "lexical_declaration" | "variable_declaration" => {
                        for decl in statement.named_children(&mut statement.walk()) {
                            if let Some(binding) = decl
                                .child_by_field_name("name")
                                .filter(|n| n.kind() == "identifier")
                            {
                                protect(binding);
                            }
                        }
                    }
                    "function_declaration"
                    | "generator_function_declaration"
                    | "class_declaration"
                    | "enum_declaration" => {
                        if let Some(binding) = statement.child_by_field_name("name") {
                            protect(binding);
                        }
                    }
                    _ => {}
                }
            }
        }
        // メソッドの計算名やデコレーターは引数のスコープ外で評価される。
        if matches!(
            node.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "generator_function"
                | "arrow_function"
        ) && let Some(params) = node.child_by_field_name("parameters")
        {
            for param in params.named_children(&mut params.walk()) {
                if let Some(binding) = param
                    .child_by_field_name("pattern")
                    .filter(|n| n.kind() == "identifier")
                {
                    protect(binding);
                }
            }
        }
        if matches!(node.kind(), "arrow_function" | "catch_clause")
            && let Some(binding) = node
                .child_by_field_name("parameter")
                .filter(|n| n.kind() == "identifier")
        {
            protect(binding);
        }
        if node.kind() == "for_in_statement"
            && node.child_by_field_name("kind").is_some()
            && let Some(binding) = node
                .child_by_field_name("left")
                .filter(|n| n.kind() == "identifier")
        {
            protect(binding);
        }
        if !value_name(node) || Some(node.id()) == allowed {
            return false;
        }
        let Ok(name) = node.utf8_text(source) else {
            return true;
        };
        name.contains('\\')
            || name == "eval"
            || (names.contains(name)
                && local_ends
                    .get(name)
                    .is_none_or(|end| node.start_byte() >= *end))
    })
}

/// `props.label` のように既存のキーだけを読む利用は、props 自体を返したり
/// 別の引数の既定値に渡したりする利用と区別する。
fn reads_only_retained_properties(
    roots: &[Node<'_>],
    source: &[u8],
    extension: &OptionalPropsExtension<'_>,
) -> Option<bool> {
    let mut objects = HashMap::new();
    let mut proven = HashSet::new();
    for param in &extension.parameters {
        let pattern = param.parameter.child_by_field_name("pattern")?;
        match pattern.kind() {
            "identifier" => {
                objects.insert(pattern.utf8_text(source).ok()?, &param.members);
                proven.insert(pattern.id());
            }
            "object_pattern" => {
                // rest・計算キーなどは、束縛する値の型が変わり得る。
                pattern_extension::entries(pattern, source)?;
            }
            _ => return None,
        }
    }
    Some(
        objects.is_empty()
            || !roots.iter().any(|&root| {
                any_node(root, Children::Named, |node| {
                    if node.kind() == "member_expression"
                        && let Some(object) = node
                            .child_by_field_name("object")
                            .filter(|n| n.kind() == "identifier")
                        && let Some(members) =
                            object.utf8_text(source).ok().and_then(|s| objects.get(s))
                        && node.child_by_field_name("property").is_some_and(|key| {
                            key.kind() == "property_identifier"
                                && key.utf8_text(source).is_ok_and(|s| {
                                    !s.contains('\\') && !members.named.contains_key(s)
                                })
                        })
                    {
                        proven.insert(object.id());
                    }
                    if !value_name(node) || proven.contains(&node.id()) {
                        return false;
                    }
                    let Ok(name) = node.utf8_text(source) else {
                        return true;
                    };
                    if name.contains('\\') || name == "eval" {
                        return true;
                    }
                    objects.contains_key(name)
                })
            }),
    )
}

pub(in super::super) fn preserves_return(
    old: Node<'_>,
    a: &[u8],
    new: Node<'_>,
    b: &[u8],
    extension: &OptionalPropsExtension<'_>,
) -> bool {
    let (Some(old_body), Some(new_body)) = (
        old.child_by_field_name("body"),
        new.child_by_field_name("body"),
    ) else {
        return false;
    };
    let identical = a[old_body.byte_range()] == b[new_body.byte_range()];
    let declared = old.child_by_field_name("return_type");
    let allowed = declared
        .filter(|_| identical)
        .and_then(|ty| string_fallback(old_body, a, ty, extension, b));
    let names = extension
        .bindings
        .iter()
        .map(|binding| binding.name.as_str())
        .collect();
    if captures_outer(old_body, a, &names, allowed) {
        return false;
    }
    if let Some(ty) = new.child_by_field_name("return_type") {
        return !any_node(ty, Children::Named, |n| n.kind() == "type_query")
            || new.child_by_field_name("parameters").is_some_and(|params| {
                reads_only_retained_properties(&[params, ty], b, extension) == Some(true)
            });
    }
    if reads_only_retained_properties(&[new], b, extension) != Some(true) {
        return false;
    }
    if (any_node(old_body, Children::Named, jsx) || any_node(new_body, Children::Named, jsx))
        && (has_jsx_pragma(module_root(old), a) || has_jsx_pragma(module_root(new), b))
    {
        return false;
    }
    identical
        || (single_return(old_body, a).is_some_and(jsx)
            && single_return(new_body, b).is_some_and(jsx)
            && !new.children(&mut new.walk()).any(|node| node.kind() == "*"))
}
