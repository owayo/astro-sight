//! 局所関数・リテラル・外部 module 名前空間に属する呼出し位置の証明。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

struct LocalFunction {
    name: String,
    scope: std::ops::Range<usize>,
    after: usize,
}

struct Module {
    binding: String,
    receiver: String,
    after: usize,
}

#[derive(Default)]
pub(super) struct NamespaceCache {
    directories: Mutex<HashMap<PathBuf, Option<Arc<DirectoryShadows>>>>,
    source_roots: Mutex<HashMap<PathBuf, Arc<SourceRoot>>>,
}

enum SourceRoot {
    Absent,
    Directory(PathBuf),
    Unproven,
}

struct DirectoryShadows {
    roots: HashSet<String>,
    entries: usize,
}

impl NamespaceCache {
    fn directory(&self, path: &Path) -> Option<Arc<DirectoryShadows>> {
        self.directories
            .lock()
            .expect("Python namespace cache poisoned")
            .entry(path.to_owned())
            .or_insert_with(|| read_directory(path).map(Arc::new))
            .clone()
    }

    fn source_root(&self, workspace: &Path) -> Arc<SourceRoot> {
        self.source_roots
            .lock()
            .expect("Python source root cache poisoned")
            .entry(workspace.to_owned())
            .or_insert_with(|| {
                let src = workspace.join("src");
                let root = match std::fs::symlink_metadata(&src) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        SourceRoot::Absent
                    }
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                        match src.canonicalize() {
                            Ok(path) if path.starts_with(workspace) => SourceRoot::Directory(path),
                            _ => SourceRoot::Unproven,
                        }
                    }
                    _ => SourceRoot::Unproven,
                };
                Arc::new(root)
            })
            .clone()
    }
}

pub(super) fn sites(
    root: Node<'_>,
    source: &[u8],
    workspace: &Path,
    path: &Path,
    cache: &NamespaceCache,
) -> HashSet<usize> {
    if source.len() > 1024 * 1024 || root.has_error() {
        return HashSet::new();
    }
    let mut ancestors = Vec::<Node<'_>>::new();
    let mut functions = Vec::new();
    let mut calls = Vec::new();
    let mut ignored = HashSet::new();
    let traversal = walk_preorder(root, Children::Named, |node, depth| {
        ancestors.truncate(depth);
        if node.kind() == "identifier"
            && node.utf8_text(source).ok().is_none_or(|name| {
                !name.is_ascii()
                    || matches!(
                        name,
                        "__builtins__"
                            | "getattr"
                            | "delattr"
                            | "meta_path"
                            | "path_hooks"
                            | "path_importer_cache"
                            | "sys"
                            | "chdir"
                            | "fchdir"
                            | "import_module"
                    )
            })
            || node.kind() == "lambda"
        {
            return ControlFlow::Break(());
        }
        if node.kind() == "type_parameter"
            && ancestors
                .last()
                .is_some_and(|parent| parent.kind() == "generic_type")
        {
            ignored.insert(node.id());
        }
        if node.kind() == "function_definition"
            && let Some(parent) = ancestors.last()
        {
            let direct_function = parent.kind() == "module"
                || (parent.kind() == "block"
                    && ancestors
                        .get(ancestors.len().saturating_sub(2))
                        .is_some_and(|owner| owner.kind() == "function_definition"));
            let scope = direct_function.then(|| parent.byte_range());
            if let Some(scope) = scope
                && let Some(name) = node.child_by_field_name("name")
                && let Ok(text) = name.utf8_text(source)
            {
                ignored.insert(name.id());
                functions.push(LocalFunction {
                    name: text.to_owned(),
                    scope,
                    after: node.end_byte(),
                });
            }
        }
        if node.kind() == "call" {
            calls.push(node);
        }
        ancestors.push(node);
        ControlFlow::Continue(Walk::Descend)
    });
    if traversal.is_break() {
        return HashSet::new();
    }
    let mut modules = Vec::new();
    let shadows = std::cell::OnceCell::new();
    let mut cursor = root.walk();
    for statement in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "import_statement")
    {
        let mut cursor = statement.walk();
        for imported in statement.children_by_field_name("name", &mut cursor) {
            let (module, alias) = if imported.kind() == "aliased_import" {
                (
                    imported.child_by_field_name("name"),
                    imported.child_by_field_name("alias"),
                )
            } else {
                (Some(imported), None)
            };
            let Some(module) = module.and_then(|node| node.utf8_text(source).ok()) else {
                continue;
            };
            let Some(first) = module.split('.').next() else {
                continue;
            };
            if first.is_empty()
                || !first
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || shadows
                    .get_or_init(|| shadow_roots(workspace, path, cache))
                    .as_ref()
                    .is_none_or(|directories| {
                        directories
                            .iter()
                            .any(|directory| directory.roots.contains(&first.to_ascii_lowercase()))
                    })
            {
                continue;
            }
            let alias = alias.and_then(|node| node.utf8_text(source).ok());
            ignored.insert(imported.id());
            modules.push(Module {
                binding: alias.unwrap_or(first).to_owned(),
                receiver: alias.unwrap_or(module).to_owned(),
                after: statement.end_byte(),
            });
        }
    }
    let Some(blocked) = super::blocked_names(root, source, &ignored) else {
        return HashSet::new();
    };
    let mut counts = HashMap::<&str, usize>::new();
    for name in functions
        .iter()
        .map(|function| function.name.as_str())
        .chain(modules.iter().map(|module| module.binding.as_str()))
    {
        *counts.entry(name).or_default() += 1;
    }
    let mut sites = HashSet::new();
    let functions: HashMap<_, _> = functions
        .iter()
        .map(|function| (function.name.as_str(), function))
        .collect();
    let modules: HashMap<_, _> = modules
        .iter()
        .map(|module| (module.receiver.as_str(), module))
        .collect();
    for call in calls {
        let Some(callee) = call.child_by_field_name("function") else {
            continue;
        };
        if callee.kind() == "identifier" {
            let Ok(name) = callee.utf8_text(source) else {
                continue;
            };
            if !blocked.contains(name)
                && counts.get(name) == Some(&1)
                && functions.get(name).is_some_and(|function| {
                    function.after < call.start_byte()
                        && function.scope.contains(&call.start_byte())
                })
            {
                sites.insert(callee.id());
            }
        } else if callee.kind() == "attribute" {
            let (Some(receiver), Some(member)) = (
                callee.child_by_field_name("object"),
                callee.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            if matches!(receiver.kind(), "string" | "concatenated_string") {
                sites.insert(member.id());
                continue;
            }
            let Ok(receiver) = receiver.utf8_text(source) else {
                continue;
            };
            if modules.get(receiver).is_some_and(|module| {
                module.after < call.start_byte()
                    && !blocked.contains(&module.binding)
                    && counts.get(module.binding.as_str()) == Some(&1)
            }) {
                sites.insert(member.id());
            }
        }
    }
    sites
}

