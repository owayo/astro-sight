//! TypeScript の型宣言メンバーを、宣言の改行位置に依らず比較する。

use super::*;
use crate::models::symbol::Symbol;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

mod addition;
mod narrowing;
mod properties;
mod removal;
mod trailing_parameters;
pub(crate) use addition::classify_required_member_addition;
pub(crate) use narrowing::classify_member_type_narrowing;
use properties::{is_scalar_atom, simple_annotation_type};
pub(crate) use removal::classify_member_property_removal;
pub(crate) use trailing_parameters::detect_trailing_default_type_params_compatible_mod;

pub(crate) fn declaration_signature(
    symbol: &Symbol,
    root: Node<'_>,
    source: &[u8],
) -> Option<String> {
    let start = tree_sitter::Point {
        row: symbol.range.start.line,
        column: symbol.range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol.range.end.line,
        column: symbol.range.end.column,
    };
    let mut node = root.descendant_for_point_range(start, end)?;
    loop {
        if matches!(
            node.kind(),
            "type_alias_declaration" | "interface_declaration"
        ) {
            if !node_field_name_eq(node, &symbol.name, source) {
                return None;
            }
            let outer = node
                .parent()
                .filter(|parent| parent.kind() == "export_statement")
                .unwrap_or(node);
            if root.has_error() {
                return Some(super::super::signature::normalize_signature_whitespace(
                    outer.utf8_text(source).ok()?.as_bytes(),
                ));
            }
            if let Some(body) = type_member_body(node) {
                let header = normalized_prefix(outer, body, source)?;
                let footer = normalized_suffix(outer, body, source)?;
                let members = normalized_members(body, source)?;
                return Some(format!(
                    "{header} {{ {} }} {footer}",
                    canonical_members(&members, source)
                ));
            }
            return normalized_node(outer, source);
        }
        node = node.parent()?;
    }
}

/// 追加した optional メンバーが既存の型利用を壊さない場合に限り互換。
pub(crate) fn detect_type_members_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
    public_name_differs: bool,
) -> Option<Result<CompatibleApiModification, ()>> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    if !matches!(site.kind, "type" | "trait" | "interface") {
        return None;
    }
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    let change = optional_member_change(
        old_tree.root_node(),
        &src.old,
        new_tree.root_node(),
        &src.new,
        site.name,
    )?;
    // 宣言ファイル用に隠す前の型も、同じ optional キーだけを追加した場合に限る。
    let same_source_change = || -> Option<bool> {
        if src.original_new() == src.new.as_ref() && src.original_old() == src.old.as_slice() {
            return Some(true);
        }
        let old = parser::parse_source(src.original_old(), lang).ok()?;
        let new = parser::parse_source(src.original_new(), lang).ok()?;
        let raw = optional_member_change(
            old.root_node(),
            src.original_old(),
            new.root_node(),
            src.original_new(),
            site.name,
        )?;
        Some(raw.retained.is_some() && raw.added == change.added)
    };
    let compatible = !public_name_differs
        && site.new_path == site.old_path
        && same_source_change() == Some(true)
        && change.retained.is_some_and(|retained| {
            trailing_parameters::prove_optional_member_uses(
                index,
                site,
                cache,
                src.original_new(),
                retained,
                change.added,
            )
            .is_some()
        });
    Some(
        compatible
            .then(|| site.compatible("optional_type_members"))
            .ok_or(()),
    )
}

struct OptionalMemberChange {
    retained: Option<HashSet<String>>,
    added: HashSet<String>,
}

