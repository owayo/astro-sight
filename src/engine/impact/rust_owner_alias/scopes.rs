//! スコープの終端と引数の束縛を保持し、参照ごとに祖先や兄弟を走査し直さない。

use super::super::rust_path_owner::path_segments;
use super::*;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use std::ops::ControlFlow;

impl Resolver<'_> {
    pub(super) fn index_calls(
        &mut self,
        root: Node<'_>,
        source: &[u8],
        wanted: impl Fn(&str) -> bool,
    ) -> HashMap<usize, String> {
        let mut owners = HashMap::new();
        let mut shadowed_until = HashMap::new();
        let mut parameters = HashMap::new();
        let mut receivers = HashMap::new();
        let mut receiver_end = 0;
        let mut nested_end = 0;
        let upstream_globs = self.upstream_globs(&self.names);
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
            let skip = ControlFlow::Continue(Walk::Skip);
            match node.kind() {
                "token_tree"
                | "attribute_item"
                | "inner_attribute_item"
                | "mod_item"
                | "use_declaration" => return skip,
                "block" | "declaration_list" => {
                    if !index_scope(node, source, &mut shadowed_until) {
                        return skip;
                    }
                }
                "function_item" if depth == 1 => {
                    parameters = receiver_parameters(node, source);
                    receiver_end = node.end_byte();
                }
                "function_item" => {
                    nested_end = nested_end.max(node.end_byte());
                }
                _ => {}
            }
            if let Some(params) = node.child_by_field_name("type_parameters") {
                let unknown = walk_preorder(params, Children::Named, |n, _| {
                    if n.kind() == "type_identifier" {
                        let Some(name) = plain_name(n, source) else {
                            return ControlFlow::Break(());
                        };
                        extend_scope(&mut shadowed_until, name, node.end_byte());
                    }
                    ControlFlow::Continue(Walk::Descend)
                });
                if unknown.is_break() {
                    return skip;
                }
            }
            let plain = |name: &str| {
                shadowed_until
                    .get(name)
                    .is_none_or(|end| node.start_byte() >= *end)
            };
            if node.kind() == "scoped_identifier"
                && upstream_globs
                && let Some(member) = node.child_by_field_name("name")
                && plain_name(member, source).is_some_and(&wanted)
                && let Some(parts) = node
                    .child_by_field_name("path")
                    .and_then(|p| path_segments(p, source))
                && let Some(first) = parts.first()
                && let first = first.strip_prefix("r#").unwrap_or(first)
                && plain(first)
                && let Some(owner) = self.path_owner(&parts, first)
            {
                owners.insert(member.id(), owner);
            } else if node.kind() == "call_expression"
                && node.start_byte() < receiver_end
                && node.start_byte() >= nested_end
                && let Some(member) = node
                    .child_by_field_name("function")
                    .filter(|n| n.kind() == "field_expression")
                && let Some(field) = member.child_by_field_name("field")
                && let Some(method) = plain_name(field, source).filter(|name| wanted(name))
                && let Some(variable) = member
                    .child_by_field_name("value")
                    .filter(|n| n.kind() == "identifier")
                    .and_then(|n| plain_name(n, source))
                && plain(variable)
                && let Some(&(ty, arg)) = parameters.get(variable)
                && let Some(owner) = receivers
                    .entry((ty, arg, method))
                    .or_insert_with(|| self.receiver_owner(ty, arg, method))
                    .as_ref()
            {
                owners.insert(field.id(), owner.clone());
            }
            ControlFlow::Continue(Walk::Descend)
        });
        owners
    }
}

fn extend_scope<'a>(ends: &mut HashMap<&'a str, usize>, name: &'a str, end: usize) {
    let until = ends.entry(name).or_default();
    *until = (*until).max(end);
}