// 外部 module の実装は実行せず、観測した workspace に同名の import 元が無いことを確かめる。
fn shadow_roots(
    workspace: &Path,
    path: &Path,
    cache: &NamespaceCache,
) -> Option<Vec<Arc<DirectoryShadows>>> {
    let mut current = path.parent()?.canonicalize().ok()?;
    if !current.starts_with(workspace) {
        return None;
    }
    let mut directories = Vec::new();
    let mut entries = 0usize;
    loop {
        let directory = cache.directory(&current)?;
        entries = entries.checked_add(directory.entries)?;
        if entries > 4096 {
            return None;
        }
        directories.push(directory);
        if current == workspace {
            match cache.source_root(workspace).as_ref() {
                SourceRoot::Absent => {}
                SourceRoot::Unproven => return None,
                SourceRoot::Directory(path) => {
                    let directory = cache.directory(path)?;
                    if !directories
                        .iter()
                        .any(|existing| Arc::ptr_eq(existing, &directory))
                    {
                        entries = entries.checked_add(directory.entries)?;
                        if entries > 4096 {
                            return None;
                        }
                        directories.push(directory);
                    }
                }
            }
            return Some(directories);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn read_directory(path: &Path) -> Option<DirectoryShadows> {
    let mut roots = HashSet::new();
    let mut entries = 0usize;
    for entry in std::fs::read_dir(path).ok()? {
        entries += 1;
        if entries > 4096 {
            return None;
        }
        let name = entry.ok()?.file_name().to_str()?.to_ascii_lowercase();
        if name.ends_with(".pth")
            || matches!(
                name.as_str(),
                "sitecustomize.py"
                    | "usercustomize.py"
                    | "pyproject.toml"
                    | "setup.cfg"
                    | "setup.py"
            )
        {
            return None;
        }
        if [".py", ".pyi", ".pyw", ".pyc", ".pyo", ".pyd", ".so"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
        {
            roots.insert(name.split('.').next()?.to_owned());
        }
        roots.insert(name);
    }
    Some(DirectoryShadows { roots, entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    #[test]
    fn python_namespace_packaging_configs_keep_module_calls_unproven() {
        let source = b"import helper\n\ndef local():\n    return 0\n\nlocal()\n\", \".join(values)\nhelper.join(value)\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        assert!(!tree.root_node().has_error());
        for config in ["pyproject.toml", "setup.cfg", "setup.py"] {
            for directory in ["", "nested", "src"] {
                let dir = tempfile::tempdir().unwrap();
                let workspace = dir.path().canonicalize().unwrap();
                std::fs::create_dir(workspace.join("nested")).unwrap();
                std::fs::create_dir(workspace.join("src")).unwrap();
                std::fs::write(workspace.join(directory).join(config), "").unwrap();
                let result = sites(
                    tree.root_node(),
                    source,
                    &workspace,
                    &workspace.join("nested/caller.py"),
                    &NamespaceCache::default(),
                );
                let mut found = Vec::new();
                let _: ControlFlow<()> =
                    walk_preorder(tree.root_node(), Children::Named, |node, _| {
                        if result.contains(&node.id()) {
                            found.push(node.utf8_text(source).unwrap().to_owned());
                        }
                        ControlFlow::Continue(Walk::Descend)
                    });
                assert_eq!(found, ["local", "join"], "{directory}/{config}");
            }
        }
    }

    #[test]
    fn python_namespace_src_root_keeps_local_shadows_and_invalid_roots_unproven() {
        let source = b"import helper\nhelper.join(value)\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        for shadow in ["helper.py", "helper.pyc", "helper.pyd", "helper"] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().canonicalize().unwrap();
            std::fs::create_dir(workspace.join("src")).unwrap();
            std::fs::write(workspace.join("src").join(shadow), "").unwrap();
            assert!(
                sites(
                    tree.root_node(),
                    source,
                    &workspace,
                    &workspace.join("caller.py"),
                    &NamespaceCache::default()
                )
                .is_empty(),
                "{shadow}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        assert_eq!(
            sites(
                tree.root_node(),
                source,
                &workspace,
                &workspace.join("caller.py"),
                &NamespaceCache::default()
            )
            .len(),
            1
        );
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::write(workspace.join("src"), "").unwrap();
        assert!(
            sites(
                tree.root_node(),
                source,
                &workspace,
                &workspace.join("caller.py"),
                &NamespaceCache::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn python_namespace_src_root_shares_the_ancestor_entry_budget_without_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let src = workspace.join("src");
        std::fs::create_dir(&src).unwrap();
        for index in 0..4095 {
            std::fs::write(src.join(format!("file{index}.txt")), "").unwrap();
        }
        let cache = NamespaceCache::default();
        assert!(shadow_roots(&workspace, &workspace.join("caller.py"), &cache).is_some());
        assert!(shadow_roots(&workspace, &src.join("caller.py"), &cache).is_some());
        std::fs::write(workspace.join("one_more.txt"), "").unwrap();
        assert!(
            shadow_roots(
                &workspace,
                &workspace.join("caller.py"),
                &NamespaceCache::default()
            )
            .is_none()
        );
    }

    #[test]
    fn python_namespace_cache_is_limited_to_one_query_including_failed_lookups() {
        let source = b"import helper\nhelper.join(value)\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let path = workspace.join("caller.py");
        let cache = NamespaceCache::default();
        assert_eq!(
            sites(tree.root_node(), source, &workspace, &path, &cache).len(),
            1
        );
        std::fs::write(
            workspace.join("helper.py"),
            "from project import Owner\njoin = Owner.join\n",
        )
        .unwrap();
        assert_eq!(
            sites(tree.root_node(), source, &workspace, &path, &cache).len(),
            1
        );
        assert!(
            sites(
                tree.root_node(),
                source,
                &workspace,
                &path,
                &NamespaceCache::default()
            )
            .is_empty()
        );

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let path = workspace.join("caller.py");
        let cache = NamespaceCache::default();
        std::fs::write(workspace.join("paths.pth"), "").unwrap();
        assert!(sites(tree.root_node(), source, &workspace, &path, &cache).is_empty());
        std::fs::remove_file(workspace.join("paths.pth")).unwrap();
        assert!(sites(tree.root_node(), source, &workspace, &path, &cache).is_empty());
        assert_eq!(
            sites(
                tree.root_node(),
                source,
                &workspace,
                &path,
                &NamespaceCache::default()
            )
            .len(),
            1
        );
    }

    #[test]
    fn python_namespace_cache_preserves_the_combined_ancestor_entry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let nested = workspace.join("nested");
        std::fs::create_dir(&nested).unwrap();
        for index in 0..4095 {
            std::fs::write(workspace.join(format!("file{index}.txt")), "").unwrap();
        }
        std::fs::write(nested.join("caller.py"), "").unwrap();
        let cache = NamespaceCache::default();
        assert!(shadow_roots(&workspace, &workspace.join("caller.py"), &cache).is_some());
        assert!(shadow_roots(&workspace, &nested.join("caller.py"), &cache).is_none());
        assert!(shadow_roots(&workspace, &workspace.join("caller.py"), &cache).is_some());
        std::fs::write(workspace.join("one_more.txt"), "").unwrap();
        assert!(
            shadow_roots(
                &workspace,
                &workspace.join("caller.py"),
                &NamespaceCache::default()
            )
            .is_none()
        );
    }

    #[test]
    fn python_non_method_sites_distinguish_three_static_call_forms() {
        let source = b"import os.path\n\ndef label(words: list[str]) -> str:\n    return \", \".join(words)\n\ndef location(root, name):\n    return os.path.join(root, name)\n\ndef chain(words):\n    out = []\n    def join(word):\n        out.append(word)\n    for word in words:\n        join(word)\n    return out\n";
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        let result = sites(
            tree.root_node(),
            source,
            &workspace,
            &workspace.join("labels.py"),
            &NamespaceCache::default(),
        );
        let mut found = Vec::new();
        let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if result.contains(&node.id()) {
                found.push(node.utf8_text(source).unwrap().to_owned());
            }
            ControlFlow::Continue(Walk::Descend)
        });
        assert_eq!(found, ["join", "join", "join"]);
    }

    #[test]
    fn python_non_method_sites_keep_dynamic_or_shadowed_bindings_unproven() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        for source in [
            "import helper\nhelper = other\nhelper.join(value)\n",
            "import helper\nhelper.join = method\nhelper.join(value)\n",
            "import helper\nimport other as helper\nhelper.join(value)\n",
            "import helper\ndef use(helper):\n    return helper.join(value)\n",
            "import helper\nfrom library import *\nhelper.join(value)\n",
            "import helper\nexec(code)\nhelper.join(value)\n",
            "import helper\nvalue: list[exec(code)]\nhelper.join(value)\n",
            "import helper\nvalue: list[globals()]\nhelper.join(value)\n",
            "import helper\n__builtins__[\"exec\"](code)\nhelper.join(value)\n",
            "import sys\nsys.path.insert(0, \"src\")\nimport helper\nhelper.join(value)\n",
            "import sys as s\ns.path.append(\"src\")\nimport helper\nhelper.join(value)\n",
            "from sys import path as paths\npaths.insert(0, \"src\")\nimport helper\nhelper.join(value)\n",
            "import os\nos.chdir(\"src\")\nimport helper\nhelper.join(value)\n",
            "from os import chdir as move\nmove(\"src\")\nimport helper\nhelper.join(value)\n",
            "def use(join):\n    return join(value)\n",
            "def use(value):\n    def join(part):\n        return part\n    join = value.join\n    return join(value)\n",
            "def use(value):\n    def join(part):\n        return part\n    return value.join(1)\n",
            "def use(value):\n    def join(part):\n        return part\n    return relay(value).join(1)\n",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            assert!(
                sites(
                    tree.root_node(),
                    source.as_bytes(),
                    &workspace,
                    &workspace.join("caller.py"),
                    &NamespaceCache::default()
                )
                .is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn python_non_method_sites_keep_local_modules_and_lookup_failures_unproven() {
        let source = b"import helper\nhelper.join(value)\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        for shadow in [
            "helper.py",
            "helper.pyi",
            "helper.pyc",
            "helper.pyo",
            "helper.pyd",
            "helper.cpython-315-x86_64-linux-gnu.so",
            "helper",
            "sitecustomize.py",
            "paths.pth",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().canonicalize().unwrap();
            std::fs::write(workspace.join(shadow), "").unwrap();
            assert!(
                sites(
                    tree.root_node(),
                    source,
                    &workspace,
                    &workspace.join("caller.py"),
                    &NamespaceCache::default()
                )
                .is_empty(),
                "{shadow}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        assert!(
            sites(
                tree.root_node(),
                source,
                &workspace,
                &workspace.join("missing/caller.py"),
                &NamespaceCache::default()
            )
            .is_empty()
        );
        let oversized = vec![b' '; 1024 * 1024 + 1];
        assert!(
            sites(
                tree.root_node(),
                &oversized,
                &workspace,
                &workspace.join("caller.py"),
                &NamespaceCache::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn python_non_method_node_kinds_exist() {
        let language = LangId::Python.ts_language();
        for kind in [
            "identifier",
            "lambda",
            "function_definition",
            "module",
            "block",
            "type_parameter",
            "generic_type",
            "call",
            "import_statement",
            "aliased_import",
            "attribute",
            "string",
            "concatenated_string",
        ] {
            assert!(
                tree_sitter::Query::new(&language, &format!("({kind}) @probe")).is_ok(),
                "{kind}"
            );
        }
    }
}
