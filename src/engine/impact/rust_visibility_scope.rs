//! Proof for a Rust file-root item whose only edit widens private visibility.
//!
//! The byte comparison is deliberately strict: formatting, attributes, or
//! another edit in the file make the old binding uncertain.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::engine::{diff, parser};
use crate::language::LangId;
use crate::models::impact::AffectedSymbol;

const DECLARATION_KINDS: &[&str] = &[
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "type_item",
    "function_item",
    "const_item",
    "static_item",
];

#[derive(Clone, Debug)]
enum Visibility {
    Public,
    Restricted(Vec<String>),
}

#[derive(Clone, Debug)]
pub(super) struct ExplicitWidening {
    old_path: Vec<String>,
    new_visibility: Visibility,
}

impl ExplicitWidening {
    pub(super) fn old_boundary(&self, module: &[String]) -> Option<Vec<String>> {
        let old = resolve_visibility_path(&self.old_path, module)?;
        match &self.new_visibility {
            Visibility::Public => Some(old),
            Visibility::Restricted(path) => {
                let new = resolve_visibility_path(path, module)?;
                (old.len() > new.len() && old.starts_with(&new)).then_some(old)
            }
        }
    }
}

fn resolve_visibility_path(path: &[String], module: &[String]) -> Option<Vec<String>> {
    let (first, rest) = path.split_first()?;
    let mut resolved = match first.as_str() {
        "crate" => Vec::new(),
        "self" => module.to_vec(),
        "super" => {
            let mut parent = module.to_vec();
            parent.pop()?;
            parent
        }
        _ => return None,
    };
    let mut leading_super = first == "super";
    for part in rest {
        match part.as_str() {
            "super" if leading_super => {
                resolved.pop()?;
            }
            "crate" | "self" | "super" => return None,
            _ => {
                leading_super = false;
                resolved.push(part.clone());
            }
        }
    }
    module.starts_with(&resolved).then_some(resolved)
}

fn visibility_path(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(source).ok()?;
            let name = name.strip_prefix("r#").unwrap_or(name);
            (!matches!(name, "crate" | "self" | "super" | "Self")).then(|| vec![name.to_owned()])
        }
        "crate" | "self" | "super" => Some(vec![node.utf8_text(source).ok()?.to_owned()]),
        "scoped_identifier" => {
            let mut path = visibility_path(node.child_by_field_name("path")?, source)?;
            path.extend(visibility_path(node.child_by_field_name("name")?, source)?);
            Some(path)
        }
        _ => None,
    }
}

fn visibility<'a>(item: Node<'a>, source: &[u8]) -> Option<(Node<'a>, Visibility)> {
    let mut cursor = item.walk();
    let node = item
        .named_children(&mut cursor)
        .find(|node| node.kind() == "visibility_modifier")?;
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
    let value = match children.next() {
        None => Visibility::Public,
        Some(path) if children.next().is_none() => {
            Visibility::Restricted(visibility_path(path, source)?)
        }
        _ => return None,
    };
    Some((node, value))
}

fn unique_declaration<'a>(
    root: Node<'a>,
    source: &[u8],
    name: &str,
    kind: &str,
) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    let mut nodes = root.named_children(&mut cursor).filter(|node| {
        node.kind() == kind
            && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some(name)
    });
    let item = nodes.next()?;
    nodes.next().is_none().then_some(item)
}

pub(super) fn explicit_widenings(
    file_diff: &str,
    file_path: &str,
    source: &[u8],
    root: Node<'_>,
    affected: &[AffectedSymbol],
) -> HashMap<String, ExplicitWidening> {
    let mut result = HashMap::new();
    if root.has_error() {
        return result;
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, file_path, source) else {
        return result;
    };
    let Ok(old_tree) = parser::parse_source(&old.source, LangId::Rust) else {
        return result;
    };
    let old_root = old_tree.root_node();
    if old_root.has_error() {
        return result;
    }
    for changed in affected
        .iter()
        .filter(|item| item.change_type == "modified")
    {
        // 型と値で同じ名前を使えるので、一意性は宣言の kind ごとに確認する。
        for kind in DECLARATION_KINDS {
            let Some(new_item) = unique_declaration(root, source, &changed.name, kind) else {
                continue;
            };
            let Some(old_item) = unique_declaration(old_root, &old.source, &changed.name, kind)
            else {
                continue;
            };
            let Some((new_node, new_visibility)) = visibility(new_item, source) else {
                continue;
            };
            let Some((old_node, Visibility::Restricted(old_path))) =
                visibility(old_item, &old.source)
            else {
                continue;
            };
            // 可視性の外は空白やコメントも含めて同じでなければ証明しない。
            if source[..new_node.start_byte()] == old.source[..old_node.start_byte()]
                && source[new_node.end_byte()..] == old.source[old_node.end_byte()..]
            {
                result.insert(
                    changed.name.clone(),
                    ExplicitWidening {
                        old_path,
                        new_visibility,
                    },
                );
            }
        }
    }
    result
}

