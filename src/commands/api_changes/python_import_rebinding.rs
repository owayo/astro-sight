//! Python の公開名を残す明示 import と、独立した狭い同値証明。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::rc::Rc;

use tree_sitter::{Node, Tree};

use super::prepare::{ApiChangeBuckets, DetectionInputs};
use super::{ApiSymbolCandidate, load_new_source};
use crate::engine::parser::{self, SourceBuf};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use crate::models::python_import_rebinding::{
    PythonImportRebinding, PythonRebindingTarget, PythonRebindingUse, RebindingEquivalence,
};
use crate::models::review::{
    ApiModificationUnverified, ApiModificationUnverifiedReason, ApiSymbolChange,
    CompatibleApiModification,
};

#[derive(Clone)]
enum Binding {
    Function,
    Other,
    Import { module: String, name: String },
}

struct Module {
    source: SourceBuf,
    tree: Tree,
    bindings: BTreeMap<String, Vec<Binding>>,
    imports: Vec<String>,
}

#[derive(Default)]
struct Cache {
    files: HashMap<String, Option<Rc<Module>>>,
    project_closed: HashMap<(String, String), bool>,
}

impl Cache {
    fn file(&mut self, dir: &str, path: &str) -> Option<Rc<Module>> {
        if let Some(file) = self.files.get(path) {
            return file.clone();
        }
        let file = (|| {
            let root = std::fs::canonicalize(dir).ok()?;
            let canonical = std::fs::canonicalize(root.join(path)).ok()?;
            if !canonical.starts_with(root) {
                return None;
            }
            Module::parse(load_new_source(dir, path)?)
        })()
        .map(Rc::new);
        self.files.insert(path.to_owned(), file.clone());
        file
    }

    fn graph_is_acyclic(&mut self, dir: &str, entry: &str) -> bool {
        let mut states = HashMap::<String, u8>::new();
        let mut stack = vec![(entry.to_owned(), false)];
        while let Some((path, exit)) = stack.pop() {
            if exit {
                states.insert(path, 2);
                continue;
            }
            match states.get(&path) {
                Some(1) => return false,
                Some(2) => continue,
                _ => {}
            }
            let Some(file) = self.file(dir, &path) else {
                return false;
            };
            states.insert(path.clone(), 1);
            stack.push((path, true));
            for imported in file.imports.iter().rev() {
                let Some(target) = super::python_module_path::resolve(dir, imported) else {
                    return false;
                };
                stack.push((target, false));
            }
        }
        true
    }
}

impl Module {
    fn parse(source: SourceBuf) -> Option<Self> {
        let tree = parser::parse_source(&source, LangId::Python).ok()?;
        if tree.root_node().has_error() || has_dynamic_binding(tree.root_node(), &source) {
            return None;
        }
        let mut bindings: BTreeMap<String, Vec<Binding>> = BTreeMap::new();
        let mut imports = Vec::new();
        for statement in children(tree.root_node()) {
            match statement.kind() {
                "function_definition" => {
                    if !inert_header(statement, &source) {
                        return None;
                    }
                    let name = text(statement.child_by_field_name("name")?, &source)?;
                    bindings
                        .entry(name.to_owned())
                        .or_default()
                        .push(Binding::Function);
                }
                "import_from_statement" => {
                    let module = statement.child_by_field_name("module_name")?;
                    if module.kind() != "dotted_name" {
                        return None;
                    }
                    let module_text = text(module, &source)?;
                    imports.push(module_text.to_owned());
                    for import in children(statement) {
                        if import.id() == module.id() || import.kind() == "comment" {
                            continue;
                        }
                        let (imported, local) = import_names(import, &source)?;
                        bindings
                            .entry(local.to_owned())
                            .or_default()
                            .push(Binding::Import {
                                module: module_text.to_owned(),
                                name: imported.to_owned(),
                            });
                    }
                }
                "comment" | "pass_statement" => {}
                "expression_statement"
                    if statement.named_child(0).is_some_and(|node| {
                        node.kind() == "string" && plain_string(node, &source)
                    }) => {}
                "expression_statement" => {
                    let assignment = statement.named_child(0)?;
                    let left = assignment.child_by_field_name("left")?;
                    if assignment.kind() != "assignment"
                        || left.kind() != "identifier"
                        || assignment.child_by_field_name("type").is_some()
                        || !literal(assignment.child_by_field_name("right")?, &source)
                    {
                        return None;
                    }
                    bindings
                        .entry(text(left, &source)?.to_owned())
                        .or_default()
                        .push(Binding::Other);
                }
                _ => return None,
            }
        }
        Some(Self {
            source,
            tree,
            bindings,
            imports,
        })
    }

