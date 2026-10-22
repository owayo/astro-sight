//! Conservative owner proof for Rust type paths used only by impact.
//!
//! A proof requires an ordinary Cargo source layout, an unambiguous chain of
//! `mod` declarations, and a local declaration in the resolved module. Any
//! source transformation or uncertain binding leaves the reference unchanged.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tree_sitter::Node;

use crate::engine::{
    bounded_read::{read_input_file, read_input_text_file},
    parser,
    rust_macro_name::{
        has_token_tree_ancestor, is_field_name_in_token_tree, last_macro_identifier,
    },
    rust_targets::RustTargets,
};
use crate::language::LangId;
use crate::models::impact::DiffFile;

struct FileEvidence {
    types: HashSet<String>,
    free_functions: HashSet<String>,
    modules: HashSet<String>,
}

impl FileEvidence {
    fn collect(root: Node<'_>, source: &[u8]) -> Option<Self> {
        if root.kind() != "source_file" || root.has_error() {
            return None;
        }
        let mut imports = Vec::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if matches!(
                node.kind(),
                "attribute_item"
                    | "inner_attribute_item"
                    | "macro_invocation"
                    | "macro_definition"
                    | "extern_crate_declaration"
                    | "type_parameters"
            ) {
                return None;
            }
            if node.kind() == "use_declaration" {
                imports.push(node.utf8_text(source).ok()?.to_owned());
            }
            if matches!(
                node.kind(),
                "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item"
            ) && node.parent().is_none_or(|parent| parent.id() != root.id())
            {
                return None;
            }
            if node.kind() == "mod_item" && node.child_by_field_name("body").is_some() {
                return None;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        let mut types = HashSet::new();
        let mut free_functions = HashSet::new();
        let mut modules = HashSet::new();
        let import_may_bind = |name: &str| {
            imports
                .iter()
                .any(|import| !import.is_ascii() || import.contains('*') || import.contains(name))
        };
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if matches!(
                child.kind(),
                "struct_item" | "enum_item" | "union_item" | "trait_item"
            ) {
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                if import_may_bind(name) || !types.insert(name.to_owned()) {
                    return None;
                }
            } else if child.kind() == "function_item" {
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                if import_may_bind(name) || !free_functions.insert(name.to_owned()) {
                    return None;
                }
            } else if child.kind() == "mod_item" {
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                if import_may_bind(name) || !modules.insert(name.to_owned()) {
                    return None;
                }
            }
        }
        Some(Self {
            types,
            free_functions,
            modules,
        })
    }
}

pub(super) struct RustPathOwnerIndex {
    current: Option<Arc<FileEvidence>>,
    source_hash: blake3::Hash,
    direct_imports: HashMap<String, Vec<String>>,
    local_type_names: HashSet<String>,
    local_function_names: HashSet<String>,
    module_context: OnceLock<Option<(PathBuf, PathBuf, Vec<String>)>>,
    resolved_paths: RefCell<HashMap<Vec<String>, Option<PathBuf>>>,
}

/// Module-relative paths need a unique compilation context for each source
/// file. A path attribute or macro elsewhere in the tree can include that
/// source under a different parent module or crate root.
pub(super) type RustSourceModule = (PathBuf, PathBuf, Vec<String>);
pub(super) type RustModuleKey = (PathBuf, Vec<String>);

#[derive(Clone)]
struct VisibilityModule {
    crate_root: PathBuf,
    modules: Vec<String>,
}

pub(super) struct RustPathOwnerProjectProof {
    pub(super) root: PathBuf,
    pub(super) canonical_root: Option<PathBuf>,
    ordinary_sources: OnceLock<bool>,
    private_scope_sources: OnceLock<bool>,
    reexport_sources: OnceLock<bool>,
    safe_private_import_environment: OnceLock<bool>,
    safe_macro_value_environment: OnceLock<bool>,
    no_source_inclusion: OnceLock<bool>,
    no_glob_imports: OnceLock<bool>,
    module_wiring_unchanged: bool,
    private_scope_wiring_unchanged: bool,
    reexport_cargo_enabled: bool,
    pub(super) reexport_old_sources: HashMap<PathBuf, Option<Vec<u8>>>,
    pub(super) reexport_source_modules: Mutex<HashMap<PathBuf, Option<RustSourceModule>>>,
    pub(super) reexport_module_files: Mutex<HashMap<RustModuleKey, Option<PathBuf>>>,
    pub(super) reexport_stable_chains: Mutex<HashMap<RustModuleKey, bool>>,
    files: Mutex<HashMap<PathBuf, Option<Arc<FileEvidence>>>>,
    pub(super) reexport_files:
        Mutex<HashMap<PathBuf, Option<Arc<super::rust_reexport_owner::ModuleBindings>>>>,
    leaf_sources: Mutex<HashMap<String, bool>>,
    visibility_modules: Mutex<HashMap<PathBuf, Option<VisibilityModule>>>,
}

impl RustPathOwnerProjectProof {
    pub(super) fn new(
        root: &Path,
        module_wiring_unchanged: bool,
        private_scope_wiring_unchanged: bool,
        reexport_cargo_enabled: bool,
        diff_files: &[DiffFile],
        diff_input: &str,
    ) -> Self {
        let mut reexport_old_sources = HashMap::new();
        for file in diff_files {
            let path = PathBuf::from(&file.new_path);
            if path.extension().is_some_and(|ext| ext == "rs") {
                let old = read_input_file(&root.join(&path)).and_then(|new_source| {
                    crate::engine::diff::reconstruct_old_source(
                        diff_input,
                        &file.new_path,
                        &new_source,
                    )
                    .map(|source| source.source)
                });
                reexport_old_sources.insert(path, old);
            }
            if file.old_path != file.new_path {
                reexport_old_sources.insert(PathBuf::from(&file.old_path), None);
            }
        }
        Self {
            root: root.to_owned(),
            canonical_root: root.canonicalize().ok(),
            ordinary_sources: OnceLock::new(),
            private_scope_sources: OnceLock::new(),
            reexport_sources: OnceLock::new(),
            safe_private_import_environment: OnceLock::new(),
            safe_macro_value_environment: OnceLock::new(),
            no_source_inclusion: OnceLock::new(),
            no_glob_imports: OnceLock::new(),
            module_wiring_unchanged,
            private_scope_wiring_unchanged,
            reexport_cargo_enabled,
            reexport_old_sources,
            reexport_source_modules: Mutex::new(HashMap::new()),
            reexport_module_files: Mutex::new(HashMap::new()),
            reexport_stable_chains: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            reexport_files: Mutex::new(HashMap::new()),
            leaf_sources: Mutex::new(HashMap::new()),
            visibility_modules: Mutex::new(HashMap::new()),
        }
    }

    fn allows_module_paths(&self) -> bool {
        self.module_wiring_unchanged
            && *self
                .ordinary_sources
                .get_or_init(|| scan_ordinary_sources(&self.root))
    }

    fn allows_private_scope_paths(&self) -> bool {
        self.private_scope_wiring_unchanged
            && self.has_safe_private_import_environment()
            && *self
                .private_scope_sources
                .get_or_init(|| scan_private_scope_sources(&self.root))
    }

    pub(super) fn allows_reexport_paths(&self) -> bool {
        self.reexport_cargo_enabled
            && *self
                .reexport_sources
                .get_or_init(|| scan_reexport_sources(&self.root))
    }

    pub(super) fn allows_external_alias_paths(&self) -> bool {
        self.reexport_cargo_enabled
    }

