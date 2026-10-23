//! Rust impact evidence for names owned by the standard crates. Unproven names retain
//! the existing impact route; this index does not change refs or dead-code output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use tree_sitter::Node;

use crate::engine::bounded_read::{read_input_file, read_input_text_file};
use crate::engine::parser;
use crate::language::LangId;

fn is_sysroot(name: &str) -> bool {
    matches!(name, "std" | "core" | "alloc")
}

#[derive(Clone)]
struct Binding {
    root: Option<String>,
}

#[derive(Default)]
struct Scope {
    parent: Option<usize>,
    module_boundary: bool,
    bindings: HashMap<String, Vec<Binding>>,
    has_glob: bool,
    has_macro_or_attribute: bool,
    shadowable_attributes: Vec<String>,
}

impl Scope {
    fn bind(&mut self, name: &str, root: Option<String>) {
        self.bindings
            .entry(name.to_owned())
            .or_default()
            .push(Binding { root });
    }
}

/// Only compiler-defined attributes with no ability to introduce a sibling name
/// can leave the module scope open. Unknown proc macros remain a barrier.
pub(crate) fn safe_attribute(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let attribute = node.named_child(0)?;
    if attribute.kind() != "attribute" || attribute.has_error() {
        return None;
    }
    let name = attribute.named_child(0)?.utf8_text(source).ok()?;
    if matches!(
        name,
        "cfg"
            | "allow"
            | "warn"
            | "deny"
            | "forbid"
            | "expect"
            | "doc"
            | "inline"
            | "cold"
            | "must_use"
            | "repr"
            | "non_exhaustive"
            | "deprecated"
            | "track_caller"
    ) {
        return Some(Vec::new());
    }
    if name == "test" {
        return Some(vec![name.to_owned()]);
    }
    if name != "derive" {
        return None;
    }
    let arguments = attribute.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let derives: Option<Vec<String>> = arguments
        .named_children(&mut cursor)
        .map(|derive| {
            if derive.kind() != "identifier" {
                return None;
            }
            let name = derive.utf8_text(source).ok()?;
            matches!(
                name,
                "Clone"
                    | "Copy"
                    | "Debug"
                    | "Default"
                    | "Eq"
                    | "PartialEq"
                    | "Ord"
                    | "PartialOrd"
                    | "Hash"
            )
            .then(|| name.to_owned())
        })
        .collect();
    derives
        .filter(|derives| !derives.is_empty())
        .map(|mut derives| {
            derives.push("derive".to_owned());
            derives
        })
}

pub(super) struct RustStdOriginIndex {
    scopes: HashMap<usize, Scope>,
    std_use_nodes: HashMap<usize, String>,
}

impl RustStdOriginIndex {
    pub(super) fn build(mut node: Node<'_>, source: &[u8]) -> Option<Self> {
        if !["std", "core", "alloc"]
            .iter()
            .any(|name| memchr::memmem::find(source, name.as_bytes()).is_some())
        {
            return None;
        }
        while let Some(parent) = node.parent() {
            node = parent;
        }
        if node.kind() != "source_file" || node.has_error() {
            return None;
        }
        let mut index = Self {
            scopes: HashMap::new(),
            std_use_nodes: HashMap::new(),
        };
        index.scopes.insert(
            node.id(),
            Scope {
                module_boundary: true,
                ..Scope::default()
            },
        );
        index.walk(node, source);
        Some(index)
    }

