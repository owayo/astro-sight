//! Conservative Rust owner resolution through stable import and re-export chains.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tree_sitter::Node;

use crate::engine::bounded_read::{read_input_file, read_input_text_file};
use crate::engine::parser;
use crate::engine::rust_use_tree::{RustPubUseEdge, expand_rust_use_tree_edges_ast};
use crate::language::LangId;

use super::rust_path_owner::{
    RustPathOwnerProjectProof, has_attribute_prefix, has_uncertain_ancestor,
    is_allowed_expression_macro, path_segments, private_module_file, scope_can_rebind_import_name,
    scope_can_rebind_name, source_module_path,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Namespace {
    Type,
    Function,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Visibility {
    Crate,
    Private,
}

impl Visibility {
    fn allows(self, owner: &[String], caller: &[String]) -> bool {
        matches!(self, Self::Crate) || caller.starts_with(owner)
    }
}

#[derive(Eq, PartialEq)]
struct NamedImport {
    name: String,
    target_module: Vec<String>,
    target_name: String,
    visibility: Visibility,
}

#[derive(Eq, PartialEq)]
struct GlobImport {
    target_module: Vec<String>,
    visibility: Visibility,
}

#[derive(Eq, PartialEq)]
pub(super) struct ModuleBindings {
    types: HashMap<String, Visibility>,
    functions: HashMap<String, Visibility>,
    unknown_types: HashSet<String>,
    unknown_functions: HashSet<String>,
    named: Vec<NamedImport>,
    globs: Vec<GlobImport>,
    modules: Vec<(String, Visibility)>,
    opaque: bool,
}

pub(super) fn owner(
    node: Node<'_>,
    source: &[u8],
    dir: &Path,
    ref_path: &str,
    project: &RustPathOwnerProjectProof,
    namespace: Namespace,
) -> Option<PathBuf> {
    if !project.allows_reexport_paths() || !matches!(node.kind(), "identifier" | "type_identifier")
    {
        return None;
    }
    let name = node.utf8_text(source).ok()?;
    let ref_file = Path::new(ref_path);
    if project.reexport_old_sources.contains_key(ref_file) {
        return None;
    }
    let (_package, src, current_module) = cached_source_module_path(dir, ref_file, project)?;
    if cached_module_file(dir, &src, &current_module, project)? != ref_file {
        return None;
    }
    let mut ancestor = node.parent();
    let mut use_statement = None;
    while let Some(parent) = ancestor {
        if parent.kind() == "use_declaration" {
            if parent
                .parent()
                .is_none_or(|root| root.kind() != "source_file")
                || has_attribute_prefix(parent)
            {
                return None;
            }
            use_statement = Some(parent);
            break;
        }
        ancestor = parent.parent();
    }
    if use_statement.is_none()
        && !matches!(
            (namespace, node.kind()),
            (Namespace::Type, "type_identifier") | (Namespace::Function, "identifier")
        )
    {
        return None;
    }
    let (target_module, target_name) = if let Some(statement) = use_statement {
        let argument = statement.child_by_field_name("argument")?;
        let edges = parse_use_edges(argument, source, &current_module)?;
        let mut matching = edges.iter().filter_map(|edge| match edge {
            RustPubUseEdge::Named {
                exported_name,
                target_module,
                target_item,
                ..
            } if target_item == name || exported_name == name => {
                Some((target_module.clone(), target_item.clone()))
            }
            _ => None,
        });
        let target = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        target
    } else {
        if has_uncertain_ancestor(node) {
            return None;
        }
        let mut scope = node.parent();
        while let Some(parent) = scope {
            if namespace == Namespace::Type
                && matches!(
                    parent.kind(),
                    "function_item" | "impl_item" | "trait_item" | "struct_item" | "enum_item"
                )
                && parent
                    .child_by_field_name("type_parameters")
                    .and_then(|params| params.utf8_text(source).ok())
                    .is_some_and(|params| params.contains(name))
            {
                return None;
            }
            if matches!(
                parent.kind(),
                "function_item" | "closure_expression" | "block"
            ) && (has_attribute_prefix(parent)
                || match namespace {
                    Namespace::Type => scope_can_rebind_name(parent, source, name),
                    Namespace::Function => scope_can_rebind_import_name(parent, source, name),
                })
            {
                return None;
            }
            scope = parent.parent();
        }
        if let Some(parent) = node.parent().filter(|parent| {
            matches!(
                parent.kind(),
                "scoped_type_identifier" | "scoped_identifier"
            ) && parent
                .child_by_field_name("name")
                .is_some_and(|field| field.id() == node.id())
        }) {
            qualified_target(path_segments(parent, source)?, &current_module, name)?
        } else {
            (current_module.clone(), name.to_owned())
        }
    };
    let mut seen = HashSet::new();
    resolve_name(
        dir,
        &src,
        &target_module,
        &target_name,
        namespace,
        &current_module,
        project,
        &mut seen,
        0,
    )
}

pub(super) fn alias_import_used(
    node: Node<'_>,
    source: &[u8],
    dir: &Path,
    ref_path: &str,
    project: &RustPathOwnerProjectProof,
) -> bool {
    let mut ancestor = node.parent();
    let mut alias = None;
    let mut statement = None;
    while let Some(parent) = ancestor {
        if parent.kind() == "use_as_clause" {
            let mut cursor = parent.walk();
            let children = parent.named_children(&mut cursor).collect::<Vec<_>>();
            if children.len() == 2
                && children[0].start_byte() <= node.start_byte()
                && node.end_byte() <= children[0].end_byte()
            {
                alias = children[1].utf8_text(source).ok();
            }
        }
        if parent.kind() == "use_declaration" {
            statement = Some(parent);
            break;
        }
        ancestor = parent.parent();
    }
    let (Some(alias), Some(statement)) = (alias, statement) else {
        return false;
    };
    if alias == "_" || alias == node.utf8_text(source).unwrap_or("") {
        return false;
    }
    if proven_external_alias(statement, source, dir, ref_path, project) {
        return false;
    }
    if has_any_pub_visibility(statement, source) {
        return true;
    }
    let mut pending = vec![statement.parent().unwrap_or(statement)];
    while let Some(current) = pending.pop() {
        if current.id() == statement.id() {
            continue;
        }
        if current.kind() == "mod_item" && current.child_by_field_name("body").is_none() {
            return true;
        }
        if current.kind() == "macro_invocation"
            && !current
                .child_by_field_name("macro")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(is_allowed_expression_macro)
        {
            return true;
        }
        if matches!(current.kind(), "identifier" | "type_identifier")
            && current.utf8_text(source).ok() == Some(alias)
        {
            return true;
        }
        // 属性の文字列 (`#[serde(skip_serializing_if = "is_zero")]` など) も別名の使用箇所
        // (refs は文字列由来の参照として数える)。別名の使用は元名の参照として検索に出ない
        // ので、ここで見落とすと元名の import 行が情報提供に落ち、blocking がどこにも
        // 出なくなる。属性ごとのキーまでは見ず、属性内の文字列に別名が識別子として現れれば
        // 使用とみなす (保守側)。
        if current.kind() == "string_content"
            && string_mentions_identifier(current, source, alias)
            && has_attribute_ancestor(current)
        {
            return true;
        }
        let mut cursor = current.walk();
        pending.extend(current.named_children(&mut cursor));
    }
    false
}

/// 文字列の中身を識別子の区切りで分けたとき、`name` (`r#` は外して比べる) が現れるか。
fn string_mentions_identifier(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let name = name.strip_prefix("r#").unwrap_or(name);
    node.utf8_text(source).is_ok_and(|text| {
        text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|token| token == name)
    })
}

/// 属性 (`#[...]` / `#![...]`) の中にあるノードか。
fn has_attribute_ancestor(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        match parent.kind() {
            "attribute_item" | "inner_attribute_item" => return true,
            "source_file" | "declaration_list" | "block" => return false,
            _ => ancestor = parent.parent(),
        }
    }
    false
}