fn optional_member_change(
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
    name: &str,
) -> Option<OptionalMemberChange> {
    if old_root.has_error() || new_root.has_error() {
        return None;
    }
    let (old_decl, old_outer) = unique_type_declaration(old_root, name, old_source)?;
    let (new_decl, new_outer) = unique_type_declaration(new_root, name, new_source)?;
    if old_decl.kind() != new_decl.kind() {
        return None;
    }
    let old_body = type_member_body(old_decl)?;
    let new_body = type_member_body(new_decl)?;
    if normalized_prefix(old_outer, old_body, old_source)?
        != normalized_prefix(new_outer, new_body, new_source)?
        || normalized_suffix(old_outer, old_body, old_source)?
            != normalized_suffix(new_outer, new_body, new_source)?
    {
        return None;
    }
    let old_members = normalized_members(old_body, old_source)?;
    let new_members = normalized_members(new_body, new_source)?;
    if new_members.len() <= old_members.len() {
        return None;
    }
    let retained = old_members
        .iter()
        .map(|(_, member)| member_name(*member, old_source).flatten())
        .collect::<Option<HashSet<_>>>();
    let added = optional_superset(old_members, new_members, old_source, new_source)?;
    let plain = new_outer.kind() == "export_statement"
        && new_decl.child_by_field_name("type_parameters").is_none()
        && !new_decl
            .named_children(&mut new_decl.walk())
            .any(|child| child.kind() == "extends_type_clause");
    Some(OptionalMemberChange {
        retained: retained.filter(|_| plain),
        added,
    })
}

fn unique_type_declaration<'a>(
    root: Node<'a>,
    name: &str,
    source: &[u8],
) -> Option<(Node<'a>, Node<'a>)> {
    let mut matches = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "type_alias_declaration" | "interface_declaration"
        ) && node_field_name_eq(node, name, source)
        {
            matches.push(node);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    if matches.len() != 1 {
        return None;
    }
    let decl = matches[0];
    let parent = decl.parent()?;
    let outer = if parent.id() == root.id() {
        decl
    } else if parent.kind() == "export_statement" && parent.parent()?.id() == root.id() {
        parent
    } else {
        return None;
    };
    Some((decl, outer))
}

/// 一意な宣言を証明できない場合も、直下の同名 object 型の存在は別途確かめる。
fn root_has_object_type(root: Node<'_>, name: &str, source: &[u8]) -> bool {
    let mut cursor = root.walk();
    root.named_children(&mut cursor).any(|statement| {
        let declaration = if statement.kind() == "export_statement" {
            statement.child_by_field_name("declaration")
        } else {
            Some(statement)
        };
        declaration.is_some_and(|node| {
            matches!(
                node.kind(),
                "interface_declaration" | "type_alias_declaration"
            ) && node_field_name_eq(node, name, source)
                && type_member_body(node).is_some()
        })
    })
}

