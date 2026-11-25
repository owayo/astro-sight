//! Promise の戻り値にだけ使うローカルな object 型の、省略可能プロパティの削除。

use super::*;
use crate::commands::api_changes::{
    ts_const_arg::collect_bindings_named,
    ts_required_members::{generic_argument, only_named_child},
};

pub(super) fn removal<'tree>(
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'tree>,
    new_source: &[u8],
    name: &str,
) -> Option<PropsRemoval<'tree>> {
    let (old_fn, old_statement) = unique_function_declaration(old_root, old_source, name)?;
    let (new_fn, new_statement) = unique_function_declaration(new_root, new_source, name)?;
    if old_statement.kind() != "export_statement"
        || new_statement.kind() != "export_statement"
        || node_signature_tokens(old_statement, old_source)?
            != node_signature_tokens(new_statement, new_source)?
    {
        return None;
    }
    let old_object = return_object(old_root, old_source, old_fn)?;
    let new_object = return_object(new_root, new_source, new_fn)?;
    let removed = removed_optional_members(old_object, old_source, new_object, new_source)?;
    if removed.contains("then") {
        return None;
    }
    let mut retained = HashSet::new();
    for member in members(new_object, new_source)? {
        let ty = only_named_child(member.node.child_by_field_name("type")?)?;
        // await が解決する値の形と、取り出した値の型を変えるプロパティは扱わない。
        if member.name == "then"
            || ty.kind() != "predefined_type"
            || !matches!(
                ty.utf8_text(new_source).ok()?,
                "boolean" | "number" | "string"
            )
        {
            return None;
        }
        retained.insert(member.name);
    }
    prove_resolved_literal(new_fn, new_source, &retained)?;
    Some(PropsRemoval {
        local_alias: true,
        uses: PropertyUses::AwaitedResult { retained },
        type_roots: vec![new_fn.child_by_field_name("parameters")?, new_object],
    })
}

fn return_object<'tree>(
    root: Node<'tree>,
    source: &[u8],
    function: Node<'tree>,
) -> Option<Node<'tree>> {
    let promise = only_named_child(function.child_by_field_name("return_type")?)?;
    let union = generic_argument(promise, source, "Promise")?;
    if union.kind() != "union_type"
        || !collect_bindings_named(root, source, "undefined").is_empty()
        || collect_bindings_named(root, source, "Promise")
            .iter()
            .any(|binding| binding.kind() != "generic_type")
    {
        return None;
    }
    let mut cursor = union.walk();
    let mut arms = union
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra());
    let first = arms.next()?;
    let second = arms.next()?;
    let (alias, missing) = if first.kind() == "type_identifier" {
        (first, second)
    } else {
        (second, first)
    };
    if arms.next().is_some()
        || alias.kind() != "type_identifier"
        || missing.kind() != "literal_type"
        || only_named_child(missing)?.kind() != "undefined"
    {
        return None;
    }
    exclusive_local_object(root, source, alias)
}

/// 戻り値を組み立てる側も、新しい型に残るキーだけを持つリテラルか undefined を返す。
fn prove_resolved_literal(
    function: Node<'_>,
    source: &[u8],
    retained: &HashSet<String>,
) -> Option<()> {
    let statement = only_named_child(function.child_by_field_name("body")?)?;
    let call = only_named_child(statement)?;
    let member = call.child_by_field_name("function")?;
    if statement.kind() != "return_statement"
        || call.kind() != "call_expression"
        || call.child_by_field_name("type_arguments").is_some()
        || member.kind() != "member_expression"
        || member
            .child_by_field_name("object")?
            .utf8_text(source)
            .ok()?
            != "Promise"
        || member
            .child_by_field_name("property")?
            .utf8_text(source)
            .ok()?
            != "resolve"
    {
        return None;
    }
    let argument = only_named_child(call.child_by_field_name("arguments")?)?;
    let proven = |value: Node<'_>| {
        if value.kind() == "undefined" {
            return true;
        }
        let mut cursor = value.walk();
        value.kind() == "object"
            && value
                .named_children(&mut cursor)
                .filter(|n| !n.is_extra())
                .all(|pair| {
                    pair.kind() == "pair"
                        && pair.child_by_field_name("key").is_some_and(|key| {
                            key.kind() == "property_identifier"
                                && key.utf8_text(source).is_ok_and(|name| {
                                    name != "__proto__" && retained.contains(name)
                                })
                        })
                        && pair.child_by_field_name("value").is_some_and(|value| {
                            matches!(value.kind(), "true" | "false" | "string" | "number")
                        })
                })
    };
    let safe = if argument.kind() == "ternary_expression" {
        proven(argument.child_by_field_name("consequence")?)
            && proven(argument.child_by_field_name("alternative")?)
    } else {
        proven(argument)
    };
    safe.then_some(())
}
