//! 名前空間から取り出した値が渡りうる代入先を、Python の構文木ごとに求める。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::target_identifiers;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

pub(crate) const DYNAMIC_NAMES: &[&str] = &[
    "globals",
    "vars",
    "exec",
    "eval",
    "__import__",
    "modules",
    "getattr",
    "setattr",
    "__builtins__",
    "__dict__",
    "import_module",
];

pub(crate) const REFLECTIVE_NAMES: &[&str] = &[
    "locals",
    "delattr",
    "__globals__",
    "f_globals",
    "f_locals",
    "f_builtins",
    "__getattribute__",
    "__getattr__",
    "__setattr__",
    "__delattr__",
    "attrgetter",
    "methodcaller",
    "getattr_static",
    "getmembers",
    "getmembers_static",
    "get_objects",
    "get_referrers",
    "get_referents",
];

pub(crate) fn dynamic_name(name: &str) -> bool {
    DYNAMIC_NAMES.contains(&name)
}

pub(crate) fn reflective_name(name: &str) -> bool {
    REFLECTIVE_NAMES.contains(&name)
}

pub(super) fn binding_targets(root: Node<'_>, source: &[u8]) -> HashSet<usize> {
    let mut uses: HashMap<(Option<usize>, &str), Vec<usize>> = HashMap::new();
    let mut shared_names = HashSet::new();
    let mut targets = Vec::new();
    let mut functions = Vec::<Node<'_>>::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        while functions
            .last()
            .is_some_and(|f| f.end_byte() <= node.start_byte())
        {
            functions.pop();
        }
        if node.kind() == "function_definition" {
            functions.push(node);
        }
        if matches!(node.kind(), "global_statement" | "nonlocal_statement") {
            let mut cursor = node.walk();
            shared_names.extend(
                node.named_children(&mut cursor)
                    .filter_map(|n| n.utf8_text(source).ok()),
            );
        }
        // 入れ子の関数は外側の束縛を閉じ込めうる。独立した関数のローカル別名は混ぜない。
        let scope = functions.first().map(|f| f.id());
        let (target, value) = match node.kind() {
            "assignment" | "augmented_assignment" | "for_statement" => (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ),
            "aliased_import" => (
                node.child_by_field_name("alias"),
                node.child_by_field_name("name"),
            ),
            "as_pattern" => (node.child_by_field_name("alias"), node.named_child(0)),
            "return_statement" => (
                functions.last().and_then(|f| f.child_by_field_name("name")),
                node.named_child(0),
            ),
            "default_parameter" | "typed_default_parameter" => (
                node.child_by_field_name("name"),
                node.child_by_field_name("value"),
            ),
            _ => return ControlFlow::Continue(Walk::Descend),
        };
        if let (Some(target), Some(value)) = (target, value) {
            let index = targets.len();
            let mut value_kind = None;
            let mut read = |name| {
                uses.entry((None, name)).or_default().push(index);
                if scope.is_some() {
                    uses.entry((scope, name)).or_default().push(index);
                }
            };
            let _: ControlFlow<()> = walk_preorder(value, Children::Named, |expr, _| {
                if value_kind.is_none()
                    && !expr.is_extra()
                    && expr.kind() != "parenthesized_expression"
                {
                    value_kind = Some(expr.kind());
                }
                // 入れ子の代入や既定値はその束縛名を経由し、同じ右辺を繰り返し辿らない。
                let nested_target = match expr.kind() {
                    "assignment" => Some("left"),
                    "as_pattern" => Some("alias"),
                    "default_parameter" | "typed_default_parameter" => Some("name"),
                    _ => None,
                };
                if let Some(field) = nested_target {
                    if let Some(target) = expr.child_by_field_name(field) {
                        for identifier in target_identifiers(target) {
                            if let Ok(name) = identifier.utf8_text(source) {
                                read(name);
                            }
                        }
                    }
                    return ControlFlow::Continue(Walk::Skip);
                }
                if expr.kind() == "identifier"
                    && let Ok(name) = expr.utf8_text(source)
                {
                    read(name);
                }
                ControlFlow::Continue(Walk::Descend)
            });
            // lambda 自体は新しいローカル関数だが、呼び出すと名前空間の値を返しうる。
            let owner = if node.kind() == "return_statement" && functions.len() == 1 {
                None
            } else {
                scope
            };
            targets.push((target, owner, value_kind == Some("lambda")));
        }
        ControlFlow::Continue(Walk::Descend)
    });
    let mut pending: Vec<_> = uses
        .keys()
        .copied()
        .filter(|(scope, name)| scope.is_none() && (dynamic_name(name) || reflective_name(name)))
        .collect();
    let mut found = HashSet::new();
    let mut expanded = vec![false; targets.len()];
    while let Some(name) = pending.pop() {
        for index in uses.remove(&name).into_iter().flatten() {
            let (target, owner, is_lambda) = targets[index];
            if !std::mem::replace(&mut expanded[index], true) {
                if !is_lambda {
                    found.insert(target.id());
                }
                let mut shared_end = 0;
                let _: ControlFlow<()> = walk_preorder(target, Children::Named, |node, _| {
                    if matches!(node.kind(), "attribute" | "subscript") {
                        shared_end = node.end_byte().max(shared_end);
                    }
                    if node.kind() == "identifier"
                        && let Ok(name) = node.utf8_text(source)
                    {
                        let local = node.start_byte() >= shared_end && !shared_names.contains(name);
                        pending.push((owner.filter(|_| local), name));
                    }
                    ControlFlow::Continue(Walk::Descend)
                });
            }
        }
    }
    found
}
