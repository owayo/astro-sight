//! star の先に無い公開名を、旧いモジュールから今も取り込んでいるか調べる。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use super::super::{name as export_name, node_name, visit_imports};
use super::{ApiClosureCaches, ApiSymbolCandidate, Imports, NameAt, Target};
use crate::commands::api_changes::ref_index::inventory::{
    opaque_transport, transport_source, untracked_loader, visit_project_sources,
};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

pub(super) fn still_imported(
    dir: &str,
    candidates: Vec<(NameAt, ApiSymbolCandidate)>,
    imports: &mut Imports,
    caches: &mut ApiClosureCaches,
) -> Vec<(NameAt, ApiSymbolCandidate)> {
    if candidates.is_empty() {
        return candidates;
    }
    let mut by_path = HashMap::<&str, Vec<usize>>::new();
    let mut by_name = HashMap::<&str, Vec<usize>>::new();
    for (i, (_, c)) in candidates.iter().enumerate() {
        by_path.entry(&c.file).or_default().push(i);
        by_name.entry(&c.name).or_default().push(i);
    }
    let mut used = HashSet::new();
    let names: Vec<_> = by_name.keys().copied().collect();
    let complete = visit_project_sources(dir, &names, caches, |path, root, source| {
        let mut record = |module: Option<&str>, name: Option<&str>| {
            if name.is_some_and(|n| !by_name.contains_key(n)) {
                return;
            }
            let target = module.and_then(|s| imports.target(dir, path, s));
            if let Some(target) = target {
                if let Some(indices) = by_path.get(target) {
                    used.extend(
                        indices
                            .iter()
                            .copied()
                            .filter(|&i| name.is_none_or(|n| n == candidates[i].1.name)),
                    );
                }
            } else if let Some(name) = name {
                used.extend(by_name[name].iter().copied());
            } else {
                used.extend(0..candidates.len());
            }
        };
        crate::engine::symbols::visit_js_ts_export_clause_specifiers(root, source, |spec| {
            if let Some(module) = spec.statement.child_by_field_name("source") {
                record(
                    node_name(module, source).as_deref(),
                    export_name(spec.local_name).as_deref(),
                );
            }
        });
        let mut namespaces = HashMap::new();
        for stmt in root
            .named_children(&mut root.walk())
            .filter(|n| n.kind() == "import_statement")
        {
            if stmt.child_by_field_name("source").is_none() {
                record(None, None);
            }
            visit_imports(stmt, source, |local, b| {
                if let Target::Remote { module, name } = b.target {
                    if let Some(name) = name {
                        record(Some(&module), Some(&name));
                    } else {
                        namespaces.insert(local, module);
                    }
                }
            });
        }
        let mut objects = HashSet::new();
        let mut invoked = HashSet::new();
        let checked = walk_preorder(root, Children::Named, |n, _| {
            if untracked_loader(n, source, &mut invoked) {
                return ControlFlow::Break(());
            }
            match n.kind() {
                "import_statement" => return ControlFlow::Continue(Walk::Skip),
                "export_statement" if n.child_by_field_name("source").is_some() => {
                    if opaque_transport(n, source) {
                        record(transport_source(n, source), None);
                    }
                    return ControlFlow::Continue(Walk::Skip);
                }
                "call_expression" if opaque_transport(n, source) => {
                    record(transport_source(n, source), None)
                }
                "member_expression" | "subscript_expression" if !namespaces.is_empty() => {
                    if let Some(object) = n
                        .child_by_field_name("object")
                        .filter(|obj| obj.kind() == "identifier")
                        && let Some(module) =
                            node_name(object, source).and_then(|name| namespaces.get(&name))
                    {
                        objects.insert(object.id());
                        let key = n.child_by_field_name("property").or_else(|| {
                            n.child_by_field_name("index")
                                .filter(|key| key.kind() == "string")
                        });
                        record(
                            Some(module),
                            key.and_then(|key| node_name(key, source)).as_deref(),
                        );
                    }
                }
                "identifier" | "type_identifier" | "shorthand_property_identifier"
                    if !namespaces.is_empty() && !objects.contains(&n.id()) =>
                {
                    if let Some(module) =
                        node_name(n, source).and_then(|name| namespaces.get(&name))
                    {
                        record(Some(module), None);
                    }
                }
                _ => {}
            }
            ControlFlow::<(), Walk>::Continue(Walk::Descend)
        });
        checked.is_continue().then_some(())
    })
    .is_some();
    candidates
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| (!complete || used.contains(&i)).then_some(c))
        .collect()
}
