//! 新しい単一基底に同じ通常メソッドが残る、限定した公開束縛の証明。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tree_sitter::Node;

use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

use super::prepare::DetectionInputs;

const MAX_SOURCE: usize = 1024 * 1024;
const MAX_BYTES: usize = 8 * MAX_SOURCE;
const MAX_FILES: usize = 32;
const MAX_NODES: usize = 100_000;
const MAX_DIRECTORIES: usize = 64;

#[derive(Default)]
pub(crate) struct InheritedMembers {
    old: HashMap<String, Option<Rc<Module>>>,
    new: HashMap<String, Option<Rc<Module>>>,
    resolutions: HashMap<String, Option<String>>,
    directories: HashMap<PathBuf, bool>,
    bytes: usize,
}

#[derive(Clone)]
enum Binding {
    Class,
    Import {
        module: String,
        name: String,
        after: usize,
    },
    Other,
}

struct Module {
    bindings: BTreeMap<String, Binding>,
    classes: BTreeMap<String, Class>,
    inert: bool,
}

struct Class {
    base: Option<String>,
    bindings: BTreeSet<String>,
    methods: BTreeMap<String, Option<Method>>,
    annotations: BTreeSet<String>,
    start: usize,
    end: usize,
}

struct Method {
    tokens: Vec<(String, usize, Vec<u8>)>,
}

impl InheritedMembers {
    pub(crate) fn proves(
        &mut self,
        inputs: &DetectionInputs<'_>,
        old_path: &str,
        new_path: &str,
        name: &str,
        kind: &str,
    ) -> bool {
        self.proof(inputs, old_path, new_path, name, kind).is_some()
    }

    fn proof(
        &mut self,
        inputs: &DetectionInputs<'_>,
        old_path: &str,
        new_path: &str,
        name: &str,
        kind: &str,
    ) -> Option<()> {
        let (owner, member) = name.split_once('.')?;
        if old_path != new_path
            || !old_path.ends_with(".py")
            || !matches!(kind, "function" | "method")
            || owner.starts_with('_')
            || member.starts_with('_')
            || member.contains('.')
        {
            return None;
        }
        let old = self.load(inputs, old_path, true)?;
        let new = self.load(inputs, new_path, false)?;
        if old
            .bindings
            .values()
            .any(|binding| matches!(binding, Binding::Import { .. }))
        {
            return None;
        }
        if old.classes.values().any(|class| class.base.is_some())
            || new
                .classes
                .iter()
                .any(|(name, class)| name != owner && class.base.is_some())
        {
            return None;
        }
        let old_owner = old.classes.get(owner)?;
        let new_owner = new.classes.get(owner)?;
        if old_owner.base.is_some() || new_owner.bindings.contains(member) {
            return None;
        }
        self.owner_packages(inputs, new_path)?;
        let old_method = old_owner.methods.get(member)?.as_ref()?;
        let base_name = new_owner.base.as_ref()?;
        if new
            .bindings
            .iter()
            .any(|(name, binding)| name != base_name && matches!(binding, Binding::Import { .. }))
        {
            return None;
        }
        let (base_module, declared_name) = match new.bindings.get(base_name)? {
            Binding::Class => {
                if old.bindings.contains_key(base_name) {
                    return None;
                }
                let base = new.classes.get(base_name)?;
                if base.end >= new_owner.start {
                    return None;
                }
                (Rc::clone(&new), base_name.clone())
            }
            Binding::Import {
                module,
                name,
                after,
            } => {
                if *after >= new_owner.start {
                    return None;
                }
                let path = self.resolve(inputs, module)?;
                if path == new_path {
                    return None;
                }
                let mut diffs = inputs
                    .diff_files
                    .iter()
                    .filter(|file| file.new_path == path);
                let diff = diffs.next()?;
                if diffs.next().is_some() {
                    return None;
                }
                if diff.old_path != "/dev/null"
                    && (diff.old_path != path
                        || self
                            .load(inputs, &diff.old_path, true)?
                            .bindings
                            .contains_key(name))
                {
                    return None;
                }
                (self.load(inputs, &path, false)?, name.clone())
            }
            Binding::Other => return None,
        };
        let base = base_module.classes.get(&declared_name)?;
        if base.base.is_some()
            || (!Rc::ptr_eq(&base_module, &new)
                && (base_module
                    .bindings
                    .values()
                    .any(|binding| matches!(binding, Binding::Import { .. }))
                    || base_module
                        .classes
                        .values()
                        .any(|class| class.base.is_some())))
        {
            return None;
        }
        let inherited = base.methods.get(member)?.as_ref()?;
        if old_method.tokens != inherited.tokens {
            return None;
        }
        Some(())
    }

