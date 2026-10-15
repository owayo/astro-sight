//! Python の残存フィールドへ帰属する出現だけを、削除メソッドの参照から外す。
//! 受け手が不明な属性・bound method・型のない内包表記は証明しない。

use std::collections::HashMap;
use std::rc::Rc;

use tree_sitter::{Node, Tree};

use super::removed_attribution::RemovedCandidateRef;
use crate::commands::git_input::GitBlobBatch;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use crate::models::reference::SymbolReference;

const MAX_SOURCE_BYTES: usize = 1_000_000;
const MAX_FILES: usize = 512;
const MAX_PROJECT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct ClassIdentity {
    path: String,
    name: String,
}

struct Binding {
    id: usize,
    byte: usize,
}

struct PythonFile {
    source: Vec<u8>,
    tree: Tree,
    bindings: HashMap<String, Vec<Binding>>,
}

#[derive(Default)]
pub(super) struct PythonMemberOrigins {
    files: HashMap<String, Option<Rc<PythonFile>>>,
    old_files: HashMap<String, Option<Rc<PythonFile>>>,
    classes: HashMap<(String, String), Option<ClassIdentity>>,
    fields: HashMap<(String, String, String), bool>,
    inventory: Option<bool>,
    cached_source_bytes: usize,
}

impl PythonMemberOrigins {
    /// 全出現が別クラスの field 宣言・読み取りだと確認した候補だけを降格する。
    pub(super) fn proves_unrelated_fields(
        &mut self,
        dir: &str,
        base: &GitBlobBatch,
        candidate: &RemovedCandidateRef<'_>,
        references: &[SymbolReference],
        owner_references: &[SymbolReference],
    ) -> bool {
        if !matches!(candidate.kind, "method" | "function")
            || LangId::from_path(camino::Utf8Path::new(candidate.old_path)).ok()
                != Some(LangId::Python)
            || references.is_empty()
        {
            return false;
        }
        let Some((owner, member)) = candidate.name.split_once('.') else {
            return false;
        };
        if member.contains('.') {
            return false;
        }
        let old = if let Some(old) = self.old_files.get(candidate.old_path) {
            old.clone()
        } else {
            if self.old_files.len() + self.files.len() >= MAX_FILES {
                return false;
            }
            let old = base
                .read_limited(candidate.old_path, MAX_SOURCE_BYTES)
                .filter(|source| {
                    source.len() <= MAX_CACHED_BYTES.saturating_sub(self.cached_source_bytes)
                })
                .and_then(PythonFile::parse)
                .map(Rc::new);
            if let Some(file) = &old {
                self.cached_source_bytes += file.source.len();
            }
            self.old_files
                .insert(candidate.old_path.to_owned(), old.clone());
            old
        };
        // 注釈の型は subclass も許す。削除 owner に基底クラスがある場合は、
        // 別クラスの field と同じ継承系列になり得るため証明しない。
        if old
            .as_ref()
            .and_then(|file| local_class(file, owner))
            .is_none()
        {
            return false;
        }
        let inventory = if let Some(inventory) = self.inventory {
            inventory
        } else {
            let inventory = self.project_is_static(dir);
            self.inventory = Some(inventory);
            inventory
        };
        if !inventory {
            return false;
        }
        if owner_references.is_empty()
            || !owner_references
                .iter()
                .all(|reference| self.class_does_not_escape(dir, reference, owner))
        {
            return false;
        }
        references.iter().all(|reference| {
            self.reference_owner(dir, reference, member)
                .is_some_and(|class| class.path != candidate.old_path || class.name != owner)
        })
    }

    fn file(&mut self, dir: &str, path: &str) -> Option<Rc<PythonFile>> {
        if let Some(file) = self.files.get(path) {
            return file.clone();
        }
        if self.files.len() + self.old_files.len() >= MAX_FILES {
            return None;
        }
        let parsed = (|| {
            if LangId::from_path(camino::Utf8Path::new(path)).ok() != Some(LangId::Python)
                || !crate::engine::impact::is_safe_diff_path(path)
            {
                return None;
            }
            let root = std::fs::canonicalize(dir).ok()?;
            let full = std::fs::canonicalize(root.join(path)).ok()?;
            let relative = crate::git_support::normalize_workspace_separators(
                full.strip_prefix(root).ok()?.to_str()?,
            );
            if relative != path || full.with_extension("pyi").try_exists().ok()? {
                return None;
            }
            let source =
                crate::engine::bounded_read::read_file_limited(&full, MAX_SOURCE_BYTES as u64)?;
            if source.len() > MAX_CACHED_BYTES.saturating_sub(self.cached_source_bytes) {
                return None;
            }
            PythonFile::parse(source).map(Rc::new)
        })();
        if let Some(file) = &parsed {
            self.cached_source_bytes += file.source.len();
        }
        self.files.insert(path.to_owned(), parsed.clone());
        parsed
    }