    /// Widening visibility can introduce a name through a glob anywhere in
    /// the crate, including an ancestor module's re-export. Keep those cases
    /// blocking until the import chain can be proved locally.
    pub(super) fn allows_private_widening_paths(&self) -> bool {
        self.allows_private_scope_paths()
            && *self.no_glob_imports.get_or_init(|| {
                scan_sources(&self.root, super::rust_visibility_scope::has_glob_import)
            })
    }

    /// 旧境界内と確定した参照だけを認める。範囲外や不明の参照は除外しない。
    pub(super) fn explicit_widening_contains(
        &self,
        source_path: &str,
        ref_path: &str,
        proof: &super::rust_visibility_scope::ExplicitWidening,
    ) -> bool {
        if !self.allows_private_widening_paths() {
            return false;
        }
        let Some(source) = self.visibility_module(Path::new(source_path)) else {
            return false;
        };
        let Some(reference) = self.visibility_module(Path::new(ref_path)) else {
            return false;
        };
        if source.crate_root != reference.crate_root {
            return false;
        }
        proof
            .old_boundary(&source.modules)
            .is_some_and(|old| reference.modules.starts_with(&old))
    }

    fn visibility_module(&self, file: &Path) -> Option<VisibilityModule> {
        if let Some(found) = self
            .visibility_modules
            .lock()
            .expect("visibility cache mutex poisoned")
            .get(file)
        {
            return found.clone();
        }
        let result = (|| {
            let (_, src, modules) = source_module_path_inner(&self.root, file, true)?;
            let mut owner = None;
            for root in [src.join("lib.rs"), src.join("main.rs")] {
                // crate root 自体を別モジュールとして読む配置は所有元が一意でない。
                if modules.is_empty()
                    && private_module_file_from_roots(
                        &self.root,
                        &src,
                        &[file.file_stem()?.to_str()?.to_owned()],
                        true,
                        true,
                        [root.clone()],
                    )
                    .as_deref()
                        == Some(file)
                {
                    return None;
                }
                if private_module_file_from_roots(
                    &self.root,
                    &src,
                    &modules,
                    true,
                    true,
                    [root.clone()],
                )
                .as_deref()
                    == Some(file)
                {
                    if owner.is_some() {
                        // 同じファイルでも複数の crate に属すれば所有元は一意でない。
                        return None;
                    }
                    owner = Some(root);
                }
            }
            Some(VisibilityModule {
                crate_root: owner?,
                modules,
            })
        })();
        self.visibility_modules
            .lock()
            .expect("visibility cache mutex poisoned")
            .insert(file.to_owned(), result.clone());
        result
    }

    fn has_safe_private_import_environment(&self) -> bool {
        *self
            .safe_private_import_environment
            .get_or_init(|| scan_safe_private_import_environment(&self.root))
    }

    pub(super) fn allows_macro_value_bindings(&self) -> bool {
        *self.safe_macro_value_environment.get_or_init(|| {
            let mut shadowable_attributes = HashSet::new();
            let mut other_bindings = HashSet::new();
            let safe = scan_safe_source_environment(&self.root, |source| {
                if source_defines_allowed_macro(source) || source_may_include_elsewhere(source) {
                    return true;
                }
                let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
                    return true;
                };
                let root = tree.root_node();
                if root.has_error() {
                    return true;
                }
                let mut pending = vec![root];
                while let Some(node) = pending.pop() {
                    if node.is_missing() || node.kind() == "extern_crate_declaration" {
                        return true;
                    }
                    if matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
                        let Some(names) = super::rust_std_origin::safe_attribute(node, source)
                        else {
                            return true;
                        };
                        shadowable_attributes.extend(names);
                    }
                    if node.kind() == "macro_definition" {
                        let Some(name) = node
                            .child_by_field_name("name")
                            .and_then(|name| name.utf8_text(source).ok())
                        else {
                            return true;
                        };
                        let name = name.strip_prefix("r#").unwrap_or(name);
                        if crate::engine::rust_scope::is_value_expression_macro(name) {
                            return true;
                        }
                        other_bindings.insert(name.to_owned());
                    }
                    if node.kind() == "use_declaration" {
                        let Ok(text) = node.utf8_text(source) else {
                            return true;
                        };
                        if text.contains('*') {
                            return true;
                        }
                        for token in text.split(|ch: char| !ch.is_alphanumeric() && ch != '_') {
                            if crate::engine::rust_scope::is_value_expression_macro(token) {
                                return true;
                            }
                            other_bindings.insert(token.to_owned());
                        }
                    }
                    let mut cursor = node.walk();
                    pending.extend(node.named_children(&mut cursor));
                }
                false
            });
            safe && shadowable_attributes.is_disjoint(&other_bindings)
        })
    }

    /// A leaf module has no other source file in its privacy subtree. A
    /// separate include! could inline it into a crate root, so reject that
    /// project-wide before using the local declaration as a leaf proof.
    pub(super) fn private_source_is_leaf(&self, source_path: &str) -> bool {
        if !self.private_scope_wiring_unchanged
            || !*self
                .no_source_inclusion
                .get_or_init(|| scan_no_source_inclusion(&self.root))
        {
            return false;
        }
        if let Some(found) = self
            .leaf_sources
            .lock()
            .expect("Rust leaf cache mutex poisoned")
            .get(source_path)
        {
            return *found;
        }
        let found = (|| {
            let file = self.root.join(source_path);
            if !file
                .canonicalize()
                .ok()?
                .starts_with(self.canonical_root.as_ref()?)
            {
                return None;
            }
            let source = read_input_file(&file)?;
            Some(!source_has_external_child_module(&source))
        })()
        .unwrap_or(false);
        self.leaf_sources
            .lock()
            .expect("Rust leaf cache mutex poisoned")
            .insert(source_path.to_owned(), found);
        found
    }
}

/// A worktree-only owner cannot prove the old binding when the diff changes
/// module declarations, imports, or attributes anywhere in Rust sources.
pub(super) fn module_wiring_unchanged(
    dir: &Path,
    diff_files: &[DiffFile],
    diff_input: &str,
) -> bool {
    wiring_unchanged(dir, diff_files, diff_input, module_wiring)
}

pub(super) fn private_scope_wiring_unchanged(
    dir: &Path,
    diff_files: &[DiffFile],
    diff_input: &str,
) -> bool {
    wiring_unchanged(dir, diff_files, diff_input, private_module_wiring)
}

fn wiring_unchanged(
    dir: &Path,
    diff_files: &[DiffFile],
    diff_input: &str,
    collect: fn(&[u8]) -> Option<Vec<Vec<u8>>>,
) -> bool {
    diff_files.iter().all(|file| {
        let path = &file.new_path;
        if !Path::new(path).extension().is_some_and(|ext| {
            ext.to_str()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        }) {
            return true;
        }
        let Some(new_source) = read_input_file(&dir.join(path)) else {
            return false;
        };
        let Some(old_source) =
            crate::engine::diff::reconstruct_old_source(diff_input, path, &new_source)
        else {
            return false;
        };
        match (collect(&old_source.source), collect(&new_source)) {
            (Some(old), Some(new)) => old == new,
            _ => false,
        }
    })
}

fn module_wiring(source: &[u8]) -> Option<Vec<Vec<u8>>> {
    collect_module_wiring(source, false)
}

fn private_module_wiring(source: &[u8]) -> Option<Vec<Vec<u8>>> {
    collect_module_wiring(source, true)
}

