//! Rust の呼び出し側で明示された型と import を解決する。解決できない名前は impact に残す。

mod package;
mod scopes;
use package::{Declaration, Package, ReceiverBound};
pub(super) use package::{PackageCache, items_with_attributes};
pub(super) use scopes::private_calls;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use crate::engine::bounded_read::{read_input_file, read_input_text_file};
use crate::engine::parser;
use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};

use crate::language::LangId;

use super::rust_path_owner::{PrivateModule, RustPathOwnerProjectProof};

fn field<'a>(node: Node<'_>, key: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(key)?.utf8_text(source).ok()
}

fn plain_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = node.utf8_text(source).ok()?;
    name.is_ascii()
        .then(|| name.strip_prefix("r#").unwrap_or(name))
}

/// cfg_attr の属性が lint 水準だけなら、名前の束縛には影響しない。
fn ordinary_attribute(item: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    if let Some(names) = super::rust_std_origin::safe_attribute(item, source) {
        return Some(names);
    }
    let attribute = item.named_child(0)?;
    if attribute.named_child(0).and_then(|n| plain_name(n, source)) != Some("cfg_attr") {
        return None;
    }
    let arguments = attribute.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let mut tokens = arguments
        .children(&mut cursor)
        .skip_while(|n| n.kind() != ",")
        .skip(1);
    (matches!(
        tokens.next().and_then(|n| plain_name(n, source)),
        Some("allow" | "warn" | "deny" | "expect" | "forbid")
    ) && tokens.next().is_some_and(|n| n.kind() == "token_tree")
        && tokens.next().is_some_and(|n| n.kind() == ")"))
    .then(Vec::new)
}

fn identifier(name: &str) -> String {
    crate::language::normalize_identifier(LangId::Rust, name).into_owned()
}

#[derive(Default)]
pub(super) struct Names {
    items: HashSet<String>,
    imports: HashMap<String, Option<(UsePathOrigin, Vec<String>)>>,
    globs: Vec<(UsePathOrigin, Vec<String>)>,
}

impl Names {
    pub(super) fn extern_path(&self, mut path: Vec<String>) -> Option<Vec<String>> {
        let first = path.first()?;
        if !self.globs.is_empty() || self.items.contains(first) {
            return None;
        }
        if let Some(alias) = self.imports.get(first) {
            let (origin, target) = alias.as_ref()?;
            let base = target.first()?;
            if *origin != UsePathOrigin::Global
                && (*origin != UsePathOrigin::Bare
                    || self.items.contains(base)
                    || self.imports.contains_key(base))
            {
                return None;
            }
            path.splice(..1, target.iter().cloned());
        }
        Some(path)
    }

    pub(super) fn collect(root: Node<'_>, source: &[u8], module: &[String]) -> Option<Self> {
        if root.has_error() {
            return None;
        }
        let mut names = Self::default();
        let mut attribute_names = Vec::new();
        let mut cursor = root.walk();
        let mut attributed = false;
        for item in root.named_children(&mut cursor) {
            match item.kind() {
                "attribute_item" | "inner_attribute_item" => {
                    attribute_names.extend(ordinary_attribute(item, source)?);
                    attributed |= item.kind() == "attribute_item";
                    continue;
                }
                "line_comment" | "block_comment" => continue,
                "use_declaration" => {
                    let mut edges = Vec::new();
                    expand_rust_use_tree_edges_ast(
                        item.child_by_field_name("argument")?,
                        source,
                        &[],
                        module,
                        module,
                        None,
                        &mut edges,
                    )?;
                    for edge in edges {
                        match edge {
                            RustPubUseEdge::Named {
                                exported_name,
                                mut target_module,
                                target_item,
                                origin,
                                ..
                            } => {
                                target_module.push(target_item);
                                let entry = names.imports.entry(identifier(&exported_name));
                                let value = (!attributed).then(|| {
                                    (
                                        origin,
                                        target_module.iter().map(|name| identifier(name)).collect(),
                                    )
                                });
                                entry.and_modify(|old| *old = None).or_insert(value);
                            }
                            RustPubUseEdge::Wildcard {
                                target_module,
                                origin,
                                ..
                            } => names.globs.push((
                                origin,
                                target_module.iter().map(|name| identifier(name)).collect(),
                            )),
                        }
                    }
                }
                "expression_statement" | "macro_invocation" | "extern_crate_declaration" => {
                    return None;
                }
                _ => {
                    if let Some(identifier) = item.child_by_field_name("name") {
                        names
                            .items
                            .insert(plain_name(identifier, source)?.to_owned());
                    }
                }
            }
            attributed = false;
        }
        attribute_names
            .iter()
            .all(|name| !names.items.contains(name) && !names.imports.contains_key(name))
            .then_some(names)
    }
}

