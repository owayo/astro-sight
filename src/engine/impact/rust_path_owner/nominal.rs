//! 型名と variant の出現を、参照位置で見える宣言に結び付ける。

use super::*;
use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use std::ops::ControlFlow;

enum Binding {
    Local,
    Import(Vec<String>),
    Unknown,
}

#[derive(Default)]
struct Scope {
    parent: Option<usize>,
    bindings: HashMap<String, Binding>,
    glob: bool,
    opaque: bool,
    shadowable_attribute: bool,
    attribute_binding: bool,
}

impl Scope {
    fn bind(&mut self, name: &str, binding: Binding) {
        self.bindings
            .entry(crate::language::normalize_identifier(LangId::Rust, name).into_owned())
            .and_modify(|old| *old = Binding::Unknown)
            .or_insert(binding);
    }

    fn import(&mut self, item: Node<'_>, source: &[u8], module: &[String], stable: bool) {
        let mut edges = Vec::new();
        if item
            .child_by_field_name("argument")
            .and_then(|arg| {
                expand_rust_use_tree_edges_ast(arg, source, &[], module, module, None, &mut edges)
            })
            .is_none()
        {
            self.opaque = true;
            return;
        }
        for edge in edges {
            match edge {
                RustPubUseEdge::Named {
                    exported_name,
                    mut target_module,
                    target_item,
                    origin,
                    ..
                } => {
                    let binding = if stable && origin == UsePathOrigin::Anchored {
                        target_module.push(target_item);
                        Binding::Import(target_module)
                    } else {
                        Binding::Unknown
                    };
                    self.bind(&exported_name, binding);
                }
                RustPubUseEdge::Wildcard { .. } => self.glob = true,
            }
        }
    }
}

fn lookup<'a>(scopes: &'a [Scope], mut scope: usize, name: &str) -> Option<&'a Binding> {
    loop {
        let facts = &scopes[scope];
        if facts.opaque {
            return None;
        }
        if let Some(binding) = facts.bindings.get(name) {
            return Some(binding);
        }
        if facts.glob {
            return None;
        }
        scope = facts.parent?;
    }
}

struct LexicalSites<'tree> {
    scopes: Vec<Scope>,
    candidates: Vec<(Node<'tree>, Node<'tree>, usize)>,
}