    fn owner_packages(&mut self, inputs: &DetectionInputs<'_>, path: &str) -> Option<()> {
        // src 自体が package の場合との所有名の曖昧さは証明しない。
        if path.starts_with("src/") {
            return None;
        }
        let module = path.strip_suffix(".py")?;
        let parts: Vec<_> = module.split('/').collect();
        if parts.last() == Some(&"__init__")
            || self.resolve(inputs, &parts.join("."))?.as_str() != path
        {
            return None;
        }
        let mut package = String::new();
        for part in &parts[..parts.len() - 1] {
            package.push_str(part);
            package.push('/');
            // package import 後の上書きがある旧版も、通常メソッドの維持とは証明しない。
            if !self
                .load(inputs, &format!("{package}__init__.py"), true)?
                .inert
            {
                return None;
            }
        }
        Some(())
    }

    fn load(&mut self, inputs: &DetectionInputs<'_>, path: &str, old: bool) -> Option<Rc<Module>> {
        let cache = if old { &self.old } else { &self.new };
        if let Some(module) = cache.get(path) {
            return module.clone();
        }
        if self.old.len() + self.new.len() >= MAX_FILES {
            return None;
        }
        let module = (|| {
            if !path.ends_with(".py") || !crate::engine::impact::is_safe_diff_path(path) {
                return None;
            }
            let remaining = MAX_SOURCE.min(MAX_BYTES.checked_sub(self.bytes)?);
            let source = if old {
                inputs.base_blobs.is_regular_file(path).then_some(())?;
                inputs.base_blobs.read_limited(path, remaining)?
            } else {
                normal_file(inputs.dir, path)?;
                crate::engine::bounded_read::read_file_limited(
                    &Path::new(inputs.dir).join(path),
                    remaining as u64,
                )?
            };
            if source.len() > MAX_SOURCE {
                return None;
            }
            self.bytes = self.bytes.checked_add(source.len())?;
            if self.bytes > MAX_BYTES {
                return None;
            }
            parse_module(&source).map(Rc::new)
        })();
        if old { &mut self.old } else { &mut self.new }.insert(path.to_owned(), module.clone());
        module
    }

    fn resolve(&mut self, inputs: &DetectionInputs<'_>, module: &str) -> Option<String> {
        if let Some(path) = self.resolutions.get(module) {
            return path.clone();
        }
        if self.resolutions.len() >= MAX_FILES {
            return None;
        }
        let resolved = (|| {
            let dir = inputs.dir;
            let parts: Vec<_> = module.split('.').collect();
            if parts.is_empty() || parts.len() > 32 || !parts.iter().all(|part| identifier(part)) {
                return None;
            }
            let root = std::fs::canonicalize(dir).ok()?;
            for source_root in [root.clone(), root.join("src")] {
                match std::fs::symlink_metadata(&source_root) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return None,
                    Ok(_) => {}
                }
                let mut current = source_root;
                self.check_directory(&root, &current).then_some(())?;
                // 基底の置き場は通常の .py に限り、末端パッケージは証明しない。
                match std::fs::symlink_metadata(current.join(parts.join("/"))) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return None,
                }
                for part in &parts[..parts.len() - 1] {
                    current.push(part);
                    match std::fs::symlink_metadata(&current) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                        Err(_) => return None,
                        Ok(_) => self.check_directory(&root, &current).then_some(())?,
                    }
                }
            }
            super::python_module_path::resolve_with_package_check(dir, module, |initializer| {
                let relative = crate::git_support::normalize_workspace_separators(
                    initializer.strip_prefix(&root).ok()?.to_str()?,
                );
                self.load(inputs, &relative, false)?.inert.then_some(())
            })
        })();
        self.resolutions.insert(module.to_owned(), resolved.clone());
        resolved
    }

    fn check_directory(&mut self, root: &Path, path: &Path) -> bool {
        if let Some(result) = self.directories.get(path) {
            return *result;
        }
        if self.directories.len() >= MAX_DIRECTORIES {
            return false;
        }
        let valid = (|| {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || !path.canonicalize().ok()?.starts_with(root)
            {
                return None;
            }
            for (index, entry) in std::fs::read_dir(path).ok()?.enumerate() {
                if index >= 4096 {
                    return None;
                }
                let name = entry.ok()?.file_name().to_str()?.to_ascii_lowercase();
                if name.ends_with(".pth")
                    || matches!(
                        name.as_str(),
                        "pyproject.toml"
                            | "setup.py"
                            | "setup.cfg"
                            | "sitecustomize.py"
                            | "usercustomize.py"
                    )
                {
                    return None;
                }
            }
            Some(())
        })()
        .is_some();
        self.directories.insert(path.to_owned(), valid);
        valid
    }
}