fn collect_module_wiring(source: &[u8], ignore_builtin_body_macros: bool) -> Option<Vec<Vec<u8>>> {
    let tree = parser::parse_source(source, LangId::Rust).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut declarations = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(node) = pending.pop() {
        let safe_body_macro = ignore_builtin_body_macros
            && node.kind() == "macro_invocation"
            && node
                .child_by_field_name("macro")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(is_allowed_expression_macro)
            && has_function_ancestor(node);
        if !safe_body_macro
            && matches!(
                node.kind(),
                "mod_item"
                    | "use_declaration"
                    | "extern_crate_declaration"
                    | "attribute_item"
                    | "inner_attribute_item"
                    | "macro_invocation"
                    | "macro_definition"
            )
        {
            declarations.push(source.get(node.byte_range())?.to_vec());
        }
        let mut cursor = node.walk();
        let mut children = node.named_children(&mut cursor).collect::<Vec<_>>();
        children.reverse();
        pending.extend(children);
    }
    Some(declarations)
}

fn has_function_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "function_item" | "closure_expression") {
            return true;
        }
        node = parent;
    }
    false
}

impl RustPathOwnerIndex {
    /// Resolve both files in one ordinary crate before comparing the private
    /// declaration's module subtree. Unknown layouts never exclude a caller.
    pub(super) fn private_subtree_contains(
        &self,
        dir: &Path,
        source_path: &str,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<bool> {
        if !project.allows_private_scope_paths() {
            return None;
        }
        let source_file = Path::new(source_path);
        let ref_file = Path::new(ref_path);
        let (source_package, source_src, source_modules) = source_module_path(dir, source_file)?;
        let (ref_package, ref_src, ref_modules) = source_module_path(dir, ref_file)?;
        if source_package != ref_package || source_src != ref_src {
            return None;
        }
        if private_module_file(dir, &source_src, &source_modules, false)? != source_file
            || private_module_file(dir, &ref_src, &ref_modules, false)? != ref_file
        {
            return None;
        }
        Some(ref_modules.starts_with(&source_modules))
    }

    /// A plain root-level import can identify an unrelated declaration even
    /// when an unsafe construct in another file prevents a project-wide proof.
    pub(super) fn imports_other_private_owner(
        &self,
        dir: &Path,
        source_path: &str,
        ref_path: &str,
        ref_source: &[u8],
        name: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        if blake3::hash(ref_source) != self.source_hash
            || !project.private_scope_wiring_unchanged
            || !project.has_safe_private_import_environment()
            || source_can_retarget_private_scope(ref_source)
        {
            return false;
        }
        let Some(import) = self.direct_imports.get(name) else {
            return false;
        };
        let source_file = Path::new(source_path);
        let ref_file = Path::new(ref_path);
        let Some((source_package, source_src, source_modules)) =
            source_module_path(dir, source_file)
        else {
            return false;
        };
        let Some((ref_package, ref_src, ref_modules)) = source_module_path(dir, ref_file) else {
            return false;
        };
        if source_package != ref_package || source_src != ref_src || source_modules.is_empty() {
            return false;
        }
        let target_modules = &import[1..import.len() - 1];
        if ref_modules.starts_with(&source_modules) || target_modules.starts_with(&source_modules) {
            return false;
        }
        let Some(verified_ref) = private_module_file(dir, &ref_src, &ref_modules, true) else {
            return false;
        };
        let Some(target_file) = private_module_file(dir, &ref_src, target_modules, true) else {
            return false;
        };
        if verified_ref != ref_file || target_file == source_file {
            return false;
        }
        let Some(target_source) = read_input_file(&dir.join(&target_file)) else {
            return false;
        };
        if source_can_retarget_private_scope(&target_source) {
            return false;
        }
        let Ok(tree) = parser::parse_source(&target_source, LangId::Rust) else {
            return false;
        };
        let root = tree.root_node();
        if root.has_error() {
            return false;
        }
        let mut cursor = root.walk();
        root.named_children(&mut cursor).any(|child| {
            matches!(
                child.kind(),
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "type_item"
                    | "trait_item"
            ) && child
                .child_by_field_name("name")
                .and_then(|node| node.utf8_text(&target_source).ok())
                == Some(name)
        })
    }

    /// A direct declaration in this file owns an unqualified reference unless
    /// a nested scope can rebind it. This does not require project-wide module
    /// evidence, so unrelated test modules do not disable the proof.
    pub(super) fn plain_local_owner(&self, node: Node<'_>, source: &[u8], name: &str) -> bool {
        if node.kind() != "type_identifier"
            || !self.local_type_names.contains(name)
            || blake3::hash(source) != self.source_hash
            || has_uncertain_ancestor(node)
        {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(
                scope.kind(),
                "function_item" | "closure_expression" | "block"
            ) && (has_attribute_prefix(scope) || scope_can_rebind_name(scope, source, name))
            {
                return false;
            }
            ancestor = scope.parent();
        }
        true
    }

    /// A unique file-root function owns an unqualified value call even when
    /// its body contains expression macros or the source module is ambiguous.
    pub(super) fn plain_local_function_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> bool {
        if node.kind() != "identifier"
            || !self.local_function_names.contains(name)
            || blake3::hash(source) != self.source_hash
            || has_uncertain_ancestor(node)
        {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(
                scope.kind(),
                "function_item" | "closure_expression" | "block"
            ) && (has_attribute_prefix(scope)
                || scope_can_rebind_import_name(scope, source, name))
            {
                return false;
            }
            ancestor = scope.parent();
        }
        true
    }

    pub(super) fn is_tracked_direct_import_site(
        &self,
        mut node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> bool {
        let Some(target) = self.direct_imports.get(name) else {
            return false;
        };
        loop {
            if node.kind() == "use_declaration" {
                return node
                    .utf8_text(source)
                    .ok()
                    .and_then(plain_crate_import_path)
                    .as_ref()
                    == Some(target);
            }
            let Some(parent) = node.parent() else {
                return false;
            };
            node = parent;
        }
    }

    pub(super) fn plain_import_binding_safe(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> bool {
        if !self.direct_imports.contains_key(name)
            || blake3::hash(source) != self.source_hash
            || has_uncertain_ancestor(node)
        {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(
                scope.kind(),
                "function_item" | "closure_expression" | "block"
            ) && (has_attribute_prefix(scope)
                || scope_can_rebind_import_name(scope, source, name))
            {
                return false;
            }
            ancestor = scope.parent();
        }
        true
    }

    pub(super) fn build(mut node: Node<'_>, source: &[u8]) -> Self {
        while let Some(parent) = node.parent() {
            node = parent;
        }
        let mut direct_imports = HashMap::new();
        let mut ambiguous_imports = HashSet::new();
        let mut type_counts = HashMap::<String, usize>::new();
        let mut function_counts = HashMap::<String, usize>::new();
        let mut root_imports = Vec::new();
        let mut root_uncertain = node.has_error();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(
                child.kind(),
                "macro_invocation" | "extern_crate_declaration"
            ) {
                root_uncertain = true;
            }
            if child.kind() != "use_declaration" {
                if !has_attribute_prefix(child)
                    && let Some(name) = child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                {
                    if matches!(
                        child.kind(),
                        "struct_item" | "enum_item" | "union_item" | "trait_item"
                    ) {
                        *type_counts.entry(name.to_owned()).or_default() += 1;
                    } else if child.kind() == "function_item" {
                        *function_counts.entry(name.to_owned()).or_default() += 1;
                    }
                }
                continue;
            }
            if let Ok(text) = child.utf8_text(source) {
                root_imports.push(text);
            } else {
                root_uncertain = true;
            }
            let Some(path) = child
                .utf8_text(source)
                .ok()
                .and_then(plain_crate_import_path)
            else {
                continue;
            };
            let name = path.last().cloned().unwrap_or_default();
            if has_attribute_prefix(child) {
                ambiguous_imports.insert(name);
                continue;
            }
            if direct_imports.insert(name.clone(), path).is_some() {
                ambiguous_imports.insert(name);
            }
        }
        for name in ambiguous_imports {
            direct_imports.remove(&name);
        }
        direct_imports.retain(|name, _| {
            root_imports
                .iter()
                .filter(|import| import.contains('*') || import.contains(name))
                .count()
                == 1
        });
        let safe_name = |name: &str| {
            !root_uncertain
                && !root_imports
                    .iter()
                    .any(|import| import.contains('*') || import.contains(name))
        };
        let local_type_names = type_counts
            .into_iter()
            .filter_map(|(name, count)| (count == 1 && safe_name(&name)).then_some(name))
            .collect();
        let local_function_names = function_counts
            .into_iter()
            .filter_map(|(name, count)| (count == 1 && safe_name(&name)).then_some(name))
            .collect();
        Self {
            current: FileEvidence::collect(node, source).map(Arc::new),
            source_hash: blake3::hash(source),
            direct_imports,
            local_type_names,
            local_function_names,
            module_context: OnceLock::new(),
            resolved_paths: RefCell::new(HashMap::new()),
        }
    }

    /// Returns a repository-relative file only when the type is locally
    /// declared in the module selected by this reference.
    pub(super) fn type_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        let current = self.current.as_ref()?;
        if node.kind() != "type_identifier" || has_uncertain_ancestor(node) {
            return None;
        }
        let name = node.utf8_text(source).ok()?;
        let ref_file = Path::new(ref_path);
        let parent = node.parent()?;
        if matches!(
            parent.kind(),
            "scoped_type_identifier" | "scoped_identifier"
        ) {
            if !project.allows_module_paths() {
                return None;
            }
            if !parent
                .child_by_field_name("name")
                .is_some_and(|last| last.id() == node.id())
            {
                return None;
            }
            let segments = path_segments(parent, source)?;
            let owner = self.resolve_qualified(dir, ref_file, &segments, project)?;
            let evidence = self.evidence(ref_file, &owner, project)?;
            return evidence.types.contains(name).then_some(owner);
        }
        current.types.contains(name).then(|| ref_file.to_owned())
    }

    pub(super) fn free_function_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        let current = self.current.as_ref()?;
        if node.kind() != "identifier" || has_uncertain_ancestor(node) {
            return None;
        }
        let name = node.utf8_text(source).ok()?;
        let ref_file = Path::new(ref_path);
        let parent = node.parent()?;
        if parent.kind() == "scoped_identifier" {
            if !parent
                .child_by_field_name("name")
                .is_some_and(|last| last.id() == node.id())
                || !project.allows_module_paths()
            {
                return None;
            }
            let segments = path_segments(parent, source)?;
            let owner = self.resolve_qualified(dir, ref_file, &segments, project)?;
            let evidence = self.evidence(ref_file, &owner, project)?;
            return evidence.free_functions.contains(name).then_some(owner);
        }
        current
            .free_functions
            .contains(name)
            .then(|| ref_file.to_owned())
    }

