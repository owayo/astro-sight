//! 文字列 union の要素変更と判別 union の要素削除を、限定した値の利用から証明する。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use tree_sitter::Node;

use super::ref_index::ApiRefIndex;
use super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::engine::js_scope::{Namespace, ScopeBinding, resolve_binding_scope};
use crate::engine::tree_walk::{Children, any_node};
use crate::language::LangId;
use crate::models::review::ApiModificationUnverifiedReason;

mod addition;
mod decode;
mod flows;
mod inventory;
mod residue;
mod shape;
mod slots;

/// escape を含む識別子の表記を、名前へ戻す。必須メンバーの証明でも使う。
pub(super) fn decode_identifier(raw: &str) -> Option<String> {
    decode::identifier(raw)
}

/// 置換の無い文字列の値を、escape を解いて返す。必須メンバーの証明でも使う。
pub(super) fn decode_string(node: Node<'_>, source: &str) -> Option<String> {
    decode::string(node, source)
}

const MAX_DEPTH: usize = 128;
type Span = (usize, usize);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Site {
    path: String,
    name: String,
    span: Span,
}

#[derive(Default)]
pub(super) struct ProofCache {
    inventory: Option<Option<Rc<inventory::Inventory>>>,
    residue: HashMap<Vec<String>, bool>,
}

/// 対象の変更形を認めた後の失敗は、参照行だけの証拠へ戻さない。
pub(super) fn prove_change(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut ProofCache,
) -> Option<Result<(), ApiModificationUnverifiedReason>> {
    if !matches!(site.lang_id, Some(LangId::Typescript | LangId::Tsx))
        || site.kind != "type"
        || site.old_path != site.new_path
    {
        return None;
    }
    let pair = sources.get(site)?;
    use ApiModificationUnverifiedReason::{LiteralUnionProofFailed, UnsupportedTypeChange};
    let (removal, objects) = match shape::classify(site, pair) {
        shape::Change::NotApplicable => return None,
        shape::Change::Unsupported => return Some(Err(LiteralUnionProofFailed)),
        shape::Change::UnsupportedAddition => return Some(Err(UnsupportedTypeChange)),
        shape::Change::Addition(values) => {
            return Some(
                cache
                    .inventory_for(index, site, &pair.new)
                    .and_then(|inventory| addition::audit(&inventory, site, &values))
                    .ok_or(UnsupportedTypeChange),
            );
        }
        shape::Change::Removal(removal) => (removal, None),
        shape::Change::ObjectRemoval(removal, objects) => (removal, Some(objects)),
    };
    let Some(inventory) = cache.inventory_for(index, site, &pair.new) else {
        return Some(Err(LiteralUnionProofFailed));
    };
    let Some(plan) = slots::collect(&inventory, site, &removal) else {
        return Some(Err(LiteralUnionProofFailed));
    };
    let audited = match objects.as_ref() {
        Some(objects) => flows::audit_objects(&inventory, &plan, &removal, objects),
        None => flows::audit(&inventory, &plan, &removal),
    };
    if audited.is_none() {
        return Some(Err(LiteralUnionProofFailed));
    }
    let key = removal.removed.iter().cloned().collect::<Vec<_>>();
    let absent = *cache
        .residue
        .entry(key)
        .or_insert_with(|| residue::absent(&inventory, &removal));
    Some(absent.then_some(()).ok_or(LiteralUnionProofFailed))
}

impl ProofCache {
    fn inventory_for(
        &mut self,
        index: &ApiRefIndex,
        site: &CompatibleModSite<'_>,
        source: &[u8],
    ) -> Option<Rc<inventory::Inventory>> {
        let references = index.complete_refs_for(site.name)?;
        let inventory = self
            .inventory
            .get_or_insert_with(|| inventory::Inventory::load(site.dir).map(Rc::new))
            .clone()?;
        if inventory.source(site.new_path)?.as_bytes() != source
            || references.iter().any(|reference| {
                matches!(
                    LangId::from_path(camino::Utf8Path::new(&reference.path)),
                    Ok(LangId::Javascript | LangId::Typescript | LangId::Tsx)
                ) && !inventory.files.contains_key(&reference.path)
            })
        {
            return None;
        }
        Some(inventory)
    }
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
fn pattern_contains(pattern: Node<'_>, source: &str, name: &str) -> bool {
    any_node(pattern, Children::Named, |node| {
        matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) && text(node, source) == Some(name)
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
