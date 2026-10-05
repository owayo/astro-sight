//! impact 専用の相対 import 束縛証明。解決できない参照は従来の判定に残す。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;

struct ImportBinding {
    owner: PathBuf,
    after: usize,
}

pub(super) struct PythonImportIndex {
    bindings: HashMap<String, ImportBinding>,
}

/// 1 回の impact 走査に閉じた証拠。別の問い合わせへ再利用しない。
#[derive(Default)]
pub(super) struct PythonImportCache {
    workspace: OnceLock<Option<PathBuf>>,
    packages: Mutex<HashMap<PathBuf, bool>>,
    owners: Mutex<HashMap<PathBuf, Option<HashSet<String>>>>,
    pub(super) source_paths: OnceLock<Vec<Option<String>>>,
}

impl PythonImportIndex {
    pub(super) fn build(
        mut root: Node<'_>,
        source: &[u8],
        dir: &str,
        path: &str,
        changed_names: &HashMap<String, usize>,
        cache: &PythonImportCache,
    ) -> Option<Self> {
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let workspace = cache
            .workspace
            .get_or_init(|| Path::new(dir).canonicalize().ok())
            .as_ref()?;
        let ref_path = workspace.join(path);
        let mut candidates = HashMap::new();
        let mut ignored = HashSet::new();
        let mut duplicates = HashSet::new();
        let mut cursor = root.walk();
        for statement in root.named_children(&mut cursor) {
            if statement.kind() != "import_from_statement" {
                continue;
            }
            let Some(module) = statement.child_by_field_name("module_name") else {
                continue;
            };
            let mut names_cursor = statement.walk();
            let names: Vec<_> = statement
                .children_by_field_name("name", &mut names_cursor)
                .filter(|name| {
                    name.kind() == "dotted_name"
                        && name.named_child_count() == 1
                        && name
                            .utf8_text(source)
                            .ok()
                            .is_some_and(|text| changed_names.contains_key(text))
                })
                .collect();
            if names.is_empty() {
                continue;
            }
            let Some(owner) = module.utf8_text(source).ok().and_then(|module| {
                let mut packages = cache
                    .packages
                    .lock()
                    .expect("Python package cache poisoned");
                resolve_relative_module(workspace, &ref_path, module, &mut packages)
            }) else {
                continue;
            };
            // 別名のデータフローと、変更に無関係な import 名は証明対象にしない。
            for name in names {
                let text = name.utf8_text(source).ok()?;
                if candidates.contains_key(text) {
                    duplicates.insert(text.to_owned());
                }
                ignored.insert(name.id());
                candidates.insert(
                    text.to_owned(),
                    ImportBinding {
                        owner: owner.clone(),
                        after: statement.end_byte(),
                    },
                );
            }
        }
        if candidates.is_empty() {
            return Some(Self {
                bindings: candidates,
            });
        }
        let blocked = blocked_names(root, source, &ignored)?;
        let mut owners = cache.owners.lock().expect("Python owner cache poisoned");
        candidates.retain(|name, binding| {
            if duplicates.contains(name) || blocked.contains(name) {
                return false;
            }
            owners
                .entry(binding.owner.clone())
                .or_insert_with(|| direct_function_names(&binding.owner))
                .as_ref()
                .is_some_and(|names| names.contains(name))
        });
        Some(Self {
            bindings: candidates,
        })
    }

    pub(super) fn owner(&self, node: Node<'_>, source: &[u8]) -> Option<&Path> {
        if node.kind() != "identifier" {
            return None;
        }
        let name = node.utf8_text(source).ok()?;
        let binding = self.bindings.get(name)?;
        if node.start_byte() < binding.after {
            return None;
        }
        let parent = node.parent()?;
        if (parent.kind() == "attribute" && field_contains(parent, "attribute", node))
            || (parent.kind() == "keyword_argument" && field_contains(parent, "name", node))
        {
            return None;
        }
        let mut current = node;
        while let Some(parent) = current.parent() {
            if matches!(
                parent.kind(),
                "import_statement"
                    | "import_from_statement"
                    | "type"
                    | "class_definition"
                    | "lambda"
                    | "list_comprehension"
                    | "set_comprehension"
                    | "dictionary_comprehension"
                    | "generator_expression"
            ) {
                return None;
            }
            current = parent;
        }
        Some(&binding.owner)
    }
}

