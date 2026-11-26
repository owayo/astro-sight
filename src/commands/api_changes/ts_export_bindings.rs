//! 宣言名と独立して、モジュールが明示的に公開している名前とその束縛を比較する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::prepare::ApiChangeBuckets;
use crate::engine::js_string::decoded;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::models::review::ApiSymbolChange;

mod moves;

#[derive(Clone, PartialEq, Eq)]
enum Target {
    Local(String),
    Remote {
        module: String,
        name: Option<String>,
    },
}

#[derive(Clone)]
struct Binding {
    target: Target,
    type_only: bool,
}

impl Binding {
    fn signature(&self, public: &str) -> String {
        let kind = if self.type_only { "type " } else { "" };
        let (name, from) = match &self.target {
            Target::Local(name) => (name, String::new()),
            Target::Remote {
                module,
                name: Some(name),
            } => (name, format!(" from {module:?}")),
            Target::Remote { module, name: None } => {
                return format!("export {kind}* as {} from {module:?}", display_name(public));
            }
        };
        let alias = if public == name {
            String::new()
        } else {
            format!(" as {}", display_name(public))
        };
        format!("export {kind}{{ {}{alias} }}{from}", display_name(name))
    }
}

fn display_name(name: &str) -> String {
    if name.is_empty()
        || name
            .chars()
            .any(|c| !c.is_alphanumeric() && c != '_' && c != '$')
    {
        format!("{name:?}")
    } else {
        name.to_owned()
    }
}

#[derive(Default)]
pub(super) struct Exports {
    names: BTreeMap<String, Binding>,
    locals: HashSet<String>,
    stars: Vec<(String, bool)>,
    incomplete_names: bool,
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    node.children(&mut node.walk()).any(|n| n.kind() == kind)
}

fn name(raw: &str) -> Option<String> {
    let inner = if let Some(quote) = raw.chars().next().filter(|c| matches!(c, '\'' | '"')) {
        raw.strip_prefix(quote)?.strip_suffix(quote)?
    } else {
        raw
    };
    Some(decoded(inner)?.into_owned())
}

fn node_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    name(node.utf8_text(source).ok()?)
}