fn proven_external_alias(
    statement: Node<'_>,
    source: &[u8],
    dir: &Path,
    ref_path: &str,
    project: &RustPathOwnerProjectProof,
) -> bool {
    let proof = || -> Option<()> {
        if !project.allows_external_alias_paths()
            || project
                .reexport_old_sources
                .contains_key(Path::new(ref_path))
        {
            return None;
        }
        let argument = statement.child_by_field_name("argument")?;
        let alias_clause = argument.kind().eq("use_as_clause").then_some(argument)?;
        let target = alias_clause.named_child(0)?;
        let segments = path_segments(target, source)?;
        let first = segments.first()?.as_str();
        if segments.len() < 2 || matches!(first, "crate" | "self" | "super") {
            return None;
        }
        let (_package, src, _module) =
            cached_source_module_path(dir, Path::new(ref_path), project)?;
        let manifest = read_input_text_file(&dir.join(src.parent()?.join("Cargo.toml")))?;
        let manifest: toml::Value = toml::from_str(&manifest).ok()?;
        let edition = manifest.get("package")?.get("edition")?.as_str()?;
        if !matches!(edition, "2018" | "2021" | "2024") {
            return None;
        }
        let package_name = manifest.get("package")?.get("name")?.as_str()?;
        let lib_name = manifest
            .get("lib")
            .and_then(|lib| lib.get("name"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| package_name.replace('-', "_"));
        if first == lib_name {
            return None;
        }
        if !matches!(first, "std" | "core" | "alloc") {
            if manifest.get("patch").is_some() || manifest.get("replace").is_some() {
                return None;
            }
            // 祖先のワークスペースの `[patch]` と、Cargo が読む設定 (祖先と `$CARGO_HOME`) による
            // 依存の取得元・コンパイラの差し替えを除く。
            let package_root =
                super::rust_cargo_config::absolute_package_root(&dir.join(src.parent()?))?;
            let cargo = &project.cargo_configs;
            if cargo
                .ancestors(&package_root)
                .skip(1)
                .any(|parent| parent.join("Cargo.toml").exists())
                || !cargo.keeps_dependency_sources(&package_root)
            {
                return None;
            }
            let dependency = ["dependencies", "dev-dependencies", "build-dependencies"]
                .iter()
                .find_map(|table| manifest.get(table).and_then(|deps| deps.get(first)))?;
            let registry_version = dependency.is_str()
                || dependency.as_table().is_some_and(|table| {
                    table.get("version").is_some_and(toml::Value::is_str)
                        && !["path", "workspace", "git"]
                            .iter()
                            .any(|key| table.contains_key(*key))
                });
            if !registry_version {
                return None;
            }
        }
        let root_file = cached_module_file(dir, &src, &[], project)?;
        if project.reexport_old_sources.contains_key(&root_file) {
            return None;
        }
        let root_source = read_input_file(&dir.join(&root_file))?;
        let tree = parser::parse_source(&root_source, LangId::Rust).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let mut cursor = root.walk();
        for item in root.named_children(&mut cursor) {
            match item.kind() {
                "attribute_item" | "inner_attribute_item" => {
                    super::rust_std_origin::safe_attribute(item, &root_source)?;
                }
                "macro_invocation" | "extern_crate_declaration" => return None,
                "use_declaration" => {
                    let argument = item.child_by_field_name("argument")?;
                    let edges = expand_use_edges(argument, &root_source, &[])?;
                    if edges.iter().any(|edge| match edge {
                        RustPubUseEdge::Named { exported_name, .. } => exported_name == first,
                        RustPubUseEdge::Wildcard { .. } => true,
                    }) {
                        return None;
                    }
                }
                _ => {
                    if item
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(&root_source).ok())
                        == Some(first)
                    {
                        return None;
                    }
                }
            }
        }
        Some(())
    };
    proof().is_some()
}

