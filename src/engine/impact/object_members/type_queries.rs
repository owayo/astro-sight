//! 型問い合わせは別モジュールにも公開できるため、参照行の振り分け前に変更元ごとに確認する。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::project::Candidate;
use crate::engine::impact::ts_barrels::TsBarrelIndex;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};

fn text<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    node.utf8_text(source).ok().filter(|s| !s.contains('\\'))
}

fn unquoted<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    let raw = text(node, source)?;
    let quote = raw.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    raw.strip_prefix(quote)?.strip_suffix(quote)
}

pub(in crate::engine::impact) fn independent_literal(node: Node<'_>) -> bool {
    !any_node(node, Children::Named, |n| {
        !matches!(
            n.kind(),
            "object"
                | "pair"
                | "property_identifier"
                | "array"
                | "string"
                | "string_fragment"
                | "escape_sequence"
                | "number"
                | "true"
                | "false"
                | "null"
                | "comment"
        )
    })
}

/// import のローカル名から (モジュール指定、元の名前) へ戻す。namespace は元の名前を持たない。
fn imports<'s>(root: Node<'_>, source: &'s [u8]) -> HashMap<&'s str, (&'s str, Option<&'s str>)> {
    let mut bindings = HashMap::new();
    for statement in root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "import_statement")
    {
        let Some(specifier) = statement
            .child_by_field_name("source")
            .and_then(|n| unquoted(n, source))
        else {
            continue;
        };
        let _ = walk_preorder(statement, Children::Named, |n, _| {
            match n.kind() {
                "import_clause" => {
                    if let Some(local) = child(n)
                        .filter(|v| v.kind() == "identifier")
                        .and_then(|v| text(v, source))
                    {
                        bindings.insert(local, (specifier, Some("default")));
                    }
                }
                "import_specifier" => {
                    if let Some(name) = n.child_by_field_name("name").and_then(|v| text(v, source))
                    {
                        let local = n
                            .child_by_field_name("alias")
                            .and_then(|v| text(v, source))
                            .unwrap_or(name);
                        bindings.insert(local, (specifier, Some(name)));
                    }
                }
                "namespace_import" => {
                    if let Some(local) = n.named_child(0).and_then(|v| text(v, source)) {
                        bindings.insert(local, (specifier, None));
                    }
                }
                _ => {}
            }
            ControlFlow::<(), Walk>::Continue(Walk::Descend)
        });
    }
    bindings
}

