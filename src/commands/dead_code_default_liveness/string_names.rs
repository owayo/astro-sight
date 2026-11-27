use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use tree_sitter::Node;

use super::{ExportNames, NamedUses};
use crate::commands::{reference_evidence::record, ts_const_rebinding::resolved_const_import};
use crate::engine::js_scope::{
    Namespace, find_binding_in_scope, is_scope_node, pattern_binds_name,
};
use crate::engine::js_string::{decoded, quoted_contents};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

fn string_value<'a>(node: Node<'_>, source: &'a [u8]) -> Option<Cow<'a, str>> {
    let text = node.utf8_text(source).ok()?;
    let contents = match node.kind() {
        "string" => quoted_contents(text)?,
        "template_string"
            if !node
                .named_children(&mut node.walk())
                .any(|n| n.kind() == "template_substitution") =>
        {
            text.strip_prefix('`')?.strip_suffix('`')?
        }
        _ => return None,
    };
    decoded(contents)
}

fn expression(mut node: Node<'_>) -> Option<Node<'_>> {
    while matches!(
        node.kind(),
        "parenthesized_expression" | "parenthesized_type"
    ) {
        node = node
            .named_children(&mut node.walk())
            .find(|n| !n.is_extra())?;
    }
    Some(node)
}

fn child(node: Node<'_>) -> Option<Node<'_>> {
    node.named_children(&mut node.walk())
        .find(|p| !p.is_extra())
}

fn access(node: Node<'_>) -> Option<(Node<'_>, Node<'_>, bool)> {
    match node.kind() {
        "subscript_expression" => Some((
            expression(node.child_by_field_name("object")?)?,
            expression(node.child_by_field_name("index")?)?,
            false,
        )),
        "lookup_type" => {
            let mut cursor = node.walk();
            let mut parts = node.named_children(&mut cursor).filter(|n| !n.is_extra());
            let object = expression(parts.next()?)?;
            let key = expression(parts.next()?)?;
            if object.kind() != "type_query" || key.kind() != "literal_type" {
                return None;
            }
            Some((child(object)?, child(key)?, true))
        }
        _ => None,
    }
}

fn type_only(node: Node<'_>) -> bool {
    node.children(&mut node.walk()).any(|n| n.kind() == "type")
}

fn shadows(scope: Node<'_>, parameters: Option<Node<'_>>, source: &[u8], name: &str) -> bool {
    if scope.kind() == "class" || parameters.is_some() {
        let self_name = matches!(
            scope.kind(),
            "class" | "function_expression" | "generator_function"
        ) && scope
            .child_by_field_name("name")
            .is_some_and(|n| n.utf8_text(source).ok() == Some(name));
        // 既定引数は本体の変数がまだ見えない環境で評価される。
        return self_name
            || parameters.is_some_and(|params| pattern_binds_name(params, source, name));
    }
    find_binding_in_scope(scope, source, name, Namespace::Value).is_some()
}

/// 直接の import 辺だけを解決する。文字列そのものを名前検索の対象に加えない。
pub(super) fn collect(
    root: Node<'_>,
    source: &[u8],
    dir: &Path,
    file: &str,
    targets: &HashMap<String, ExportNames>,
    uses: &mut NamedUses,
) {
    let Some(dir) = dir.to_str() else { return };
    let mut namespaces = HashMap::new();
    let mut report = |target: &str, name: &str, node: Node<'_>| {
        if targets
            .get(target)
            .is_some_and(|names| names.named.contains(name))
        {
            let refs = uses
                .entry(target.to_owned())
                .or_default()
                .entry(name.to_owned())
                .or_default();
            let point = node.start_position();
            record(refs, file, point.row, point.column);
        }
    };
    for stmt in root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "import_statement")
    {
        let Some(clause) = stmt
            .named_children(&mut stmt.walk())
            .find(|n| n.kind() == "import_clause")
        else {
            continue;
        };
        let Some(spec) = stmt
            .child_by_field_name("source")
            .and_then(|n| string_value(n, source))
        else {
            continue;
        };
        let target = std::cell::LazyCell::new(|| {
            resolved_const_import(dir, file, &spec).filter(|target| targets.contains_key(target))
        });
        let _ = walk_preorder(clause, Children::Named, |node, _| {
            match node.kind() {
                "namespace_import" => {
                    if let Some(local) = node
                        .named_children(&mut node.walk())
                        .find(|n| n.kind() == "identifier")
                        && let Ok(raw) = local.utf8_text(source)
                        && let Some(key) = decoded(raw)
                        && let Some(target) = target.as_ref()
                    {
                        namespaces.insert(key, (target.clone(), type_only(stmt)));
                    }
                }
                "import_specifier" => {
                    if let Some(public) = node.child_by_field_name("name")
                        && let Some(name) = string_value(public, source)
                        && let Some(target) = target.as_ref()
                    {
                        report(target, &name, public);
                    }
                }
                _ => {}
            }
            ControlFlow::<(), _>::Continue(Walk::Descend)
        });
    }
    if namespaces.is_empty() {
        return;
    }
    let mut spellings: HashMap<_, HashSet<&str>> = HashMap::new();
    if memchr::memmem::find(source, br"\u").is_some() {
        let _ = walk_preorder(root, Children::Named, |node, _| {
            if crate::engine::js_occurrence::identifier_kind(node.kind())
                && let Ok(raw) = node.utf8_text(source)
                && raw.contains('\\')
                && let Some(name) = decoded(raw)
                && namespaces.contains_key(&name)
            {
                spellings.entry(name).or_default().insert(raw);
            }
            ControlFlow::<(), _>::Continue(Walk::Descend)
        });
    }
    let mut scopes = Vec::new();
    let mut scope_shadows = HashMap::new();
    let _ = walk_preorder(root, Children::Named, |node, depth| {
        while scopes.last().is_some_and(|(d, _)| *d >= depth) {
            scopes.pop();
        }
        if node != root && (is_scope_node(node.kind()) || node.kind() == "class") {
            scopes.push((depth, node));
        }
        if let Some((object, index, type_query)) = access(node)
            && object.kind() == "identifier"
            && let Some(receiver) = object.utf8_text(source).ok().and_then(decoded)
            && let Some((key, (target, type_only))) = namespaces.get_key_value(&receiver)
            && (!type_only || type_query)
            && let Some(name) = string_value(index, source)
            && !scopes.iter().rev().any(|(_, scope)| {
                let parameters = scope
                    .child_by_field_name("parameters")
                    .filter(|params| params.byte_range().contains(&object.start_byte()));
                *scope_shadows
                    .entry((scope.id(), key.as_ref(), parameters.is_some()))
                    .or_insert_with(|| {
                        std::iter::once(key.as_ref())
                            .chain(spellings.get(key).into_iter().flatten().copied())
                            .any(|spelling| shadows(*scope, parameters, source, spelling))
                    })
            })
        {
            report(target, &name, index);
        }
        ControlFlow::<(), _>::Continue(Walk::Descend)
    });
}