fn qualified_target(
    segments: Vec<String>,
    current: &[String],
    name: &str,
) -> Option<(Vec<String>, String)> {
    let (first, rest) = segments.split_first()?;
    if rest.last().map(String::as_str) != Some(name) {
        return None;
    }
    let mut target = match first.as_str() {
        "crate" => Vec::new(),
        "self" => current.to_vec(),
        "super" => {
            let mut parent = current.to_vec();
            parent.pop()?;
            parent
        }
        _ => return None,
    };
    for segment in rest.iter().take(rest.len().saturating_sub(1)) {
        match segment.as_str() {
            "super" if first == "super" => {
                target.pop()?;
            }
            "self" => {}
            "crate" | "super" => return None,
            _ => target.push(segment.clone()),
        }
    }
    Some((target, name.to_owned()))
}

#[allow(clippy::too_many_arguments)]
fn resolve_name(
    dir: &Path,
    src: &Path,
    module: &[String],
    name: &str,
    namespace: Namespace,
    caller: &[String],
    project: &RustPathOwnerProjectProof,
    seen: &mut HashSet<(Vec<String>, String, Namespace)>,
    depth: usize,
) -> Option<PathBuf> {
    if depth >= 64 || !seen.insert((module.to_vec(), name.to_owned(), namespace)) {
        return None;
    }
    let file = cached_module_file(dir, src, module, project)?;
    stable_module_chain(dir, src, module, project)?;
    let bindings = load_bindings(dir, &file, module, project)?;
    if bindings.opaque {
        return None;
    }
    let (declarations, unknown) = match namespace {
        Namespace::Type => (&bindings.types, &bindings.unknown_types),
        Namespace::Function => (&bindings.functions, &bindings.unknown_functions),
    };
    if unknown.contains(name) {
        return None;
    }
    let imports: Vec<_> = bindings
        .named
        .iter()
        .filter(|edge| edge.name == name)
        .collect();
    if declarations.contains_key(name) && !imports.is_empty() {
        return None;
    }
    if let Some(visibility) = declarations.get(name) {
        return visibility.allows(module, caller).then_some(file);
    }
    if imports.len() > 1 {
        return None;
    }
    if let Some(edge) = imports.first() {
        if !edge.visibility.allows(module, caller) {
            return None;
        }
        return resolve_name(
            dir,
            src,
            &edge.target_module,
            &edge.target_name,
            namespace,
            caller,
            project,
            seen,
            depth + 1,
        );
    }
    let mut owner = None;
    for edge in &bindings.globs {
        if !edge.visibility.allows(module, caller) {
            return None;
        }
        let next = resolve_name(
            dir,
            src,
            &edge.target_module,
            name,
            namespace,
            caller,
            project,
            seen,
            depth + 1,
        )?;
        if owner.as_ref().is_some_and(|previous| previous != &next) {
            return None;
        }
        owner = Some(next);
    }
    owner
}