    fn walk(&mut self, root: Node<'_>, source: &[u8]) {
        let mut pending = vec![(root, root.id())];
        while let Some((node, scope_id)) = pending.pop() {
            let active = self.visit_node(node, source, scope_id);
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                pending.push((child, active));
            }
        }
    }

    fn visit_node(&mut self, node: Node<'_>, source: &[u8], scope_id: usize) -> usize {
        let mut active = scope_id;
        if node.kind() == "block"
            || (node.kind() == "declaration_list"
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "mod_item"))
        {
            active = node.id();
            self.scopes.insert(
                active,
                Scope {
                    parent: Some(scope_id),
                    module_boundary: node.kind() == "declaration_list",
                    ..Scope::default()
                },
            );
        }
        match node.kind() {
            "use_declaration" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    self.collect_use(argument, source, active, &[]);
                } else {
                    self.mark_unknown_use(active);
                }
            }
            "extern_crate_declaration" => {
                let name = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"));
                if let Some(name) = name.and_then(|name| name.utf8_text(source).ok()) {
                    self.scopes.get_mut(&active).unwrap().bind(name, None);
                }
            }
            "macro_invocation" => {
                self.scopes.get_mut(&active).unwrap().has_macro_or_attribute = true;
            }
            "attribute_item" | "inner_attribute_item" => {
                let scope = self.scopes.get_mut(&active).unwrap();
                if let Some(names) = safe_attribute(node, source) {
                    scope.shadowable_attributes.extend(names);
                } else {
                    scope.has_macro_or_attribute = true;
                }
            }
            "struct_item"
            | "enum_item"
            | "union_item"
            | "type_item"
            | "trait_item"
            | "function_item"
            | "function_signature_item"
            | "const_item"
            | "static_item"
            | "mod_item"
            | "macro_definition"
            | "type_parameter" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    self.scopes.get_mut(&active).unwrap().bind(name, None);
                }
            }
            _ => {}
        }
        active
    }

    fn collect_use(&mut self, node: Node<'_>, source: &[u8], scope: usize, prefix: &[String]) {
        // `a::{b::{c::{…}}}` の入れ子の深さだけ再帰しないよう、明示スタックで前順に辿る。
        // 子は逆順に積み、束縛を登録する順序を子を順に再帰した場合と同じに保つ。
        let mut pending: Vec<(Node<'_>, Rc<[String]>)> = vec![(node, Rc::from(prefix))];
        while let Some((node, prefix)) = pending.pop() {
            match node.kind() {
                "scoped_use_list" => {
                    let mut complete = prefix.to_vec();
                    if let Some(path) = node.child_by_field_name("path") {
                        let Some(mut segments) = path_segments(path, source) else {
                            self.mark_unknown_use(scope);
                            continue;
                        };
                        complete.append(&mut segments);
                    }
                    if let Some(list) = node.child_by_field_name("list") {
                        pending.push((list, Rc::from(complete)));
                    } else {
                        self.mark_unknown_use(scope);
                    }
                }
                "use_list" => {
                    let mut cursor = node.walk();
                    let children: Vec<_> = node.named_children(&mut cursor).collect();
                    for child in children.into_iter().rev() {
                        pending.push((child, Rc::clone(&prefix)));
                    }
                }
                "use_wildcard" => {
                    self.mark_unknown_use(scope);
                }
                "use_as_clause" => {
                    let Some(path) = node.child_by_field_name("path") else {
                        self.mark_unknown_use(scope);
                        continue;
                    };
                    let Some(mut segments) = path_segments(path, source) else {
                        self.mark_unknown_use(scope);
                        continue;
                    };
                    let Some(alias) = node
                        .child_by_field_name("alias")
                        .and_then(|alias| alias.utf8_text(source).ok())
                    else {
                        self.mark_unknown_use(scope);
                        continue;
                    };
                    if alias == "_" {
                        continue;
                    }
                    let mut complete = prefix.to_vec();
                    complete.append(&mut segments);
                    self.record_use_path(path, &complete);
                    self.bind_use(scope, alias, &complete);
                }
                "self" => {
                    if let Some(name) = prefix.last() {
                        self.bind_use(scope, name, &prefix);
                    } else {
                        self.mark_unknown_use(scope);
                    }
                }
                _ => {
                    if let Some(mut segments) = path_segments(node, source) {
                        let mut complete = prefix.to_vec();
                        complete.append(&mut segments);
                        if let Some(name) = complete.last() {
                            self.record_use_path(node, &complete);
                            self.bind_use(scope, name, &complete);
                        }
                    } else {
                        self.mark_unknown_use(scope);
                    }
                }
            }
        }
    }

    fn mark_unknown_use(&mut self, scope: usize) {
        self.scopes.get_mut(&scope).unwrap().has_glob = true;
    }

    fn bind_use(&mut self, scope: usize, name: &str, path: &[String]) {
        let root = path.first().filter(|root| is_sysroot(root)).cloned();
        self.scopes.get_mut(&scope).unwrap().bind(name, root);
    }

    fn record_use_path(&mut self, path: Node<'_>, segments: &[String]) {
        let Some(root) = segments.first().filter(|root| is_sysroot(root)) else {
            return;
        };
        let leaf = path.child_by_field_name("name").unwrap_or(path);
        self.std_use_nodes.insert(leaf.id(), root.clone());
    }

    /// Only a structural path or a uniquely bound bare type can be excluded.
    pub(super) fn proves_std_owner(&self, node: Node<'_>, source: &[u8]) -> bool {
        let mut current = Some(node);
        while let Some(ancestor) = current {
            if matches!(
                ancestor.kind(),
                "token_tree" | "attribute_item" | "inner_attribute_item"
            ) {
                return false;
            }
            current = ancestor.parent();
        }
        let Some(scope) = self.scope_of(node) else {
            return false;
        };
        if let Some(root) = self.std_use_nodes.get(&node.id()) {
            return self.root_unbound(scope, root);
        }
        if let Some(parent) = node.parent()
            && matches!(
                parent.kind(),
                "scoped_identifier" | "scoped_type_identifier"
            )
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
        {
            return path_segments(parent, source).is_some_and(|path| {
                path.first().is_some_and(|root| is_sysroot(root))
                    && self.root_unbound(scope, &path[0])
            });
        }
        if node.kind() != "type_identifier" {
            return false;
        }
        let Some(name) = node.utf8_text(source).ok() else {
            return false;
        };
        for scope_id in self.scope_chain(scope) {
            let scope = &self.scopes[&scope_id];
            if scope.has_glob
                || scope.has_macro_or_attribute
                || self.has_shadowed_attribute(scope_id)
            {
                return false;
            }
            if let Some(bindings) = scope.bindings.get(name) {
                return bindings.len() == 1
                    && bindings[0]
                        .root
                        .as_deref()
                        .is_some_and(|root| self.root_unbound(scope_id, root));
            }
            if scope.module_boundary {
                break;
            }
        }
        false
    }

    fn scope_of(&self, node: Node<'_>) -> Option<usize> {
        let mut current = Some(node);
        while let Some(ancestor) = current {
            if self.scopes.contains_key(&ancestor.id()) {
                return Some(ancestor.id());
            }
            current = ancestor.parent();
        }
        None
    }

    fn scope_chain(&self, start: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(Some(start), |id| self.scopes[id].parent)
    }

    fn has_shadowed_attribute(&self, scope_id: usize) -> bool {
        self.scopes[&scope_id]
            .shadowable_attributes
            .iter()
            .any(|name| {
                self.scope_chain(scope_id)
                    .any(|ancestor| self.scopes[&ancestor].bindings.contains_key(name))
            })
    }

    fn root_unbound(&self, scope: usize, root: &str) -> bool {
        for scope_id in self.scope_chain(scope) {
            let scope = &self.scopes[&scope_id];
            if scope.has_glob
                || scope.has_macro_or_attribute
                || self.has_shadowed_attribute(scope_id)
                || scope.bindings.contains_key(root)
            {
                return false;
            }
            if scope.module_boundary {
                break;
            }
        }
        true
    }
}