pub(super) fn private_widening_names(
    file_diff: &str,
    file_path: &str,
    source: &[u8],
    root: Node<'_>,
    affected: &[AffectedSymbol],
) -> HashSet<String> {
    if root.has_error() {
        return HashSet::new();
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, file_path, source) else {
        return HashSet::new();
    };
    let Ok(old_tree) = parser::parse_source(&old.source, LangId::Rust) else {
        return HashSet::new();
    };
    if old_tree.root_node().has_error() {
        return HashSet::new();
    }

    let mut result = HashSet::new();
    for changed in affected
        .iter()
        .filter(|item| item.change_type == "modified")
    {
        let mut cursor = root.walk();
        let mut matches = root.named_children(&mut cursor).filter(|node| {
            matches!(
                node.kind(),
                "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "type_item"
                    | "function_item"
                    | "const_item"
                    | "static_item"
            ) && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some(changed.name.as_str())
        });
        let Some(item) = matches.next() else { continue };
        if matches.next().is_some() {
            continue;
        }
        let mut children = item.walk();
        let Some(visibility) = item
            .named_children(&mut children)
            .find(|child| child.kind() == "visibility_modifier")
        else {
            continue;
        };
        if !matches!(visibility.utf8_text(source), Ok("pub(crate)" | "pub")) {
            continue;
        }
        let mut end = visibility.end_byte();
        while end < source.len() && source[end].is_ascii_whitespace() {
            end += 1;
        }
        let mut without_visibility =
            Vec::with_capacity(source.len() - (end - visibility.start_byte()));
        without_visibility.extend_from_slice(&source[..visibility.start_byte()]);
        without_visibility.extend_from_slice(&source[end..]);
        if without_visibility == old.source {
            result.insert(changed.name.clone());
        }
    }
    result
}

/// A newly visible item can enter scope through a glob import. In that case
/// even an unchanged reference to another item might become ambiguous.
pub(super) fn has_glob_import(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration" && source[node.byte_range()].contains(&b'*') {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// 親以外からの glob は、公開範囲の拡大で新しい同名束縛を取り込み得る。
pub(super) fn has_non_parent_glob(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "use_wildcard" {
            let mut cursor = node.walk();
            let mut children = node
                .named_children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
            if !node
                .parent()
                .is_some_and(|parent| parent.kind() == "use_declaration")
                || children.next().is_none_or(|child| child.kind() != "super")
                || children.next().is_some()
            {
                return true;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod explicit_tests {
    use super::*;

    #[test]
    fn explicit_visibility_grammar_nodes_and_fields_exist() {
        let language = LangId::Rust.ts_language();
        for kind in DECLARATION_KINDS {
            tree_sitter::Query::new(&language, &format!("({kind} name: (_) @name)")).unwrap();
        }
        for query in [
            "(visibility_modifier)",
            "(use_declaration argument: (use_wildcard (super)))",
            "(mod_item name: (identifier))",
            "(scoped_identifier path: (_) @path name: (_) @name)",
            "(identifier)",
            "(crate)",
            "(self)",
            "(super)",
        ] {
            tree_sitter::Query::new(&language, query).unwrap();
        }
    }

    #[test]
    fn private_widening_globs_require_a_direct_parent_path() {
        for source in [
            "use super::*;",
            "use super /* parent */ :: *;",
            "pub(crate) use super::*;",
            "use super::item;",
            "const TEXT: &str = \"use other::*;\"; // use other::*;",
        ] {
            assert!(!has_non_parent_glob(source.as_bytes()), "{source}");
        }
        for source in [
            "use other::*;",
            "use super::other::*;",
            "use crate::*;",
            "use super::{*};",
            "use super::{item, other::*};",
            "fn broken(",
        ] {
            assert!(has_non_parent_glob(source.as_bytes()), "{source}");
        }
    }

    #[test]
    fn explicit_visibility_all_root_declaration_kinds_use_the_same_byte_proof() {
        for declaration in [
            "struct Item { value: bool }",
            "enum Item {}",
            "union Item { value: u32 }",
            "trait Item {}",
            "type Item = u32;",
            "fn Item() {}",
            "const Item: u32 = 0;",
            "static Item: u32 = 0;",
        ] {
            let old = format!("pub(crate) {declaration}\n");
            let new = format!("pub {declaration}\n");
            let patch = format!("--- a/src/item.rs\n+++ b/src/item.rs\n@@ -1 +1 @@\n-{old}+{new}");
            let tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
            let changed = [AffectedSymbol {
                name: "Item".into(),
                kind: "function".into(),
                change_type: "modified".into(),
            }];
            let proofs = explicit_widenings(
                &patch,
                "src/item.rs",
                new.as_bytes(),
                tree.root_node(),
                &changed,
            );
            assert_eq!(proofs.len(), 1, "{declaration}");
            assert_eq!(proofs["Item"].old_boundary(&["a".into()]), Some(Vec::new()));
        }
    }

    #[test]
    fn explicit_visibility_boundaries_reject_non_ancestors_and_illegal_paths() {
        let module = ["a".into(), "b".into()];
        for (path, expected) in [
            (vec!["crate", "a"], Some(vec!["a"])),
            (vec!["super", "super"], Some(vec![])),
            (vec!["super", "b"], Some(vec!["a", "b"])),
            (vec!["self"], Some(vec!["a", "b"])),
            (vec!["crate", "missing"], None),
            (vec!["crate", "super"], None),
            (vec!["self", "super"], None),
            (vec!["super", "b", "super"], None),
            (vec!["super", "super", "super"], None),
        ] {
            let parts = path
                .iter()
                .map(|part| (*part).to_owned())
                .collect::<Vec<_>>();
            let expected =
                expected.map(|parts| parts.into_iter().map(str::to_owned).collect::<Vec<_>>());
            assert_eq!(
                resolve_visibility_path(&parts, &module),
                expected,
                "{path:?}"
            );
        }
    }
}