    /// A locally declared type (or `Self` inside an impl) cannot name a free
    /// function in another module through `Type::function` syntax.
    pub(super) fn is_local_associated_path(&self, node: Node<'_>, source: &[u8]) -> bool {
        if node.kind() != "identifier" || has_uncertain_ancestor(node) {
            return false;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() != "scoped_identifier"
            || !parent
                .child_by_field_name("name")
                .is_some_and(|last| last.id() == node.id())
        {
            return false;
        }
        let Some(path) = path_segments(parent, source) else {
            return false;
        };
        if path.len() != 2 {
            return false;
        }
        if path[0] == "Self" {
            let mut ancestor = node.parent();
            while let Some(current) = ancestor {
                if current.kind() == "impl_item" {
                    return self.current.is_some();
                }
                ancestor = current.parent();
            }
            return false;
        }
        self.current
            .as_ref()
            .is_some_and(|evidence| evidence.types.contains(&path[0]))
    }

    fn resolve_qualified(
        &self,
        dir: &Path,
        ref_file: &Path,
        segments: &[String],
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        if let Some(cached) = self.resolved_paths.borrow().get(segments) {
            return cached.clone();
        }
        let resolved = (|| {
            let (package, src, current_modules) = self
                .module_context
                .get_or_init(|| source_module_path(dir, ref_file))
                .as_ref()?;
            let current_file = self.module_file(dir, ref_file, src, current_modules, project)?;
            if current_file != ref_file {
                return None;
            }
            let (first, rest) = segments.split_first()?;
            let mut target = match first.as_str() {
                "crate" => Vec::new(),
                "self" => current_modules.clone(),
                "super" => {
                    let mut parent = current_modules.clone();
                    parent.pop()?;
                    parent
                }
                _ => return None,
            };
            for segment in rest.iter().take(rest.len().saturating_sub(1)) {
                if segment == "super" && first == "super" {
                    target.pop()?;
                } else if segment == "self" {
                    continue;
                } else {
                    target.push(segment.clone());
                }
            }
            let owner = self.module_file(dir, ref_file, src, &target, project)?;
            owner.strip_prefix(package).ok()?;
            Some(owner)
        })();
        self.resolved_paths
            .borrow_mut()
            .insert(segments.to_vec(), resolved.clone());
        resolved
    }

    fn module_file(
        &self,
        dir: &Path,
        current: &Path,
        src: &Path,
        modules: &[String],
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        let lib = src.join("lib.rs");
        let main = src.join("main.rs");
        let root = match (dir.join(&lib).is_file(), dir.join(&main).is_file()) {
            (true, false) => lib,
            (false, true) => main,
            _ => return None,
        };
        let mut parent = root;
        let mut prefix = PathBuf::new();
        for module in modules {
            let evidence = self.evidence(current, &parent, project)?;
            if !evidence.modules.contains(module) {
                return None;
            }
            let base = src.join(&prefix);
            let file = base.join(format!("{module}.rs"));
            let nested = base.join(module).join("mod.rs");
            parent = match (dir.join(&file).is_file(), dir.join(&nested).is_file()) {
                (true, false) => file,
                (false, true) => nested,
                _ => return None,
            };
            prefix.push(module);
        }
        Some(parent)
    }

    fn evidence(
        &self,
        current: &Path,
        file: &Path,
        project: &RustPathOwnerProjectProof,
    ) -> Option<Arc<FileEvidence>> {
        if file == current {
            return self.current.clone();
        }
        if let Some(found) = project
            .files
            .lock()
            .expect("Rust owner file cache mutex poisoned")
            .get(file)
        {
            return found.clone();
        }
        let parsed = (|| {
            let full = project.root.join(file);
            if !full
                .canonicalize()
                .ok()?
                .starts_with(project.canonical_root.as_ref()?)
            {
                return None;
            }
            let source = read_input_file(&full)?;
            let tree = parser::parse_source(&source, LangId::Rust).ok()?;
            FileEvidence::collect(tree.root_node(), &source).map(Arc::new)
        })();
        project
            .files
            .lock()
            .expect("Rust owner file cache mutex poisoned")
            .insert(file.to_owned(), parsed.clone());
        parsed
    }
}

fn scan_ordinary_sources(root: &Path) -> bool {
    scan_sources(root, source_can_retarget_modules)
}

fn scan_private_scope_sources(root: &Path) -> bool {
    scan_sources(root, source_can_retarget_private_scope)
}

fn scan_reexport_sources(root: &Path) -> bool {
    let mut shadowable_names = HashSet::new();
    let mut defined_macros = HashSet::new();
    let mut imports = Vec::new();
    let mut external_glob = false;
    let ordinary = scan_sources(root, |source| {
        let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
            return true;
        };
        let root = tree.root_node();
        if root.has_error() {
            return true;
        }
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            match node.kind() {
                "macro_invocation" => {
                    if !has_function_ancestor(node) {
                        return true;
                    }
                    let name = node
                        .child_by_field_name("macro")
                        .and_then(|name| name.utf8_text(source).ok());
                    if !name.is_some_and(is_allowed_expression_macro) {
                        return true;
                    }
                    shadowable_names.insert(name.unwrap().to_owned());
                }
                "macro_definition" => {
                    let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                    else {
                        return true;
                    };
                    defined_macros.insert(name.to_owned());
                }
                "extern_crate_declaration" => return true,
                "attribute_item" | "inner_attribute_item" => {
                    let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                        return true;
                    };
                    shadowable_names.extend(names);
                }
                "use_declaration" => {
                    let Ok(text) = node.utf8_text(source) else {
                        return true;
                    };
                    let argument = node.child_by_field_name("argument");
                    let anchored = argument
                        .and_then(|value| value.utf8_text(source).ok())
                        .is_some_and(|text| {
                            ["crate::", "self::", "super::"]
                                .iter()
                                .any(|prefix| text.trim_start().starts_with(prefix))
                        });
                    if text.contains('*') && !anchored {
                        external_glob = true;
                    }
                    imports.push(text.to_owned());
                }
                _ => {}
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        false
    });
    ordinary
        && shadowable_names.iter().all(|name| {
            !defined_macros.contains(name)
                && !external_glob
                && !imports.iter().any(|import| {
                    import
                        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                        .any(|token| token == name)
                })
        })
}

