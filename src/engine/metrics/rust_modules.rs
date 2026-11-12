use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::ControlFlow;
use std::path::Path;

use anyhow::Result;
use tree_sitter::Node;

use crate::engine::{
    bounded_read, parser,
    tree_walk::{Children, Walk, walk_preorder},
};
use crate::language::LangId;

use super::resolve::{join, parent, text};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Context {
    pub root: String,
    pub module: Vec<String>,
    directory: String,
}

#[derive(Default)]
struct Facts {
    hash: String,
    items: BTreeMap<Vec<String>, BTreeSet<String>>,
    declarations: Vec<Declaration>,
    imported: BTreeMap<Vec<String>, BTreeSet<String>>,
    unsupported_directories: BTreeSet<Vec<String>>,
}

struct Declaration {
    inline: Vec<String>,
    name: String,
    custom: Option<Option<String>>,
}

#[derive(Default)]
struct Package {
    name: String,
    library: Option<String>,
    external: BTreeSet<String>,
    edition: Option<u32>,
}

#[derive(Default)]
pub(super) struct RustModules {
    facts: BTreeMap<String, Facts>,
    contexts: BTreeMap<String, Vec<Context>>,
    owners: BTreeMap<(String, Vec<String>), BTreeSet<String>>,
    items: BTreeMap<(String, Vec<String>), BTreeSet<String>>,
    packages: BTreeMap<String, Package>,
    imported: BTreeMap<(String, Vec<String>), BTreeSet<String>>,
    uncertain_roots: BTreeSet<String>,
}