fn path_segments(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    // `a::b::c` は `path` 側へ入れ子になる。段数だけ再帰しないよう、`path` を先頭まで
    // たどって末尾側から名前を集め、最後に並べ直す (`path` の無い `::name` はそこで終わる)。
    let mut reversed = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                reversed.push(current.utf8_text(source).ok()?.to_owned());
                break;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let name = current
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?;
                reversed.push(name.to_owned());
                match current.child_by_field_name("path") {
                    Some(path) => current = path,
                    None => break,
                }
            }
            _ => return None,
        }
    }
    reversed.reverse();
    Some(reversed)
}

#[derive(Clone)]
struct ManifestInfo {
    directory: PathBuf,
    edition: u32,
}

/// The scan is lazy, shared by workers, and lives for one impact request only.
pub(super) struct RustStdProjectProof {
    root: PathBuf,
    manifests: OnceLock<Option<Vec<ManifestInfo>>>,
}

impl RustStdProjectProof {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            manifests: OnceLock::new(),
        }
    }

    pub(super) fn allows(&self, ref_path: &str) -> bool {
        let Some(manifests) = self.manifests.get_or_init(|| scan_project(&self.root)) else {
            return false;
        };
        let path = Path::new(ref_path);
        manifests
            .iter()
            .filter(|manifest| path.starts_with(&manifest.directory))
            .max_by_key(|manifest| manifest.directory.components().count())
            .is_some_and(|manifest| manifest.edition >= 2018)
    }
}

