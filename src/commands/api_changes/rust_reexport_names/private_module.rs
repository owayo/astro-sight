use super::{Index, ProviderKind, opaque_entry, provider_entry};
use crate::commands::api_changes::cargo_files::{Files, external_settings_known};
use crate::commands::api_changes::rust_dependencies::Opened;
use crate::commands::api_changes::rust_dependencies::crates::{CratePair, join_path};
use crate::commands::api_changes::signature_tokens::node_signature_tokens;
use crate::commands::git_input::GitBlobBatch;
use crate::engine::impact::rust_cargo_config::config_keeps_sources;
use crate::engine::rust_edition::RustEdition;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

pub(super) struct Project<'a> {
    pub(super) dir: &'a str,
    checkout: &'a str,
    base: &'a GitBlobBatch,
    settings_known: OnceCell<bool>,
    macros: RefCell<HashMap<(String, String, String), bool>>,
}

impl<'a> Project<'a> {
    pub(super) fn new(dir: &'a str, checkout: &'a str, base: &'a GitBlobBatch) -> Self {
        Self {
            dir,
            checkout,
            base,
            settings_known: OnceCell::new(),
            macros: RefCell::default(),
        }
    }

    /// マクロの順序だけによる再比較を省く。通常の名前・置き場・属性・呼び出しの差分は別に比べる。
    pub(super) fn independent_additions(
        &self,
        pair: &CratePair<'_>,
        versions: &[&Index<'_>; 2],
        changes: &[&(String, String)],
    ) -> HashSet<Vec<String>> {
        let mut uncovered: HashSet<_> = changes
            .iter()
            .map(|(old, new)| (old.as_str(), new.as_str()))
            .collect();
        let mut modules = HashSet::new();
        for (old, new) in changes {
            if old != new {
                continue;
            }
            for (module, _) in versions[1].by_file.get(new).into_iter().flatten() {
                if let Some(leaf) = self.independent_child(pair, versions, module) {
                    uncovered.remove(&(old.as_str(), new.as_str()));
                    uncovered.remove(&("/dev/null", leaf));
                    modules.insert(module.clone());
                }
            }
        }
        if !uncovered.is_empty() {
            modules.clear();
        }
        modules
    }

    fn independent_child<'p>(
        &self,
        pair: &CratePair<'_>,
        versions: &[&'p Index<'_>; 2],
        module: &[String],
    ) -> Option<&'p str> {
        let [old] = versions[0].scopes(module) else {
            return None;
        };
        let [new] = versions[1].scopes(module) else {
            return None;
        };
        let [call] = new.exports.opaque.as_slice() else {
            return None;
        };
        let [previous] = old.exports.opaque.as_slice() else {
            return None;
        };
        let [head] = call.paths.as_slice() else {
            return None;
        };
        let name = call.macro_name.as_ref()?;
        if !head.is_ascii()
            || matches!(
                head.as_str(),
                "core" | "std" | "alloc" | "proc_macro" | "crate" | "self" | "super"
            )
            || old.file != new.file
            || super::placement_identity(versions[0], module)
                != super::placement_identity(versions[1], module)
            || !call.calls.is_empty()
            || opaque_entry(previous) != opaque_entry(call)
            || call.mentions.contains(head)
            || versions.iter().any(|version| {
                version.edition != Some(RustEdition::E2018OrLater)
                    || version.module_uncertain(module)
            })
        {
            return None;
        }
        let before: BTreeSet<_> = old.exports.providers.iter().map(provider_entry).collect();
        let mut added = new
            .exports
            .providers
            .iter()
            .filter(|item| !before.contains(&provider_entry(item)));
        let child = added.next()?;
        if before.len() + 1 != new.exports.providers.len()
            || added.next().is_some()
            || !matches!(child.kind, ProviderKind::Declared("module"))
            || child.scope.as_deref() != Some(module)
            || !child.availability_raw.is_empty()
            || child.name == *head
            || call.mentions.contains(&child.name)
        {
            return None;
        }
        let mut path = module.to_vec();
        path.push(child.name.clone());
        let [leaf] = versions[1].scopes(&path) else {
            return None;
        };
        if !versions[0].scopes(&path).is_empty()
            || versions[1].module_uncertain(&path)
            || leaf
                .exports
                .providers
                .iter()
                .any(|p| !matches!(p.kind, ProviderKind::Declared("function")) || p.scope.is_none())
            || !leaf.exports.opaque.is_empty()
            || !leaf.exports.macro_order.is_empty()
            || !leaf.exports.globs.is_empty()
        {
            return None;
        }
        let declaration = format!("mod {} ", child.name);
        if !new
            .exports
            .macro_order
            .iter()
            .filter(|entry| !entry.starts_with(&declaration))
            .eq(&old.exports.macro_order)
        {
            return None;
        }
        // 外部 crate の名前を隠す宣言、別名、解けない glob、親のマクロが作る束縛は決めない。
        for (version, krate) in versions.iter().zip([&pair.old, &pair.new]) {
            for len in 0..=module.len() {
                let [scope] = version.scopes(&module[..len]) else {
                    return None;
                };
                let Opened::File(file) = krate.open_file(&scope.file) else {
                    return None;
                };
                if !file.keeps_macro_prelude()
                    || !file.scope_attributed(&scope.inline).is_empty()
                    || !file.scope_body_attributed(&scope.inline).is_empty()
                    || scope.exports.providers.iter().any(|p| p.name == *head)
                    || (len != module.len() && !scope.exports.opaque.is_empty())
                    || scope.exports.globs.iter().any(|g| {
                        g.origin == crate::engine::rust_use_tree::UsePathOrigin::Anchored
                            || g.module.len() != 2
                            || g.module[0] != *head
                    })
                    || scope
                        .exports
                        .macro_order
                        .iter()
                        .any(|entry| entry.starts_with("use-"))
                {
                    return None;
                }
            }
        }
        let key = (pair.package.clone(), head.clone(), name.clone());
        let trusted = *self
            .macros
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| self.local_macro(&pair.package, head, name));
        trusted.then_some(leaf.file.as_str())
    }

    fn local_macro(&self, package: &str, head: &str, name: &str) -> bool {
        if !*self
            .settings_known
            .get_or_init(|| external_settings_known(Path::new(self.checkout)).is_some())
        {
            return false;
        }
        let root = Path::new(self.dir);
        let view = crate::git_support::tree_view(root);
        let old_files = Files::Base(self.base);
        let new_files = Files::Current(root, &view);
        let source = |files: &Files<'_>| direct_source(files, package, head);
        let (Some(old), Some(new)) = (source(&old_files), source(&new_files)) else {
            return false;
        };
        old.lines().eq(new.lines()) && declaration_macro(&new, head, name)
    }
}