/// 通常の path 依存（workspace からの継承・rename を含む）が指す既定の library。
pub(super) fn direct_library(
    directory: &Path,
    manifest: &toml::Value,
    workspace: &toml::Value,
    name: &str,
    project: &RustPathOwnerProjectProof,
    accepts: impl Fn(&toml::Value) -> bool,
) -> Option<PathBuf> {
    let mut matches = manifest
        .get("dependencies")?
        .as_table()?
        .iter()
        .filter(|(key, _)| key.replace('-', "_") == name);
    let (key, spec) = matches.next()?;
    if matches.next().is_some() || !accepts(spec) {
        return None;
    }
    let (base, spec) = if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        (
            Path::new(""),
            workspace.get("workspace")?.get("dependencies")?.get(key)?,
        )
    } else {
        (directory, spec)
    };
    if !accepts(spec) {
        return None;
    }
    let root = &project.root;
    let absolute = root
        .join(base)
        .join(spec.get("path")?.as_str()?)
        .canonicalize()
        .ok()?;
    let dir = absolute
        .strip_prefix(project.canonical_root.as_ref()?)
        .ok()?;
    let dependency = project.alias_cache.manifest(root, dir)?;
    let package_name = dependency.get("package")?.get("name")?.as_str()?;
    if package_name
        != spec
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(key)
        || dependency.get("lib").is_some_and(|lib| {
            lib.get("path").is_some()
                || lib
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|lib| lib != name)
        })
    {
        return None;
    }
    Some(dir.join("src/lib.rs"))
}
pub(super) struct MethodBindings {
    owners: HashMap<usize, String>,
}

struct Resolver<'a> {
    names: Names,
    package: &'a Package,
    project: &'a RustPathOwnerProjectProof,
    bounds: HashMap<(String, String), bool>,
}

impl MethodBindings {
    pub(super) fn build(
        mut root: Node<'_>,
        source: &[u8],
        file: &str,
        module: &PrivateModule,
        project: &RustPathOwnerProjectProof,
        wanted: impl Fn(&str) -> bool,
    ) -> Option<Self> {
        if !module.library_only || project.reexport_old_sources.contains_key(Path::new(file)) {
            return None;
        }
        let package = project.alias_cache.package(project, &module.src)?;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let names = Names::collect(root, source, &module.path)?;
        let mut resolver = Resolver {
            names,
            package: &package,
            project,
            bounds: HashMap::new(),
        };
        let owners = resolver.index_calls(root, source, wanted);
        Some(Self { owners })
    }

    pub(super) fn call_owner(&self, node: Node<'_>) -> Option<&str> {
        self.owners.get(&node.id()).map(String::as_str)
    }
}