fn field_contains(parent: Node<'_>, field: &str, node: Node<'_>) -> bool {
    parent.child_by_field_name(field).is_some_and(|field| {
        field.start_byte() <= node.start_byte() && node.end_byte() <= field.end_byte()
    })
}

fn resolve_relative_module(
    workspace: &Path,
    ref_path: &Path,
    module: &str,
    packages: &mut HashMap<PathBuf, bool>,
) -> Option<PathBuf> {
    let dots = module.bytes().take_while(|byte| *byte == b'.').count();
    if dots == 0 {
        return None;
    }
    let mut base = ref_path.parent()?.to_path_buf();
    for level in 0..dots {
        // namespace package や workspace 外の package の名前解決は証明しない。
        if !base.starts_with(workspace) || !package_is_inert(&base, packages) {
            return None;
        }
        if level + 1 < dots {
            base = base.parent()?.to_path_buf();
        }
    }
    let suffix = &module[dots..];
    if !suffix.is_empty() {
        let parts: Vec<_> = suffix.split('.').collect();
        for (index, part) in parts.iter().enumerate() {
            if part.is_empty() || part.contains(['/', '\\']) {
                return None;
            }
            base.push(part);
            if index + 1 < parts.len() && !package_is_inert(&base, packages) {
                return None;
            }
        }
    }
    let package = base.join("__init__.py");
    let module_file = base.with_extension("py");
    let owner = match (module_file.try_exists().ok()?, package.try_exists().ok()?) {
        (true, false) if !suffix.is_empty() => module_file,
        (false, true) => package,
        _ => return None,
    };
    let owner = owner.canonicalize().ok()?;
    owner.starts_with(workspace).then_some(owner)
}

/// 初期化による __path__ の変更や循環 import を証明の外に残す。
fn package_is_inert(path: &Path, packages: &mut HashMap<PathBuf, bool>) -> bool {
    *packages.entry(path.to_owned()).or_insert_with(|| {
        let init = path.join("__init__.py");
        let Some(source) =
            camino::Utf8Path::from_path(&init).and_then(|path| parser::read_file(path).ok())
        else {
            return false;
        };
        let Ok(tree) = parser::parse_source(&source, LangId::Python) else {
            return false;
        };
        let root = tree.root_node();
        let mut cursor = root.walk();
        !root.has_error()
            && root.named_children(&mut cursor).all(|node| {
                matches!(node.kind(), "comment" | "pass_statement")
                    || (node.kind() == "expression_statement"
                        && node.named_child_count() == 1
                        && node.named_child(0).is_some_and(|child| {
                            child.kind() == "string"
                                && child
                                    .utf8_text(&source)
                                    .ok()
                                    .is_some_and(|text| text.starts_with(['\'', '"']))
                        }))
            })
    })
}

fn direct_function_names(path: &Path) -> Option<HashSet<String>> {
    let source = parser::read_file(camino::Utf8Path::from_path(path)?).ok()?;
    let tree = parser::parse_source(&source, LangId::Python).ok()?;
    let root = tree.root_node();
    let mut definitions = HashMap::new();
    let mut ignored = HashSet::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        // decorator や条件付き定義は実体が別関数になる可能性がある。
        if node.kind() == "function_definition" {
            let name = node.child_by_field_name("name")?;
            let text = name.utf8_text(&source).ok()?;
            *definitions.entry(text.to_owned()).or_insert(0usize) += 1;
            ignored.insert(name.id());
        }
    }
    let blocked = blocked_names(root, &source, &ignored)?;
    Some(
        definitions
            .into_iter()
            .filter_map(|(name, count)| (count == 1 && !blocked.contains(&name)).then_some(name))
            .collect(),
    )
}