/// 通常の workspace member への直接の path 依存。名前の読み替えは Cargo の package キーで確かめる。
fn direct_source(files: &Files<'_>, owner: &str, head: &str) -> Option<String> {
    let manifest = |directory: &Path| {
        let mut prefix = PathBuf::new();
        for component in directory.components() {
            prefix.push(component);
            if !files.directory(&prefix) {
                return None;
            }
        }
        toml::from_str::<toml::Table>(&files.text(&directory.join("Cargo.toml"))??).ok()
    };
    let parent = Path::new(owner);
    let viewer = manifest(parent)?;
    let package = viewer.get("package")?.as_table()?;
    if package.contains_key("workspace")
        || viewer.contains_key("target")
        || viewer.contains_key("lib")
        || package.contains_key("build")
        || files.text(&parent.join("build.rs"))?.is_some()
    {
        return None;
    }
    let dependencies = viewer.get("dependencies")?.as_table()?;
    let mut matches = dependencies
        .iter()
        .filter(|(key, _)| key.replace('-', "_") == head);
    let (key, spec) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let spec = spec.as_table()?;
    if !spec
        .keys()
        .all(|key| matches!(key.as_str(), "path" | "package"))
    {
        return None;
    }
    let relative = spec.get("path")?.as_str()?;
    if relative
        .split('/')
        .skip_while(|part| matches!(*part, ".." | "." | ""))
        .any(|part| part == "..")
    {
        return None;
    }
    let target = PathBuf::from(join_path(owner, relative)?);
    let dependency = manifest(&target)?;
    let package = dependency.get("package")?.as_table()?;
    if package.get("name")?.as_str()?
        != spec
            .get("package")
            .map_or(Some(key.as_str()), toml::Value::as_str)?
        || dependency.contains_key("lib")
        || package.contains_key("workspace")
        || package.contains_key("build")
        || files.text(&target.join("build.rs"))?.is_some()
    {
        return None;
    }
    let workspace = manifest(Path::new(""))?;
    let workspace = workspace.get("workspace")?.as_table()?;
    if workspace.contains_key("exclude") {
        return None;
    }
    let members = workspace.get("members")?.as_array()?;
    for path in [parent, target.as_path()] {
        if !members
            .iter()
            .any(|m| m.as_str().is_some_and(|p| Path::new(p) == path))
        {
            return None;
        }
        for dir in path.ancestors() {
            if !dir.as_os_str().is_empty() && !files.directory(dir) {
                return None;
            }
            let config = dir.join(".cargo");
            if !files.directory(&config) {
                return None;
            }
            for file in ["config", "config.toml"] {
                if files
                    .text(&config.join(file))?
                    .is_some_and(|text| !config_keeps_sources(&text))
                {
                    return None;
                }
            }
            let manifest_path = dir.join("Cargo.toml");
            if let Some(text) = files.text(&manifest_path)? {
                let table = toml::from_str::<toml::Table>(&text).ok()?;
                if table.contains_key("patch")
                    || table.contains_key("replace")
                    || (!dir.as_os_str().is_empty() && table.contains_key("workspace"))
                {
                    return None;
                }
            }
        }
    }
    files.directory(&target.join("src")).then_some(())?;
    files.text(&target.join("src/lib.rs"))?
}