impl Resolver<'_> {
    fn upstream(&self, name: &str) -> bool {
        self.package.upstream.contains(name)
    }

    fn unbound_upstream(&self, name: &str) -> bool {
        self.upstream(name)
            && !self.names.items.contains(name)
            && !self.names.imports.contains_key(name)
    }

    fn upstream_globs(&self, names: &Names) -> bool {
        names.globs.iter().all(|(origin, path)| {
            *origin != UsePathOrigin::Anchored
                && path.first().is_some_and(|name| {
                    self.upstream(name)
                        && !names.items.contains(name)
                        && !names.imports.contains_key(name)
                })
        })
    }

    fn path_owner(&self, parts: &[String], first: &str) -> Option<String> {
        if first == "crate" {
            let [_, name] = parts else {
                return None;
            };
            return self
                .package
                .library
                .declares(name, Declaration::Nominal)
                .then(|| name.clone());
        }
        if self.names.items.contains(first) {
            return None;
        }
        if let Some(import) = self.names.imports.get(first) {
            let (origin, target) = import.as_ref()?;
            if *origin == UsePathOrigin::Anchored {
                let [name] = target.as_slice() else {
                    return None;
                };
                if parts.len() != 1 {
                    return None;
                }
                return (self.package.library.declares(name, Declaration::Nominal)
                    || self.package.library.declares(name, Declaration::Module))
                .then(|| name.clone());
            }
            if self.unbound_upstream(target.first()?) {
                return Some(String::new());
            }
            return None;
        }
        // glob が依存クレート由来でも、そのクレートはこのライブラリの inherent impl の型を定義できない。
        (self.unbound_upstream(first) || first == "Box" && parts.len() == 1).then(String::new)
    }

    fn receiver_owner(&mut self, owner: &str, arg: Option<&str>, method: &str) -> Option<String> {
        let (UsePathOrigin::Anchored, imported) = self.names.imports.get(owner)?.as_ref()? else {
            return None;
        };
        let [nominal] = imported.as_slice() else {
            return None;
        };
        let nominal = nominal.clone();
        let library = &self.package.library;
        if !library.declares(&nominal, Declaration::Nominal) {
            return None;
        }
        let bounds = library.methods.get(&nominal)?.get(method)?;
        let applicable = bounds.iter().any(|bound| match (bound, arg) {
            (ReceiverBound::Concrete, None) | (ReceiverBound::Generic, Some(_)) => true,
            (ReceiverBound::Trait(bound), Some(arg)) => {
                let key = (arg.to_owned(), bound.clone());
                if let Some(value) = self.bounds.get(&key) {
                    return *value;
                }
                let value = self.satisfies_bound(arg, bound) == Some(true);
                self.bounds.insert(key, value);
                value
            }
            _ => false,
        });
        applicable.then_some(nominal)
    }

    /// 同じ直接の path 依存のルートにある具体型・trait・無条件の impl を照合する。
    fn satisfies_bound(&self, argument: &str, bound: &str) -> Option<bool> {
        let library_names = self.package.library.names.as_ref()?;
        let direct = |names: &Names, name: &str| -> Option<Vec<String>> {
            let (origin, target) = names.imports.get(name)?.as_ref()?;
            (*origin == UsePathOrigin::Bare && target.len() == 2 && !names.items.contains(name))
                .then(|| target.clone())
        };
        let ty = direct(&self.names, argument)?;
        let tr = direct(library_names, bound)?;
        if ty[0] != tr[0]
            || !self.upstream_globs(&self.names)
            || !self.upstream_globs(library_names)
            || !self.unbound_upstream(&ty[0])
            || library_names.items.contains(&tr[0])
            || library_names.imports.contains_key(&tr[0])
        {
            return None;
        }
        let root = &self.project.root;
        let file = direct_library(
            &self.package.directory,
            &self.package.manifest,
            &self.package.workspace,
            &ty[0],
            self.project,
            |_| true,
        )?;
        if self.project.reexport_old_sources.contains_key(&file) {
            return None;
        }
        let src = self.project.alias_cache.module(root, &file)?;
        Some(
            src.declares(&ty[1], Declaration::Nominal)
                && src.declares(&tr[1], Declaration::Trait)
                && src
                    .implementations
                    .get(&ty[1])
                    .is_some_and(|traits| traits.contains(&tr[1])),
        )
    }
}

fn simple_type<'a>(ty: Node<'_>, source: &'a [u8]) -> Option<(&'a str, Option<&'a str>)> {
    if ty.kind() == "type_identifier" {
        return Some((plain_name(ty, source)?, None));
    }
    if ty.kind() != "generic_type" {
        return None;
    }
    let owner = ty.child_by_field_name("type")?;
    let arg = only_child(ty.child_by_field_name("type_arguments")?)?;
    if owner.kind() != "type_identifier" || arg.kind() != "type_identifier" {
        return None;
    }
    Some((plain_name(owner, source)?, Some(plain_name(arg, source)?)))
}

fn only_child(node: Node<'_>) -> Option<Node<'_>> {
    (node.named_child_count() == 1)
        .then(|| node.named_child(0))
        .flatten()
}

fn has_child(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|n| n.kind() == kind)
}

/// 属性のない自由関数の式だけを書き換えたファイルは、他ファイルの名前の束縛に影響しない。
pub(super) fn plain_function_body_edit(old: &[u8], new: &[u8]) -> bool {
    fn outside_bodies(source: &[u8]) -> Option<Vec<&[u8]>> {
        use crate::engine::tree_walk::{Children, any_node};
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let mut cursor = root.walk();
        let mut start = 0;
        let mut sections = Vec::new();
        for (item, attributed) in package::items_with_attributes(root.named_children(&mut cursor)) {
            if item.kind() == "inner_attribute_item"
                && !ordinary_attribute(item, source).is_some_and(|names| names.is_empty())
            {
                return None;
            }
            if attributed || item.kind() != "function_item" {
                continue;
            }
            let body = item.child_by_field_name("body")?;
            if any_node(item, Children::Named, |n| {
                n != item
                    && (n.kind().ends_with("_item")
                        || matches!(
                            n.kind(),
                            "macro_invocation"
                                | "macro_definition"
                                | "extern_crate_declaration"
                                | "use_declaration"
                        ))
            }) {
                continue;
            }
            sections.push(source.get(start..body.start_byte())?);
            start = body.end_byte();
        }
        sections.push(source.get(start..)?);
        Some(sections)
    }
    outside_bodies(old)
        .zip(outside_bodies(new))
        .is_some_and(|(old, new)| old == new)
}
