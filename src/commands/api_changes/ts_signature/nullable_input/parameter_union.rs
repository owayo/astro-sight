//! 引数の最上位の union と、変更した引数を使う戻り式の照合。

use std::cell::OnceCell;
use std::collections::{BTreeSet, HashMap, HashSet};

use super::super::type_dependencies::local_type_values;

use super::{
    NullableTypeChange, TypeChangeDirection, TypeChangePosition, inner, named, union_arms,
};
use crate::commands::api_changes::ts_required_members::only_named_child;
use crate::engine::tree_walk::{Children, any_node};
use tree_sitter::Node;

fn scalar(node: Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "predefined_type" => matches!(
            node.utf8_text(source),
            Ok("string" | "number" | "boolean" | "bigint")
        ),
        "literal_type" => only_named_child(node).is_some_and(|value| {
            matches!(
                value.kind(),
                "string" | "number" | "true" | "false" | "null" | "undefined"
            )
        }),
        _ => false,
    }
}

pub(super) fn added_members(
    old: Node<'_>,
    a: &[u8],
    new: Node<'_>,
    b: &[u8],
) -> Option<Vec<String>> {
    if inner(new)?.kind() != "union_type" {
        return None;
    }
    fn members<'s>(ty: Node<'_>, source: &'s [u8]) -> Option<BTreeSet<&'s str>> {
        if any_node(ty, Children::Named, |node| node.kind() == "comment") {
            return None;
        }
        union_arms(ty)?
            .into_iter()
            .map(|arm| arm.utf8_text(source).ok())
            .collect()
    }
    let old = members(old, a)?;
    let new = members(new, b)?;
    (old.is_subset(&new) && old.len() < new.len())
        .then(|| new.difference(&old).map(|&text| text.to_owned()).collect())
}

/// 宣言の型がテンプレートへ直接展開できるスカラーだけか。呼び出しの文脈型を
/// 変え得る関数型や、解決できない別名は、引数が未使用でも証明しない。
fn scalar_input<'tree>(
    ty: Node<'tree>,
    source: &[u8],
    resolve: impl Fn(&str) -> Option<Node<'tree>>,
) -> Option<bool> {
    let mut all_scalar = true;
    for arm in union_arms(ty)? {
        let value = if arm.kind() == "type_identifier" {
            resolve(arm.utf8_text(source).ok()?)?
        } else {
            arm
        };
        for value in union_arms(value)? {
            if scalar(value, source) {
                continue;
            }
            if !super::non_function_type(value, source) {
                return None;
            }
            all_scalar = false;
        }
    }
    Some(all_scalar)
}

/// 変更した引数自体の展開か、リテラルを既定値にする展開。
fn interpolated_parameter(substitution: Node<'_>) -> Option<Node<'_>> {
    let value = only_named_child(substitution)?;
    let candidate = match value.kind() {
        "identifier" => value,
        "binary_expression"
            if value.child_by_field_name("operator")?.kind() == "??"
                && matches!(
                    value.child_by_field_name("right")?.kind(),
                    "string" | "number" | "true" | "false" | "null"
                ) =>
        {
            value.child_by_field_name("left")?
        }
        _ => return None,
    };
    (candidate.kind() == "identifier").then_some(candidate)
}

/// 推論される戻り値を引数から切り離す。テンプレートの他の式は、変更した引数を使わなければよい。
pub(super) fn body_preserves_contract(
    root: Node<'_>,
    params: Node<'_>,
    parameters: &[Node<'_>],
    body: Node<'_>,
    source: &[u8],
    return_type: Option<Node<'_>>,
    changes: &[NullableTypeChange],
) -> Option<()> {
    let positions = changes
        .iter()
        .map(|change| {
            (change.position == TypeChangePosition::Input
                && change.direction == TypeChangeDirection::Widening
                && change.property_path.is_empty())
            .then_some(change.parameter)?
        })
        .collect::<Option<BTreeSet<_>>>()?;
    let locals = OnceCell::new();
    let resolve = |name: &str| {
        locals
            .get_or_init(|| local_type_values(root, source, true))
            .as_ref()?
            .get(name)
            .copied()
    };
    let mut names = HashMap::new();
    let mut allowed = HashSet::new();
    for i in positions {
        let param = parameters.get(i)?;
        let pattern = param
            .child_by_field_name("pattern")
            .filter(|node| node.kind() == "identifier")?;
        let scalar = scalar_input(param.child_by_field_name("type")?, source, resolve)?;
        allowed.insert(pattern.id());
        names.insert(pattern.utf8_text(source).ok()?, scalar);
    }
    let returned = only_named_child(body)
        .filter(|n| n.kind() == "return_statement")
        .and_then(only_named_child);
    if return_type.is_none()
        && !named(body).is_empty()
        && returned.is_none_or(|value| {
            !matches!(
                value.kind(),
                "template_string" | "string" | "number" | "true" | "false" | "null"
            )
        })
    {
        return None;
    }
    let string_result = return_type.is_none_or(|annotation| {
        inner(annotation).is_some_and(|ty| {
            ty.kind() == "predefined_type" && ty.utf8_text(source) == Ok("string")
        })
    });
    if string_result
        && let Some(template) = returned.filter(|node| node.kind() == "template_string")
    {
        for input in named(template)
            .into_iter()
            .filter(|node| node.kind() == "template_substitution")
            .filter_map(interpolated_parameter)
        {
            if names.get(input.utf8_text(source).ok()?) == Some(&true) {
                allowed.insert(input.id());
            }
        }
    }
    let unproven = [params, body].into_iter().any(|tree| {
        any_node(tree, Children::Named, |node| {
            matches!(
                node.kind(),
                "identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
            ) && node
                .utf8_text(source)
                .is_ok_and(|name| names.contains_key(name))
                && !allowed.contains(&node.id())
        })
    });
    (!unproven).then_some(())
}
