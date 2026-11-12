use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::ControlFlow;
use std::path::Path;

use anyhow::Result;
use tree_sitter::Node;

use crate::engine::{
    parser,
    tree_walk::{Children, Walk, walk_preorder},
};
use crate::language::LangId;

use super::resolve::{join, parent, text};

mod manifest;
use manifest::Package;

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
        let package_dirs = index.collect_rust_facts(dir, paths, validate);
        let mut roots: BTreeSet<_> = index
            .facts
            .keys()
            .filter(|p| matches!(p.rsplit('/').next(), Some("lib.rs" | "main.rs")))
            .cloned()
            .collect();
        index.packages =
            manifest::load_cargo_packages(dir, package_dirs, &index.facts, &mut roots, validate)?;
        index.link_declared_modules(paths, roots);
        Ok(index)
    }

    fn collect_rust_facts(
        &mut self,
        dir: &Path,
        paths: &BTreeMap<String, usize>,
        validate: &impl Fn(&Path) -> Result<()>,
    ) -> BTreeSet<String> {
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
                self.facts
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
        package_dirs
    }

    fn link_declared_modules(&mut self, paths: &BTreeMap<String, usize>, roots: BTreeSet<String>) {
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
            let contexts = self.contexts.entry(file.clone()).or_default();
            if contexts.contains(&context) {
                continue;
            }
            contexts.push(context.clone());
            if !visited.insert((context.root.clone(), file.clone())) {
                self.uncertain_roots.insert(context.root.clone());
                continue;
            }
            let Some(facts) = self.facts.get(&file) else {
                continue;
            };
            for (inline, items) in &facts.items {
                let mut module = context.module.clone();
                module.extend(inline.iter().cloned());
                self.owners
                    .entry((context.root.clone(), module.clone()))
                    .or_default()
                    .insert(file.clone());
                self.items
                    .entry((context.root.clone(), module))
                    .or_default()
                    .extend(items.iter().cloned());
            }
            for (inline, bindings) in &facts.imported {
                let mut module = context.module.clone();
                module.extend(inline.iter().cloned());
                self.imported
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
                    self.owners
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
