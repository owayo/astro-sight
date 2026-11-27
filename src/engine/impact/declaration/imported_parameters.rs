//! 外部の nominal 型への明示 use による引数型の短縮を照合する。

use super::{DeclarationChangeInput, LazyOldSide};
use crate::engine::bounded_read::read_input_file;
use crate::engine::impact::rust_line_endings::same_text;
use crate::engine::impact::rust_owner_alias::Names;
use crate::engine::impact::rust_path_owner::RustPathOwnerProjectProof;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::language::LangId;
use crate::models::impact::SignatureChange;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use tree_sitter::Node;

impl LazyOldSide {
    pub(in crate::engine::impact) fn imported_parameter_shortening<'p>(
        &self,
        input: &DeclarationChangeInput<'_>,
        signatures: &[SignatureChange],
        project: impl FnOnce() -> Option<&'p RustPathOwnerProjectProof>,
    ) -> Option<HashSet<String>> {
        if !input.complete_single_file_diff
            || !signatures.iter().any(|s| s.old_signature.contains("::"))
            || !input
                .root
                .named_children(&mut input.root.walk())
                .any(|n| n.kind() == "use_declaration")
        {
            return None;
        }
        let old = self.get(input)?;
        let before = Module::collect(old.tree.root_node(), &old.source)?;
        let after = Module::collect(input.root, input.source)?;
        if before.items.len() != after.items.len()
            || !before
                .names
                .imports
                .iter()
                .all(|(name, binding)| after.names.imports.get(name) == Some(binding))
        {
            return None;
        }
        let added: HashSet<&str> = after
            .names
            .imports
            .keys()
            .filter(|name| !before.names.imports.contains_key(*name))
            .map(String::as_str)
            .collect();
        if !added.is_empty() && before.uses_unqualified(old.tree.root_node(), &added) {
            return None;
        }
        let mut functions = HashSet::new();
        let mut shortened = HashSet::new();
        let mut dependencies = HashMap::<String, HashSet<String>>::new();
        for (&previous, &current) in before.items.iter().zip(&after.items) {
            if same_text(
                &before.source[previous.byte_range()],
                &after.source[current.byte_range()],
            ) {
                continue;
            }
            if previous.kind() != "function_item" || current.kind() != "function_item" {
                return None;
            }
            let name = after.name(current.child_by_field_name("name")?)?;
            for path in shortened_parameters(&before, previous, &after, current)? {
                dependencies
                    .entry(path.dependency.to_owned())
                    .or_default()
                    .insert(path.nominal.to_owned());
                shortened.insert(path.alias);
            }
            functions.insert(name.to_owned());
        }
        if functions.is_empty() || !added.iter().all(|s| shortened.contains(*s)) {
            return None;
        }
        // 署名の除外は名前単位なので、同じファイルの別宣言に適用しない。
        for symbols in [&old.symbols[..], input.syms] {
            let mut seen = HashSet::new();
            for symbol in symbols {
                let name = symbol.name.trim_start_matches("r#");
                if functions.contains(name) && !seen.insert(name) {
                    return None;
                }
            }
        }
        let project = project()?;
        let file = Path::new(input.file_path);
        let module = project.ordinary_private_module(file)?;
        if !module.library_only || !project.unrelocated(file) {
            return None;
        }
        let directory = module.src.parent()?;
        for (dependency, types) in dependencies {
            let library = project.required_path_library(directory, &dependency)?;
            public_nominal_types(project, &library, &types)?;
        }
        Some(functions)
    }
}

struct ImportedType<'a> {
    dependency: &'a str,
    nominal: &'a str,
    alias: &'a str,
}

/// 引数型の先頭だけを取り除いて関数全体を比較する。型引数・ライフタイム・戻り型・本文も一致を求める。
fn shortened_parameters<'a>(
    before: &Module<'a>,
    previous: Node<'_>,
    after: &Module<'a>,
    current: Node<'_>,
) -> Option<Vec<ImportedType<'a>>> {
    let mut paths = Vec::new();
    let mut generics = HashSet::new();
    if let Some(params) = current.child_by_field_name("type_parameters") {
        walk_preorder(params, Children::Named, |n, _| {
            if matches!(n.kind(), "type_identifier" | "identifier") {
                let Some(name) = after.name(n) else {
                    return ControlFlow::Break(());
                };
                generics.insert(name);
            }
            ControlFlow::Continue(Walk::Descend)
        })
        .is_continue()
        .then_some(())?;
    }
    let old_params = previous.child_by_field_name("parameters")?;
    let new_params = current.child_by_field_name("parameters")?;
    if old_params.named_child_count() != new_params.named_child_count() {
        return None;
    }
    let mut old_start = previous.start_byte();
    let mut new_start = current.start_byte();
    for (old_parameter, new_parameter) in old_params
        .named_children(&mut old_params.walk())
        .zip(new_params.named_children(&mut new_params.walk()))
    {
        let Some((long, short)) = parameter_head(old_parameter).zip(parameter_head(new_parameter))
        else {
            continue;
        };
        if long.kind() != "scoped_type_identifier" || short.kind() != "type_identifier" {
            continue;
        }
        let prefix = long.child_by_field_name("path")?;
        if prefix.kind() != "identifier" {
            return None;
        }
        let dependency = before.name(prefix)?;
        let ty = before.name(long.child_by_field_name("name")?)?;
        let alias = after.name(short)?;
        let target = vec![dependency.to_owned(), ty.to_owned()];
        if !after.names.imports.contains_key(alias)
            || before.names.extern_path(target.clone())? != target
            || after.names.extern_path(vec![alias.to_owned()])? != target
            || !same_text(
                &before.source[old_start..long.start_byte()],
                &after.source[new_start..short.start_byte()],
            )
            || generics.contains(alias)
            || generics.contains(dependency)
        {
            return None;
        }
        paths.push(ImportedType {
            dependency,
            nominal: ty,
            alias,
        });
        old_start = long.end_byte();
        new_start = short.end_byte();
    }
    if old_start == previous.start_byte()
        || !same_text(
            &before.source[old_start..previous.end_byte()],
            &after.source[new_start..current.end_byte()],
        )
    {
        return None;
    }
    Some(paths)
}