impl<'tree> LexicalSites<'tree> {
    fn collect(root: Node<'tree>, source: &[u8], path: &[String], wanted: &HashSet<&str>) -> Self {
        let mut scopes = vec![Scope::default()];
        let mut ancestors = Vec::new();
        let mut prefix = Vec::new();
        let mut candidates = Vec::new();
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
            ancestors.truncate(depth);
            prefix.truncate(depth + 1);
            prefix.resize(depth + 1, (true, true));
            let (stable, transparent) = prefix[depth];
            let (enclosing, parent) = ancestors.last().copied().unwrap_or((0, root));
            let kind = node.kind();
            let skip = ControlFlow::Continue(Walk::Skip);
            if matches!(kind, "line_comment" | "block_comment") {
                return skip;
            }
            if matches!(kind, "attribute_item" | "inner_attribute_item") {
                let attribute = enum_owner::attribute_name(node, source);
                scopes[enclosing].shadowable_attribute |=
                    matches!(attribute, Some("derive" | "test"));
                let builtin = super::super::rust_std_origin::safe_attribute(node, source);
                let safe = attribute == Some("derive") || builtin.is_some();
                if kind == "inner_attribute_item" {
                    scopes[enclosing].opaque |= !safe;
                } else {
                    prefix[depth] = (
                        stable && safe && attribute != Some("cfg"),
                        transparent && safe,
                    );
                }
                return skip;
            }
            prefix[depth] = (true, true);
            // 未知の属性は対象の項目とともに、同じスコープへ別の import を生成し得る。
            scopes[enclosing].opaque |= !transparent;
            if kind == "use_declaration" {
                scopes[enclosing].import(node, source, path, stable);
                return skip;
            }
            if matches!(
                kind,
                "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "type_item"
                    | "mod_item"
                    | "trait_item"
                    | "type_parameter"
                    | "extern_crate_declaration"
            ) {
                if let Some(name) = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"))
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let local = enclosing == 0
                        && stable
                        && matches!(kind, "struct_item" | "enum_item" | "union_item");
                    scopes[enclosing].bind(
                        name,
                        if local {
                            Binding::Local
                        } else {
                            Binding::Unknown
                        },
                    );
                }
                if kind == "mod_item" {
                    return skip;
                }
            }
            if kind == "macro_invocation" {
                scopes[enclosing].opaque |= matches!(
                    parent.kind(),
                    "source_file" | "block" | "expression_statement" | "declaration_list"
                );
                return skip;
            }
            if matches!(kind, "macro_definition" | "token_tree") {
                return skip;
            }
            let scope = if matches!(
                kind,
                "block"
                    | "function_item"
                    | "impl_item"
                    | "trait_item"
                    | "enum_item"
                    | "struct_item"
                    | "union_item"
                    | "type_item"
                    | "closure_expression"
            ) {
                let id = scopes.len();
                scopes.push(Scope {
                    parent: Some(enclosing),
                    ..Default::default()
                });
                id
            } else {
                enclosing
            };
            ancestors.push((scope, node));
            if matches!(kind, "identifier" | "type_identifier")
                && let Ok(spelling) = node.utf8_text(source)
                && wanted.contains(
                    crate::language::normalize_identifier(LangId::Rust, spelling).as_ref(),
                )
            {
                candidates.push((node, parent, scope));
            }
            ControlFlow::Continue(Walk::Descend)
        });
        // 後方の import やマクロが生成する項目も、そのブロック全体で見える。
        for id in 0..scopes.len() {
            let parent = scopes[id].parent;
            let inherited = parent.is_some_and(|p| scopes[p].attribute_binding);
            scopes[id].attribute_binding = inherited
                || scopes[id].glob
                || scopes[id].bindings.contains_key("derive")
                || scopes[id].bindings.contains_key("test");
            scopes[id].opaque |= scopes[id].attribute_binding && scopes[id].shadowable_attribute
                || parent.is_some_and(|p| scopes[p].opaque);
        }
        Self { scopes, candidates }
    }
}

fn stable_file(
    project: &RustPathOwnerProjectProof,
    module: &PrivateModule,
    path: &[String],
) -> Option<PathBuf> {
    let provider = project.declared_module_file(&module.src, path)?;
    let provider_module = project.ordinary_private_module(&provider)?;
    // lib と bin の両方が同じ参照を読むと crate/super の指す名前が変わり得る。
    if module.library_only != provider_module.library_only
        || (!module.library_only && checked_is_file(&project.root, &module.src.join("lib.rs"))?)
    {
        return None;
    }
    let unchanged = ["lib.rs", "main.rs"].iter().all(|name| {
        !project
            .reexport_old_sources
            .contains_key(&module.src.join(name))
    }) && (1..path.len()).all(|end| {
        project
            .declared_module_file(&module.src, &path[..end])
            .is_some_and(|p| !project.reexport_old_sources.contains_key(&p))
    });
    if !unchanged {
        super::super::rust_reexport_owner::stable_module_chain(
            &project.root,
            &module.src,
            path,
            project,
        )?;
    }
    Some(provider)
}

fn type_path(
    parts: &[String],
    scopes: &[Scope],
    scope: usize,
    module: &[String],
) -> Option<Vec<String>> {
    let (first, mut rest) = parts.split_first()?;
    let mut result = match first.as_str() {
        "crate" => Vec::new(),
        "self" => module.to_vec(),
        "super" => {
            let mut path = module.to_vec();
            path.pop()?;
            while rest.first().is_some_and(|part| part == "super") {
                path.pop()?;
                rest = &rest[1..];
            }
            path
        }
        _ => {
            let Binding::Import(import) = lookup(scopes, scope, first)? else {
                return None;
            };
            import.clone()
        }
    };
    result.extend_from_slice(rest);
    Some(result)
}

