//! 文字列 union の純粋な削除を、限定した値の受け口と利用から証明する。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use tree_sitter::Node;

use super::ref_index::ApiRefIndex;
use super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::engine::js_scope::{Namespace, ScopeBinding, resolve_binding_scope};
use crate::engine::tree_walk::{Children, any_node};
use crate::language::LangId;

mod decode;
mod flows;
mod inventory;
mod residue;
mod shape;
mod slots;

const MAX_DEPTH: usize = 128;
type Span = (usize, usize);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Site {
    path: String,
    name: String,
    span: Span,
}

#[derive(Default)]
pub(super) struct RemovalCache {
    inventory: Option<Option<Rc<inventory::Inventory>>>,
    residue: HashMap<Vec<String>, bool>,
}

/// 対象の削除形を認めた後の失敗は、参照行だけの証拠へ戻さない。
pub(super) fn prove_removal(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut RemovalCache,
) -> Option<bool> {
    if !matches!(site.lang_id, Some(LangId::Typescript | LangId::Tsx))
        || site.kind != "type"
        || site.old_path != site.new_path
    {
        return None;
    }
    let pair = sources.get(site)?;
    let removal = match shape::classify(site, pair) {
        shape::Change::NotApplicable => return None,
        shape::Change::Unsupported => return Some(false),
        shape::Change::Removal(removal) => removal,
    };
    let Some(references) = index.complete_refs_for(site.name) else {
        return Some(false);
    };
    let Some(inventory) = cache
        .inventory
        .get_or_insert_with(|| inventory::Inventory::load(site.dir).map(Rc::new))
        .clone()
    else {
        return Some(false);
    };
    if inventory
        .files
        .get(site.new_path)
        .is_none_or(|file| file.source.as_bytes() != pair.new.as_ref())
    {
        return Some(false);
    }
    if references.iter().any(|reference| {
        matches!(
            LangId::from_path(camino::Utf8Path::new(&reference.path)),
            Ok(LangId::Javascript | LangId::Typescript | LangId::Tsx)
        ) && !inventory.files.contains_key(&reference.path)
    }) {
        return Some(false);
    }
    let Some(plan) = slots::collect(&inventory, site, &removal) else {
        return Some(false);
    };
    if flows::audit(&inventory, &plan, &removal).is_none() {
        return Some(false);
    }
    let key = removal.removed.iter().cloned().collect::<Vec<_>>();
    Some(
        *cache
            .residue
            .entry(key)
            .or_insert_with(|| residue::absent(&inventory, &removal)),
    )
}

fn span(node: Node<'_>) -> Span {
    (node.start_byte(), node.end_byte())
}
fn text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    node.utf8_text(source.as_bytes()).ok()
}
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect()
}
fn only(node: Node<'_>) -> Option<Node<'_>> {
    let children = children(node);
    match children.as_slice() {
        [child] => Some(*child),
        _ => None,
    }
}
fn token(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| !child.is_named() && child.kind() == kind)
}
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || byte == b'_'
                || byte == b'$'
                || (index > 0 && byte.is_ascii_digit())
        })
}
fn unparen(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_DEPTH {
        if node.kind() != "parenthesized_expression" {
            return Some(node);
        }
        node = only(node)?;
    }
    None
}
fn ordinary_call(node: Node<'_>) -> bool {
    node.kind() == "call_expression"
        && !token(node, "?.")
        && node.child_by_field_name("optional_chain").is_none()
        && node.child_by_field_name("type_arguments").is_none()
        && node
            .child_by_field_name("arguments")
            .is_some_and(|arguments| {
                arguments.kind() == "arguments"
                    && children(arguments)
                        .iter()
                        .all(|node| node.kind() != "spread_element")
            })
}
fn outer_declaration(node: Node<'_>) -> Option<Node<'_>> {
    let mut outer = node;
    if outer.kind() == "variable_declarator" {
        outer = outer.parent()?;
    }
    if outer.parent()?.kind() == "export_statement" {
        outer = outer.parent()?;
    }
    (outer.parent()?.kind() == "program").then_some(outer)
}
fn exported(node: Node<'_>) -> bool {
    let declaration = if node.kind() == "variable_declarator" {
        node.parent()
    } else {
        Some(node)
    };
    declaration
        .and_then(|node| node.parent())
        .is_some_and(|parent| parent.kind() == "export_statement" && !token(parent, "default"))
}