fn public_nominal_types(
    project: &RustPathOwnerProjectProof,
    library: &Path,
    types: &HashSet<String>,
) -> Option<()> {
    let directory = library.parent()?.parent()?;
    let manifest = project.alias_cache.manifest(&project.root, directory)?;
    // src/lib.rs が残っていても、Cargo が依存先として渡す library とは限らない。
    if let Some(target) = manifest.get("lib") {
        if target
            .get("proc-macro")
            .or_else(|| target.get("proc_macro"))
            .is_some_and(|v| v.as_bool() != Some(false))
            || target
                .get("crate-type")
                .or_else(|| target.get("crate_type"))
                .is_some_and(|v| {
                    v.as_array().is_none_or(|kinds| {
                        !kinds
                            .iter()
                            .any(|k| matches!(k.as_str(), Some("lib" | "rlib" | "dylib")))
                    })
                })
        {
            return None;
        }
    } else if manifest
        .get("package")?
        .get("autolib")
        .is_some_and(|v| v.as_bool() != Some(true))
    {
        return None;
    }
    let source = read_input_file(&project.root.join(library))?;
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    let root = tree.root_node();
    let names = Names::collect(root, &source, &[])?;
    if !names.globs.is_empty() {
        return None;
    }
    let mut declared = HashSet::new();
    for (item, attributed) in crate::engine::impact::rust_owner_alias::items_with_attributes(
        root.named_children(&mut root.walk()),
    ) {
        if !attributed
            && matches!(item.kind(), "struct_item" | "enum_item" | "union_item")
            && item.named_child(0).is_some_and(|n| {
                n.kind() == "visibility_modifier" && n.utf8_text(&source) == Ok("pub")
            })
        {
            let name = item.child_by_field_name("name")?.utf8_text(&source).ok()?;
            if !declared.insert(name) {
                return None;
            }
        }
    }
    types
        .iter()
        .all(|ty| declared.contains(ty.as_str()) && !names.imports.contains_key(ty))
        .then_some(())
}

fn parameter_head(parameter: Node<'_>) -> Option<Node<'_>> {
    let ty = parameter.child_by_field_name("type")?;
    if ty.kind() == "generic_type" {
        ty.child_by_field_name("type")
    } else {
        Some(ty)
    }
}

struct Module<'a> {
    source: &'a [u8],
    items: Vec<Node<'a>>,
    names: Names,
}

impl<'a> Module<'a> {
    fn collect(root: Node<'a>, source: &'a [u8]) -> Option<Self> {
        let names = Names::collect(root, source, &[])?;
        if !names.globs.is_empty()
            || any_node(root, Children::Named, |n| {
                matches!(
                    n.kind(),
                    "mod_item"
                        | "attribute_item"
                        | "inner_attribute_item"
                        | "macro_definition"
                        | "macro_invocation"
                        | "extern_crate_declaration"
                )
            })
        {
            return None;
        }
        let mut items = Vec::new();
        for item in root.named_children(&mut root.walk()) {
            if item.kind() != "use_declaration" {
                items.push(item);
            } else if item
                .named_child(0)
                .is_some_and(|n| n.kind() == "visibility_modifier")
            {
                return None;
            }
        }
        Some(Self {
            source,
            items,
            names,
        })
    }

    fn name(&self, n: Node<'_>) -> Option<&'a str> {
        let text = n.utf8_text(self.source).ok()?;
        (text.is_ascii() && !text.starts_with("r#")).then_some(text)
    }

    /// 新しい private use が既存の非修飾名も取り込む場合は、その束縛を確定しない。
    fn uses_unqualified(&self, root: Node<'_>, names: &HashSet<&str>) -> bool {
        let mut tails = HashSet::new();
        walk_preorder(root, Children::Named, |n, _| {
            if matches!(n.kind(), "scoped_identifier" | "scoped_type_identifier")
                && let Some(tail) = n.child_by_field_name("name")
                && self.name(tail).is_some_and(|name| names.contains(name))
            {
                tails.insert(tail.id());
            }
            if matches!(n.kind(), "identifier" | "type_identifier")
                && !tails.contains(&n.id())
                && self.name(n).is_none_or(|name| names.contains(name))
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(Walk::Descend)
        })
        .is_break()
    }
}
