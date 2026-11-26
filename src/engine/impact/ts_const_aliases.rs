//! named import の別名が指す値を読む位置だけ、元の const の caller に加える。

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::js_scope::{
    Namespace, find_binding_in_scope, is_scope_node, visit_import_bindings,
};
use crate::engine::js_string::decoded;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[derive(Clone, Copy)]
pub(super) struct Binding<T> {
    pub(super) target: T,
    pub(super) type_only: bool,
}

fn has_type_keyword(node: Node<'_>) -> bool {
    node.children(&mut node.walk())
        .any(|child| child.kind() == "type")
}

pub(super) fn import_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<Cow<'a, str>> {
    let raw = node.utf8_text(source).ok()?;
    match node.kind() {
        "identifier" => decoded(raw),
        "string" => decoded(crate::engine::js_string::quoted_contents(raw)?),
        _ => None,
    }
}

pub(super) fn module_imports<'s>(
    root: Node<'_>,
    source: &'s [u8],
    mut visit: impl FnMut(&str, Cow<'s, str>, Option<Cow<'s, str>>, bool, bool),
) {
    for statement in root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "import_statement")
    {
        let Some(module) = statement
            .child_by_field_name("source")
            .and_then(|s| import_name(s, source))
        else {
            continue;
        };
        let type_only = has_type_keyword(statement);
        let _ = walk_preorder(statement, Children::Named, |node, _| {
            let spec = match node.kind() {
                "import_specifier" => node.child_by_field_name("name").and_then(|name| {
                    Some((
                        node.child_by_field_name("alias").unwrap_or(name),
                        Some(import_name(name, source)?),
                    ))
                }),
                "namespace_import" => node
                    .named_children(&mut node.walk())
                    .find(|n| n.kind() == "identifier")
                    .map(|n| (n, None)),
                "import_clause" => node
                    .named_children(&mut node.walk())
                    .find(|n| n.kind() == "identifier")
                    .map(|n| (n, Some(Cow::Borrowed("default")))),
                _ => None,
            };
            if let Some((name, exported)) = spec
                && let Some(local) = import_name(name, source)
            {
                visit(
                    &module,
                    local,
                    exported,
                    type_only || has_type_keyword(node),
                    node.kind() == "import_specifier",
                );
            }
            ControlFlow::<(), Walk>::Continue(Walk::Descend)
        });
    }
}

fn imports<'a, T: Copy>(
    root: Node<'_>,
    source: &'a [u8],
    aliases_only: bool,
    mut resolve: impl FnMut(&str, &str) -> Option<T>,
) -> HashMap<Cow<'a, str>, Binding<T>> {
    let mut names = HashSet::new();
    let mut duplicates = HashSet::new();
    let mut result = HashMap::new();
    for stmt in root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "import_statement")
    {
        visit_import_bindings(stmt, source, |name| {
            if let Some(name) = decoded(name)
                && !names.insert(name.to_string())
            {
                duplicates.insert(name.into_owned());
            }
            false
        });
    }
    module_imports(root, source, |module, local, exported, type_only, named| {
        if let Some(imported) = exported
            && (!aliases_only || local != imported)
            && named
            && let Some(target) = resolve(module, &imported)
        {
            result.insert(local, Binding { target, type_only });
        }
    });
    result.retain(|name, _| !duplicates.contains(name.as_ref()));
    result
}

struct Scope<'a, 'tree> {
    depth: usize,
    node: Node<'tree>,
    visible: HashMap<&'a str, bool>,
}