fn load_bindings(
    dir: &Path,
    file: &Path,
    module: &[String],
    project: &RustPathOwnerProjectProof,
) -> Option<Arc<ModuleBindings>> {
    if let Some(cached) = project
        .reexport_files
        .lock()
        .expect("Rust re-export cache mutex poisoned")
        .get(file)
    {
        return cached.clone();
    }
    let parsed = (|| {
        let full = dir.join(file);
        if !full
            .canonicalize()
            .ok()?
            .starts_with(project.canonical_root.as_ref()?)
        {
            return None;
        }
        let source = read_input_file(&full)?;
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        let bindings = parse_bindings(tree.root_node(), &source, module)?;
        if let Some(old_source) = project.reexport_old_sources.get(file) {
            let old_source = old_source.as_ref()?;
            let old_tree = parser::parse_source(old_source, LangId::Rust).ok()?;
            let old_bindings = parse_bindings(old_tree.root_node(), old_source, module)?;
            if old_bindings != bindings {
                return None;
            }
        }
        Some(Arc::new(bindings))
    })();
    project
        .reexport_files
        .lock()
        .expect("Rust re-export cache mutex poisoned")
        .insert(file.to_owned(), parsed.clone());
    parsed
}

fn stable_module_chain(
    dir: &Path,
    src: &Path,
    module: &[String],
    project: &RustPathOwnerProjectProof,
) -> Option<()> {
    let key = (src.to_owned(), module.to_vec());
    if let Some(stable) = project
        .reexport_stable_chains
        .lock()
        .expect("Rust re-export chain cache mutex poisoned")
        .get(&key)
    {
        return stable.then_some(());
    }
    let result = stable_module_chain_uncached(dir, src, module, project).is_some();
    project
        .reexport_stable_chains
        .lock()
        .expect("Rust re-export chain cache mutex poisoned")
        .insert(key, result);
    result.then_some(())
}