fn local_inherent_calls(
    root: Node<'_>,
    source: &[u8],
    module: &[String],
    wanted: &HashSet<&str>,
) -> HashSet<usize> {
    use super::super::rust_owner_alias::items_with_attributes;
    let mut methods = HashSet::new();
    for (item, attributed) in items_with_attributes(root.named_children(&mut root.walk())) {
        if attributed
            || item.kind() != "impl_item"
            || item.child_by_field_name("trait").is_some()
            || item.child_by_field_name("type_parameters").is_some()
            || item
                .named_children(&mut item.walk())
                .any(|n| n.kind() == "where_clause")
        {
            continue;
        }
        let Some(owner) = item
            .child_by_field_name("type")
            .filter(|ty| ty.kind() == "type_identifier")
            .and_then(|ty| ty.utf8_text(source).ok())
        else {
            continue;
        };
        let Some(body) = item.child_by_field_name("body") else {
            continue;
        };
        for (method, attributed) in items_with_attributes(body.named_children(&mut body.walk())) {
            if !attributed
                && method.kind() == "function_item"
                && let Some(name) = method
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                && wanted.contains(name.trim_start_matches("r#"))
            {
                methods.insert((
                    owner.trim_start_matches("r#"),
                    name.trim_start_matches("r#"),
                ));
            }
        }
    }
    if methods.is_empty() {
        return HashSet::new();
    }
    let LexicalSites { scopes, candidates } = LexicalSites::collect(root, source, module, wanted);
    candidates
        .into_iter()
        .filter_map(|(node, parent, scope)| {
            if parent.kind() != "scoped_identifier"
                || parent.child_by_field_name("name") != Some(node)
            {
                return None;
            }
            if scopes[scope].opaque {
                return None;
            }
            let path = parent.child_by_field_name("path")?;
            let (owner_node, binding_scope) = match path.kind() {
                "identifier" => (path, scope),
                "scoped_identifier" if path.child_by_field_name("path")?.kind() == "self" => {
                    (path.child_by_field_name("name")?, 0)
                }
                _ => return None,
            };
            let owner = owner_node.utf8_text(source).ok()?.trim_start_matches("r#");
            let name = node.utf8_text(source).ok()?.trim_start_matches("r#");
            (matches!(lookup(&scopes, binding_scope, owner), Some(Binding::Local))
                && methods.contains(&(owner, name)))
            .then_some(node.id())
        })
        .collect()
}

pub(super) struct NominalSites(HashMap<usize, PathBuf>);

impl RustPathOwnerIndex {
    /// 参照ファイルが独自に宣言した型の inherent 関数を、同じ綴りの別の型から区別する。
    pub(in crate::engine::impact) fn separate_inherent_call(
        &self,
        node: Node<'_>,
        source: &[u8],
        file: &Path,
        ctx: &super::super::FileContext,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        let changed = Path::new(&ctx.new_path);
        if changed == file
            || project.reexport_old_sources.len() != 1
            || !project.reexport_old_sources.contains_key(changed)
            || !self.same_source(source)
        {
            return false;
        }
        self.local_inherent_sites
            .get_or_init(|| {
                let prove = || {
                    let origin = project.ordinary_private_module(changed)?;
                    let reader = project.ordinary_private_module(file)?;
                    if origin.src != reader.src
                        || !project.callable_layout_stable()
                        || !project.unrelocated(changed)
                        || !project.unrelocated(file)
                        || stable_file(project, &origin, &origin.path)?.as_path() != changed
                        || stable_file(project, &reader, &reader.path)?.as_path() != file
                    {
                        return None;
                    }
                    let mut root = node;
                    while let Some(parent) = root.parent() {
                        root = parent;
                    }
                    if root.has_error() {
                        return None;
                    }
                    let wanted = ctx
                        .rust_inherent_method_owners
                        .keys()
                        .map(String::as_str)
                        .chain(
                            ctx.rust_narrowed_method
                                .iter()
                                .map(|edit| edit.name.as_str()),
                        )
                        .collect();
                    Some(local_inherent_calls(root, source, &reader.path, &wanted))
                };
                prove().unwrap_or_default()
            })
            .contains(&node.id())
    }