// 同じスコープ・名前への参照は、その親スコープまで含む解決結果を共用する。
fn visible<'a>(
    scopes: &mut [Scope<'a, '_>],
    source: &[u8],
    name: &'a str,
    escaped: Option<&HashSet<&str>>,
) -> bool {
    let mut from = scopes.len();
    let mut value = true;
    for (i, scope) in scopes.iter().enumerate().rev() {
        if let Some(&previous) = scope.visible.get(name) {
            value = previous;
            break;
        }
        from = i;
        if std::iter::once(name)
            .chain(escaped.into_iter().flatten().copied())
            .any(|spelling| {
                find_binding_in_scope(scope.node, source, spelling, Namespace::Value).is_some()
                    || (matches!(
                        scope.node.kind(),
                        "class"
                            | "class_declaration"
                            | "abstract_class_declaration"
                            | "internal_module"
                    ) && scope
                        .node
                        .child_by_field_name("name")
                        .is_some_and(|n| n.utf8_text(source).ok() == Some(spelling)))
            })
        {
            value = false;
            break;
        }
    }
    for scope in &mut scopes[from..] {
        scope.visible.insert(name, value);
    }
    value
}

pub(super) fn visit<'tree, T: Copy>(
    root: Node<'tree>,
    source: &[u8],
    aliases_only: bool,
    resolve: impl FnMut(&str, &str) -> Option<T>,
    mut read: impl FnMut(Node<'tree>, T),
) {
    if root.has_error() {
        return;
    }
    let bindings = imports(root, source, aliases_only, resolve);
    visit_bindings(root, source, &bindings, |node, _, _, binding| {
        read(node, *binding)
    });
}

pub(super) fn visit_bindings<'tree, T>(
    root: Node<'tree>,
    source: &[u8],
    bindings: &HashMap<Cow<'_, str>, Binding<T>>,
    mut read: impl FnMut(Node<'tree>, &[Node<'tree>], bool, &T),
) {
    if bindings.is_empty() {
        return;
    }
    // 字句解決はソースの綴りを見るので、escape された同名の束縛も照合する。
    let mut escaped: HashMap<String, HashSet<&str>> = HashMap::new();
    if memchr::memmem::find(source, br"\u").is_some() {
        let _ = walk_preorder(root, Children::Named, |node, _| {
            if matches!(
                node.kind(),
                "identifier" | "type_identifier" | "shorthand_property_identifier_pattern"
            ) && let Ok(raw) = node.utf8_text(source)
                && raw.contains('\\')
                && let Some(name) = decoded(raw)
                && bindings.contains_key(name.as_ref())
            {
                escaped.entry(name.into_owned()).or_default().insert(raw);
            }
            ControlFlow::<(), Walk>::Continue(Walk::Descend)
        });
    }
    let mut scopes = Vec::<Scope<'_, '_>>::new();
    let mut type_query_depth = None;
    let mut ancestors = Vec::new();
    let _ = walk_preorder(root, Children::Named, |node, depth| {
        ancestors.truncate(depth);
        while scopes.last().is_some_and(|scope| scope.depth >= depth) {
            scopes.pop();
        }
        if type_query_depth.is_some_and(|start| start >= depth) {
            type_query_depth = None;
        }
        let kind = node.kind();
        if matches!(
            kind,
            "import_statement" | "export_clause" | "namespace_export" | "with_statement"
        ) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if depth > 0
            && (is_scope_node(kind)
                || matches!(
                    kind,
                    "class"
                        | "class_declaration"
                        | "abstract_class_declaration"
                        | "internal_module"
                        | "function_signature"
                        | "method_signature"
                        | "abstract_method_signature"
                        | "function_type"
                        | "constructor_type"
                        | "construct_signature"
                        | "call_signature"
                ))
        {
            scopes.push(Scope {
                depth,
                node,
                visible: HashMap::new(),
            });
        }
        if kind == "type_query" {
            type_query_depth = Some(depth);
        }
        if matches!(
            kind,
            "identifier" | "shorthand_property_identifier" | "import"
        ) && let Ok(text) = node.utf8_text(source)
            && let Some(text) = decoded(text)
            && let Some((name, binding)) = bindings.get_key_value(text.as_ref())
            && (!binding.type_only || type_query_depth.is_some())
            && visible(&mut scopes, source, name, escaped.get(name.as_ref()))
        {
            read(
                node,
                &ancestors,
                type_query_depth.is_some(),
                &binding.target,
            );
        }
        ancestors.push(node);
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
}