fn stable_module_chain_uncached(
    dir: &Path,
    src: &Path,
    module: &[String],
    project: &RustPathOwnerProjectProof,
) -> Option<()> {
    for (depth, child_name) in module.iter().enumerate() {
        let parent_file = cached_module_file(dir, src, &module[..depth], project)?;
        let Some(old_source) = project.reexport_old_sources.get(&parent_file) else {
            continue;
        };
        let old_source = old_source.as_ref()?;
        let new_source = read_input_file(&dir.join(&parent_file))?;
        let old_tree = parser::parse_source(old_source, LangId::Rust).ok()?;
        let new_tree = parser::parse_source(&new_source, LangId::Rust).ok()?;
        let old = parse_bindings(old_tree.root_node(), old_source, &module[..depth])?;
        let new = parse_bindings(new_tree.root_node(), &new_source, &module[..depth])?;
        if old.opaque || new.opaque {
            return None;
        }
        let old_declarations = old
            .modules
            .iter()
            .filter(|(name, _)| name == child_name)
            .collect::<Vec<_>>();
        let new_declarations = new
            .modules
            .iter()
            .filter(|(name, _)| name == child_name)
            .collect::<Vec<_>>();
        if old_declarations.len() != 1 || old_declarations != new_declarations {
            return None;
        }
    }
    Some(())
}

fn cached_source_module_path(
    dir: &Path,
    file: &Path,
    project: &RustPathOwnerProjectProof,
) -> Option<(PathBuf, PathBuf, Vec<String>)> {
    if let Some(cached) = project
        .reexport_source_modules
        .lock()
        .expect("Rust source module cache mutex poisoned")
        .get(file)
    {
        return cached.clone();
    }
    let result = source_module_path(dir, file);
    project
        .reexport_source_modules
        .lock()
        .expect("Rust source module cache mutex poisoned")
        .insert(file.to_owned(), result.clone());
    result
}

fn cached_module_file(
    dir: &Path,
    src: &Path,
    module: &[String],
    project: &RustPathOwnerProjectProof,
) -> Option<PathBuf> {
    let key = (src.to_owned(), module.to_vec());
    if let Some(cached) = project
        .reexport_module_files
        .lock()
        .expect("Rust module file cache mutex poisoned")
        .get(&key)
    {
        return cached.clone();
    }
    let result = private_module_file(dir, src, module, false);
    project
        .reexport_module_files
        .lock()
        .expect("Rust module file cache mutex poisoned")
        .insert(key, result.clone());
    result
}

fn parse_bindings(root: Node<'_>, source: &[u8], module: &[String]) -> Option<ModuleBindings> {
    if root.kind() != "source_file" || root.has_error() {
        return None;
    }
    let mut bindings = ModuleBindings {
        types: HashMap::new(),
        functions: HashMap::new(),
        unknown_types: HashSet::new(),
        unknown_functions: HashSet::new(),
        named: Vec::new(),
        globs: Vec::new(),
        modules: Vec::new(),
        opaque: false,
    };
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "inner_attribute_item" {
            let name = child
                .named_child(0)
                .and_then(|value| value.named_child(0))
                .and_then(|value| value.utf8_text(source).ok());
            if super::rust_std_origin::safe_attribute(child, source).is_none()
                || name == Some("cfg")
            {
                bindings.opaque = true;
            }
            continue;
        }
        if matches!(
            child.kind(),
            "macro_invocation" | "macro_definition" | "extern_crate_declaration"
        ) {
            bindings.opaque = true;
            continue;
        }
        if child.kind() == "use_declaration" {
            let attributes = item_attributes(child, source);
            if attributes == AttributeStatus::Opaque {
                bindings.opaque = true;
                continue;
            }
            let argument = child.child_by_field_name("argument")?;
            let Some(edges) = expand_use_edges(argument, source, module) else {
                bindings.opaque = true;
                continue;
            };
            let visibility = visibility(child, source);
            let anchored = use_is_anchored(argument, source);
            let bare_local = use_is_simple_bare(argument, source);
            for edge in edges {
                match edge {
                    RustPubUseEdge::Named {
                        exported_name,
                        mut target_module,
                        target_item,
                        ..
                    } => {
                        if (anchored || bare_local) && attributes == AttributeStatus::Safe {
                            if bare_local {
                                target_module.splice(0..0, module.iter().cloned());
                            }
                            bindings.named.push(NamedImport {
                                name: exported_name,
                                target_module,
                                target_name: target_item,
                                visibility,
                            });
                        } else {
                            bindings.unknown_types.insert(exported_name.clone());
                            bindings.unknown_functions.insert(exported_name);
                        }
                    }
                    RustPubUseEdge::Wildcard { target_module, .. } => {
                        if anchored && attributes == AttributeStatus::Safe {
                            bindings.globs.push(GlobImport {
                                target_module,
                                visibility,
                            });
                        } else {
                            bindings.opaque = true;
                        }
                    }
                }
            }
            continue;
        }
        let named = child
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok());
        if let Some(name) = named {
            if item_attributes(child, source) != AttributeStatus::Safe {
                bindings.unknown_types.insert(name.to_owned());
                bindings.unknown_functions.insert(name.to_owned());
                continue;
            }
            let visibility = visibility(child, source);
            match child.kind() {
                "struct_item" | "enum_item" | "union_item" | "trait_item" => {
                    if bindings.types.insert(name.to_owned(), visibility).is_some() {
                        bindings.unknown_types.insert(name.to_owned());
                    }
                }
                "function_item" => {
                    if bindings
                        .functions
                        .insert(name.to_owned(), visibility)
                        .is_some()
                    {
                        bindings.unknown_functions.insert(name.to_owned());
                    }
                }
                "type_item" => {
                    bindings.unknown_types.insert(name.to_owned());
                }
                "const_item" | "static_item" => {
                    bindings.unknown_functions.insert(name.to_owned());
                }
                "mod_item" if child.child_by_field_name("body").is_some() => {
                    bindings.opaque = true;
                }
                "mod_item" => {
                    bindings.modules.push((name.to_owned(), visibility));
                }
                _ => {}
            }
        }
    }
    Some(bindings)
}