fn child(node: Node<'_>) -> Option<Node<'_>> {
    node.named_children(&mut node.walk())
        .find(|n| !n.is_extra())
}

/// `typeof obj.a` の dot と `(typeof obj)['a']` の文字列添字を同じパスにする。
/// 動的な添字なら、そこまでに確定した部分 (無ければオブジェクト全体) の型に依存する。
fn query_path<'tree, 's>(
    query: Node<'tree>,
    ancestors: &[Node<'tree>],
    source: &'s [u8],
) -> Option<(Node<'tree>, Vec<&'s str>)> {
    let root = child(query)?;
    let (root, mut path) = member_path(root, source)?;
    let mut inner = query;
    for &wrapper in ancestors.iter().rev() {
        if !matches!(wrapper.kind(), "parenthesized_type" | "lookup_type") {
            break;
        }
        let mut cursor = wrapper.walk();
        let mut children = wrapper
            .named_children(&mut cursor)
            .filter(|n| !n.is_extra());
        if children.next()?.id() != inner.id() {
            break;
        }
        if wrapper.kind() == "lookup_type" {
            let Some(key) = children
                .next()
                .filter(|n| n.kind() == "literal_type")
                .and_then(child)
                .and_then(|n| unquoted(n, source))
                .filter(|key| !key.contains('.'))
            else {
                break;
            };
            path.push(key);
        }
        inner = wrapper;
    }
    Some((root, path))
}

pub(super) fn member_path<'tree, 's>(
    mut root: Node<'tree>,
    source: &'s [u8],
) -> Option<(Node<'tree>, Vec<&'s str>)> {
    let mut path = Vec::new();
    while root.kind() == "member_expression" {
        // 再帰する member_expression では field 検索が内側の property を返すことがある。
        let mut cursor = root.walk();
        let mut parts = root.named_children(&mut cursor).filter(|n| !n.is_extra());
        let object = parts.next()?;
        let property = parts.next().filter(|n| n.kind() == "property_identifier")?;
        if parts.next().is_some() {
            return None;
        }
        path.push(text(property, source)?);
        root = object;
    }
    path.reverse();
    Some((root, path))
}

struct Alias<'s> {
    base: &'s str,
    members: Vec<&'s str>,
    whole: bool,
}

fn aliases<'s>(
    root: Node<'_>,
    source: &'s [u8],
    opaque: &mut HashSet<&'s str>,
) -> HashMap<&'s str, Vec<Alias<'s>>> {
    let mut aliases: HashMap<_, Vec<_>> = HashMap::new();
    let _ = walk_preorder(root, Children::Named, |decl, _| {
        if matches!(
            decl.kind(),
            "variable_declarator" | "required_parameter" | "optional_parameter"
        ) && let Some(pattern) = decl
            .child_by_field_name("name")
            .or_else(|| decl.child_by_field_name("pattern"))
        {
            let value = decl.child_by_field_name("value");
            let literal = value.is_some_and(independent_literal)
                && decl.child_by_field_name("type").is_none()
                || (decl.kind() != "variable_declarator" && value.is_none());
            let root_path = value
                .and_then(|v| member_path(v, source))
                .and_then(|(base, path)| {
                    if base.kind() == "identifier" {
                        Some((text(base, source)?, path))
                    } else {
                        None
                    }
                });
            let mut keys = HashMap::new();
            if pattern.kind() == "object_pattern" {
                for property in pattern.named_children(&mut pattern.walk()) {
                    match property.kind() {
                        "shorthand_property_identifier_pattern" => {
                            if let Some(key) = text(property, source) {
                                keys.insert(property.id(), key);
                            }
                        }
                        "pair_pattern" => {
                            if let (Some(key), Some(binding)) = (
                                property
                                    .child_by_field_name("key")
                                    .filter(|k| k.kind() == "property_identifier")
                                    .and_then(|k| text(k, source)),
                                property
                                    .child_by_field_name("value")
                                    .filter(|v| v.kind() == "identifier"),
                            ) {
                                keys.insert(binding.id(), key);
                            }
                        }
                        _ => {}
                    }
                }
            }
            let _ = crate::engine::js_binding_pattern::visit_pattern_bindings(
                pattern,
                &mut |binding| {
                    if let Some(local) = text(binding, source) {
                        if let Some((base, path)) = &root_path {
                            let key = keys.get(&binding.id());
                            let mut members = path.clone();
                            members.extend(key.copied());
                            aliases.entry(local).or_default().push(Alias {
                                base,
                                members,
                                whole: binding != pattern && key.is_none(),
                            });
                        } else if !literal {
                            opaque.insert(local);
                        }
                    }
                    ControlFlow::<()>::Continue(())
                },
            );
        }
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
    aliases
}

pub(super) fn check_file(
    root: Node<'_>,
    source: &[u8],
    caller: &str,
    barrels: &TsBarrelIndex,
    candidates: &mut [Candidate<'_>],
) {
    let mut locals = None;
    let mut ancestors = Vec::new();
    let _ = walk_preorder(root, Children::Named, |node, depth| {
        ancestors.truncate(depth);
        if node.kind() == "type_query" {
            let (bindings, aliases, opaque) = locals.get_or_insert_with(|| {
                let mut opaque = HashSet::new();
                let aliases = aliases(root, source, &mut opaque);
                (imports(root, source), aliases, opaque)
            });
            let Some((base, path)) = query_path(node, &ancestors, source) else {
                for candidate in &mut *candidates {
                    if candidate.change.type_changes.is_empty() {
                        continue;
                    }
                    candidate.change.queried_type |=
                        barrels.module_may_reach(caller, candidate.path);
                }
                return ControlFlow::Continue(Walk::Skip);
            };
            let inline_import = if base.kind() == "call_expression" {
                base.child_by_field_name("arguments")
                    .and_then(child)
                    .and_then(|n| unquoted(n, source))
                    .map(|specifier| (specifier, None))
            } else {
                None
            };
            let mut pending = vec![(text(base, source), path)];
            let mut seen = std::collections::HashSet::new();
            while let Some((name, path)) = pending.pop() {
                if let Some(name) = name
                    && !seen.insert(name)
                {
                    for candidate in &mut *candidates {
                        if candidate.change.type_changes.is_empty() {
                            continue;
                        }
                        candidate.change.queried_type |=
                            barrels.module_may_reach(caller, candidate.path);
                    }
                    break;
                }
                if let Some(list) = name.and_then(|name| aliases.get(name)) {
                    for alias in list {
                        let mut members = alias.members.clone();
                        if !alias.whole {
                            members.extend_from_slice(&path);
                        }
                        pending.push((Some(alias.base), members));
                    }
                }
                let import = inline_import.or_else(|| name.and_then(|n| bindings.get(n).copied()));
                let (exported, members) = match import {
                    Some((_, None)) => (path.first().copied(), path.get(1..).unwrap_or_default()),
                    Some((_, exported)) => (exported, path.as_slice()),
                    None => (name, path.as_slice()),
                };
                let member = members.join(".");
                for candidate in &mut *candidates {
                    if !candidate.change.queried_type
                        && !candidate.change.type_changes.is_empty()
                        && match import {
                            Some((specifier, _)) => barrels.query_observes_change(
                                caller,
                                specifier,
                                exported,
                                candidate.path,
                                candidate.name,
                                !candidate.change.query_observes_path(&member),
                            ),
                            None => {
                                ((base.kind() != "identifier"
                                    || name.is_none()
                                    || name.is_some_and(|s| opaque.contains(s)))
                                    && barrels.module_may_reach(caller, candidate.path))
                                    || (caller == candidate.path
                                        && name == Some(candidate.name)
                                        && candidate.change.query_observes_path(&member))
                            }
                        }
                    {
                        candidate.change.queried_type = true;
                    }
                }
            }
        }
        ancestors.push(node);
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
}