    fn function(&self, name: &str) -> Option<Node<'_>> {
        if !matches!(self.bindings.get(name)?.as_slice(), [Binding::Function]) {
            return None;
        }
        children(self.tree.root_node()).into_iter().find(|node| {
            node.kind() == "function_definition"
                && node
                    .child_by_field_name("name")
                    .and_then(|node| text(node, &self.source))
                    == Some(name)
        })
    }
}

pub(super) fn index_names(
    dir: &str,
    diff_files: &[crate::models::impact::DiffFile],
    prepared: &[super::prepare::PreparedDiffFile],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut cache = Cache::default();
    for (file, prepared) in diff_files.iter().zip(prepared) {
        if !file.new_path.ends_with(".py") {
            continue;
        }
        let super::prepare::PreparedDiffFile::Modified {
            old_syms: Some(old),
            new_syms: Some(new),
            ..
        } = prepared
        else {
            continue;
        };
        let Some(current) = cache.file(dir, &file.new_path) else {
            continue;
        };
        for (name, kind, _) in old {
            if kind == "function"
                && !name.contains('.')
                && !new.iter().any(|(current, _, _)| current == name)
                && matches!(
                    current.bindings.get(name).map(Vec::as_slice),
                    Some([Binding::Import { .. }])
                )
            {
                names.insert(name.clone());
            }
        }
    }
    names
}

pub(super) fn partition(
    inputs: &DetectionInputs<'_>,
    buckets: &mut ApiChangeBuckets,
    candidates: Vec<ApiSymbolCandidate>,
) -> (Vec<ApiSymbolCandidate>, Vec<PythonImportRebinding>) {
    let mut cache = Cache::default();
    let mut removed = Vec::new();
    let mut rebindings = Vec::new();
    for candidate in candidates {
        let Some((evidence, new_signature)) = prove_binding(inputs, &candidate, &mut cache) else {
            removed.push(candidate);
            continue;
        };
        if evidence.equivalence == RebindingEquivalence::Proven {
            buckets.compatible_modified.push(CompatibleApiModification {
                change_evidence: None,
                name: candidate.name,
                inline_module: candidate.inline_module,
                kind: candidate.kind,
                file: candidate.file,
                old_signature: Some(candidate.signature),
                new_signature: Some(new_signature),
                reason: "python_import_rebinding".to_owned(),
            });
        } else {
            buckets.modified.push(ApiSymbolChange {
                inline_module: candidate.inline_module,
                unverified: Some(ApiModificationUnverified {
                    reason: ApiModificationUnverifiedReason::PythonImportRebindingUnverified,
                    first_unproven: None,
                }),
                ..ApiSymbolChange::new(
                    candidate.name,
                    candidate.kind,
                    candidate.file,
                    Some(candidate.signature),
                    Some(new_signature),
                )
            });
        }
        rebindings.push(evidence);
    }
    (removed, rebindings)
}