fn scan_sources<F>(root: &Path, mut source_is_uncertain: F) -> bool
where
    F: FnMut(&[u8]) -> bool,
{
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join("Cargo.toml").exists() || ancestor.join(".git").exists() {
            return false;
        }
    }
    let Some(manifest) = read_input_text_file(&root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&manifest) else {
        return false;
    };
    if manifest_has_source_paths(&manifest)
        || manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .is_some_and(toml::Value::is_str)
        || root.join("build.rs").exists()
        || root.join("src/mod.rs").exists()
    {
        return false;
    }
    if manifest
        .get("package")
        .and_then(|package| package.get("workspace"))
        .is_some()
        || ["members", "default-members"].iter().any(|key| {
            manifest
                .get("workspace")
                .and_then(|workspace| workspace.get(key))
                .is_some_and(|value| {
                    value.as_array().is_none_or(|members| {
                        members.iter().any(|member| member.as_str() != Some("."))
                    })
                })
        })
    {
        return false;
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let path = entry.path();
            let name = entry.file_name();
            if directory == root.as_path() && matches!(name.to_str(), Some(".git" | "target")) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && directory != root
                && name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                return false;
            } else if kind.is_file()
                && let Some(extension) = path.extension()
            {
                let Some(extension) = extension.to_str() else {
                    return false;
                };
                if !extension.is_ascii() {
                    return false;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    let Some(source) = read_input_file(&path) else {
                        return false;
                    };
                    if source_is_uncertain(&source) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn manifest_has_source_paths(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .any(|(key, value)| key == "path" || manifest_has_source_paths(value)),
        toml::Value::Array(values) => values.iter().any(manifest_has_source_paths),
        _ => false,
    }
}

fn source_can_retarget_modules(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut shadowable_attributes = Vec::new();
    let mut imports = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "macro_invocation" | "macro_definition" | "extern_crate_declaration"
        ) {
            return true;
        }
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
            let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                return true;
            };
            shadowable_attributes.extend(names);
        }
        if node.kind() == "use_declaration" {
            let Ok(text) = node.utf8_text(source) else {
                return true;
            };
            imports.push(text);
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    shadowable_attributes.iter().any(|name| {
        imports
            .iter()
            .any(|import| import.contains('*') || import.contains(name))
    })
}

pub(super) fn is_allowed_expression_macro(name: &str) -> bool {
    matches!(
        name,
        "println"
            | "eprintln"
            | "print"
            | "eprint"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "format"
            | "format_args"
            | "write"
            | "writeln"
            | "matches"
            | "panic"
            | "unreachable"
            | "todo"
            | "unimplemented"
            | "concat"
            | "stringify"
            | "include_str"
            | "include_bytes"
            | "env"
            | "option_env"
            | "file"
            | "line"
            | "column"
            | "module_path"
            | "vec"
            | "dbg"
    )
}