    /// 名前を文字列から取得する経路は owner の refs だけでは見えない。
    /// 同じ探索範囲を一度だけ調べ、不要な source と Tree はその場で捨てる。
    fn project_is_static(&self, dir: &str) -> bool {
        let Some(root) = std::fs::canonicalize(dir).ok() else {
            return false;
        };
        let Ok(files) = crate::engine::refs::collect_files(&root, None) else {
            return false;
        };
        let mut count = 0usize;
        let mut bytes = 0usize;
        for path in files {
            if crate::engine::refs::detect_source_lang(&path) != Some(LangId::Python) {
                continue;
            }
            count += 1;
            if count > MAX_FILES {
                return false;
            }
            let Some(relative) = path.strip_prefix(&root).ok().and_then(|path| path.to_str())
            else {
                return false;
            };
            let relative = crate::git_support::normalize_workspace_separators(relative);
            if let Some(Some(file)) = self.files.get(&relative) {
                bytes += file.source.len();
                if bytes > MAX_PROJECT_BYTES {
                    return false;
                }
                continue;
            }
            if std::fs::canonicalize(&path).ok().as_ref() != Some(&path) {
                return false;
            }
            let Some(source) =
                crate::engine::bounded_read::read_file_limited(&path, MAX_SOURCE_BYTES as u64)
            else {
                return false;
            };
            bytes += source.len();
            if bytes > MAX_PROJECT_BYTES || PythonFile::parse(source).is_none() {
                return false;
            }
        }
        true
    }