/// 参照位置で書かれた型のパスと、変更元のクレート・モジュール・型名が一致する呼び出し。
pub(in super::super) fn private_calls(
    mut root: Node<'_>,
    source: &[u8],
    file: &Path,
    contexts: &[super::super::FileContext],
    project: &RustPathOwnerProjectProof,
) -> Option<HashMap<usize, u32>> {
    let reference = project.private_call_module(file)?;
    let mut targets = HashMap::<String, HashMap<Vec<String>, u32>>::new();
    for (index, ctx) in contexts.iter().enumerate() {
        if ctx.rust_private_method_owners.is_empty() || Path::new(&ctx.new_path) == file {
            continue;
        }
        let Some(origin) = project.private_call_module(Path::new(&ctx.new_path)) else {
            continue;
        };
        if reference.crate_root != origin.crate_root
            || !reference.modules.starts_with(&origin.modules)
        {
            continue;
        }
        for (method, owner) in &ctx.rust_private_method_owners {
            if !owner.unconditional {
                continue;
            }
            let mut path = origin.modules.clone();
            path.push(identifier(&owner.name));
            targets
                .entry(identifier(method))
                .or_default()
                .entry(path)
                .and_modify(|index| *index = u32::MAX)
                .or_insert(index as u32);
        }
    }
    if targets.is_empty() {
        return None;
    }
    while let Some(parent) = root.parent() {
        root = parent;
    }
    if root.has_error() {
        return None;
    }
    let mut module = reference.modules;
    let mut scopes = vec![(root, 0, std::cell::OnceCell::<Option<Names>>::new())];
    let mut shadowed = HashMap::new();
    let mut generic_end = 0;
    let mut calls = HashMap::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
        let skip = ControlFlow::Continue(Walk::Skip);
        while scopes.len() > 1 && scopes.last().unwrap().0.end_byte() <= node.start_byte() {
            scopes.pop();
            module.pop();
        }
        let (scope, body_depth, names) = scopes.last().unwrap();
        if node.kind() == "mod_item" {
            if depth != body_depth + 1 {
                return skip;
            }
            let (Some(body), Some(name)) = (
                node.child_by_field_name("body"),
                field(node, "name", source),
            ) else {
                return skip;
            };
            module.push(identifier(name));
            scopes.push((body, depth + 1, Default::default()));
        } else if matches!(
            node.kind(),
            "token_tree" | "use_declaration" | "attribute_item"
        ) || node.kind() == "block" && !index_scope(node, source, &mut shadowed)
        {
            return skip;
        } else if node.child_by_field_name("type_parameters").is_some() {
            generic_end = generic_end.max(node.end_byte());
        } else if node.kind() == "call_expression"
            && let Some(callee) = node.child_by_field_name("function")
            && let Some(member) = callee
                .child_by_field_name("name")
                .filter(|_| callee.kind() == "scoped_identifier")
            && let Some(owners) = plain_name(member, source).and_then(|name| targets.get(name))
            && let Some(receiver) = callee.child_by_field_name("path")
        {
            let target = match receiver.kind() {
                "identifier" => plain_name(receiver, source).and_then(|alias| {
                    if node.start_byte() < generic_end
                        || shadowed
                            .get(alias)
                            .is_some_and(|&end| node.start_byte() < end)
                    {
                        return None;
                    }
                    let names = names
                        .get_or_init(|| Names::collect(*scope, source, &module))
                        .as_ref()?;
                    let alias = identifier(alias);
                    let (origin, target) = names.imports.get(&alias)?.as_ref()?;
                    (*origin == UsePathOrigin::Anchored
                        && !names.items.contains(&alias)
                        && names.globs.is_empty())
                    .then(|| target.clone())
                }),
                "scoped_identifier" if path_segments(receiver, source).is_some() => {
                    crate::engine::rust_use_tree::resolve_use_path_node(
                        receiver,
                        source,
                        &[],
                        &module,
                    )
                    .and_then(|(mut path, name, origin)| {
                        if origin != UsePathOrigin::Anchored {
                            return None;
                        }
                        path.push(name?);
                        Some(path.iter().map(|s| identifier(s)).collect())
                    })
                }
                _ => None,
            };
            if let Some(index) = target.and_then(|target| owners.get(&target)) {
                calls.insert(member.id(), *index);
            }
        }
        ControlFlow::Continue(Walk::Descend)
    });
    Some(calls)
}

fn index_scope<'a>(scope: Node<'_>, source: &'a [u8], ends: &mut HashMap<&'a str, usize>) -> bool {
    #[cfg(test)]
    tests::BLOCK_SCANS.update(|n| n + 1);
    let mut cursor = scope.walk();
    for item in scope.named_children(&mut cursor) {
        if matches!(item.kind(), "attribute_item" | "inner_attribute_item")
            && !ordinary_attribute(item, source).is_some_and(|names| names.is_empty())
        {
            return false;
        }
        if scope.kind() == "declaration_list" {
            continue;
        }
        if matches!(
            item.kind(),
            "use_declaration" | "extern_crate_declaration" | "macro_invocation"
        ) || item.kind() == "expression_statement"
            && item
                .named_child(0)
                .is_some_and(|n| n.kind() == "macro_invocation")
        {
            return false;
        }
        if let Some(n) = item.child_by_field_name("name") {
            let Some(name) = plain_name(n, source) else {
                return false;
            };
            extend_scope(ends, name, scope.end_byte());
        }
    }
    true
}