fn prove_binding(
    inputs: &DetectionInputs<'_>,
    candidate: &ApiSymbolCandidate,
    cache: &mut Cache,
) -> Option<(PythonImportRebinding, String)> {
    if candidate.kind != "function"
        || candidate.name.contains('.')
        || !candidate.file.ends_with(".py")
    {
        return None;
    }
    let current = cache.file(inputs.dir, &candidate.file)?;
    let [Binding::Import { module, name }] = current.bindings.get(&candidate.name)?.as_slice()
    else {
        return None;
    };
    let target_path = super::python_module_path::resolve(inputs.dir, module)?;
    if !cache.graph_is_acyclic(inputs.dir, &candidate.file) {
        return None;
    }
    let target = cache.file(inputs.dir, &target_path)?;
    let target_function = target.function(name)?;
    let old = Module::parse(SourceBuf::Vec(inputs.base_blobs.read(&candidate.file)?))?;
    let old_function = old.function(&candidate.name)?;
    let target_body = target_function.child_by_field_name("body")?;
    let new_signature = text(target_function, &target.source)?
        .get(..target_body.start_byte() - target_function.start_byte())?
        .trim()
        .to_owned();
    let mut uses = Vec::new();
    let _: ControlFlow<()> = walk_preorder(current.tree.root_node(), Children::Named, |node, _| {
        if matches!(node.kind(), "import_from_statement" | "import_statement") {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "identifier"
            && text(node, &current.source) == Some(candidate.name.as_str())
        {
            uses.push(PythonRebindingUse {
                line: node.start_position().row,
                column: node.start_position().column,
                binding_verified: false,
            });
        }
        ControlFlow::Continue(Walk::Descend)
    });
    let equal = closed_function_equal(&old, old_function, &target, target_function)
        && project_uses_are_closed(inputs, candidate, name, cache);
    for site in &mut uses {
        site.binding_verified = equal;
    }
    Some((
        PythonImportRebinding {
            name: candidate.name.clone(),
            kind: candidate.kind.clone(),
            file: candidate.file.clone(),
            target: PythonRebindingTarget {
                file: target_path,
                name: name.clone(),
                line: target_function.start_position().row,
            },
            equivalence: if equal {
                RebindingEquivalence::Proven
            } else {
                RebindingEquivalence::Unverified
            },
            same_file_uses: uses,
        },
        new_signature,
    ))
}

fn import_names<'a>(node: Node<'_>, source: &'a [u8]) -> Option<(&'a str, &'a str)> {
    let (imported, local) = if node.kind() == "aliased_import" {
        (
            node.child_by_field_name("name")?,
            node.child_by_field_name("alias")?,
        )
    } else if node.kind() == "dotted_name" && node.named_child_count() == 1 {
        (node, node)
    } else {
        return None;
    };
    let imported = text(imported, source)?;
    let local = text(local, source)?;
    (!imported.contains('.') && !local.contains('.')).then_some((imported, local))
}

fn inert_header(function: Node<'_>, source: &[u8]) -> bool {
    let Some(name) = function.child_by_field_name("name") else {
        return false;
    };
    if source
        .get(function.start_byte()..name.start_byte())
        .and_then(|s| std::str::from_utf8(s).ok())
        .map(str::trim)
        != Some("def")
    {
        return false;
    }
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return false;
    };
    for parameter in children(parameters) {
        if parameter.kind() == "comment" {
            continue;
        }
        if parameter.kind() == "identifier" {
            continue;
        }
        if !matches!(
            parameter.kind(),
            "typed_parameter" | "default_parameter" | "typed_default_parameter"
        ) {
            return false;
        }
        if parameter
            .child_by_field_name("type")
            .is_some_and(|node| !primitive_type(node, source))
            || parameter
                .child_by_field_name("value")
                .is_some_and(|node| !literal(node, source))
        {
            return false;
        }
    }
    function
        .child_by_field_name("return_type")
        .is_none_or(|node| primitive_type(node, source))
}

fn primitive_type(node: Node<'_>, source: &[u8]) -> bool {
    matches!(
        text(node, source),
        Some("str" | "int" | "float" | "bool" | "bytes" | "object" | "None")
    )
}