fn type_member_body(decl: Node<'_>) -> Option<Node<'_>> {
    match decl.kind() {
        "type_alias_declaration" => decl
            .child_by_field_name("value")
            .filter(|body| body.kind() == "object_type"),
        "interface_declaration" => decl.child_by_field_name("body"),
        _ => None,
    }
}

/// 内側のノードより前のトークン列。読取り失敗の扱いは各分類器に委ねる。
fn normalized_prefix(outer: Node<'_>, inner: Node<'_>, source: &[u8]) -> Option<String> {
    normalized_range(outer, source, outer.start_byte(), inner.start_byte())
}

/// 内側のノードより後のトークン列。読取り失敗の扱いは各分類器に委ねる。
fn normalized_suffix(outer: Node<'_>, inner: Node<'_>, source: &[u8]) -> Option<String> {
    normalized_range(outer, source, inner.end_byte(), outer.end_byte())
}

fn normalized_range(node: Node<'_>, source: &[u8], start: usize, end: usize) -> Option<String> {
    let tokens = signature_tokens_in_range(node, source, start, end)?;
    Some(render_tokens(tokens))
}

fn normalized_node(node: Node<'_>, source: &[u8]) -> Option<String> {
    Some(render_tokens(node_signature_tokens(node, source)?))
}

fn render_tokens(tokens: SigTokens) -> String {
    tokens
        .0
        .into_iter()
        .filter(|(kind, _)| kind != "comment")
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_members<'a>(body: Node<'a>, source: &[u8]) -> Option<Vec<(String, Node<'a>)>> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|member| member.kind() != "comment")
        .map(|member| {
            let mut tokens = node_signature_tokens(member, source)?;
            tokens.0.retain(|(kind, _)| kind != "comment");
            while tokens
                .0
                .last()
                .is_some_and(|(_, text)| text == ";" || text == ",")
            {
                tokens.0.pop();
            }
            Some((render_tokens(tokens), member))
        })
        .collect()
}

fn member_name(member: Node<'_>, source: &[u8]) -> Option<Option<String>> {
    if !matches!(member.kind(), "property_signature" | "method_signature") {
        return Some(None);
    }
    let name = member.child_by_field_name("name")?;
    if name.kind() != "property_identifier" {
        return None;
    }
    Some(Some(name.utf8_text(source).ok()?.to_string()))
}

/// Property の並べ替えだけは意味を変えない。call / construct / method の順序は
/// overload 解決に関わるため、元の順番を残してシグネチャを作る。
fn canonical_members(members: &[(String, Node<'_>)], source: &[u8]) -> String {
    let original = || {
        members
            .iter()
            .map(|(signature, _)| signature.as_str())
            .collect::<Vec<_>>()
            .join(" ; ")
    };
    let mut properties = Vec::new();
    let mut ordered = Vec::new();
    let mut property_names = HashSet::new();
    for (signature, member) in members {
        if member.kind() == "property_signature" {
            let Some(Some(name)) = member_name(*member, source) else {
                return original();
            };
            if !property_names.insert(name) {
                return original();
            }
            properties.push(signature.as_str());
        } else {
            ordered.push(signature.as_str());
        }
    }
    properties.sort_unstable();
    format!(
        "properties: {} | ordered: {}",
        properties.join(" ; "),
        ordered.join(" ; ")
    )
}

fn optional_superset(
    old: Vec<(String, Node<'_>)>,
    new: Vec<(String, Node<'_>)>,
    old_source: &[u8],
    new_source: &[u8],
) -> Option<HashSet<String>> {
    let mut old_names = HashSet::new();
    let mut old_property_names = HashSet::new();
    let mut old_properties = Vec::new();
    let mut old_ordered = Vec::new();
    for (signature, member) in old {
        let name = member_name(member, old_source)?;
        if let Some(name) = name {
            if member.kind() == "property_signature" && !old_property_names.insert(name.clone()) {
                return None;
            }
            old_names.insert(name);
        }
        if member.kind() == "property_signature" {
            old_properties.push(signature);
        } else {
            old_ordered.push(signature);
        }
    }

    let mut new_properties: HashMap<String, Vec<Node<'_>>> = HashMap::new();
    let mut new_ordered = Vec::new();
    for (signature, member) in new {
        if member.kind() == "property_signature" {
            new_properties.entry(signature).or_default().push(member);
        } else {
            new_ordered.push((signature, member));
        }
    }
    for signature in old_properties {
        match new_properties.get_mut(&signature) {
            Some(nodes) if !nodes.is_empty() => {
                nodes.pop();
            }
            _ => return None,
        }
    }

    let mut added_names = HashSet::new();
    let mut accept_added = |member: Node<'_>| {
        if !matches!(member.kind(), "property_signature" | "method_signature")
            || !ts_property_signature_is_optional(member)
        {
            return false;
        }
        let Some(Some(name)) = member_name(member, new_source) else {
            return false;
        };
        !old_names.contains(&name) && added_names.insert(name)
    };
    for member in new_properties.values().flatten() {
        if !accept_added(*member) {
            return None;
        }
    }
    let mut old_ordered = old_ordered.into_iter();
    for (signature, member) in new_ordered {
        if old_ordered
            .as_slice()
            .first()
            .is_some_and(|expected| expected == &signature)
        {
            old_ordered.next();
        } else if !accept_added(member) {
            return None;
        }
    }
    old_ordered.next().is_none().then_some(added_names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_same_name_declaration_is_ambiguous_for_compatibility() {
        let source = b"export interface Shape { theme: string }\nnamespace Extra { export interface Shape { size?: number } }\n";
        let tree = parser::parse_source(source, crate::language::LangId::Typescript).unwrap();
        assert!(unique_type_declaration(tree.root_node(), "Shape", source).is_none());
    }
}
