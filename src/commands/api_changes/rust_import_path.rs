//! 同じ nominal 型への crate 起点の明示 use による、戻り値の短縮だけを証明する。

use std::ops::Range;

use tree_sitter::Node;

use super::prepare::DetectionInputs;
use super::rust_dependencies::crates::{CrateVersion, Located};
use super::rust_dependencies::{Opened, Resolver};
use super::signature_tokens::{node_signature_tokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::engine::{parser, tree_walk};
use crate::language::LangId;
use crate::models::review::CompatibleApiModification;

pub(super) fn detect(
    inputs: &DetectionInputs<'_>,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<CompatibleApiModification> {
    if site.lang_id != Some(LangId::Rust)
        || site.kind != "function"
        || site.name.contains("::")
        || site.old_path != site.new_path
        || inputs
            .diff_files
            .iter()
            .any(|file| file.old_path != site.old_path || file.new_path != site.new_path)
    {
        return None;
    }
    let pair = sources.get(site)?;
    let (old, new) = pair.parse_pair(LangId::Rust)?;
    let old_root = old.root_node();
    let new_root = new.root_node();
    // 子 module の glob が、追加した private use を取り込み得る。
    if [old_root, new_root].into_iter().any(|root| {
        tree_walk::any_node(root, tree_walk::Children::Named, |node| {
            node.kind() == "mod_item"
        })
    }) {
        return None;
    }
    if !plain_source(old_root, false) || !plain_source(new_root, true) {
        return None;
    }
    let old_function = root_function(old_root, &pair.old, site.name)?;
    let new_function = root_function(new_root, &pair.new, site.name)?;
    let old_return = old_function.child_by_field_name("return_type")?;
    let new_return = new_function.child_by_field_name("return_type")?;
    if old_return.kind() != "scoped_type_identifier" || new_return.kind() != "type_identifier" {
        return None;
    }
    let segments = crate_path(old_return, &pair.old)?;
    let name = new_return.utf8_text(&pair.new).ok()?;
    if segments.last()?.as_str() != name {
        return None;
    }
    let mut cursor = new_root.walk();
    let imports: Vec<_> = new_root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "use_declaration")
        .collect();
    let [import] = imports.as_slice() else {
        return None;
    };
    if import.child_by_field_name("visibility").is_some()
        || import
            .named_child(0)
            .is_some_and(|node| node.kind() == "visibility_modifier")
        || crate_path(import.child_by_field_name("argument")?, &pair.new)? != segments
        || !no_bare_name(old_root, &pair.old, name, &segments)
        || tokens_except(old_root, &pair.old, vec![old_return.byte_range()])?
            != tokens_except(
                new_root,
                &pair.new,
                vec![import.byte_range(), new_return.byte_range()],
            )?
    {
        return None;
    }
    if !crate::engine::impact::rust_import_path_environment_is_closed(std::path::Path::new(
        site.dir,
    )) {
        return None;
    }
    let versions = inputs.rust_crates.library_versions(site.new_path)?;
    let before = resolved_owner(&versions.old, site.old_path, &segments)?;
    let after = resolved_owner(&versions.new, site.new_path, &segments)?;
    if before != after {
        return None;
    }
    let old_modules = versions.old.tree_modules(site.old_path)?;
    let new_modules = versions.new.tree_modules(site.new_path)?;
    if old_modules != new_modules {
        return None;
    }
    let factory_module = old_modules.first()?;
    let namespaces = (0..segments.len() - 1)
        .map(|length| &segments[1..length + 1])
        .chain((0..=factory_module.len()).map(|length| &factory_module[..length]));
    for scope in namespaces {
        let (old_path, old_scope) = namespace(&versions.old, scope, site.old_path, false)?;
        let (new_path, new_scope) = namespace(&versions.new, scope, site.new_path, true)?;
        if old_path != new_path
            || old_scope != new_scope
            || (old_path != site.old_path
                && versions.old.read(&old_path)? != versions.new.read(&new_path)?)
        {
            return None;
        }
    }
    let Opened::File(file) = versions.new.open_file(site.new_path) else {
        return None;
    };
    let resolver = Resolver::new(Some(&versions.new), file.clone(), None, None);
    let short = single_nominal(resolver.resolve_type_path(&file, &[], &[name.to_owned()])?)?;
    (short == after).then(|| site.compatible("rust_explicit_import_path"))
}