fn parse_use_edges(
    argument: Node<'_>,
    source: &[u8],
    module: &[String],
) -> Option<Vec<RustPubUseEdge>> {
    if !use_is_anchored(argument, source) {
        return None;
    }
    expand_use_edges(argument, source, module)
}

fn use_is_anchored(argument: Node<'_>, source: &[u8]) -> bool {
    argument.utf8_text(source).ok().is_some_and(|text| {
        let text = text.trim_start();
        ["crate", "self", "super"]
            .iter()
            .any(|anchor| text == *anchor || text.starts_with(&format!("{anchor}::")))
    })
}

fn use_is_simple_bare(argument: Node<'_>, source: &[u8]) -> bool {
    argument.utf8_text(source).ok().is_some_and(|text| {
        let text = text.trim_start();
        !use_is_anchored(argument, source)
            && text
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    })
}

fn expand_use_edges(
    argument: Node<'_>,
    source: &[u8],
    module: &[String],
) -> Option<Vec<RustPubUseEdge>> {
    let mut edges = Vec::new();
    expand_rust_use_tree_edges_ast(argument, source, &[], module, module, None, &mut edges)?;
    Some(edges)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AttributeStatus {
    Safe,
    Conditional,
    Opaque,
}

fn item_attributes(node: Node<'_>, source: &[u8]) -> AttributeStatus {
    let mut status = AttributeStatus::Safe;
    let mut previous = node.prev_named_sibling();
    while let Some(attribute) = previous {
        if matches!(attribute.kind(), "line_comment" | "block_comment") {
            previous = attribute.prev_named_sibling();
            continue;
        }
        if attribute.kind() != "attribute_item" {
            break;
        }
        if super::rust_std_origin::safe_attribute(attribute, source).is_none() {
            return AttributeStatus::Opaque;
        }
        let name = attribute
            .named_child(0)
            .and_then(|value| value.named_child(0))
            .and_then(|value| value.utf8_text(source).ok());
        if matches!(name, Some("cfg" | "test")) {
            status = AttributeStatus::Conditional;
        }
        previous = attribute.prev_named_sibling();
    }
    status
}

fn has_any_pub_visibility(node: Node<'_>, source: &[u8]) -> bool {
    let text = node.utf8_text(source).unwrap_or("").trim_start();
    text.strip_prefix("pub")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace) || rest.starts_with('('))
}

fn visibility(node: Node<'_>, source: &[u8]) -> Visibility {
    let text = node.utf8_text(source).unwrap_or("").trim_start();
    if text.strip_prefix("pub").is_some_and(|rest| {
        rest.starts_with(char::is_whitespace) && !rest.trim_start().starts_with('(')
    }) || text
        .strip_prefix("pub(crate)")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    {
        Visibility::Crate
    } else {
        Visibility::Private
    }
}