fn plain_string(node: Node<'_>, source: &[u8]) -> bool {
    children(node)
        .into_iter()
        .all(|child| child.kind() != "interpolation")
        && text(node, source).is_some_and(|value| !value.trim_start().starts_with(['f', 'F']))
}

fn literal(node: Node<'_>, source: &[u8]) -> bool {
    matches!(node.kind(), "integer" | "float" | "true" | "false" | "none")
        || (node.kind() == "string" && plain_string(node, source))
}

fn closed_function_equal(
    old: &Module,
    old_function: Node<'_>,
    new: &Module,
    new_function: Node<'_>,
) -> bool {
    if !inert_header(old_function, &old.source) || !inert_header(new_function, &new.source) {
        return false;
    }
    for builtin in ["str", "int", "float", "bool", "bytes", "object"] {
        if old.bindings.contains_key(builtin) || new.bindings.contains_key(builtin) {
            return false;
        }
    }
    let Some(old_body) = old_function.child_by_field_name("body") else {
        return false;
    };
    let Some(new_body) = new_function.child_by_field_name("body") else {
        return false;
    };
    let parameters: HashSet<_> = children(old_function.child_by_field_name("parameters").unwrap())
        .into_iter()
        .filter_map(|parameter| {
            let name = if parameter.kind() == "identifier" {
                parameter
            } else {
                parameter
                    .child_by_field_name("name")
                    .or_else(|| parameter.named_child(0))?
            };
            text(name, &old.source).map(str::to_owned)
        })
        .collect();
    let statements: Vec<_> = children(old_body)
        .into_iter()
        .filter(|node| node.kind() != "comment")
        .collect();
    if !matches!(statements.as_slice(), [node] if node.kind() == "return_statement") {
        return false;
    }
    let closed = walk_preorder(old_body, Children::Named, |node, _| {
        if node.kind() == "string" {
            return if plain_string(node, &old.source) {
                ControlFlow::Continue(Walk::Skip)
            } else {
                ControlFlow::Break(())
            };
        }
        if node.kind() == "identifier" {
            return if text(node, &old.source).is_some_and(|name| parameters.contains(name)) {
                ControlFlow::Continue(Walk::Skip)
            } else {
                ControlFlow::Break(())
            };
        }
        if matches!(
            node.kind(),
            "block"
                | "return_statement"
                | "binary_operator"
                | "unary_operator"
                | "boolean_operator"
                | "not_operator"
                | "comparison_operator"
                | "conditional_expression"
                | "parenthesized_expression"
                | "integer"
                | "float"
                | "true"
                | "false"
                | "none"
                | "comment"
        ) {
            ControlFlow::Continue(Walk::Descend)
        } else {
            ControlFlow::Break(())
        }
    })
    .is_continue();
    closed
        && super::signature_tokens::node_signature_tokens(old_function, &old.source)
            .zip(super::signature_tokens::node_signature_tokens(
                new_function,
                &new.source,
            ))
            .is_some_and(|(old, new)| old == new)
        && super::signature_tokens::node_signature_tokens(old_body, &old.source)
            == super::signature_tokens::node_signature_tokens(new_body, &new.source)
}

fn has_dynamic_binding(root: Node<'_>, source: &[u8]) -> bool {
    crate::engine::tree_walk::any_node(root, Children::Named, |node| {
        matches!(
            node.kind(),
            "global_statement" | "nonlocal_statement" | "wildcard_import"
        ) || (node.kind() == "identifier"
            && matches!(
                text(node, source),
                Some(
                    "exec"
                        | "eval"
                        | "globals"
                        | "locals"
                        | "vars"
                        | "getattr"
                        | "setattr"
                        | "delattr"
                        | "__import__"
                        | "__builtins__"
                        | "__dict__"
                        | "__getattribute__"
                        | "__getattr__"
                        | "__traceback__"
                        | "tb_frame"
                        | "f_globals"
                        | "f_locals"
                        | "importlib"
                        | "inspect"
                        | "sys"
                )
            ))
    })
}