    fn class_does_not_escape(
        &mut self,
        dir: &str,
        reference: &SymbolReference,
        owner: &str,
    ) -> bool {
        let Some(file) = self.file(dir, &reference.path) else {
            return false;
        };
        let point = tree_sitter::Point::new(reference.line, reference.column);
        let Some(node) = file
            .tree
            .root_node()
            .named_descendant_for_point_range(point, point)
        else {
            return false;
        };
        if node.kind() != "identifier" || text(node, &file.source) != Some(owner) {
            return false;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "class_definition"
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
        {
            return local_class(&file, owner).is_some();
        }
        // import の別名・class 値の代入・基底クラス・return は、別の名前から
        // subclass を作れるため証明しない。constructor も type(Owner()) を
        // 通じた subclass の経路になり得るため、この限定証明では許さない。
        parent.kind() == "dotted_name"
            && text(parent, &file.source) == Some(owner)
            && parent.parent().is_some_and(|statement| {
                statement.kind() == "import_from_statement"
                    && statement
                        .parent()
                        .is_some_and(|root| root.id() == file.tree.root_node().id())
                    && statement
                        .child_by_field_name("module_name")
                        .is_some_and(|module| module.id() != parent.id())
                    && file.bound_once(owner, statement.id(), None)
            })
    }

    fn class_for_name(
        &mut self,
        dir: &str,
        path: &str,
        file: &PythonFile,
        name: &str,
    ) -> Option<ClassIdentity> {
        let key = (path.to_owned(), name.to_owned());
        if let Some(found) = self.classes.get(&key) {
            return found.clone();
        }
        let found = if local_class(file, name).is_some() {
            Some(ClassIdentity {
                path: path.to_owned(),
                name: name.to_owned(),
            })
        } else {
            (|| {
                let mut module = None;
                for statement in children(file.tree.root_node()) {
                    if statement.kind() != "import_from_statement" {
                        continue;
                    }
                    let source_module = statement.child_by_field_name("module_name")?;
                    if source_module.kind() != "dotted_name" {
                        continue;
                    }
                    if children(statement).into_iter().any(|child| {
                        child.id() != source_module.id()
                            && child.kind() == "dotted_name"
                            && text(child, &file.source) == Some(name)
                    }) {
                        if module.is_some() || !file.bound_once(name, statement.id(), None) {
                            return None;
                        }
                        module = Some(text(source_module, &file.source)?.to_owned());
                    }
                }
                let target = super::python_module_path::resolve(dir, &module?)?;
                let imported = self.file(dir, &target)?;
                local_class(&imported, name)?;
                Some(ClassIdentity {
                    path: target,
                    name: name.to_owned(),
                })
            })()
        };
        self.classes.insert(key, found.clone());
        found
    }

    fn reference_owner(
        &mut self,
        dir: &str,
        reference: &SymbolReference,
        member: &str,
    ) -> Option<ClassIdentity> {
        let path = &reference.path;
        let file = self.file(dir, path)?;
        let point = tree_sitter::Point::new(reference.line, reference.column);
        let node = file
            .tree
            .root_node()
            .named_descendant_for_point_range(point, point)?;
        if node.kind() != "identifier" || text(node, &file.source) != Some(member) {
            return None;
        }
        if let Some(class) = field_declaration_owner(&file, node) {
            let name = text(class.child_by_field_name("name")?, &file.source)?;
            local_class(&file, name)?;
            return Some(ClassIdentity {
                path: path.clone(),
                name: name.to_owned(),
            });
        }
        let attribute = node.parent()?;
        if attribute.kind() != "attribute"
            || attribute.child_by_field_name("attribute")?.id() != node.id()
            || !is_field_read(attribute)
        {
            return None;
        }
        let receiver = attribute.child_by_field_name("object")?;
        let class = self.receiver_class(dir, path, &file, receiver)?;
        let key = (class.path.clone(), class.name.clone(), member.to_owned());
        let found = if let Some(found) = self.fields.get(&key) {
            *found
        } else {
            let definition_file = self.file(dir, &class.path)?;
            let definition = local_class(&definition_file, &class.name)?;
            let found = class_has_field(&definition_file, definition, member);
            self.fields.insert(key, found);
            found
        };
        found.then_some(class)
    }

    fn receiver_class(
        &mut self,
        dir: &str,
        path: &str,
        file: &PythonFile,
        receiver: Node<'_>,
    ) -> Option<ClassIdentity> {
        if receiver.kind() == "call" {
            return self.constructor_class(dir, path, file, receiver);
        }
        if receiver.kind() != "identifier" {
            return None;
        }
        let name = text(receiver, &file.source)?;
        let function = enclosing_function(receiver)?;
        let body = function.child_by_field_name("body")?;
        // default 値と注釈は、この関数の引数がまだ束縛されない外側 scope で評価される。
        if receiver.start_byte() < body.start_byte() || receiver.end_byte() > body.end_byte() {
            return None;
        }
        let mut ancestor = receiver.parent();
        while let Some(node) = ancestor {
            match node.kind() {
                "lambda" => return None,
                "list_comprehension" | "set_comprehension" | "generator_expression" => {
                    let clauses: Vec<_> = children(node)
                        .into_iter()
                        .filter(|child| child.kind() == "for_in_clause")
                        .collect();
                    let [clause] = clauses.as_slice() else {
                        return None;
                    };
                    let left = clause.child_by_field_name("left")?;
                    let right = clause.child_by_field_name("right")?;
                    let body = node.child_by_field_name("body")?;
                    if left.kind() != "identifier"
                        || text(left, &file.source) != Some(name)
                        || right.kind() != "identifier"
                        || receiver.start_byte() < body.start_byte()
                        || receiver.end_byte() > body.end_byte()
                        || !file.bound_once(name, clause.id(), Some(node))
                    {
                        return None;
                    }
                    let function = enclosing_function(node)?;
                    let annotation =
                        parameter_annotation(file, function, text(right, &file.source)?)?;
                    let element = list_element(file, annotation)?;
                    return self.class_for_name(dir, path, file, element);
                }
                "function_definition" => break,
                _ => {}
            }
            ancestor = node.parent();
        }
        if let Some(annotation) = parameter_annotation(file, function, name) {
            let annotation = annotation.named_child(0)?;
            if annotation.kind() != "identifier" {
                return None;
            }
            return self.class_for_name(dir, path, file, text(annotation, &file.source)?);
        }
        let mut assignment = None;
        for node in nodes(function) {
            if node.kind() == "assignment"
                && node.child_by_field_name("left").is_some_and(|left| {
                    left.kind() == "identifier" && text(left, &file.source) == Some(name)
                })
            {
                if assignment.is_some()
                    || node.end_byte() > receiver.start_byte()
                    || !file.bound_once(name, node.id(), Some(function))
                    || node.parent()?.parent()?.id() != function.child_by_field_name("body")?.id()
                {
                    return None;
                }
                assignment = Some(node);
            }
        }
        self.constructor_class(dir, path, file, assignment?.child_by_field_name("right")?)
    }

    fn constructor_class(
        &mut self,
        dir: &str,
        path: &str,
        file: &PythonFile,
        call: Node<'_>,
    ) -> Option<ClassIdentity> {
        if call.kind() != "call" {
            return None;
        }
        let function = call.child_by_field_name("function")?;
        if function.kind() != "identifier" {
            return None;
        }
        self.class_for_name(dir, path, file, text(function, &file.source)?)
    }
}

impl PythonFile {
    fn parse(source: Vec<u8>) -> Option<Self> {
        if source.len() > MAX_SOURCE_BYTES {
            return None;
        }
        let tree = parser::parse_source(&source, LangId::Python).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let all = nodes(root);
        if all.len() > 100_000
            || all.iter().any(|node| {
                node.is_missing()
                    || matches!(
                        node.kind(),
                        "wildcard_import"
                            | "named_expression"
                            | "match_statement"
                            | "global_statement"
                            | "nonlocal_statement"
                            | "type_alias_statement"
                    )
                    || node.child_by_field_name("type_parameters").is_some()
                    || (node.kind() == "identifier"
                        && text(*node, &source).is_some_and(|name| {
                            matches!(
                                name,
                                "globals"
                                    | "locals"
                                    | "vars"
                                    | "exec"
                                    | "eval"
                                    | "__import__"
                                    | "setattr"
                                    | "delattr"
                                    | "getattr"
                                    | "__dict__"
                                    | "__class__"
                                    | "__bases__"
                                    | "type"
                                    | "__mro__"
                                    | "__subclasses__"
                                    | "__base__"
                                    | "attrgetter"
                                    | "import_module"
                                    | "new_class"
                                    | "modules"
                                    | "__annotations__"
                                    | "get_type_hints"
                                    | "__builtins__"
                                    | "resolve_name"
                                    | "getmembers"
                                    | "getmembers_static"
                                    | "locate"
                                    | "get_objects"
                                    | "get_referrers"
                                    | "__reduce_ex__"
                            )
                        }))
            })
        {
            return None;
        }
        let mut bindings: HashMap<String, Vec<Binding>> = HashMap::new();
        for node in all {
            let pattern = match node.kind() {
                "class_definition" | "function_definition" => node.child_by_field_name("name"),
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    node.child_by_field_name("left")
                }
                "typed_parameter" | "typed_default_parameter" | "default_parameter" => node
                    .child_by_field_name("name")
                    .or_else(|| node.named_child(0)),
                "identifier"
                    if node.parent().is_some_and(|parent| {
                        matches!(parent.kind(), "parameters" | "lambda_parameters")
                    }) =>
                {
                    Some(node)
                }
                "as_pattern" => children(node).last().copied(),
                _ => None,
            };
            if let Some(pattern) = pattern {
                record_pattern(&mut bindings, pattern, node, &source);
            }
            if matches!(node.kind(), "import_statement" | "import_from_statement") {
                let module = node.child_by_field_name("module_name");
                for child in children(node) {
                    if module.is_some_and(|module| module.id() == child.id()) {
                        continue;
                    }
                    let binding = if child.kind() == "aliased_import" {
                        child.child_by_field_name("alias")
                    } else if child.kind() == "dotted_name" {
                        child.named_child(0)
                    } else {
                        None
                    };
                    if let Some(binding) = binding {
                        record_pattern(&mut bindings, binding, node, &source);
                    }
                }
            }
        }
        let file = Self {
            source,
            tree,
            bindings,
        };
        class_bases_are_static(&file).then_some(file)
    }