    pub(in crate::engine::impact) fn nominal_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        file: &Path,
        project: &RustPathOwnerProjectProof,
        wanted: &HashSet<&str>,
    ) -> Option<PathBuf> {
        if !matches!(node.kind(), "identifier" | "type_identifier") || !self.same_source(source) {
            return None;
        }
        self.nominal_sites
            .get_or_init(|| {
                let mut root = node;
                while let Some(parent) = root.parent() {
                    root = parent;
                }
                if root.has_error() || project.reexport_old_sources.contains_key(file) {
                    return None;
                }
                self.collect_nominal_sites(root, source, file, project, wanted)
            })
            .as_ref()?
            .0
            .get(&node.id())
            .cloned()
    }

    fn collect_nominal_sites(
        &self,
        root: Node<'_>,
        source: &[u8],
        file: &Path,
        project: &RustPathOwnerProjectProof,
        wanted: &HashSet<&str>,
    ) -> Option<NominalSites> {
        let module = project.ordinary_private_module(file);
        let path = module.as_ref().map_or(&[][..], |m| &m.path);
        let LexicalSites { scopes, candidates } = LexicalSites::collect(root, source, path, wanted);
        let mut owners = HashMap::new();
        let mut checked = HashMap::new();
        let mut resolved = |path: &[String]| -> Option<PathBuf> {
            let module = module.as_ref()?;
            checked
                .entry(path.to_vec())
                .or_insert_with(|| stable_file(project, module, path))
                .clone()
        };
        for (node, parent, scope) in candidates {
            if scopes[scope].opaque {
                continue;
            }
            let spelling =
                crate::language::normalize_identifier(LangId::Rust, node.utf8_text(source).ok()?);
            let qualified = matches!(
                parent.kind(),
                "scoped_identifier" | "scoped_type_identifier"
            );
            let base = qualified && parent.child_by_field_name("path") == Some(node);
            if (!qualified && node.kind() == "type_identifier" || base)
                && matches!(lookup(&scopes, scope, &spelling), Some(Binding::Local))
            {
                owners.insert(node.id(), file.to_owned());
                continue;
            }
            if !qualified || parent.child_by_field_name("name") != Some(node) {
                continue;
            }
            let Some(parts) = path_segments(parent, source) else {
                continue;
            };
            let parts: Vec<_> = parts
                .iter()
                .map(|s| crate::language::normalize_identifier(LangId::Rust, s).into_owned())
                .collect();
            let Some((variant, ty)) = parts.split_last() else {
                continue;
            };
            if let [ty] = ty
                && matches!(lookup(&scopes, scope, ty), Some(Binding::Local))
            {
                if self.has_enum_variant(file, ty, variant, project) {
                    owners.insert(node.id(), file.to_owned());
                }
                continue;
            }
            let Some(module) = module.as_ref() else {
                continue;
            };
            let Some(mut destination) = type_path(ty, &scopes, scope, &module.path) else {
                continue;
            };
            let Some(enum_name) = destination.pop() else {
                continue;
            };
            if !project.callable_layout_stable() || !project.unrelocated(file) {
                continue;
            }
            if resolved(&module.path).as_deref() != Some(file) {
                continue;
            }
            let Some(provider) = resolved(&destination) else {
                continue;
            };
            if !project.reexport_old_sources.contains_key(&provider)
                && project.unrelocated(&provider)
                && self.has_enum_variant(&provider, &enum_name, variant, project)
            {
                owners.insert(node.id(), provider);
            }
        }
        Some(NominalSites(owners))
    }

    fn has_enum_variant(
        &self,
        file: &Path,
        ty: &str,
        variant: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        self.enum_files
            .borrow_mut()
            .entry(file.to_owned())
            .or_insert_with(|| {
                read_input_file(&project.root.join(file))
                    .and_then(|data| enum_owner::EnumFile::read(&data))
            })
            .as_ref()
            .is_some_and(|record| record.contains(ty, variant))
    }
}