fn visit_imports(statement: Node<'_>, source: &[u8], mut visit: impl FnMut(String, Binding)) {
    let Some(module) = statement
        .child_by_field_name("source")
        .and_then(|n| node_name(n, source))
    else {
        return;
    };
    let type_only = has_token(statement, "type");
    let _ = walk_preorder(statement, Children::Named, |n, _| {
        let spec = match n.kind() {
            "import_specifier" => n.child_by_field_name("name").and_then(|remote| {
                Some((
                    n.child_by_field_name("alias").unwrap_or(remote),
                    Some(node_name(remote, source)?),
                ))
            }),
            "namespace_import" => n
                .named_children(&mut n.walk())
                .find(|v| v.kind() == "identifier")
                .map(|v| (v, None)),
            "import_clause" => n
                .named_children(&mut n.walk())
                .find(|v| v.kind() == "identifier")
                .map(|v| (v, Some("default".to_owned()))),
            _ => None,
        };
        if let Some((local, remote)) = spec
            && let Some(local) = node_name(local, source)
        {
            visit(
                local,
                Binding {
                    target: Target::Remote {
                        module: module.clone(),
                        name: remote,
                    },
                    type_only: type_only || has_token(n, "type"),
                },
            );
        }
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
}

/// 値の import のローカル名と公開名。namespace import の公開名は None。
pub(super) fn visit_value_imports(
    statement: Node<'_>,
    source: &[u8],
    mut visit: impl FnMut(String, Option<String>),
) {
    visit_imports(statement, source, |local, binding| {
        if !binding.type_only
            && let Target::Remote { name, .. } = binding.target
        {
            visit(local, name);
        }
    });
}

/// 元の公開名とローカル名が等しい、一意な named import。
pub(super) fn same_name_imports(root: Node<'_>, source: &[u8]) -> HashMap<String, (String, bool)> {
    let mut named = HashMap::new();
    let mut seen = HashSet::new();
    for stmt in root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "import_statement")
    {
        visit_imports(stmt, source, |local, binding| {
            if !seen.insert(local.clone()) {
                named.remove(&local);
            } else if let Target::Remote {
                module,
                name: Some(remote),
            } = binding.target
                && local == remote
            {
                named.insert(local, (module, binding.type_only));
            }
        });
    }
    named
}

impl Exports {
    pub(super) fn collect(root: Node<'_>, source: &[u8], strip_internal: bool) -> Self {
        let mut exports = Self {
            incomplete_names: root.has_error(),
            ..Self::default()
        };
        let mut locals = HashMap::new();
        let mut default_local = None;
        let mut marked = false;
        let mut stripped = HashSet::new();
        for stmt in root.named_children(&mut root.walk()) {
            if stmt.kind() == "comment" {
                marked |= strip_internal
                    && stmt
                        .utf8_text(source)
                        .is_ok_and(|comment| comment.contains("@internal"));
                continue;
            }
            if std::mem::take(&mut marked)
                && super::ts_strip_internal::has_internal_comment(stmt, source)
            {
                stripped.insert(stmt.id());
                continue;
            }
            if stmt.kind() == "import_statement" {
                visit_imports(stmt, source, |name, binding| {
                    locals.insert(name, binding);
                });
                continue;
            }
            let exported = stmt.kind() == "export_statement";
            if exported
                && let Some(ns) = stmt
                    .named_children(&mut stmt.walk())
                    .find(|n| n.kind() == "namespace_export")
                && let Some(public) = ns
                    .named_children(&mut ns.walk())
                    .find(|n| !n.is_extra())
                    .and_then(|n| node_name(n, source))
                && let Some(module) = stmt
                    .child_by_field_name("source")
                    .and_then(|n| node_name(n, source))
            {
                exports.names.insert(
                    public,
                    Binding {
                        target: Target::Remote { module, name: None },
                        type_only: has_token(stmt, "type"),
                    },
                );
                continue;
            }
            if exported && has_token(stmt, "*") {
                exports.incomplete_names = true;
                if let Some(module) = stmt
                    .child_by_field_name("source")
                    .and_then(|n| node_name(n, source))
                {
                    exports.stars.push((module, has_token(stmt, "type")));
                }
            }
            if exported
                && has_token(stmt, "default")
                && let Some(n) = stmt
                    .child_by_field_name("value")
                    .filter(|n| n.kind() == "identifier")
            {
                default_local = node_name(n, source);
            }
            let mut decl = stmt.child_by_field_name("declaration").unwrap_or(stmt);
            if decl.kind() == "ambient_declaration"
                && let Some(inner) = decl.named_child(0)
            {
                decl = inner;
            }
            let type_only = matches!(
                decl.kind(),
                "interface_declaration" | "type_alias_declaration"
            );
            let mut declare = |n: Node<'_>| {
                let Some(name) = node_name(n, source) else {
                    return ControlFlow::Continue(());
                };
                let binding = Binding {
                    target: Target::Local(name.clone()),
                    type_only: type_only
                        && locals.get(&name).is_none_or(|previous| previous.type_only),
                };
                if exported {
                    let public = if has_token(stmt, "default") {
                        "default"
                    } else {
                        &name
                    };
                    exports.names.insert(public.to_owned(), binding.clone());
                }
                exports.locals.insert(name.clone());
                locals.insert(name, binding);
                ControlFlow::Continue(())
            };
            if matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
                for var in decl.named_children(&mut decl.walk()) {
                    if let Some(pattern) = var.child_by_field_name("name") {
                        let _ = crate::engine::js_binding_pattern::visit_pattern_bindings(
                            pattern,
                            &mut declare,
                        );
                    }
                }
            } else if let Some(n) = decl.child_by_field_name("name") {
                let _ = declare(n);
            }
        }
        if let Some(local) = default_local
            && let Some(binding) = locals.get(&local)
        {
            exports.names.insert("default".into(), binding.clone());
        }
        crate::engine::symbols::visit_js_ts_export_clause_specifiers(root, source, |spec| {
            if stripped.contains(&spec.statement.id()) {
                return;
            }
            let Some(public) = name(spec.exported_name) else {
                return;
            };
            let Some(local) = name(spec.local_name) else {
                return;
            };
            let module = spec.statement.child_by_field_name("source");
            let type_only = has_token(spec.statement, "type") || has_token(spec.specifier, "type");
            let binding = if let Some(module) = module {
                let Some(module) = node_name(module, source) else {
                    return;
                };
                Binding {
                    target: Target::Remote {
                        module,
                        name: Some(local),
                    },
                    type_only,
                }
            } else {
                let Some(mut binding) = locals.get(&local).cloned() else {
                    return;
                };
                binding.type_only |= type_only;
                binding
            };
            exports.names.insert(public, binding);
        });
        exports
    }
}

