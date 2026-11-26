//! 追加した props の型を、名前付き import または定数タプルの添字から確かめる。

use super::*;
use crate::commands::api_changes::{
    js_const::unique_top_level_const,
    source_pair::relative_ts_source,
    ts_required_members::{only_named_child, visit_elements},
};

pub(super) fn imported(
    root: Node<'_>,
    source: &[u8],
    name: &str,
    value: Node<'_>,
    site: &CompatibleModSite<'_>,
) -> Option<bool> {
    let (specifier, exported) = type_dependencies::named_type_import(root, source, name)?;
    let (target, lang) = relative_ts_source(site.dir, site.new_path, specifier)?;
    let tree = parser::parse_source(&target, lang).ok()?;
    if tree.root_node().has_error() || target.windows(9).any(|text| text == b"@internal") {
        return None;
    }
    let ty = type_dependencies::exported_type_alias(tree.root_node(), &target, &exported)?;
    ts_union_type_parts(ty)
        .into_iter()
        .filter(|part| !part.is_extra())
        .try_fold(false, |found, part| {
            Some(scalar_type_matches(value, source, part, &target)? || found)
        })
}

/// トップレベルの別名 `(typeof xs)[number]['key']` を、明示型のない `const xs = [...] as const` で解く。
pub(super) fn indexed(
    root: Node<'_>,
    ty: Node<'_>,
    value: Node<'_>,
    source: &[u8],
) -> Option<bool> {
    let alias = ty.parent()?;
    if alias.kind() != "type_alias_declaration"
        || alias.child_by_field_name("value")?.id() != ty.id()
    {
        return None;
    }
    let parent = alias.parent()?;
    if parent.id() != root.id()
        && !(parent.kind() == "export_statement" && parent.parent()?.id() == root.id())
    {
        return None;
    }
    let (inner, key) = type_dependencies::lookup_type_parts(ty)?;
    let (group, index) = type_dependencies::lookup_type_parts(inner)?;
    if group.kind() != "parenthesized_type"
        || index.kind() != "predefined_type"
        || index.utf8_text(source).ok()? != "number"
        || key.kind() != "literal_type"
    {
        return None;
    }
    let key = only_named_child(key)?;
    let query = only_named_child(group)?;
    let name = only_named_child(query)?;
    if key.kind() != "string" || query.kind() != "type_query" || name.kind() != "identifier" {
        return None;
    }
    let key = static_js_string_text(key, source)?;
    let (decl, _) = unique_top_level_const(root, source, name.utf8_text(source).ok()?)?;
    let assertion = decl.child_by_field_name("value")?;
    if decl.child_by_field_name("type").is_some()
        || assertion.kind() != "as_expression"
        || !assertion
            .children(&mut assertion.walk())
            .any(|n| n.kind() == "const")
    {
        return None;
    }
    let array = only_named_child(assertion)?;
    let mut matches = Some(false);
    visit_elements(array, |_, object| {
        matches = matches.and_then(|found| {
            let literal = projected_literal(object, key, source)?;
            Some(literal_matches(value, source, literal, source)? || found)
        });
    })?;
    matches
}

fn projected_literal<'tree>(object: Node<'tree>, key: &str, source: &[u8]) -> Option<Node<'tree>> {
    if object.kind() != "object" || key.contains('\\') {
        return None;
    }
    let mut names = HashSet::new();
    let mut result = None;
    for pair in object
        .named_children(&mut object.walk())
        .filter(|n| !n.is_extra())
    {
        if pair.kind() != "pair" {
            return None;
        }
        let name = object_key_text(pair.child_by_field_name("key")?, source)?;
        if name.contains('\\') || name == "__proto__" || !names.insert(name.clone()) {
            return None;
        }
        if name == key {
            result = Some(pair.child_by_field_name("value")?);
        }
    }
    result
}