impl RustModules {
    pub fn build(
        dir: &Path,
        paths: &BTreeMap<String, usize>,
        validate: &impl Fn(&Path) -> Result<()>,
    ) -> Result<Self> {
        let mut index = Self::default();
        let mut package_dirs = BTreeSet::new();
        for file in paths.keys().filter(|p| {
            p.rsplit_once('.')
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("rs"))
        }) {
            let path = dir.join(file);
            if validate(&path).is_ok()
                && let Some(path) = camino::Utf8Path::from_path(&path)
                && let Ok(source) = parser::read_file(path)
                && let Ok(tree) = parser::parse_source(&source, LangId::Rust)
                && !tree.root_node().has_error()
            {
                index
                    .facts
                    .insert(file.clone(), collect(tree.root_node(), &source));
            }
            let mut ancestor = parent(file);
            loop {
                package_dirs.insert(ancestor.clone());
                if ancestor.is_empty() {
                    break;
                }
                ancestor = parent(&ancestor);
            }
        }
        let mut roots: BTreeSet<_> = index
            .facts
            .keys()
            .filter(|p| matches!(p.rsplit('/').next(), Some("lib.rs" | "main.rs")))
            .cloned()
            .collect();
        for directory in package_dirs {
            let manifest = dir.join(join(&directory, "Cargo.toml"));
            if !manifest.try_exists()? {
                continue;
            }
            validate(&manifest)?;
            if !std::fs::canonicalize(&manifest)?.starts_with(dir) {
                return Err(crate::error::AstroError::new(
                    crate::error::ErrorCode::PathOutOfBounds,
                    "Cargo.toml is outside the selected metrics scope",
                )
                .into());
            }
            if std::fs::symlink_metadata(&manifest)?
                .file_type()
                .is_symlink()
            {
                anyhow::bail!(crate::error::AstroError::new(
                    crate::error::ErrorCode::InvalidRequest,
                    "Symlinked Cargo.toml is not supported by metrics"
                ));
            }
            let content = bounded_read::read_utf8_file_limited(&manifest, 100 * 1024 * 1024)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cannot read Cargo.toml for metrics: {}",
                        join(&directory, "Cargo.toml")
                    )
                })?;
            let value: toml::Value = toml::from_str(&content).map_err(|e| {
                crate::error::AstroError::new(
                    crate::error::ErrorCode::InvalidRequest,
                    format!("Invalid Cargo.toml: {e}"),
                )
            })?;
            let package_name = value
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .replace('-', "_");
            let name = value
                .get("lib")
                .and_then(|v| v.get("name"))
                .and_then(toml::Value::as_str)
                .map(|name| name.replace('-', "_"))
                .unwrap_or(package_name);
            let targets =
                crate::engine::rust_targets::RustTargets::parse(&content, Path::new(&directory))
                    .ok_or_else(|| anyhow::anyhow!("Invalid Cargo targets in metrics scope"))?;
            if !targets.roots_complete {
                return Err(crate::error::AstroError::new(
                    crate::error::ErrorCode::InvalidRequest,
                    "Cargo target paths are invalid or leave the selected metrics scope",
                )
                .into());
            }
            for target in targets.explicit_roots() {
                let target =
                    crate::git_support::normalize_workspace_separators(&target.to_string_lossy());
                if index.facts.contains_key(&target) {
                    roots.insert(target);
                }
            }
            if targets.has_package {
                let prefix = if directory.is_empty() {
                    String::new()
                } else {
                    format!("{directory}/")
                };
                for (file, _) in index
                    .facts
                    .range(prefix.clone()..)
                    .take_while(|(file, _)| file.starts_with(&prefix))
                {
                    let relative = &file[prefix.len()..];
                    let parts: Vec<_> = relative.split('/').collect();
                    let enabled = |flag| {
                        value
                            .get("package")
                            .and_then(|v| v.get(flag))
                            .and_then(toml::Value::as_bool)
                            != Some(false)
                    };
                    let automatic_flag = match parts.as_slice() {
                        ["src", "bin", name] if name.ends_with(".rs") => Some("autobins"),
                        ["src", "bin", _, "main.rs"] => Some("autobins"),
                        ["tests", name] if name.ends_with(".rs") => Some("autotests"),
                        ["tests", _, "main.rs"] => Some("autotests"),
                        ["examples", name] if name.ends_with(".rs") => Some("autoexamples"),
                        ["examples", _, "main.rs"] => Some("autoexamples"),
                        ["benches", name] if name.ends_with(".rs") => Some("autobenches"),
                        ["benches", _, "main.rs"] => Some("autobenches"),
                        _ => None,
                    };
                    let automatic = automatic_flag.is_some_and(enabled)
                        || (relative == "build.rs"
                            && value
                                .get("package")
                                .and_then(|v| v.get("build"))
                                .is_none_or(|value| value.as_bool() == Some(true)));
                    if automatic {
                        roots.insert(file.clone());
                    }
                }
            }
            let library = value
                .get("lib")
                .and_then(|v| v.get("path"))
                .and_then(toml::Value::as_str)
                .map(|p| join(&directory, p))
                .or_else(|| {
                    let path = join(&directory, "src/lib.rs");
                    index.facts.contains_key(&path).then_some(path)
                });
            let mut external = BTreeSet::new();
            let mut patched = BTreeSet::new();
            let mut pending = Vec::new();
            for key in ["patch", "replace"] {
                if let Some(table) = value.get(key).and_then(toml::Value::as_table) {
                    pending.extend(table.iter());
                }
            }
            while let Some((name, entry)) = pending.pop() {
                if entry.get("path").is_some() {
                    patched.insert(name.split(':').next().unwrap_or(name).replace('-', "_"));
                } else if let Some(table) = entry.as_table() {
                    pending.extend(table.iter());
                }
            }
            let mut tables = vec![&value];
            if let Some(targets) = value.get("target").and_then(toml::Value::as_table) {
                tables.extend(targets.values());
            }
            for table in tables {
                for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(deps) = table.get(key).and_then(toml::Value::as_table) {
                        for (alias, dependency) in deps {
                            if dependency.get("path").is_none()
                                && dependency.get("workspace").is_none()
                                && !patched.contains(&alias.replace('-', "_"))
                                && !dependency
                                    .get("package")
                                    .and_then(toml::Value::as_str)
                                    .is_some_and(|name| patched.contains(&name.replace('-', "_")))
                            {
                                external.insert(alias.replace('-', "_"));
                            }
                        }
                    }
                }
            }
            let edition = match value.get("package").and_then(|v| v.get("edition")) {
                Some(toml::Value::String(value)) => Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|v| matches!(v, 2015 | 2018 | 2021 | 2024))
                        .ok_or_else(|| {
                            crate::error::AstroError::new(
                                crate::error::ErrorCode::InvalidRequest,
                                "Unsupported Cargo edition in metrics",
                            )
                        })?,
                ),
                Some(toml::Value::Table(table))
                    if table.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
                {
                    None
                }
                None if value.get("package").is_some() => Some(2015),
                None => None,
                _ => {
                    return Err(crate::error::AstroError::new(
                        crate::error::ErrorCode::InvalidRequest,
                        "Invalid Cargo edition in metrics",
                    )
                    .into());
                }
            };
            index.packages.insert(
                directory,
                Package {
                    name,
                    library,
                    external,
                    edition,
                },
            );
        }
        let mut queue: VecDeque<_> = roots
            .into_iter()
            .map(|root| {
                let directory = parent(&root);
                (
                    root.clone(),
                    Context {
                        root,
                        module: Vec::new(),
                        directory,
                    },
                )
            })
            .collect();
        let mut visited = BTreeSet::new();
        while let Some((file, context)) = queue.pop_front() {
            let contexts = index.contexts.entry(file.clone()).or_default();
            if contexts.contains(&context) {
                continue;
            }
            contexts.push(context.clone());
            if !visited.insert((context.root.clone(), file.clone())) {
                index.uncertain_roots.insert(context.root.clone());
                continue;
            }
            let Some(facts) = index.facts.get(&file) else {
                continue;
            };
            for (inline, items) in &facts.items {
                let mut module = context.module.clone();
                module.extend(inline.iter().cloned());
                index
                    .owners
                    .entry((context.root.clone(), module.clone()))
                    .or_default()
                    .insert(file.clone());
                index
                    .items
                    .entry((context.root.clone(), module))
                    .or_default()
                    .extend(items.iter().cloned());
            }
            for (inline, bindings) in &facts.imported {
                let mut module = context.module.clone();
                module.extend(inline.iter().cloned());
                index
                    .imported
                    .entry((context.root.clone(), module))
                    .or_default()
                    .extend(bindings.iter().cloned());
            }
            for decl in &facts.declarations {
                let inline_directory = join(&context.directory, &decl.inline.join("/"));
                let file_directory = parent(&file);
                let unsupported = facts
                    .unsupported_directories
                    .iter()
                    .any(|prefix| decl.inline.starts_with(prefix));
                let candidates = match &decl.custom {
                    _ if unsupported => Vec::new(),
                    Some(Some(path)) => vec![join(
                        if decl.inline.is_empty() {
                            &file_directory
                        } else {
                            &inline_directory
                        },
                        path,
                    )],
                    Some(None) => Vec::new(),
                    None => {
                        let base = join(&inline_directory, &decl.name);
                        vec![format!("{base}.rs"), format!("{base}/mod.rs")]
                    }
                };
                let targets: Vec<_> = candidates
                    .into_iter()
                    .filter(|p| paths.contains_key(p))
                    .collect();
                let mut module = context.module.clone();
                module.extend(decl.inline.iter().cloned());
                module.push(decl.name.clone());
                for target in &targets {
                    index
                        .owners
                        .entry((context.root.clone(), module.clone()))
                        .or_default()
                        .insert(target.clone());
                }
                if let [target] = targets.as_slice() {
                    let directory = if target.ends_with("/mod.rs") || target == "mod.rs" {
                        parent(target)
                    } else {
                        target.strip_suffix(".rs").unwrap_or(target).to_string()
                    };
                    queue.push_back((
                        target.clone(),
                        Context {
                            root: context.root.clone(),
                            module,
                            directory,
                        },
                    ));
                }
            }
        }
        Ok(index)
    }

    pub fn context(&self, file: &str) -> Option<&Context> {
        match self.contexts.get(file)?.as_slice() {
            [context] if !self.uncertain_roots.contains(&context.root) => Some(context),
            _ => None,
        }
    }

    pub fn matches_source(&self, file: &str, source: &[u8]) -> bool {
        self.facts
            .get(file)
            .is_none_or(|facts| facts.hash == blake3::hash(source).to_hex().as_str())
    }

    pub fn lookup(&self, context: &Context, path: &[String]) -> Option<Vec<String>> {
        for length in (0..=path.len()).rev() {
            let key = (context.root.clone(), path[..length].to_vec());
            let Some(owners) = self.owners.get(&key) else {
                continue;
            };
            if length == path.len()
                || self
                    .items
                    .get(&key)
                    .is_some_and(|items| items.contains(&path[length]))
            {
                return Some(owners.iter().cloned().collect());
            }
        }
        None
    }

    pub fn module(&self, context: &Context, path: &[String]) -> Vec<String> {
        self.owners
            .get(&(context.root.clone(), path.to_vec()))
            .map(|owners| owners.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn bare(
        &self,
        context: &Context,
        file: &str,
        path: &[String],
    ) -> (Vec<String>, &'static str) {
        let mut local = context.module.clone();
        local.extend_from_slice(path);
        let local_match = self.lookup(context, &local);
        let root_match = self.lookup(context, path);
        let bindings = self
            .imported
            .get(&(context.root.clone(), context.module.clone()));
        if path
            .first()
            .is_some_and(|name| bindings.is_some_and(|bindings| bindings.contains(name)))
        {
            return (Vec::new(), "rust_import_binding_unresolved");
        }
        let mut package_directory = parent(file);
        let package = loop {
            if let Some(package) = self.packages.get(&package_directory) {
                break Some(package);
            }
            if package_directory.is_empty() {
                break None;
            }
            package_directory = parent(&package_directory);
        };
        let found = match package.and_then(|package| package.edition) {
            Some(edition) if edition >= 2018 => local_match,
            Some(_) => root_match,
            None if local_match == root_match => local_match,
            None => None,
        };
        if let Some(found) = found {
            return (found, "rust_local_module");
        }
        if bindings.is_some_and(|bindings| bindings.contains("*")) {
            return (Vec::new(), "rust_glob_binding_unresolved");
        }
        if path.first().is_some_and(|name| {
            let mut prefix = context.module.clone();
            prefix.push(name.clone());
            !self.module(context, &prefix).is_empty()
                || !self.module(context, std::slice::from_ref(name)).is_empty()
        }) {
            return (Vec::new(), "rust_binding_unresolved");
        }
        let mut directory = parent(file);
        loop {
            if let Some(package) = self.packages.get(&directory) {
                if path.first() == Some(&package.name)
                    && let Some(root) = &package.library
                {
                    let library = Context {
                        root: root.clone(),
                        module: Vec::new(),
                        directory: parent(root),
                    };
                    if let Some(found) = self.lookup(&library, &path[1..]) {
                        return (found, "rust_package_library");
                    }
                }
                if path
                    .first()
                    .is_some_and(|first| package.external.contains(first))
                {
                    return (Vec::new(), "external");
                }
                break;
            }
            if directory.is_empty() {
                break;
            }
            directory = parent(&directory);
        }
        if path
            .first()
            .is_some_and(|first| matches!(first.as_str(), "std" | "core" | "alloc"))
        {
            return (Vec::new(), "external");
        }
        (Vec::new(), "rust_binding_unresolved")
    }
}

fn collect(root: Node<'_>, source: &[u8]) -> Facts {
    let mut facts = Facts {
        hash: blake3::hash(source).to_hex().to_string(),
        ..Default::default()
    };
    let mut module = Vec::new();
    let mut scopes = Vec::new();
    facts.items.entry(Vec::new()).or_default();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
        while scopes.last().is_some_and(|&(d, _)| d >= depth) {
            let (_, len) = scopes.pop().unwrap();
            module.truncate(len);
        }
        if matches!(
            node.kind(),
            "function_item" | "closure_expression" | "impl_item"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                facts
                    .items
                    .entry(module.clone())
                    .or_default()
                    .insert(identifier(text(name, source)));
            }
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "mod_item"
            && let Some(name) = node.child_by_field_name("name")
        {
            let name = identifier(text(name, source));
            if node.child_by_field_name("body").is_some() {
                scopes.push((depth, module.len()));
                module.push(name);
                if path_attribute(node, source).is_some() {
                    facts.unsupported_directories.insert(module.clone());
                }
                facts.items.entry(module.clone()).or_default();
            } else {
                facts.declarations.push(Declaration {
                    inline: module.clone(),
                    name,
                    custom: path_attribute(node, source),
                });
            }
        } else if node.kind() == "extern_crate_declaration" {
            if let Some(name) = node
                .child_by_field_name("alias")
                .or_else(|| node.child_by_field_name("name"))
            {
                facts
                    .imported
                    .entry(module.clone())
                    .or_default()
                    .insert(identifier(text(name, source)));
            }
            return ControlFlow::Continue(Walk::Skip);
        } else if node.kind() == "use_declaration"
            && let Some(argument) = node.child_by_field_name("argument")
        {
            let bindings = facts.imported.entry(module.clone()).or_default();
            let mut pending = vec![argument];
            while let Some(part) = pending.pop() {
                match part.kind() {
                    "use_as_clause" => {
                        if let Some(name) = part.child_by_field_name("alias") {
                            bindings.insert(identifier(text(name, source)));
                        }
                    }
                    "scoped_identifier" => {
                        if let Some(name) = part.child_by_field_name("name") {
                            bindings.insert(identifier(text(name, source)));
                        }
                    }
                    "identifier" => {
                        bindings.insert(identifier(text(part, source)));
                    }
                    "use_wildcard" => {
                        bindings.insert("*".into());
                    }
                    "scoped_use_list" => {
                        if let Some(list) = part.child_by_field_name("list") {
                            pending.push(list);
                        }
                    }
                    "use_list" => {
                        let mut cursor = part.walk();
                        pending.extend(part.named_children(&mut cursor));
                    }
                    _ => {}
                }
            }
            return ControlFlow::Continue(Walk::Skip);
        } else if matches!(
            node.kind(),
            "struct_item"
                | "enum_item"
                | "union_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
        ) && let Some(name) = node.child_by_field_name("name")
        {
            facts
                .items
                .entry(module.clone())
                .or_default()
                .insert(identifier(text(name, source)));
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    facts
}

fn identifier(value: &str) -> String {
    value.strip_prefix("r#").unwrap_or(value).to_string()
}

fn path_attribute(node: Node<'_>, source: &[u8]) -> Option<Option<String>> {
    let mut previous = node.prev_named_sibling();
    while let Some(item) = previous {
        match item.kind() {
            "attribute_item" => {
                let attribute = item.named_child(0)?;
                if attribute
                    .named_child(0)
                    .is_some_and(|name| text(name, source) == "path")
                {
                    let value = attribute
                        .child_by_field_name("value")
                        .map(|v| text(v, source));
                    return Some(value.and_then(|value| {
                        if value.starts_with('r') {
                            let first = value.find('"')?;
                            let last = value.rfind('"')?;
                            return (first < last).then(|| value[first + 1..last].to_string());
                        }
                        let value = value.strip_prefix('"')?.strip_suffix('"')?;
                        (!value.contains('\\')).then(|| value.to_string())
                    }));
                }
                if attribute
                    .named_child(0)
                    .is_some_and(|name| text(name, source) == "cfg_attr")
                {
                    let mut pending = vec![attribute];
                    while let Some(part) = pending.pop() {
                        if part.kind() == "identifier" && text(part, source) == "path" {
                            return Some(None);
                        }
                        let mut cursor = part.walk();
                        pending.extend(part.named_children(&mut cursor));
                    }
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = item.prev_named_sibling();
    }
    None
}
