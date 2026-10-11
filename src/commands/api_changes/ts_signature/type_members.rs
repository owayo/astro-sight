//! TypeScript の型宣言メンバーを、宣言の改行位置に依らず比較する。

use super::*;
use crate::models::symbol::Symbol;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

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
                let header =
                    normalized_range(outer, source, outer.start_byte(), body.start_byte())?;
                let footer = normalized_range(outer, source, body.end_byte(), outer.end_byte())?;
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

/// 既存メンバーと宣言ヘッダが不変で、追加分が optional property のみなら互換。
pub(crate) fn detect_type_members_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    if !matches!(site.kind, "type" | "trait" | "interface") {
        return None;
    }
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return None;
    }
    let (old_decl, old_outer) = unique_type_declaration(old_tree.root_node(), site.name, &src.old)?;
    let (new_decl, new_outer) = unique_type_declaration(new_tree.root_node(), site.name, &src.new)?;
    if old_decl.kind() != new_decl.kind() {
        return None;
    }
    let old_body = type_member_body(old_decl)?;
    let new_body = type_member_body(new_decl)?;
    if normalized_range(
        old_outer,
        &src.old,
        old_outer.start_byte(),
        old_body.start_byte(),
    )? != normalized_range(
        new_outer,
        &src.new,
        new_outer.start_byte(),
        new_body.start_byte(),
    )? || normalized_range(
        old_outer,
        &src.old,
        old_body.end_byte(),
        old_outer.end_byte(),
    )? != normalized_range(
        new_outer,
        &src.new,
        new_body.end_byte(),
        new_outer.end_byte(),
    )? {
        return None;
    }
    let old_members = normalized_members(old_body, &src.old)?;
    let new_members = normalized_members(new_body, &src.new)?;
    if new_members.len() <= old_members.len()
        || !optional_superset(old_members, new_members, &src.old, &src.new)
    {
        return None;
    }
    Some(site.compatible("optional_type_members"))
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

fn type_member_body(decl: Node<'_>) -> Option<Node<'_>> {
    match decl.kind() {
        "type_alias_declaration" => decl
            .child_by_field_name("value")
            .filter(|body| body.kind() == "object_type"),
        "interface_declaration" => decl.child_by_field_name("body"),
        _ => None,
    }
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
) -> bool {
    let mut old_names = HashSet::new();
    let mut old_property_names = HashSet::new();
    let mut old_properties = Vec::new();
    let mut old_ordered = Vec::new();
    for (signature, member) in old {
        let Some(name) = member_name(member, old_source) else {
            return false;
        };
        if let Some(name) = name {
            if member.kind() == "property_signature" && !old_property_names.insert(name.clone()) {
                return false;
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
            _ => return false,
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
            return false;
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
            return false;
        }
    }
    old_ordered.next().is_none()
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