#[derive(Default)]
pub(crate) struct RetargetedExports {
    changes: Vec<ApiSymbolChange>,
    moved_exports: BTreeMap<String, BTreeMap<String, Vec<(String, bool)>>>,
    replaced_locals: HashMap<String, HashSet<String>>,
    replacement_locals: HashMap<String, HashSet<String>>,
}

impl RetargetedExports {
    pub(super) fn compare(&mut self, old_path: &str, new_path: &str, old: &Exports, new: &Exports) {
        let mut replaced_locals = HashSet::new();
        let mut replacement_locals = HashSet::new();
        for (public, before) in &old.names {
            if let Target::Local(local) = &before.target
                && !new.locals.contains(local)
                && (new.names.contains_key(public) || !new.stars.is_empty())
            {
                self.moved_exports
                    .entry(old_path.to_owned())
                    .or_default()
                    .entry(local.clone())
                    .or_default()
                    .push((public.clone(), before.type_only));
            }
            let Some(after) = new.names.get(public) else {
                continue;
            };
            if before.target == after.target && (before.type_only || !after.type_only) {
                continue;
            }
            // 宣言を消して同名の再公開へ置き換えた場合は、既存の移動・リテラル比較が扱う。
            if let Target::Local(local) = &before.target {
                if local == public
                    && matches!(after.target, Target::Remote { .. })
                    && !new.locals.contains(local)
                {
                    continue;
                }
                if new.locals.contains(local) {
                    replaced_locals.insert(local.as_str());
                }
            }
            if let Target::Local(local) = &after.target {
                replacement_locals.insert(local.as_str());
            }
            self.changes.push(ApiSymbolChange::new(
                public.clone(),
                "export".into(),
                new_path.into(),
                Some(before.signature(public)),
                Some(after.signature(public)),
            ));
        }
        if !old.incomplete_names && !new.incomplete_names {
            keep_public_names(
                old_path,
                old,
                new,
                replaced_locals,
                &mut self.replaced_locals,
            );
            keep_public_names(
                new_path,
                new,
                old,
                replacement_locals,
                &mut self.replacement_locals,
            );
        }
    }

    pub(super) fn filter_candidates(&self, buckets: &mut ApiChangeBuckets) {
        if !self.replaced_locals.is_empty() {
            buckets.removed.retain(|c| {
                !self
                    .replaced_locals
                    .get(&c.file)
                    .is_some_and(|names| names.contains(&c.name))
            });
        }
        if !self.replacement_locals.is_empty() {
            for candidates in [
                &mut buckets.added,
                &mut buckets.all_new_candidates,
                &mut buckets.added_reference_candidates,
            ] {
                candidates.retain(|c| {
                    !self
                        .replacement_locals
                        .get(&c.file)
                        .is_some_and(|names| names.contains(&c.name))
                });
            }
        }
    }

    pub(super) fn record(self, modified: &mut Vec<ApiSymbolChange>) {
        if self.changes.is_empty() {
            return;
        }
        let existing: HashSet<_> = modified
            .iter()
            .map(|c| (c.file.clone(), c.name.clone()))
            .collect();
        modified.extend(
            self.changes
                .into_iter()
                .filter(|c| !existing.contains(&(c.file.clone(), c.name.clone()))),
        );
    }
}

// 出所の宣言名が公開面の追加・削除を表していなければ、その宣言から生じた候補を外す。
fn keep_public_names<'a>(
    path: &str,
    side: &'a Exports,
    other: &Exports,
    mut locals: HashSet<&'a str>,
    out: &mut HashMap<String, HashSet<String>>,
) {
    if locals.is_empty() {
        return;
    }
    for (public, binding) in &side.names {
        if !other.names.contains_key(public) {
            locals.remove(public.as_str());
            if let Target::Local(local) = &binding.target {
                locals.remove(local.as_str());
            }
        }
    }
    if !locals.is_empty() {
        out.entry(path.to_owned())
            .or_default()
            .extend(locals.into_iter().map(str::to_owned));
    }
}