fn normal_file(dir: &str, path: &str) -> Option<()> {
    let root = std::fs::canonicalize(dir).ok()?;
    let mut full = root.clone();
    for part in path.split('/') {
        full.push(part);
        if std::fs::symlink_metadata(&full)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let metadata = std::fs::symlink_metadata(&full).ok()?;
    (metadata.is_file()
        && metadata.len() <= MAX_SOURCE as u64
        && full.canonicalize().ok()?.starts_with(&root))
    .then_some(())
}

fn identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_module(source: &[u8]) -> Option<Module> {
    if source.len() > MAX_SOURCE || std::str::from_utf8(source).is_err() {
        return None;
    }
    let tree = parser::parse_source(source, LangId::Python).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut count = 0usize;
    let valid = walk_preorder(root, Children::Named, |node, depth| {
        count += 1;
        if count > MAX_NODES
            || node.is_missing()
            || matches!(
                node.kind(),
                "decorated_definition"
                    | "lambda"
                    | "type_parameter"
                    | "named_expression"
                    | "global_statement"
                    | "nonlocal_statement"
                    | "delete_statement"
                    | "match_statement"
                    | "wildcard_import"
            )
            || (node.kind() == "class_definition" && depth != 1)
            || (node.kind() == "identifier"
                && node.utf8_text(source).ok().is_none_or(|name| {
                    !identifier(name)
                        || name.starts_with("__")
                        || matches!(
                            name,
                            "super"
                                | "exec"
                                | "eval"
                                | "globals"
                                | "locals"
                                | "vars"
                                | "getattr"
                                | "setattr"
                                | "delattr"
                                | "import_module"
                                | "meta_path"
                                | "path_hooks"
                                | "sys"
                                | "chdir"
                                | "fchdir"
                                | "f_locals"
                                | "f_globals"
                                | "f_back"
                                | "gi_frame"
                                | "cr_frame"
                                | "ag_frame"
                                | "gi_code"
                                | "cr_code"
                        )
                }))
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    if valid.is_break() {
        return None;
    }
    let mut result = Module {
        bindings: BTreeMap::new(),
        classes: BTreeMap::new(),
        inert: true,
    };
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        result.inert &= matches!(statement.kind(), "comment" | "pass_statement")
            || (statement.kind() == "expression_statement"
                && statement
                    .named_child(0)
                    .is_some_and(|node| node.kind() == "string" && literal(node)));
        match statement.kind() {
            "class_definition" => {
                let name = text(statement.child_by_field_name("name")?, source)?.to_owned();
                let class = parse_class(statement, source)?;
                bind(&mut result.bindings, &name, Binding::Class)?;
                result.classes.insert(name, class);
            }
            "import_from_statement" => {
                let module = text(statement.child_by_field_name("module_name")?, source)?;
                let mut cursor = statement.walk();
                for name in statement.children_by_field_name("name", &mut cursor) {
                    if name.kind() != "dotted_name" || name.named_child_count() != 1 {
                        return None;
                    }
                    let name = text(name, source)?;
                    bind(
                        &mut result.bindings,
                        name,
                        Binding::Import {
                            module: module.to_owned(),
                            name: name.to_owned(),
                            after: statement.end_byte(),
                        },
                    )?;
                }
            }
            "expression_statement" => {
                if let Some(name) = field_or_docstring(statement, source)? {
                    bind(&mut result.bindings, &name, Binding::Other)?;
                }
            }
            "comment" | "pass_statement" => {}
            _ => return None,
        }
    }
    result
        .classes
        .values()
        .all(|class| {
            class
                .annotations
                .iter()
                .all(|name| !result.bindings.contains_key(name) && !class.bindings.contains(name))
        })
        .then_some(result)
}

fn bind(bindings: &mut BTreeMap<String, Binding>, name: &str, value: Binding) -> Option<()> {
    if !identifier(name) || bindings.insert(name.to_owned(), value).is_some() {
        return None;
    }
    Some(())
}

fn parse_class(node: Node<'_>, source: &[u8]) -> Option<Class> {
    if node.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let base = if let Some(bases) = node.child_by_field_name("superclasses") {
        let mut cursor = bases.walk();
        let bases: Vec<_> = bases
            .named_children(&mut cursor)
            .filter(|node| node.kind() != "comment")
            .collect();
        match bases.as_slice() {
            [] => None,
            [base] if base.kind() == "identifier" => Some(text(*base, source)?.to_owned()),
            _ => return None,
        }
    } else {
        None
    };
    let mut result = Class {
        base,
        bindings: BTreeSet::new(),
        methods: BTreeMap::new(),
        annotations: BTreeSet::new(),
        start: node.start_byte(),
        end: node.end_byte(),
    };
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for statement in body.named_children(&mut cursor) {
        let name = match statement.kind() {
            "function_definition" => {
                let name = text(statement.child_by_field_name("name")?, source)?.to_owned();
                let (parameters, annotation) = method_header(statement, source)?;
                result.annotations.extend(annotation.iter().cloned());
                result
                    .methods
                    .insert(name.clone(), parse_method(statement, source, parameters));
                Some(name)
            }
            "expression_statement" => field_or_docstring(statement, source)?,
            "pass_statement" | "comment" => None,
            _ => return None,
        };
        if let Some(name) = name
            && !result.bindings.insert(name)
        {
            return None;
        }
    }
    Some(result)
}

fn field_or_docstring(statement: Node<'_>, source: &[u8]) -> Option<Option<String>> {
    let value = statement.named_child(0)?;
    if literal(value) {
        return Some(None);
    }
    if value.kind() != "assignment" || value.named_child_count() != 2 {
        return None;
    }
    let name = value.child_by_field_name("left")?;
    if name.kind() != "identifier" || !literal(value.child_by_field_name("right")?) {
        return None;
    }
    Some(Some(text(name, source)?.to_owned()))
}

fn literal(node: Node<'_>) -> bool {
    match node.kind() {
        "integer" | "float" | "true" | "false" | "none" => true,
        "string" | "concatenated_string" => {
            !crate::engine::tree_walk::any_node(node, Children::Named, |node| {
                node.kind() == "interpolation"
            })
        }
        _ => false,
    }
}

fn method_header(node: Node<'_>, source: &[u8]) -> Option<(BTreeSet<String>, Option<String>)> {
    if node.child(0)?.kind() != "def" || node.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let params = node.child_by_field_name("parameters")?;
    let mut parameters = BTreeSet::new();
    let mut cursor = params.walk();
    for parameter in params.named_children(&mut cursor) {
        if parameter.kind() != "identifier"
            || !parameters.insert(text(parameter, source)?.to_owned())
        {
            return None;
        }
    }
    if parameters.is_empty() {
        return None;
    }
    let annotation = match node.child_by_field_name("return_type") {
        None => None,
        Some(annotation) => {
            let name = annotation.named_child(0)?;
            if annotation.named_child_count() != 1 || name.kind() != "identifier" {
                return None;
            }
            let name = text(name, source)?;
            if !matches!(
                name,
                "str" | "int" | "float" | "bool" | "bytes" | "complex" | "object"
            ) {
                return None;
            }
            Some(name.to_owned())
        }
    };
    Some((parameters, annotation))
}

fn parse_method(node: Node<'_>, source: &[u8], parameters: BTreeSet<String>) -> Option<Method> {
    let body = node.child_by_field_name("body")?;
    let mut ancestors = Vec::<Node<'_>>::new();
    let safe = walk_preorder(body, Children::Named, |current, depth| {
        ancestors.truncate(depth);
        if matches!(
            current.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            return ControlFlow::Break(());
        }
        if current.kind() == "identifier"
            && text(current, source).is_none_or(|name| {
                !parameters.contains(name)
                    && !ancestors.last().is_some_and(|parent| {
                        (parent.kind() == "attribute"
                            && parent
                                .child_by_field_name("attribute")
                                .is_some_and(|member| member.id() == current.id()))
                            || (parent.kind() == "keyword_argument"
                                && parent
                                    .child_by_field_name("name")
                                    .is_some_and(|label| label.id() == current.id()))
                    })
            })
        {
            return ControlFlow::Break(());
        }
        ancestors.push(current);
        ControlFlow::Continue(Walk::Descend)
    });
    if safe.is_break() {
        return None;
    }
    let mut tokens = Vec::new();
    let _: ControlFlow<()> = walk_preorder(node, Children::All, |current, depth| {
        if current.is_extra() {
            return ControlFlow::Continue(Walk::Skip);
        }
        tokens.push((
            current.kind().to_owned(),
            depth,
            if current.child_count() == 0 {
                source[current.byte_range()].to_vec()
            } else {
                Vec::new()
            },
        ));
        ControlFlow::Continue(Walk::Descend)
    });
    Some(Method { tokens })
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(source: &str) -> Method {
        parse_module(source.as_bytes())
            .unwrap()
            .classes
            .remove("Owner")
            .unwrap()
            .methods
            .remove("banner")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn fingerprints_preserve_control_flow_and_literal_contents() {
        let nested = method(
            "class Owner:\n    def banner(self):\n        if self.active:\n            return self.area()\n        return self.side\n",
        );
        let unconditional = method(
            "class Owner:\n    def banner(self):\n        if self.active:\n            return self.area()\n            return self.side\n",
        );
        assert_ne!(nested.tokens, unconditional.tokens);
        let original = method(
            "class Owner:\n    def banner(self) -> str:\n        return f'area={self.area()}'\n",
        );
        let spaces = method(
            "class Owner:\n    def banner(self) -> str:\n        return f'area = {self.area()}'\n",
        );
        assert_ne!(original.tokens, spaces.tokens);
        let comment = method(
            "class Owner:\n    def banner( self ) -> str:\n        # same expression\n        return f'area={self.area()}'\n",
        );
        assert_eq!(original.tokens, comment.tokens);
    }

    #[test]
    fn lexical_names_and_definition_time_effects_remain_unproved() {
        for body in [
            "value",
            "str(self.area())",
            "globals()",
            "__class__",
            "super().banner()",
            "self.f_locals",
        ] {
            let source =
                format!("class Owner:\n    def banner(self) -> str:\n        return {body}\n");
            let parsed = parse_module(source.as_bytes());
            assert!(
                parsed.is_none_or(|module| module.classes["Owner"].methods["banner"].is_none()),
                "{body}"
            );
        }
        for header in [
            "def other(self, value=mutate()):",
            "def other(self) -> Unknown:",
            "async def other(self):",
            "def other[T](self):",
        ] {
            let source = format!(
                "class Owner:\n    def banner(self):\n        return self.area()\n    {header}\n        pass\n"
            );
            assert!(parse_module(source.as_bytes()).is_none(), "{header}");
        }
    }

    #[test]
    fn python_proof_node_kinds_exist_in_grammar() {
        let language = LangId::Python.ts_language();
        for kind in [
            "decorated_definition",
            "lambda",
            "type_parameter",
            "named_expression",
            "global_statement",
            "nonlocal_statement",
            "delete_statement",
            "match_statement",
            "wildcard_import",
            "class_definition",
            "identifier",
            "function_definition",
            "import_from_statement",
            "dotted_name",
            "expression_statement",
            "comment",
            "pass_statement",
            "assignment",
            "integer",
            "float",
            "true",
            "false",
            "none",
            "string",
            "concatenated_string",
            "interpolation",
            "attribute",
            "keyword_argument",
            "argument_list",
            "parameters",
            "type",
            "block",
        ] {
            assert!(
                tree_sitter::Query::new(&language, &format!("({kind}) @probe")).is_ok(),
                "{kind}"
            );
        }
    }
}