type Parameters<'a> = HashMap<&'a str, (&'a str, Option<&'a str>)>;

fn receiver_parameters<'a>(function: Node<'_>, source: &'a [u8]) -> Parameters<'a> {
    let mut params = HashMap::new();
    if function.child_by_field_name("type_parameters").is_some() {
        return params;
    }
    let Some(list) = function.child_by_field_name("parameters") else {
        return params;
    };
    let mut cursor = list.walk();
    for p in list.named_children(&mut cursor) {
        let param = || {
            let pat = p.child_by_field_name("pattern")?;
            if pat.kind() != "identifier" {
                return None;
            }
            let mut ty = p.child_by_field_name("type")?;
            while ty.kind() == "reference_type" {
                ty = ty.child_by_field_name("type")?;
            }
            Some((plain_name(pat, source)?, simple_type(ty, source)?))
        };
        if let Some((name, ty)) = param() {
            params.insert(name, ty);
        }
    }
    if params.is_empty() {
        return params;
    }
    if let Some(body) = function.child_by_field_name("body") {
        #[cfg(test)]
        tests::BODY_SCANS.update(|n| n + 1);
        let mut patterns = Vec::new();
        let _: ControlFlow<()> = walk_preorder(body, Children::Named, |n, depth| {
            patterns.truncate(depth);
            let in_pattern = n.kind() == "closure_parameters"
                || patterns
                    .last()
                    .is_some_and(|&(inside, id)| inside || Some(n.id()) == id);
            patterns.push((in_pattern, n.child_by_field_name("pattern").map(|p| p.id())));
            if in_pattern {
                match n.kind() {
                    "macro_invocation" => params.clear(),
                    "identifier" | "shorthand_field_identifier" => {
                        if let Some(name) = plain_name(n, source) {
                            params.remove(name);
                        } else {
                            params.clear();
                        }
                    }
                    _ => {}
                }
            }
            if params.is_empty() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        });
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;

    thread_local! {
        pub(super) static BODY_SCANS: Cell<usize> = const { Cell::new(0) };
        pub(super) static BLOCK_SCANS: Cell<usize> = const { Cell::new(0) };
    }

    #[test]
    fn large_function_indexes_each_body_and_block_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = RustPathOwnerProjectProof::new(dir.path(), false, false, false, &[], "");
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let lib = Path::new("src/lib.rs");
        project.alias_cache.track_method_source(dir.path(), lib);
        let library = "pub struct Registry; impl Registry { pub fn add(&self, _: u32) {} }";
        std::fs::write(dir.path().join(lib), library).unwrap();
        let package = Package {
            directory: PathBuf::new(),
            manifest: Arc::new(toml::Value::Table(Default::default())),
            workspace: Arc::new(toml::Value::Table(Default::default())),
            upstream: HashSet::new(),
            library: project.alias_cache.module(dir.path(), lib).unwrap(),
        };
        for count in [8, 8000] {
            let source = format!(
                "use crate::Registry; fn fill(r: &Registry) {{ {} {{ let r = other; r.add(0); }} }} fn plain(r: &Registry) {{ {} }}",
                "r.add(1);".repeat(count),
                "r.add(2);".repeat(count)
            );
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let root = tree.root_node();
            let mut resolver = Resolver {
                names: Names::collect(root, source.as_bytes(), &[]).unwrap(),
                project: &project,
                package: &package,
                bounds: HashMap::new(),
            };
            BODY_SCANS.set(0);
            BLOCK_SCANS.set(0);
            let bindings = MethodBindings {
                owners: resolver.index_calls(root, source.as_bytes(), |name| name == "add"),
            };
            let mut proved = 0;
            let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
                proved += usize::from(bindings.call_owner(node) == Some("Registry"));
                ControlFlow::Continue(Walk::Descend)
            });
            assert_eq!(proved, count);
            assert_eq!((BODY_SCANS.get(), BLOCK_SCANS.get()), (2, 3));
            assert!(Arc::ptr_eq(
                &package.library,
                &project.alias_cache.module(dir.path(), lib).unwrap()
            ));
        }
    }
}