fn source_defines_allowed_macro(source: &[u8]) -> bool {
    if memchr::memmem::find(source, b"macro").is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "macro_definition" {
            let Some(name) = node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            else {
                return true;
            };
            if is_allowed_expression_macro(name) {
                return true;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// Direct-import evidence can cross workspace members, but only when no
/// source in the repository can place the same file in another module.
fn scan_safe_private_import_environment(root: &Path) -> bool {
    scan_safe_source_environment(root, |source| {
        source_defines_allowed_macro(source) || source_may_include_elsewhere(source)
    })
}

fn scan_safe_source_environment(
    root: &Path,
    mut source_is_uncertain: impl FnMut(&[u8]) -> bool,
) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join("Cargo.toml").exists() || ancestor.join(".git").exists() {
            return false;
        }
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            let name = entry.file_name();
            if kind.is_dir() {
                if directory == root && matches!(name.to_str(), Some(".git" | "target")) {
                    continue;
                }
                pending.push(entry.path());
            } else if kind.is_file()
                && name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                let Some(manifest) = read_input_text_file(&entry.path()) else {
                    return false;
                };
                let Ok(manifest) = toml::from_str::<toml::Value>(&manifest) else {
                    return false;
                };
                if manifest_has_source_paths(&manifest) {
                    return false;
                }
            } else if kind.is_file()
                && let Some(extension) = entry.path().extension()
            {
                let Some(extension) = extension.to_str() else {
                    return false;
                };
                if !extension.is_ascii() {
                    return false;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    let Some(source) = read_input_file(&entry.path()) else {
                        return false;
                    };
                    if source_is_uncertain(&source) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn scan_no_source_inclusion(root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join("Cargo.toml").exists() || ancestor.join(".git").exists() {
            return false;
        }
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            let name = entry.file_name();
            if kind.is_dir() {
                if directory == root && matches!(name.to_str(), Some(".git" | "target")) {
                    continue;
                }
                pending.push(entry.path());
            } else if kind.is_file()
                && let Some(extension) = entry.path().extension()
            {
                let Some(extension) = extension.to_str() else {
                    return false;
                };
                if !extension.is_ascii() {
                    return false;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    let Some(source) = read_input_file(&entry.path()) else {
                        return false;
                    };
                    if source_has_include_macro(&source) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn source_has_include_macro(source: &[u8]) -> bool {
    if memchr::memmem::find(source, b"include").is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier"
            && node
                .utf8_text(source)
                .ok()
                .is_some_and(|name| name.strip_prefix("r#").unwrap_or(name) == "include")
            && has_token_tree_ancestor(node)
            && !is_field_name_in_token_tree(node, source)
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && last_macro_identifier(node, source).is_none_or(|name| name == "include")
        {
            return true;
        }
        if node.kind() == "use_declaration"
            && node
                .utf8_text(source)
                .ok()
                .is_none_or(|text| text.contains("include"))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn source_has_external_child_module(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "mod_item" && node.child_by_field_name("body").is_none() {
            return true;
        }
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item")
            && super::rust_std_origin::safe_attribute(node, source).is_none()
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && (!has_function_ancestor(node)
                || !node
                    .child_by_field_name("macro")
                    .and_then(|name| name.utf8_text(source).ok())
                    .is_some_and(is_allowed_expression_macro))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// Ignore ordinary imports here: a glob in a test module cannot relocate a
/// source file. Attributes and item macros can, so uncertain forms reject the
/// owner proof for every target in the repository.
fn source_may_include_elsewhere(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item")
            && super::rust_std_origin::safe_attribute(node, source).is_none()
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && (!node
                .child_by_field_name("macro")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(is_allowed_expression_macro)
                || !has_function_ancestor(node))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// Module privacy only needs to rule out alternate file inclusion. Ordinary
/// expression macros cannot create a sibling module, while item macros can.
fn source_can_retarget_private_scope(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut shadowable_attributes = Vec::new();
    let mut allowed_macros = Vec::new();
    let mut defined_macros = HashSet::new();
    let mut imports = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "macro_invocation" => {
                let mut ancestor = node.parent();
                let mut expression_context = false;
                while let Some(parent) = ancestor {
                    if matches!(parent.kind(), "function_item" | "closure_expression") {
                        expression_context = true;
                        break;
                    }
                    if matches!(parent.kind(), "source_file" | "mod_item") {
                        break;
                    }
                    ancestor = parent.parent();
                }
                if !expression_context {
                    return true;
                }
                // A user macro can expand to local items, including include!.
                // These built-in expression macros do not retarget source files.
                let name = node
                    .child_by_field_name("macro")
                    .and_then(|name| name.utf8_text(source).ok());
                if !name.is_some_and(is_allowed_expression_macro) {
                    return true;
                }
                allowed_macros.push(name.unwrap());
            }
            "macro_definition" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    return true;
                };
                defined_macros.insert(name);
            }
            "extern_crate_declaration" => return true,
            "attribute_item" | "inner_attribute_item" => {
                let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                    return true;
                };
                shadowable_attributes.extend(names);
            }
            "use_declaration" => {
                let Ok(text) = node.utf8_text(source) else {
                    return true;
                };
                imports.push(text);
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    shadowable_attributes
        .iter()
        .map(String::as_str)
        .chain(allowed_macros)
        .any(|name| {
            defined_macros.contains(name)
                || imports
                    .iter()
                    .any(|import| import.contains('*') || import.contains(name))
        })
}

pub(super) fn has_uncertain_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "token_tree" | "attribute_item" | "inner_attribute_item" | "use_declaration"
        ) || (parent.kind() == "mod_item" && parent.child_by_field_name("body").is_some())
        {
            return true;
        }
        node = parent;
    }
    false
}

pub(super) fn has_attribute_prefix(node: Node<'_>) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        if matches!(sibling.kind(), "line_comment" | "block_comment") {
            previous = sibling.prev_named_sibling();
        } else {
            return sibling.kind() == "attribute_item";
        }
    }
    false
}

pub(super) fn scope_can_rebind_name(scope: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut cursor = scope.walk();
    let mut pending = scope.named_children(&mut cursor).collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "macro_invocation" | "use_declaration") {
            return true;
        }
        let possible_shadow = matches!(
            node.kind(),
            "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item"
        );
        if possible_shadow
            && node
                .child_by_field_name("name")
                .and_then(|child| child.utf8_text(source).ok())
                == Some(name)
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

pub(super) fn scope_can_rebind_import_name(scope: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut cursor = scope.walk();
    let mut pending = scope.named_children(&mut cursor).collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "macro_invocation" | "use_declaration") {
            return true;
        }
        if matches!(
            node.kind(),
            "let_declaration"
                | "let_condition"
                | "let_chain"
                | "parameters"
                | "closure_parameters"
                | "match_arm"
                | "for_expression"
        ) && node
            .utf8_text(source)
            .ok()
            .is_none_or(|text| text.contains(name))
        {
            return true;
        }
        if matches!(
            node.kind(),
            "function_item"
                | "const_item"
                | "static_item"
                | "type_item"
                | "struct_item"
                | "enum_item"
        ) && node
            .child_by_field_name("name")
            .and_then(|child| child.utf8_text(source).ok())
            == Some(name)
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

pub(super) fn path_segments(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    // `a::b::c` は `path` 側へ入れ子になる。段数だけ再帰しないよう、`path` を先頭まで
    // たどって末尾側から名前を集め、最後に並べ直す。
    let mut reversed = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                reversed.push(current.utf8_text(source).ok()?.to_owned());
                break;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let path = current.child_by_field_name("path")?;
                reversed.push(
                    current
                        .child_by_field_name("name")?
                        .utf8_text(source)
                        .ok()?
                        .to_owned(),
                );
                current = path;
            }
            _ => return None,
        }
    }
    reversed.reverse();
    Some(reversed)
}

fn plain_crate_import_path(text: &str) -> Option<Vec<String>> {
    let path = text.trim().strip_prefix("use ")?.strip_suffix(';')?.trim();
    let parts = path.split("::").collect::<Vec<_>>();
    if parts.len() < 3 || parts[0] != "crate" {
        return None;
    }
    if parts.iter().any(|part| {
        part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }) {
        return None;
    }
    Some(parts.into_iter().map(str::to_owned).collect())
}

pub(super) fn source_module_path(
    dir: &Path,
    ref_file: &Path,
) -> Option<(PathBuf, PathBuf, Vec<String>)> {
    source_module_path_inner(dir, ref_file, false)
}

fn source_module_path_inner(
    dir: &Path,
    ref_file: &Path,
    allow_nested_root_names: bool,
) -> Option<(PathBuf, PathBuf, Vec<String>)> {
    let package = ref_file
        .ancestors()
        .find(|ancestor| dir.join(ancestor).join("Cargo.toml").is_file())?;
    let manifest = read_input_text_file(&dir.join(package).join("Cargo.toml"))?;
    let manifest_value = toml::from_str::<toml::Value>(&manifest).ok()?;
    let edition = manifest_value.get("package")?.get("edition")?.as_str()?;
    if !matches!(edition, "2018" | "2021" | "2024") {
        return None;
    }
    let targets = RustTargets::parse(&manifest, package)?;
    if !targets.has_package || !targets.roots_complete || targets.has_explicit_roots() {
        return None;
    }
    let src = package.join("src");
    let relative = ref_file.strip_prefix(&src).ok()?;
    let parts = relative
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    let (folders, file) = parts.split_last().map(|(file, folders)| (folders, *file))?;
    if folders.contains(&"bin") {
        return None;
    }
    let mut modules = folders
        .iter()
        .map(|part| (*part).to_owned())
        .collect::<Vec<_>>();
    if file != "mod.rs"
        && ((!matches!(file, "lib.rs" | "main.rs"))
            || (allow_nested_root_names && !folders.is_empty()))
    {
        modules.push(file.strip_suffix(".rs")?.to_owned());
    } else if matches!(file, "lib.rs" | "main.rs") && !folders.is_empty() {
        return None;
    }
    Some((package.to_owned(), src, modules))
}

