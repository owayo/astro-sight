//! 定数の関数ポインタ初期値が指す、変更されていない直接宣言。
use super::super::rust_owner_alias::{Names, items_with_attributes};
use super::*;
use crate::engine::rust_use_tree::{UsePathOrigin, resolve_use_path_node};

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn in_pointer_initializer(node: Node<'_>) -> bool {
        let Some(mut path) = node.parent() else {
            return false;
        };
        while let Some(parent) = path.parent() {
            if parent.kind() != "parenthesized_expression" {
                return parent.kind() == "const_item"
                    && parent.child_by_field_name("value") == Some(path)
                    && parent
                        .child_by_field_name("type")
                        .is_some_and(|ty| ty.kind() == "function_type");
            }
            path = parent;
        }
        false
    }

    pub(in crate::engine::impact) fn independent_pointer(
        &self,
        node: Node<'_>,
        source: &[u8],
        file: &Path,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        self.independent_pointers
            .get_or_init(|| {
                let mut root = node;
                while let Some(parent) = root.parent() {
                    root = parent;
                }
                pointer_sites(root, source, file, project).unwrap_or_default()
            })
            .contains(&node.id())
    }
}

fn plain_path(mut node: Node<'_>) -> bool {
    while node.kind() == "scoped_identifier" {
        if !node
            .child_by_field_name("name")
            .is_some_and(|name| matches!(name.kind(), "identifier" | "super"))
        {
            return false;
        }
        let Some(path) = node.child_by_field_name("path") else {
            return node.child(0).is_some_and(|c| c.kind() == "::");
        };
        node = path;
    }
    matches!(node.kind(), "crate" | "self" | "super" | "identifier")
}

fn pointer_sites(
    root: Node<'_>,
    source: &[u8],
    file: &Path,
    project: &RustPathOwnerProjectProof,
) -> Option<HashSet<usize>> {
    if root.has_error()
        || project.reexport_old_sources.contains_key(file)
        || !project.callable_layout_stable()
    {
        return None;
    }
    let module = project.ordinary_private_module(file)?;
    if !module.library_only || !project.unrelocated(file) {
        return None;
    }
    let directory = module.src.parent()?;
    let mut sites = HashSet::new();
    let mut pending = vec![(root, module.path.clone())];
    let mut libraries = HashMap::new();
    while let Some((scope, path)) = pending.pop() {
        if !super::macro_crate::plain_scope(scope, source) {
            continue;
        }
        let mut names = None;
        let mut cursor = scope.walk();
        for (item, attributed) in items_with_attributes(scope.named_children(&mut cursor)) {
            if attributed {
                continue;
            }
            match item.kind() {
                "mod_item" => {
                    if let Some(body) = item.child_by_field_name("body") {
                        let mut nested = path.clone();
                        nested.push(
                            crate::language::normalize_identifier(
                                LangId::Rust,
                                item.child_by_field_name("name")?.utf8_text(source).ok()?,
                            )
                            .into_owned(),
                        );
                        pending.push((body, nested));
                    }
                }
                "const_item"
                    if item
                        .child_by_field_name("type")
                        .is_some_and(|ty| ty.kind() == "function_type") =>
                {
                    let mut value = item.child_by_field_name("value")?;
                    while value.kind() == "parenthesized_expression" {
                        value = value.named_child(0)?;
                    }
                    if value.kind() != "scoped_identifier" {
                        continue;
                    }
                    // use 木の解決器は未知の型ノードでも子を試すため、式の UFCS や型引数を先に除く。
                    if !plain_path(value) {
                        continue;
                    }
                    let (mut target_module, Some(target_item), origin) =
                        resolve_use_path_node(value, source, &[], &path)?
                    else {
                        continue;
                    };
                    target_module.push(target_item);
                    for part in &mut target_module {
                        *part =
                            crate::language::normalize_identifier(LangId::Rust, part).into_owned();
                    }
                    let independent = if origin == UsePathOrigin::Anchored {
                        project
                            .macro_function(file, &target_module)
                            .is_some_and(|f| !project.reexport_old_sources.contains_key(&f))
                    } else {
                        let external = if origin == UsePathOrigin::Global {
                            Some(target_module)
                        } else {
                            names
                                .get_or_insert_with(|| Names::collect(scope, source, &path))
                                .as_ref()
                                .and_then(|names| names.extern_path(target_module))
                        };
                        let Some((name, target)) =
                            external.as_deref().and_then(|p| p.split_first())
                        else {
                            continue;
                        };
                        let Some(lib) = libraries
                            .entry(name.to_owned())
                            .or_insert_with(|| project.pointer_library(directory, name))
                        else {
                            continue;
                        };
                        project.unchanged_foreign_pointer(lib, target)
                    };
                    if independent {
                        sites.insert(value.child_by_field_name("name")?.id());
                    }
                }
                _ => {}
            }
        }
    }
    Some(sites)
}

impl RustPathOwnerProjectProof {
    fn pointer_library(&self, directory: &Path, name: &str) -> Option<PathBuf> {
        if !self.edition_2018_or_later(directory)?
            || !self.in_root_workspace(directory)?
            || !self
                .alias_cache
                .pointer_prelude(self, &directory.join("src/lib.rs"))
        {
            return None;
        }
        let manifest = self.alias_cache.manifest(&self.root, directory)?;
        let workspace = self.alias_cache.manifest(&self.root, Path::new(""))?;
        if [&manifest, &workspace]
            .iter()
            .any(|m| m.get("patch").is_some() || m.get("replace").is_some())
            || ["target", "dev-dependencies", "dev_dependencies"]
                .iter()
                .any(|k| manifest.get(k).is_some())
        {
            return None;
        }
        let required = |spec: &toml::Value| {
            spec.as_table().is_some_and(|fields| {
                fields.keys().all(|k| {
                    matches!(
                        k.as_str(),
                        "path"
                            | "package"
                            | "workspace"
                            | "version"
                            | "features"
                            | "default-features"
                            | "optional"
                    )
                })
            }) && spec
                .get("optional")
                .is_none_or(|v| v.as_bool() == Some(false))
        };
        let file = super::super::rust_owner_alias::direct_library(
            directory, &manifest, &workspace, name, self, required,
        )?;
        let source = self.ordinary_private_module(&file)?;
        (source.library_only
            && source.path.is_empty()
            && self.in_root_workspace(source.src.parent()?)?)
        .then_some(file)
    }

    fn unchanged_foreign_pointer(&self, file: &Path, path: &[String]) -> bool {
        self.foreign_pointers
            .get_or_init(|| {
                let mut changed = self.reexport_old_sources.iter();
                let (owner, old) = changed.next()?;
                if changed.next().is_some() || !self.unrelocated(owner) {
                    return None;
                }
                let new = read_input_file(&self.root.join(owner))?;
                let paths =
                    super::super::declaration::unchanged_foreign_pointers(old.as_deref()?, &new)?;
                Some((owner.clone(), paths))
            })
            .as_ref()
            .is_some_and(|(owner, unchanged)| owner == file && unchanged.contains(&path.join("::")))
    }
}