    fn bound_once(&self, name: &str, allowed: usize, scope: Option<Node<'_>>) -> bool {
        let Some(bindings) = self.bindings.get(name) else {
            return false;
        };
        let mut found = false;
        for binding in bindings.iter().filter(|binding| {
            scope.is_none_or(|scope| {
                scope.start_byte() <= binding.byte && binding.byte < scope.end_byte()
            })
        }) {
            found = true;
            if binding.id != allowed {
                return false;
            }
        }
        found
    }
}

fn record_pattern(
    bindings: &mut HashMap<String, Vec<Binding>>,
    pattern: Node<'_>,
    binder: Node<'_>,
    source: &[u8],
) {
    let _: std::ops::ControlFlow<()> = walk_preorder(pattern, Children::Named, |node, _| {
        if matches!(node.kind(), "attribute" | "subscript") {
            return std::ops::ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "identifier"
            && let Some(name) = text(node, source)
        {
            bindings.entry(name.to_owned()).or_default().push(Binding {
                id: binder.id(),
                byte: binder.start_byte(),
            });
        }
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
}

/// 動的取得した値・alias・式を base に使う class は owner 名を refs に残さない。
/// 同じ file の root にある通常 class に一意に束縛された名前だけを許す。
/// import 経由の base は移入先の値まで証明せず、この限定証明では保留する。
fn class_bases_are_static(file: &PythonFile) -> bool {
    nodes(file.tree.root_node()).into_iter().all(|class| {
        if class.kind() != "class_definition" {
            return true;
        }
        let Some(bases) = class.child_by_field_name("superclasses") else {
            return true;
        };
        children(bases)
            .into_iter()
            .filter(|base| base.kind() != "comment")
            .all(|base| {
                if base.kind() != "identifier" {
                    return false;
                }
                let Some(name) = text(base, &file.source) else {
                    return false;
                };
                if name == "object" && !file.bindings.contains_key(name) {
                    return true;
                }
                children(file.tree.root_node())
                    .into_iter()
                    .any(|statement| {
                        if statement.kind() == "class_definition" {
                            return statement
                                .child_by_field_name("name")
                                .is_some_and(|defined| {
                                    text(defined, &file.source) == Some(name)
                                        && file.bound_once(name, statement.id(), None)
                                });
                        }
                        false
                    })
            })
    })
}

fn local_class<'a>(file: &'a PythonFile, name: &str) -> Option<Node<'a>> {
    let mut selected = None;
    for statement in children(file.tree.root_node()) {
        let class = if statement.kind() == "decorated_definition" {
            statement.child_by_field_name("definition")?
        } else {
            statement
        };
        if class.kind() != "class_definition"
            || text(class.child_by_field_name("name")?, &file.source) != Some(name)
        {
            continue;
        }
        if selected.is_some()
            || class.child_by_field_name("superclasses").is_some()
            || !file.bound_once(name, class.id(), None)
        {
            return None;
        }
        if statement.kind() == "decorated_definition" {
            let decorators: Vec<_> = children(statement)
                .into_iter()
                .filter(|child| child.kind() == "decorator")
                .collect();
            if decorators.len() != 1
                || text(decorators[0].named_child(0)?, &file.source) != Some("dataclass")
                || !dataclass_is_imported(file)
            {
                return None;
            }
        }
        for member in children(class.child_by_field_name("body")?) {
            let definition = if member.kind() == "decorated_definition" {
                // class を変更し得る任意の decorator は許さない。
                for decorator in children(member)
                    .into_iter()
                    .filter(|child| child.kind() == "decorator")
                {
                    let name = text(decorator.named_child(0)?, &file.source)?;
                    if !matches!(name, "property" | "staticmethod" | "classmethod")
                        || file.bindings.contains_key(name)
                    {
                        return None;
                    }
                }
                member.child_by_field_name("definition")?
            } else {
                member
            };
            match definition.kind() {
                "function_definition" => {
                    let name = text(definition.child_by_field_name("name")?, &file.source)?;
                    if dynamic_class_member(name) {
                        return None;
                    }
                }
                "expression_statement"
                    if definition
                        .named_child(0)
                        .is_some_and(|child| matches!(child.kind(), "assignment" | "string")) =>
                {
                    if definition
                        .named_child(0)
                        .and_then(|assignment| assignment.child_by_field_name("left"))
                        .and_then(|left| text(left, &file.source))
                        .is_some_and(dynamic_class_member)
                    {
                        return None;
                    }
                }
                "comment" | "pass_statement" => {}
                _ => return None,
            }
        }
        selected = Some(class);
    }
    selected
}

fn dynamic_class_member(name: &str) -> bool {
    matches!(
        name,
        "__new__"
            | "__getattribute__"
            | "__getattr__"
            | "__setattr__"
            | "__delattr__"
            | "__init_subclass__"
    )
}

fn dataclass_is_imported(file: &PythonFile) -> bool {
    children(file.tree.root_node()).into_iter().any(|node| {
        node.kind() == "import_from_statement"
            && text(node, &file.source) == Some("from dataclasses import dataclass")
            && file.bound_once("dataclass", node.id(), None)
    })
}

fn field_declaration_owner<'a>(file: &PythonFile, name: Node<'a>) -> Option<Node<'a>> {
    let parent = name.parent()?;
    if parent.kind() == "assignment" && parent.child_by_field_name("left")?.id() == name.id() {
        let class = parent.parent()?.parent()?.parent()?;
        return (class.kind() == "class_definition").then_some(class);
    }
    if parent.kind() != "attribute" || parent.child_by_field_name("attribute")?.id() != name.id() {
        return None;
    }
    let assignment = parent.parent()?;
    if assignment.kind() != "assignment"
        || assignment.child_by_field_name("left")?.id() != parent.id()
    {
        return None;
    }
    let function = enclosing_function(assignment)?;
    if text(function.child_by_field_name("name")?, &file.source) != Some("__init__")
        || function.parent()?.parent()?.kind() != "class_definition"
        || function
            .parent()
            .is_some_and(|parent| parent.kind() == "decorated_definition")
    {
        return None;
    }
    let receiver = parent.child_by_field_name("object")?;
    let first = function.child_by_field_name("parameters")?.named_child(0)?;
    if first.kind() != "identifier"
        || receiver.kind() != "identifier"
        || text(receiver, &file.source) != text(first, &file.source)
        || !file.bound_once(text(first, &file.source)?, first.id(), Some(function))
    {
        return None;
    }
    function.parent()?.parent()
}