/// 依存の単一ファイルの中に、別の展開・再公開・外部ファイルから同じ crate 名やマクロを持ち込む経路がないか。
fn declaration_macro(text: &str, head: &str, name: &str) -> bool {
    let source = text.as_bytes();
    if source.len() > 1_000_000 {
        return false;
    }
    let Ok(tree) = crate::engine::parser::parse_source(source, crate::language::LangId::Rust)
    else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    let mut exported = false;
    let mut definitions = 0;
    let result = walk_preorder(root, Children::Named, |item, depth| {
        match item.kind() {
            "attribute_item" => {
                let explicit_export = node_signature_tokens(item, source).is_some_and(|tokens| {
                    tokens.0.iter().map(|(_, text)| text.as_str()).eq([
                        "#",
                        "[",
                        "macro_export",
                        "]",
                    ])
                });
                if !explicit_export {
                    return ControlFlow::Break(());
                }
                if depth == 1 {
                    exported = true;
                }
                return ControlFlow::Continue(Walk::Skip);
            }
            "line_comment" | "block_comment" => return ControlFlow::Continue(Walk::Skip),
            "macro_definition" => {
                if item
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(source).ok())
                    == Some(name)
                {
                    if depth != 1 || !exported || !unit_structs(item, source) {
                        return ControlFlow::Break(());
                    }
                    definitions += 1;
                }
                if depth == 1 {
                    exported = false;
                }
                return ControlFlow::Continue(Walk::Skip);
            }
            "inner_attribute_item"
            | "use_declaration"
            | "extern_crate_declaration"
            | "macro_invocation" => return ControlFlow::Break(()),
            "mod_item" if item.child_by_field_name("body").is_none() => {
                return ControlFlow::Break(());
            }
            _ => {}
        }
        if depth == 1 {
            exported = false;
        }
        if item
            .child_by_field_name("name")
            .and_then(|node| node.utf8_text(source).ok())
            .is_some_and(|name| name.trim_start_matches("r#") == head)
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    result.is_continue() && definitions == 1
}

/// ident だけから unit struct を作り、作った module から再公開する transcriber。呼び出し側の名前・位置を読まない。
fn unit_structs(definition: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = definition.walk();
    let rules: Vec<_> = definition
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "macro_rule")
        .collect();
    !rules.is_empty()
        && rules.iter().all(|rule| {
            let Some(pattern) = rule.child_by_field_name("left") else {
                return false;
            };
            let mut captures = BTreeSet::new();
            let matched = walk_preorder(pattern, Children::Named, |binding, _| {
                if binding.kind() == "token_binding_pattern" {
                    let fragment = binding
                        .child_by_field_name("type")
                        .and_then(|ty| ty.utf8_text(source).ok());
                    let name = binding
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok());
                    if fragment != Some("ident") || !name.is_some_and(|name| captures.insert(name))
                    {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(Walk::Descend)
            });
            if matched.is_break() {
                return false;
            }
            let Some(output) = rule
                .child_by_field_name("right")
                .and_then(|rhs| node_signature_tokens(rhs, source))
            else {
                return false;
            };
            let captured = |name: &str| captures.contains(name);
            let output: Vec<_> = output.0.iter().map(|(_, text)| text.as_str()).collect();
            let [_, body @ .., _] = output.as_slice() else {
                return false;
            };
            let mut items = match body {
                [
                    "pub",
                    "mod",
                    module,
                    "{",
                    items @ ..,
                    "}",
                    "pub",
                    "use",
                    origin,
                    "::",
                    "*",
                    ";",
                ] if module == origin && captured(module) => items,
                _ => body,
            };
            if items.is_empty() {
                return false;
            }
            while !items.is_empty() {
                items = match items {
                    ["pub", "struct", name, ";", rest @ ..] if captured(name) => rest,
                    [
                        "$",
                        "(",
                        "pub",
                        "struct",
                        name,
                        ";",
                        ")",
                        "*" | "+" | "?",
                        rest @ ..,
                    ] if captured(name) => rest,
                    _ => return false,
                };
            }
            true
        })
}