type Owner = (String, Vec<String>, String, &'static str);

fn resolved_owner(krate: &CrateVersion<'_>, path: &str, segments: &[String]) -> Option<Owner> {
    let modules = krate.tree_modules(path)?;
    let [_] = modules.as_slice() else {
        return None;
    };
    let Opened::File(file) = krate.open_file(path) else {
        return None;
    };
    let resolver = Resolver::new(Some(krate), file.clone(), None, None);
    let placements = resolver.placement_branches(&file)?;
    if placements.as_slice() != [Vec::<String>::new()] {
        return None;
    }
    single_nominal(resolver.resolve_type_path(&file, &[], segments)?)
}

fn single_nominal(candidates: Vec<super::rust_dependencies::Candidate>) -> Option<Owner> {
    let [candidate] = candidates.as_slice() else {
        return None;
    };
    let definition = candidate.definition.as_ref()?;
    if !candidate.conditions.is_empty()
        || !candidate.entered.is_empty()
        || !candidate.replaceable.is_empty()
        || definition.opaque
        || !matches!(definition.kind, "struct_item" | "enum_item" | "union_item")
    {
        return None;
    }
    Some((
        definition.path.clone(),
        definition.scope.clone(),
        definition.name.clone(),
        definition.kind,
    ))
}

fn namespace(
    krate: &CrateVersion<'_>,
    scope: &[String],
    target: &str,
    new: bool,
) -> Option<(String, Vec<String>)> {
    let Located::Found(file, inline) = krate.locate_scope(scope) else {
        return None;
    };
    let source = krate.read(file.path())?;
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    plain_source(tree.root_node(), new && file.path() == target)
        .then(|| (file.path().to_owned(), inline))
}

fn plain_source(root: Node<'_>, allow_use: bool) -> bool {
    !root.has_error()
        && !tree_walk::any_node(root, tree_walk::Children::Named, |node| {
            node.is_error()
                || node.is_missing()
                || matches!(
                    node.kind(),
                    "attribute_item"
                        | "inner_attribute_item"
                        | "macro_invocation"
                        | "macro_definition"
                        | "extern_crate_declaration"
                        | "type_parameters"
                        | "type_item"
                        | "use_as_clause"
                        | "use_wildcard"
                )
                || (!allow_use && node.kind() == "use_declaration")
        })
}

fn root_function<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    let mut found = root.named_children(&mut cursor).filter(|node| {
        node.kind() == "function_item"
            && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some(name)
    });
    let function = found.next()?;
    found.next().is_none().then_some(function)
}

fn crate_path(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let tokens = node_signature_tokens(node, source)?.0;
    let mut segments = Vec::new();
    for (index, (kind, text)) in tokens.iter().enumerate() {
        if index % 2 == 1 {
            if text != "::" {
                return None;
            }
        } else if (index == 0 && kind == "crate" && text == "crate")
            || (index > 0
                && matches!(kind.as_str(), "identifier" | "type_identifier")
                && text.is_ascii()
                && !text.starts_with("r#"))
        {
            segments.push(text.clone());
        } else {
            return None;
        }
    }
    (tokens.len() % 2 == 1 && segments.len() >= 2).then_some(segments)
}

fn no_bare_name(root: Node<'_>, source: &[u8], name: &str, target: &[String]) -> bool {
    !tree_walk::any_node(root, tree_walk::Children::Named, |node| {
        if !matches!(node.kind(), "identifier" | "type_identifier") {
            return false;
        }
        let Ok(text) = node.utf8_text(source) else {
            return true;
        };
        if !text.is_ascii() || text.starts_with("r#") {
            return true;
        }
        text == name
            && !node.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "scoped_identifier" | "scoped_type_identifier"
                ) && parent
                    .child_by_field_name("name")
                    .is_some_and(|tail| tail.id() == node.id())
                    && crate_path(parent, source).as_deref() == Some(target)
            })
    })
}

fn tokens_except(
    root: Node<'_>,
    source: &[u8],
    mut ranges: Vec<Range<usize>>,
) -> Option<Vec<(String, String)>> {
    ranges.sort_by_key(|range| range.start);
    let mut tokens = Vec::new();
    let mut start = root.start_byte();
    for range in ranges {
        tokens.extend(signature_tokens_in_range(root, source, start, range.start)?.0);
        start = range.end;
    }
    tokens.extend(signature_tokens_in_range(root, source, start, root.end_byte())?.0);
    Some(tokens)
}