fn class_has_field(file: &PythonFile, class: Node<'_>, member: &str) -> bool {
    let mut found = false;
    for node in nodes(class) {
        if node.kind() != "identifier" || text(node, &file.source) != Some(member) {
            continue;
        }
        if node.parent().is_some_and(|parent| {
            parent.kind() == "function_definition"
                && parent
                    .child_by_field_name("name")
                    .is_some_and(|name| name.id() == node.id())
        }) {
            return false;
        }
        if field_declaration_owner(file, node).is_some_and(|owner| owner.id() == class.id()) {
            found = true;
        }
    }
    found
}

fn parameter_annotation<'a>(file: &PythonFile, function: Node<'a>, name: &str) -> Option<Node<'a>> {
    let parameters = function.child_by_field_name("parameters")?;
    let mut found = None;
    for parameter in children(parameters) {
        if parameter.kind() == "typed_parameter"
            && text(parameter.named_child(0)?, &file.source) == Some(name)
        {
            if found.is_some() || !file.bound_once(name, parameter.id(), Some(function)) {
                return None;
            }
            found = parameter.child_by_field_name("type");
        }
    }
    found
}

fn list_element<'a>(file: &'a PythonFile, annotation: Node<'_>) -> Option<&'a str> {
    let generic = annotation.named_child(0)?;
    if generic.kind() != "generic_type"
        || text(generic.named_child(0)?, &file.source) != Some("list")
        || file.bindings.contains_key("list")
    {
        return None;
    }
    let parameters = generic.named_child(1)?;
    if parameters.named_child_count() != 1 {
        return None;
    }
    let element = parameters.named_child(0)?.named_child(0)?;
    (element.kind() == "identifier")
        .then(|| text(element, &file.source))
        .flatten()
}

fn enclosing_function(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "lambda" {
            return None;
        }
        if parent.kind() == "function_definition" {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn is_field_read(attribute: Node<'_>) -> bool {
    let Some(parent) = attribute.parent() else {
        return false;
    };
    if parent.kind() == "call"
        && parent
            .child_by_field_name("function")
            .is_some_and(|function| function.id() == attribute.id())
    {
        return false;
    }
    super::removed_scope::python_is_load(attribute)
        || (matches!(
            parent.kind(),
            "list_comprehension" | "set_comprehension" | "generator_expression"
        ) && parent
            .child_by_field_name("body")
            .is_some_and(|body| body.id() == attribute.id()))
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let _: std::ops::ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        out.push(node);
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    out
}