fn collect_names(
    node: Node<'_>,
    source: &[u8],
    ignored: &HashSet<usize>,
    names: &mut HashSet<String>,
) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if ignored.contains(&node.id()) {
            continue;
        }
        if node.kind() == "identifier" {
            if let Ok(name) = node.utf8_text(source) {
                names.insert(name.to_owned());
            }
        } else {
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
    }
}

/// scope をまたぐ同名再束縛を見落とさないよう、木全体で保守的に無効化する。
fn blocked_names(
    root: Node<'_>,
    source: &[u8],
    ignored: &HashSet<usize>,
) -> Option<HashSet<String>> {
    if root.has_error() {
        return None;
    }
    let mut blocked = HashSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.is_missing()
            || (node.kind() == "identifier"
                && node.utf8_text(source).ok().is_some_and(|name| {
                    matches!(
                        name,
                        "__path__"
                            | "__package__"
                            | "__getattr__"
                            | "__dict__"
                            | "__setattr__"
                            | "f_globals"
                            | "__globals__"
                            | "f_locals"
                            | "modules"
                            | "__class__"
                            | "__getattribute__"
                            | "__spec__"
                            | "exec"
                            | "eval"
                            | "globals"
                            | "locals"
                            | "vars"
                            | "setattr"
                            | "__import__"
                    )
                }))
            || matches!(
                node.kind(),
                "wildcard_import"
                    | "named_expression"
                    | "match_statement"
                    | "type_alias_statement"
                    | "type_parameter"
            )
        {
            return None;
        }
        match node.kind() {
            "import_statement" | "import_from_statement" => {
                let mut cursor = node.walk();
                for name in node.children_by_field_name("name", &mut cursor) {
                    collect_names(name, source, ignored, &mut blocked);
                }
            }
            "assignment" | "augmented_assignment" | "for_statement" => {
                if let Some(left) = node.child_by_field_name("left") {
                    collect_names(left, source, ignored, &mut blocked);
                }
            }
            "as_pattern" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    collect_names(alias, source, ignored, &mut blocked);
                }
            }
            "function_definition" | "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    collect_names(name, source, ignored, &mut blocked);
                }
            }
            "parameters" | "lambda_parameters" | "global_statement" | "nonlocal_statement"
            | "delete_statement" => collect_names(node, source, ignored, &mut blocked),
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    if blocked.contains("__name__") {
        return None;
    }
    Some(blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg/nested")).unwrap();
        std::fs::write(dir.path().join("pkg/__init__.py"), "").unwrap();
        std::fs::write(
            dir.path().join("pkg/nested/__init__.py"),
            "\"\"\"Package.\"\"\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("pkg/cli.py"), "def main():\n    return 0\n").unwrap();
        dir
    }

    fn last_owner(source: &str, dir: &Path, path: &str) -> Option<PathBuf> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
        let root = tree.root_node();
        let cache = PythonImportCache::default();
        let changed_names = HashMap::from([("main".to_owned(), 0)]);
        let index = PythonImportIndex::build(
            root,
            source.as_bytes(),
            dir.to_str().unwrap(),
            path,
            &changed_names,
            &cache,
        )?;
        let mut pending = vec![root];
        let mut last = None;
        while let Some(node) = pending.pop() {
            if node.kind() == "identifier"
                && node.utf8_text(source.as_bytes()).unwrap() == "main"
                && last.is_none_or(|previous: Node<'_>| previous.start_byte() < node.start_byte())
            {
                last = Some(node);
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        index.owner(last?, source.as_bytes()).map(Path::to_path_buf)
    }

    #[test]
    fn relative_import_proof_checks_scope_and_usage_boundaries() {
        let dir = fixture();
        let owner = dir.path().join("pkg/cli.py").canonicalize().unwrap();
        for source in [
            "from .cli import main\nmain()\n",
            "from .cli import (main,)\ndef run():\n    return main()\n",
            "from .cli import main\ncallback = main\n",
        ] {
            assert_eq!(
                last_owner(source, dir.path(), "pkg/runner.py"),
                Some(owner.clone()),
                "{source}"
            );
        }
        for tail in [
            "def run(main):\n    return main()\n",
            "def run():\n    from legacy import main\n    return main()\n",
            "def run():\n    nonlocal main\n    return main()\n",
            "for main in []: pass\nmain()\n",
            "with context() as main: pass\nmain()\n",
            "try: pass\nexcept Exception as main: pass\nmain()\n",
            "del main\nmain()\n",
            "main += value\nmain()\n",
            "main, other = pair\nmain()\n",
            "if condition:\n    from .cli import main\nmain()\n",
            "from .cli import main\nmain()\n",
            "from .cli import helper as main\nmain()\n",
            "result = lambda: main()\n",
            "result = [main() for item in items]\n",
            "class Box:\n    callback = main\n",
            "value: main\n",
            "other(main=1)\n",
            "obj.main()\n",
            "globals().update(values)\nmain()\n",
            "match value:\n    case main: pass\nmain()\n",
            "if (main := value): pass\nmain()\n",
            "__package__ = 'other'\nmain()\n",
        ] {
            let source = format!("from .cli import main\n{tail}");
            assert_eq!(
                last_owner(&source, dir.path(), "pkg/runner.py"),
                None,
                "{source}"
            );
        }
    }

    #[test]
    fn relative_import_proof_requires_unique_local_package_resolution() {
        let dir = fixture();
        let source = "from ..cli import main\nmain()\n";
        assert_eq!(
            last_owner(source, dir.path(), "pkg/nested/runner.py"),
            Some(dir.path().join("pkg/cli.py").canonicalize().unwrap())
        );
        assert_eq!(
            last_owner(
                "from ...cli import main\nmain()\n",
                dir.path(),
                "pkg/nested/runner.py"
            ),
            None
        );
        assert_eq!(
            last_owner(
                "from pkg.cli import main\nmain()\n",
                dir.path(),
                "pkg/runner.py"
            ),
            None
        );
        std::fs::create_dir_all(dir.path().join("pkg/cli")).unwrap();
        std::fs::write(dir.path().join("pkg/cli/__init__.py"), "def main(): pass\n").unwrap();
        assert_eq!(
            last_owner(
                "from .cli import main\nmain()\n",
                dir.path(),
                "pkg/runner.py"
            ),
            None
        );
        std::fs::remove_file(dir.path().join("pkg/cli/__init__.py")).unwrap();
        std::fs::write(dir.path().join("pkg/__init__.py"), "__path__ = ['other']\n").unwrap();
        assert_eq!(
            last_owner(
                "from .cli import main\nmain()\n",
                dir.path(),
                "pkg/runner.py"
            ),
            None
        );
    }

    #[test]
    fn python_import_node_kinds_exist_in_the_grammar() {
        for kind in [
            "identifier",
            "dotted_name",
            "import_from_statement",
            "import_statement",
            "attribute",
            "keyword_argument",
            "type",
            "class_definition",
            "lambda",
            "list_comprehension",
            "set_comprehension",
            "dictionary_comprehension",
            "generator_expression",
            "comment",
            "pass_statement",
            "expression_statement",
            "string",
            "function_definition",
            "wildcard_import",
            "named_expression",
            "match_statement",
            "type_alias_statement",
            "type_parameter",
            "call",
            "assignment",
            "augmented_assignment",
            "for_statement",
            "as_pattern",
            "parameters",
            "lambda_parameters",
            "global_statement",
            "nonlocal_statement",
            "delete_statement",
        ] {
            tree_sitter::Query::new(&LangId::Python.ts_language(), &format!("({kind}) @node"))
                .unwrap_or_else(|error| panic!("{kind}: {error}"));
        }
    }
}