fn scan_project(root: &Path) -> Option<Vec<ManifestInfo>> {
    let mut pending = vec![root.to_owned()];
    let mut raw_manifests = Vec::new();
    let mut rust_files = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let path = entry.path();
            let name = entry.file_name();
            if name == ".git" || name == "target" {
                continue;
            }
            if kind.is_symlink() {
                let target_is_dir = std::fs::metadata(&path).is_ok_and(|target| target.is_dir());
                if target_is_dir
                    || matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("rs" | "toml")
                    )
                    || matches!(name.to_str(), Some("config" | "build.rs"))
                {
                    return None;
                }
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && name == "Cargo.toml" {
                let source = read_input_text_file(&path)?;
                let value = toml::from_str::<toml::Value>(&source).ok()?;
                if has_sysroot_dependency(&value, false) {
                    return None;
                }
                if target_path_escapes(&value) {
                    return None;
                }
                raw_manifests.push((dir.clone(), value));
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                rust_files.push(path);
            }
        }
    }
    if raw_manifests.is_empty() {
        return None;
    }
    let manifests: Vec<ManifestInfo> = raw_manifests
        .iter()
        .map(|(directory, value)| {
            Some(ManifestInfo {
                directory: directory.clone(),
                edition: manifest_edition(directory, value, &raw_manifests)?,
            })
        })
        .collect::<Option<_>>()?;
    for manifest in &manifests {
        for name in ["config", "config.toml"] {
            let path = manifest.directory.join(".cargo").join(name);
            if path.exists() && read_input_file(&path)?.windows(8).any(|w| w == b"--extern") {
                return None;
            }
        }
    }
    for path in rust_files {
        let source = read_input_file(&path)?;
        if memchr::memmem::find(&source, b"extern").is_none()
            && memchr::memmem::find(&source, b"macro_use").is_none()
        {
            continue;
        }
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        if tree.root_node().has_error() || has_foreign_std_alias(tree.root_node(), &source) {
            return None;
        }
    }
    Some(manifests)
}

fn has_foreign_std_alias(root: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "extern_crate_declaration"
            && let (Some(name), Some(alias)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("alias"),
            )
            && let (Ok(name), Ok(alias)) = (name.utf8_text(source), alias.utf8_text(source))
            && is_sysroot(alias)
            && !is_sysroot(name)
        {
            return true;
        }
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item")
            && node
                .utf8_text(source)
                .ok()
                .is_some_and(|attribute| attribute.contains("macro_use"))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn manifest_edition(
    directory: &Path,
    value: &toml::Value,
    manifests: &[(PathBuf, toml::Value)],
) -> Option<u32> {
    let Some(package) = value.get("package") else {
        return Some(0);
    };
    if is_sysroot(package.get("name")?.as_str()?) {
        return None;
    }
    let Some(edition) = package.get("edition") else {
        return Some(2015);
    };
    if let Some(edition) = edition.as_str() {
        return parse_edition(edition);
    }
    if edition.get("workspace").and_then(toml::Value::as_bool) != Some(true)
        || package.get("workspace").is_some()
    {
        return None;
    }
    let workspace = manifests
        .iter()
        .filter(|(candidate, value)| {
            directory.starts_with(candidate) && value.get("workspace").is_some()
        })
        .max_by_key(|(candidate, _)| candidate.components().count())?;
    parse_edition(
        workspace
            .1
            .get("workspace")?
            .get("package")?
            .get("edition")?
            .as_str()?,
    )
}

fn parse_edition(edition: &str) -> Option<u32> {
    match edition {
        "2015" => Some(2015),
        "2018" => Some(2018),
        "2021" => Some(2021),
        "2024" => Some(2024),
        _ => None,
    }
}

pub(super) fn has_sysroot_dependency(value: &toml::Value, in_dependencies: bool) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().any(|(key, value)| {
            let in_dependencies = in_dependencies
                || matches!(
                    key.as_str(),
                    "dependencies"
                        | "dev-dependencies"
                        | "dev_dependencies"
                        | "build-dependencies"
                        | "build_dependencies"
                        | "patch"
                        | "replace"
                );
            (in_dependencies && is_sysroot(key))
                || (in_dependencies && key == "package" && value.as_str().is_some_and(is_sysroot))
                || has_sysroot_dependency(value, in_dependencies)
        }),
        toml::Value::Array(values) => values
            .iter()
            .any(|value| has_sysroot_dependency(value, in_dependencies)),
        _ => false,
    }
}