fn project_uses_are_closed(
    inputs: &DetectionInputs<'_>,
    candidate: &ApiSymbolCandidate,
    target_name: &str,
    cache: &mut Cache,
) -> bool {
    let key = (candidate.name.clone(), target_name.to_owned());
    if let Some(closed) = cache.project_closed.get(&key) {
        return *closed;
    }
    let closed = (|| {
        inputs.ref_index.complete_refs_for(&candidate.name)?;
        let root = std::fs::canonicalize(inputs.dir).ok()?;
        let files = crate::engine::refs::collect_files(&root, None).ok()?;
        let mut python_paths = Vec::new();
        for path in files {
            if crate::engine::refs::detect_source_lang(&path) != Some(LangId::Python) {
                continue;
            }
            let canonical = std::fs::canonicalize(&path).ok()?;
            if !canonical.starts_with(&root) {
                return None;
            }
            let relative = crate::git_support::normalize_workspace_separators(
                canonical.strip_prefix(&root).ok()?.to_str()?,
            );
            python_paths.push(relative);
        }
        let inventory: HashSet<_> = python_paths.iter().map(String::as_str).collect();
        for relative in &python_paths {
            let source = load_new_source(inputs.dir, relative)?;
            let tree = parser::parse_source(&source, LangId::Python).ok()?;
            if tree.root_node().has_error() || has_dynamic_binding(tree.root_node(), &source) {
                return None;
            }
            let safe = walk_preorder(tree.root_node(), Children::Named, |node, _| {
                if node.kind() == "import_statement" {
                    return ControlFlow::Break(());
                }
                if node.kind() == "import_from_statement" {
                    let Some(module) = node.child_by_field_name("module_name") else {
                        return ControlFlow::Break(());
                    };
                    if node.parent().is_none_or(|parent| parent.kind() != "module") {
                        return ControlFlow::Break(());
                    }
                    let Some(imported_path) = text(module, &source)
                        .and_then(|module| super::python_module_path::resolve(inputs.dir, module))
                    else {
                        return ControlFlow::Break(());
                    };
                    // 明示 import で到達しても、走査対象外の本文の同値性は断定しない。
                    if !inventory.contains(imported_path.as_str()) {
                        return ControlFlow::Break(());
                    }
                    let Some(imported_module) = cache.file(inputs.dir, &imported_path) else {
                        return ControlFlow::Break(());
                    };
                    for import in children(node) {
                        if import.id() == module.id() || import.kind() == "comment" {
                            continue;
                        }
                        let Some((imported, local)) = import_names(import, &source) else {
                            return ControlFlow::Break(());
                        };
                        if !matches!(
                            imported_module.bindings.get(imported).map(Vec::as_slice),
                            Some([Binding::Function] | [Binding::Import { .. }])
                        ) {
                            return ControlFlow::Break(());
                        }
                        if (imported == candidate.name || imported == target_name)
                            && imported != local
                        {
                            return ControlFlow::Break(());
                        }
                    }
                    return ControlFlow::Continue(Walk::Skip);
                }
                if node.kind() == "identifier"
                    && text(node, &source)
                        .is_some_and(|name| name == candidate.name || name == target_name)
                {
                    let Some(parent) = node.parent() else {
                        return ControlFlow::Break(());
                    };
                    if (parent.kind() == "function_definition"
                        && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id()))
                        || (parent.kind() == "call"
                            && parent.child_by_field_name("function").map(|n| n.id())
                                == Some(node.id()))
                    {
                        if parent.kind() == "function_definition"
                            && parent.parent().is_none_or(|outer| outer.kind() != "module")
                        {
                            return ControlFlow::Break(());
                        }
                        return ControlFlow::Continue(Walk::Skip);
                    }
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Walk::Descend)
            })
            .is_continue();
            if !safe {
                return None;
            }
        }
        Some(())
    })()
    .is_some();
    cache.project_closed.insert(key, closed);
    closed
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}