/// Follow ordinary `mod name;` declarations without the owner resolver's
/// restrictions on unrelated imports, attributes, and expression macros.
pub(super) fn private_module_file(
    dir: &Path,
    src: &Path,
    modules: &[String],
    verify_syntax: bool,
) -> Option<PathBuf> {
    private_module_file_from_roots(
        dir,
        src,
        modules,
        verify_syntax,
        false,
        [src.join("lib.rs"), src.join("main.rs")],
    )
}

fn private_module_file_from_roots(
    dir: &Path,
    src: &Path,
    modules: &[String],
    verify_syntax: bool,
    normalize_raw_names: bool,
    roots: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let mut resolved = None;
    for root in roots {
        if !dir.join(&root).is_file() {
            continue;
        }
        let mut parent = root;
        let mut prefix = PathBuf::new();
        let mut valid = true;
        for module in modules {
            let source = read_input_file(&dir.join(&parent))?;
            if verify_syntax && source_can_retarget_private_scope(&source) {
                return None;
            }
            let tree = parser::parse_source(&source, LangId::Rust).ok()?;
            let node = tree.root_node();
            if node.has_error() {
                return None;
            }
            let mut cursor = node.walk();
            let declarations = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "mod_item")
                .filter(|child| {
                    child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(&source).ok())
                        .map(|name| {
                            if normalize_raw_names {
                                name.strip_prefix("r#").unwrap_or(name)
                            } else {
                                name
                            }
                        })
                        == Some(module.as_str())
                        && child.child_by_field_name("body").is_none()
                })
                .count();
            if declarations != 1 {
                valid = false;
                break;
            }
            let base = src.join(&prefix);
            let file = base.join(format!("{module}.rs"));
            let nested = base.join(module).join("mod.rs");
            parent = match (dir.join(&file).is_file(), dir.join(&nested).is_file()) {
                (true, false) => file,
                (false, true) => nested,
                _ => return None,
            };
            prefix.push(module);
        }
        if valid {
            match &resolved {
                Some(previous) if previous != &parent => return None,
                _ => resolved = Some(parent),
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::{
        FileEvidence, RustPathOwnerIndex, RustPathOwnerProjectProof, scan_ordinary_sources,
        scan_safe_private_import_environment, source_can_retarget_modules,
        source_can_retarget_private_scope, source_defines_allowed_macro,
        source_has_external_child_module, source_has_include_macro, source_may_include_elsewhere,
    };
    use crate::engine::parser;
    use crate::language::LangId;

    /// プロジェクト単位の証明は Rust ソースを全部読む。100MB の入力上限を超えるファイル
    /// (gitignore 済みの生成物でも `read_dir` で見つかる) は読まず、内容を確かめられない
    /// として証明を止める。旧実装は上限なしで読み、1 ファイルでメモリが際限なく増えていた。
    #[test]
    fn project_scan_does_not_read_sources_over_the_input_limit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join("out")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let generated = root.path().join("out/generated.rs");
        std::fs::write(&generated, "pub fn generated() {}\n").unwrap();
        // 対照: 上限以内のファイルは従来どおり読んで判定する。
        assert!(scan_ordinary_sources(root.path()));
        // ディスクを消費しない疎ファイルで上限を 1 byte 超えさせる。
        std::fs::OpenOptions::new()
            .write(true)
            .open(&generated)
            .and_then(|file| file.set_len(crate::commands::MAX_INPUT_SIZE as u64 + 1))
            .unwrap();
        assert!(!scan_ordinary_sources(root.path()));
    }

    #[test]
    fn type_alias_does_not_prove_an_independent_type_owner() {
        let source = b"type Alias = crate::origin::Alias;\nstruct Local;\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let evidence = FileEvidence::collect(tree.root_node(), source).unwrap();
        assert!(!evidence.types.contains("Alias"));
        assert!(evidence.types.contains("Local"));
    }

    #[test]
    fn imported_attribute_name_blocks_module_owner_proof() {
        assert!(source_can_retarget_modules(
            b"use macros::test;\n#[test]\nfn sample() {}\n"
        ));
        assert!(source_can_retarget_modules(
            b"use macros::Debug;\n#[derive(Debug)]\nstruct Sample;\n"
        ));
        assert!(source_can_retarget_modules(
            b"use macros::derive;\n#[derive(Debug)]\nstruct Sample;\n"
        ));
        assert!(!source_can_retarget_modules(
            b"#[derive(Debug)]\nstruct Sample;\n"
        ));
    }

    #[test]
    fn private_scope_allows_standard_expression_macros_but_not_shadowed_macros() {
        assert!(!source_can_retarget_private_scope(
            b"#[cfg(test)] mod tests;\nfn call() { println!(\"x\"); assert!(true); }\n"
        ));
        assert!(source_can_retarget_private_scope(
            b"use custom::println;\nfn call() { println!(\"x\"); }\n"
        ));
        assert!(source_can_retarget_private_scope(
            b"macro_rules! println { () => {} }\nfn call() { println!(); }\n"
        ));
        assert!(source_can_retarget_private_scope(b"make_modules!();\n"));
        assert!(source_defines_allowed_macro(
            b"macro_rules! dbg { () => {} }\n"
        ));
        assert!(!source_defines_allowed_macro(
            b"macro_rules! local { () => {} }\n"
        ));
    }

    #[test]
    fn private_scope_rejects_builtin_macro_shadowed_in_another_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "macro_rules! dbg { ($value:expr) => { $value }; }\nmod child;\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/child.rs"), "fn call() { dbg!(1); }\n").unwrap();
        let proof = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert!(!proof.has_safe_private_import_environment());
        assert!(!proof.allows_private_scope_paths());
    }

    #[test]
    fn private_import_environment_rejects_other_target_inclusions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/bin")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "#[cfg(test)] mod tests { use super::*; }\nmod a;\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn helper() {}\n").unwrap();
        assert!(scan_safe_private_import_environment(root.path()));
        std::fs::write(root.path().join("src/inc.rſ"), "include!(\"a.rs\");\n").unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::remove_file(root.path().join("src/inc.rſ")).unwrap();
        std::fs::create_dir_all(root.path().join("other")).unwrap();
        std::fs::write(
            root.path().join("other/cargo.toml"),
            "[lib]\npath = \"a.rs\"\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::remove_file(root.path().join("other/cargo.toml")).unwrap();
        assert!(!scan_safe_private_import_environment(
            &root.path().join("src")
        ));

        std::fs::write(
            root.path().join("src/bin/tool.rs"),
            "#[path = \"../a.rs\"] mod util;\nfn main() {}\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::write(
            root.path().join("src/bin/tool.rs"),
            "include!(\"../a.rs\");\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::write(
            root.path().join("src/bin/tool.rs"),
            "std /*a*/ :: /*b*/ include!{\"../a.rs\"}\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::remove_file(root.path().join("src/bin/tool.rs")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"src/a.rs\"\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
    }

    #[test]
    fn direct_import_index_requires_an_unconditional_unique_binding() {
        for source in [
            "#[cfg(test)] use crate::util::helper;\nfn call() { helper(); }\n",
            "use crate::util::helper;\nuse super::helper;\nfn call() { helper(); }\n",
            "use crate::util::helper;\nuse super::*;\nfn call() { helper(); }\n",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let index = RustPathOwnerIndex::build(tree.root_node(), source.as_bytes());
            assert!(!index.direct_imports.contains_key("helper"), "{source}");
        }
        let source = b"use crate::util::helper;\nfn call() { helper(); }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustPathOwnerIndex::build(tree.root_node(), source);
        assert!(index.direct_imports.contains_key("helper"));
        let source = b"type Alias = super::Alias;\nstruct Local;\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustPathOwnerIndex::build(tree.root_node(), source);
        assert!(!index.local_type_names.contains("Alias"));
        assert!(index.local_type_names.contains("Local"));
        assert!(!source_may_include_elsewhere(
            b"#[cfg(test)] mod tests { use super::*; }\n"
        ));
        assert!(!source_may_include_elsewhere(
            b"fn helper(value: u8) -> u8 { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n"
        ));
    }

    #[test]
    fn direct_local_function_owns_plain_calls_despite_body_macros() {
        let source = b"fn helper() -> u8 { println!(\"x\"); 1 }\nfn call() -> u8 { helper() }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustPathOwnerIndex::build(tree.root_node(), source);
        let root = tree.root_node();
        let mut pending = vec![root];
        let mut call = None;
        while let Some(node) = pending.pop() {
            if node.kind() == "identifier"
                && node.utf8_text(source).ok() == Some("helper")
                && node.start_position().row == 1
            {
                call = Some(node);
                break;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        assert!(index.plain_local_function_owner(call.unwrap(), source, "helper"));
    }

    #[test]
    fn private_leaf_proof_keeps_external_child_and_inclusion_barriers() {
        assert!(!source_has_external_child_module(
            b"fn helper() {}\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(1, 1); } }\n"
        ));
        assert!(source_has_external_child_module(
            b"fn helper() {}\nmod child;\n"
        ));
        assert!(source_has_external_child_module(
            b"fn helper() {}\nmake_child!();\n"
        ));
        assert!(source_has_include_macro(b"include!(\"a.rs\");\n"));
        assert!(source_has_include_macro(
            b"cfg_if::cfg_if! { if #[cfg(unix)] { include!(\"a.rs\"); } }\n"
        ));
        assert!(source_has_include_macro(
            b"macro_rules! inc { () => { include!(\"a.rs\"); } }\ninc!();\n"
        ));
        assert!(source_has_include_macro(b"use std::include as load;\n"));
        assert!(source_has_include_macro(b"std :: include!(\"a.rs\");\n"));
        assert!(source_has_include_macro(
            b"std /*a*/ :: /*b*/ include!{\"a.rs\"}\n"
        ));
        assert!(source_has_include_macro(b"include /*c*/ ! [\"a.rs\"];\n"));
        assert!(source_has_include_macro(b"r#include!(\"a.rs\");\n"));
        assert!(source_has_include_macro(
            b"macro_rules! inc { () => { r#include!(\"a.rs\"); } }\n"
        ));
        assert!(!source_has_include_macro(
            b"std :: include_str!(\"a.rs\");\n"
        ));
        assert!(!source_has_include_macro(b"my::include_all!(\"a.rs\");\n"));
        assert!(!source_has_include_macro(
            b"fn f() { println!(\"{}\", args.include); }\n"
        ));
        assert!(source_has_include_macro(
            b"macro_rules! m { () => { x . include!(\"a.rs\"); } }\n"
        ));
        assert!(!source_has_include_macro(
            b"fn helper() { println!(\"include! in text\"); }\n"
        ));
    }

    #[test]
    fn private_leaf_proof_rejects_commented_include_in_another_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "mod a; mod b;\n").unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn helper() {}\n").unwrap();
        std::fs::write(root.path().join("src/b.rs"), "fn call() {}\n").unwrap();
        let plain = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert!(plain.private_source_is_leaf("src/a.rs"));

        std::fs::write(
            root.path().join("src/b.rs"),
            "std /*a*/ :: /*b*/ include!{\"a.rs\"}\n",
        )
        .unwrap();
        let included = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert!(!included.private_source_is_leaf("src/a.rs"));
    }

    #[test]
    fn source_scan_rejects_case_folded_extensions_and_skipped_target_members() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        for extension in ["RS", "rſ"] {
            let path = root.path().join("src").join(format!("inc.{extension}"));
            std::fs::write(&path, "#[path = \"other.rs\"] mod child;\n").unwrap();
            assert!(!scan_ordinary_sources(root.path()), "{extension}");
            std::fs::remove_file(path).unwrap();
        }
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"TARGET/member\"]\n",
        )
        .unwrap();
        assert!(!scan_ordinary_sources(root.path()));
    }

    #[test]
    fn source_scan_rejects_other_cargo_roots_and_implicit_build_targets() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(scan_ordinary_sources(root.path()));

        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/Cargo.toml"), "[package]\n").unwrap();
        assert!(!scan_ordinary_sources(root.path()));
        std::fs::remove_file(root.path().join("nested/Cargo.toml")).unwrap();

        std::fs::write(root.path().join("build.rs"), "fn main() {}\n").unwrap();
        assert!(!scan_ordinary_sources(root.path()));
        std::fs::remove_file(root.path().join("build.rs")).unwrap();

        std::fs::write(root.path().join("src/mod.rs"), "mod other;\n").unwrap();
        assert!(!scan_ordinary_sources(root.path()));
        std::fs::remove_file(root.path().join("src/mod.rs")).unwrap();

        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\n[dependencies]\nother = { path = \"target/other\" }\n",
        )
        .unwrap();
        assert!(!scan_ordinary_sources(root.path()));

        std::fs::create_dir(root.path().join("child")).unwrap();
        std::fs::write(
            root.path().join("child/Cargo.toml"),
            "[package]\nname = \"child\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(!scan_ordinary_sources(&root.path().join("child")));
    }

    /// 修正前の再帰版 (浅い入力で反復版と比べるためだけに残す)。
    fn path_segments_recursive(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<Vec<String>> {
        match node.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                Some(vec![node.utf8_text(source).ok()?.to_owned()])
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut segments =
                    path_segments_recursive(node.child_by_field_name("path")?, source)?;
                segments.push(
                    node.child_by_field_name("name")?
                        .utf8_text(source)
                        .ok()?
                        .to_owned(),
                );
                Some(segments)
            }
            _ => None,
        }
    }

    /// 反復に書き換えた path の分解が、浅い入力の全ノードで修正前の再帰版と同じ結果を返す
    /// (`path` の無い `::name` や式・ジェネリクスのように分解できない形を含む)。
    #[test]
    fn iterative_path_segments_match_recursive_version() {
        let source = b"use crate::a::b::{c, d::e};\nfn f(x: self::G<super::H>, y: ::std::ops::Range<u8>, z: <T as Tr>::C) -> crate::m::N { super::super::g::h(x); a::b::<u8>::c(); }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let mut cursor = tree.root_node().walk();
        let mut nodes = vec![tree.root_node()];
        let mut next = 0;
        let mut resolved = 0;
        while next < nodes.len() {
            let node = nodes[next];
            nodes.extend(node.children(&mut cursor));
            next += 1;
            let iterative = super::path_segments(node, source);
            assert_eq!(
                iterative,
                path_segments_recursive(node, source),
                "{}: {}",
                node.kind(),
                node.utf8_text(source).unwrap()
            );
            resolved += usize::from(iterative.is_some_and(|segments| segments.len() > 1));
        }
        assert!(resolved >= 8, "修飾パスを含む: {resolved}");
    }
}