fn target_path_escapes(value: &toml::Value) -> bool {
    for name in ["lib", "bin", "example", "test", "bench"] {
        let Some(targets) = value.get(name) else {
            continue;
        };
        let candidates: Vec<&toml::Value> = match targets {
            toml::Value::Array(values) => values.iter().collect(),
            value => vec![value],
        };
        for target in candidates {
            if target.get("edition").is_some()
                || target
                    .get("name")
                    .is_some_and(|name| name.as_str().is_none_or(is_sysroot))
                || target
                    .get("path")
                    .is_some_and(|path| path.as_str().is_none())
            {
                return true;
            }
            if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                let path = Path::new(path);
                if path.is_absolute()
                    || path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_use_and_qualified_type_have_structural_origin() {
        let source = b"use std::ops::Range;\nfn f(r: Range<usize>, q: std::ops::Range<usize>, local: crate::out::Range) {}\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustStdOriginIndex::build(tree.root_node(), source).unwrap();
        let mut found = 0;
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if node.utf8_text(source).ok() == Some("Range")
                && matches!(node.kind(), "identifier" | "type_identifier")
            {
                let local = node
                    .parent()
                    .and_then(|parent| parent.utf8_text(source).ok())
                    .is_some_and(|text| text.starts_with("crate::"));
                assert_eq!(index.proves_std_owner(node, source), !local, "{node:?}");
                found += 1;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        assert_eq!(found, 4);
    }

    #[test]
    fn deeply_nested_expression_does_not_overflow_origin_index() {
        let source = format!(
            "use std::ops::Range;\nfn f(r: Range<usize>) {{ let _ = {}{{ use crate::local::Range; Range {{ start: 0, end: 1 }} }}{}; }}\n",
            "(".repeat(2_000),
            ")".repeat(2_000)
        );
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error());
        let index = RustStdOriginIndex::build(tree.root_node(), source.as_bytes()).unwrap();
        let function = tree.root_node().named_child(1).unwrap();
        let parameter = function.child_by_field_name("parameters").unwrap();
        let range = parameter
            .named_child(0)
            .unwrap()
            .child_by_field_name("type")
            .unwrap()
            .named_child(0)
            .unwrap();
        assert!(index.proves_std_owner(range, source.as_bytes()));

        let deep_offset = source.rfind("Range { start:").unwrap();
        let deep_range = tree
            .root_node()
            .named_descendant_for_byte_range(deep_offset, deep_offset + "Range".len())
            .unwrap();
        assert_eq!(deep_range.kind(), "type_identifier");
        assert!(!index.proves_std_owner(deep_range, source.as_bytes()));
    }

    #[test]
    fn very_deep_tree_builds_origin_index() {
        let source = format!(
            "use std::ops::Range;\nfn f() {{ let _ = {}1{}; }}\n",
            "(".repeat(12_000),
            ")".repeat(12_000)
        );
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error());
        assert!(RustStdOriginIndex::build(tree.root_node(), source.as_bytes()).is_some());
    }

    #[test]
    fn simple_edition_2021_package_allows_std_origin_proof() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let file = dir.path().join("src/other.rs");
        std::fs::write(&file, "use std::ops::Range;\n").unwrap();
        assert!(RustStdProjectProof::new(dir.path()).allows(file.to_str().unwrap()));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("Cargo.toml", dir.path().join("NOTES.md")).unwrap();
            assert!(RustStdProjectProof::new(dir.path()).allows(file.to_str().unwrap()));
        }

        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2015\"\n",
        )
        .unwrap();
        assert!(!RustStdProjectProof::new(dir.path()).allows(file.to_str().unwrap()));
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nstd = { path = \"../fake\" }\n",
        )
        .unwrap();
        assert!(!RustStdProjectProof::new(dir.path()).allows(file.to_str().unwrap()));
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(&file, "extern crate self as std;\n").unwrap();
        assert!(!RustStdProjectProof::new(dir.path()).allows(file.to_str().unwrap()));
    }

    #[test]
    fn local_bindings_and_globs_keep_std_origin_unknown() {
        for source in [
            "mod std { pub mod ops { pub struct Range; } }\nuse std::ops::Range;\nfn f(r: Range) {}\n",
            "use crate::out::Range;\nfn f(r: Range) {}\n",
            "use std::ops::Range;\nuse crate::out::*;\nfn f(r: Range) {}\n",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let Some(index) = RustStdOriginIndex::build(tree.root_node(), source.as_bytes()) else {
                assert!(
                    !["std", "core", "alloc"]
                        .iter()
                        .any(|name| source.contains(name))
                );
                continue;
            };
            let mut pending = vec![tree.root_node()];
            let mut found = false;
            while let Some(node) = pending.pop() {
                if node.kind() == "type_identifier"
                    && node.utf8_text(source.as_bytes()).ok() == Some("Range")
                {
                    assert!(!index.proves_std_owner(node, source.as_bytes()), "{source}");
                    found = true;
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            assert!(found, "{source}");
        }
    }

    #[test]
    fn grouped_aliased_and_absolute_std_imports_keep_their_owner() {
        for (source, import_name, usage_name) in [
            (
                "use ::std::ops::Range;\nfn f(r: Range<usize>) {}\n",
                "Range",
                "Range",
            ),
            (
                "use std::ops::{Range, Bound};\nfn f(r: Range<usize>) {}\n",
                "Range",
                "Range",
            ),
            (
                "use std::ops::Range as Span;\nfn f(r: Span<usize>) {}\n",
                "Range",
                "Span",
            ),
            (
                "use std::{ops::{Range}, io};\nfn f(r: Range<usize>) {}\n",
                "Range",
                "Range",
            ),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let index = RustStdOriginIndex::build(tree.root_node(), source.as_bytes()).unwrap();
            let mut pending = vec![tree.root_node()];
            let mut proven = 0;
            while let Some(node) = pending.pop() {
                if node
                    .utf8_text(source.as_bytes())
                    .ok()
                    .is_some_and(|name| name == import_name || name == usage_name)
                    && matches!(node.kind(), "identifier" | "type_identifier")
                    && index.proves_std_owner(node, source.as_bytes())
                {
                    proven += 1;
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            assert!(proven >= 2, "{source}: {proven}");
        }
    }

    #[test]
    fn compiler_attributes_preserve_proof_but_custom_attributes_do_not() {
        for (prefix, proven) in [
            (
                "#[derive(Debug)] struct Holder;\n#[cfg(test)] mod tests {}\n",
                true,
            ),
            ("#[derive(Custom)] struct Holder;\n", false),
            ("#[custom] struct Holder;\n", false),
            ("#[path = \"alternate.rs\"] mod alternate;\n", false),
            ("#[test] fn marker() {}\n", true),
            ("use crate::other::test;\n#[test] fn marker() {}\n", false),
            (
                "use crate::other::Debug;\n#[derive(Debug)] struct Holder;\n",
                false,
            ),
            (
                "use crate::other::derive;\n#[derive(Debug)] struct Holder;\n",
                false,
            ),
        ] {
            let source = format!("use std::ops::Range;\n{prefix}fn f(r: Range<usize>) {{}}\n");
            let bytes = source.as_bytes();
            let tree = parser::parse_source(bytes, LangId::Rust).unwrap();
            let index = RustStdOriginIndex::build(tree.root_node(), bytes).unwrap();
            let function_start = source.find("fn f").unwrap();
            let mut pending = vec![tree.root_node()];
            let mut found = false;
            while let Some(node) = pending.pop() {
                if node.kind() == "type_identifier"
                    && node.start_byte() > function_start
                    && node.utf8_text(bytes).ok() == Some("Range")
                {
                    assert_eq!(index.proves_std_owner(node, bytes), proven, "{source}");
                    found = true;
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            assert!(found, "{source}");
        }
    }

    #[test]
    fn nested_attributes_check_outer_scope_imports() {
        for (name, attribute) in [
            ("test", "#[test] fn marker() {}"),
            ("Debug", "#[derive(Debug)] struct Holder;"),
        ] {
            for shadowed in [false, true] {
                let imported = if shadowed {
                    format!("use crate::other::{name};\n")
                } else {
                    String::new()
                };
                let source = format!(
                    "use std::ops::Range;\n{imported}fn f() {{ {attribute} let _: Range<usize> = 0..1; }}\n"
                );
                let bytes = source.as_bytes();
                let tree = parser::parse_source(bytes, LangId::Rust).unwrap();
                let index = RustStdOriginIndex::build(tree.root_node(), bytes).unwrap();
                let use_start = source.find("let _").unwrap();
                let mut pending = vec![tree.root_node()];
                let mut found = false;
                while let Some(node) = pending.pop() {
                    if node.kind() == "type_identifier"
                        && node.start_byte() > use_start
                        && node.utf8_text(bytes).ok() == Some("Range")
                    {
                        assert_eq!(index.proves_std_owner(node, bytes), !shadowed, "{source}");
                        found = true;
                    }
                    let mut cursor = node.walk();
                    pending.extend(node.named_children(&mut cursor));
                }
                assert!(found, "{source}");
            }
        }
    }

    #[test]
    fn workspace_edition_and_target_overrides_gate_project_proof() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("member/src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n[workspace.package]\nedition = \"2021\"\n",
        )
        .unwrap();
        let manifest = dir.path().join("member/Cargo.toml");
        let source = dir.path().join("member/src/lib.rs");
        std::fs::write(&source, "use std::ops::Range;\n").unwrap();
        std::fs::write(
            &manifest,
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
        )
        .unwrap();
        assert!(RustStdProjectProof::new(dir.path()).allows(source.to_str().unwrap()));

        for target in ["[lib]\nname = \"std\"\n", "[lib]\nedition = \"2015\"\n"] {
            std::fs::write(
                &manifest,
                format!("[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition.workspace = true\n{target}"),
            )
            .unwrap();
            assert!(!RustStdProjectProof::new(dir.path()).allows(source.to_str().unwrap()));
        }

        std::fs::write(
            &manifest,
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
        )
        .unwrap();
        std::fs::write(&source, "#[macro_use]\nextern crate fake;\n").unwrap();
        assert!(!RustStdProjectProof::new(dir.path()).allows(source.to_str().unwrap()));
    }

    /// 修正前の再帰版 (浅い入力で反復版と比べるためだけに残す)。
    fn path_segments_recursive(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
        match node.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                Some(vec![node.utf8_text(source).ok()?.to_owned()])
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut parts = if let Some(path) = node.child_by_field_name("path") {
                    path_segments_recursive(path, source)?
                } else {
                    Vec::new()
                };
                parts.push(
                    node.child_by_field_name("name")?
                        .utf8_text(source)
                        .ok()?
                        .to_owned(),
                );
                Some(parts)
            }
            _ => None,
        }
    }

    fn collect_use_recursive(
        index: &mut RustStdOriginIndex,
        node: Node<'_>,
        source: &[u8],
        scope: usize,
        prefix: &[String],
    ) {
        match node.kind() {
            "scoped_use_list" => {
                let mut complete = prefix.to_vec();
                if let Some(path) = node.child_by_field_name("path") {
                    let Some(mut segments) = path_segments_recursive(path, source) else {
                        index.mark_unknown_use(scope);
                        return;
                    };
                    complete.append(&mut segments);
                }
                if let Some(list) = node.child_by_field_name("list") {
                    collect_use_recursive(index, list, source, scope, &complete);
                } else {
                    index.mark_unknown_use(scope);
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    collect_use_recursive(index, child, source, scope, prefix);
                }
            }
            "use_wildcard" => index.mark_unknown_use(scope),
            "use_as_clause" => {
                let Some(path) = node.child_by_field_name("path") else {
                    index.mark_unknown_use(scope);
                    return;
                };
                let Some(mut segments) = path_segments_recursive(path, source) else {
                    index.mark_unknown_use(scope);
                    return;
                };
                let Some(alias) = node
                    .child_by_field_name("alias")
                    .and_then(|alias| alias.utf8_text(source).ok())
                else {
                    index.mark_unknown_use(scope);
                    return;
                };
                if alias == "_" {
                    return;
                }
                let mut complete = prefix.to_vec();
                complete.append(&mut segments);
                index.record_use_path(path, &complete);
                index.bind_use(scope, alias, &complete);
            }
            "self" => {
                if let Some(name) = prefix.last() {
                    index.bind_use(scope, name, prefix);
                } else {
                    index.mark_unknown_use(scope);
                }
            }
            _ => {
                if let Some(mut segments) = path_segments_recursive(node, source) {
                    let mut complete = prefix.to_vec();
                    complete.append(&mut segments);
                    if let Some(name) = complete.last() {
                        index.record_use_path(node, &complete);
                        index.bind_use(scope, name, &complete);
                    }
                } else {
                    index.mark_unknown_use(scope);
                }
            }
        }
    }

    type UseSummary = (
        Vec<(String, Vec<Option<String>>)>,
        bool,
        Vec<(usize, String)>,
    );

    fn summarize_uses(index: &RustStdOriginIndex) -> UseSummary {
        let scope = &index.scopes[&0];
        let mut bindings: Vec<_> = scope
            .bindings
            .iter()
            .map(|(name, bindings)| {
                let roots = bindings
                    .iter()
                    .map(|binding| binding.root.clone())
                    .collect();
                (name.clone(), roots)
            })
            .collect();
        bindings.sort();
        let mut uses: Vec<_> = index
            .std_use_nodes
            .iter()
            .map(|(id, root)| (*id, root.clone()))
            .collect();
        uses.sort();
        (bindings, scope.has_glob, uses)
    }

    /// 反復に書き換えた use の束縛の収集と path の分解が、浅い入力で修正前の再帰版と
    /// 同じ結果 (束縛の順序、不明な use の印、std の use の位置) を返す。
    #[test]
    fn iterative_use_collection_matches_recursive_version() {
        let source = b"use std::{fmt, io::{self, Read as R, Write as _}, collections::*};\nuse core::mem;\nuse alloc::{vec::Vec, string::{String, ToString}};\nuse crate::{a::{b, c::d}, e as f};\nuse self::g::{self};\nuse ::std::ops::Range;\nuse {h, i::j};\nuse super::k::l;\nuse {std::fmt, crate::x::{fmt, y::fmt as fmt}};\nfn t(x: std::ops::Range<u8>, y: a::B, z: <T as Tr>::C) { let _ = a::b::c(); }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let mut cursor = tree.root_node().walk();
        let mut nodes = vec![tree.root_node()];
        let mut next = 0;
        while next < nodes.len() {
            let node = nodes[next];
            nodes.extend(node.named_children(&mut cursor));
            next += 1;
        }
        let mut uses = 0;
        for node in nodes {
            assert_eq!(
                path_segments(node, source),
                path_segments_recursive(node, source),
                "{}: {}",
                node.kind(),
                node.utf8_text(source).unwrap()
            );
            if node.kind() != "use_declaration" {
                continue;
            }
            let argument = node.child_by_field_name("argument").unwrap();
            let fresh = || RustStdOriginIndex {
                scopes: HashMap::from([(0, Scope::default())]),
                std_use_nodes: HashMap::new(),
            };
            for prefix in [Vec::new(), vec!["outer".to_owned()]] {
                let (mut iterative, mut recursive) = (fresh(), fresh());
                iterative.collect_use(argument, source, 0, &prefix);
                collect_use_recursive(&mut recursive, argument, source, 0, &prefix);
                assert_eq!(
                    summarize_uses(&iterative),
                    summarize_uses(&recursive),
                    "{}",
                    node.utf8_text(source).unwrap()
                );
            }
            uses += 1;
        }
        assert_eq!(uses, 9);
    }
}
