//! exported object の shape 変更に対する互換性判定。

use super::*;

pub(crate) fn detect_object_members_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_JS_LANGS)?;
    let src = sources.get(site)?;
    let old_keys = extract_object_member_keys(&src.old, lang, site.name)?;
    let new_keys = extract_object_member_keys(&src.new, lang, site.name)?;
    if old_keys.record_keys.is_some() != new_keys.record_keys.is_some() {
        return None;
    }
    if old_keys.declarator_type != new_keys.declarator_type
        || old_keys.wrappers != new_keys.wrappers
        || old_keys.methods != new_keys.methods
    {
        return None;
    }
    for (key, old_wrappers) in &old_keys.entry_wrappers {
        if let Some(new_wrappers) = new_keys.entry_wrappers.get(key)
            && new_wrappers != old_wrappers
        {
            return None;
        }
    }
    let has_added_member = new_keys
        .member_keys
        .difference(&old_keys.member_keys)
        .next()
        .is_some();
    let has_added_record_entry = match (&old_keys.record_keys, &new_keys.record_keys) {
        (Some(old_record), Some(new_record)) => {
            if old_record.difference(new_record).next().is_some() {
                return None;
            }
            new_record.difference(old_record).next().is_some()
        }
        (None, None) => false,
        _ => return None,
    };
    let removed_members: Vec<&String> = old_keys
        .member_keys
        .difference(&new_keys.member_keys)
        .collect();
    if removed_members.is_empty() && !has_added_member && !has_added_record_entry {
        return None;
    }
    for (key, old_value) in &old_keys.entry_values {
        if let Some(new_value) = new_keys.entry_values.get(key)
            && new_value != old_value
        {
            return None;
        }
    }
    let removed_member_set: HashSet<&str> =
        removed_members.iter().map(|key| key.as_str()).collect();
    if member_access_keys_have_ref(site.dir, &removed_member_set, site.name) {
        return None;
    }
    Some(site.compatible("unused_object_members"))
}

/// キーが確定しない添字と、このオブジェクトを渡し得る参照が両方あるかを調べる。
pub(super) fn may_read_dynamic_key(root: tree_sitter::Node, source: &[u8], owner: &str) -> bool {
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    use std::ops::ControlFlow;
    let mut dynamic = false;
    let mut exposed = false;
    let mut ancestors = Vec::new();
    walk_preorder(root, Children::Named, |node, depth| {
        ancestors.truncate(depth);
        let index = match node.kind() {
            "subscript_expression" => node.child_by_field_name("index"),
            "lookup_type" => node
                .named_children(&mut node.walk())
                .filter(|n| !n.is_extra())
                .nth(1),
            "computed_property_name" => node
                .named_children(&mut node.walk())
                .find(|n| !n.is_extra()),
            _ => None,
        };
        dynamic |= index.is_some_and(|key| static_index_key(key).is_none());
        if !exposed
            && matches!(
                node.kind(),
                "identifier" | "property_identifier" | "shorthand_property_identifier"
            )
            && node
                .utf8_text(source)
                .ok()
                .and_then(crate::engine::js_string::decoded)
                .as_deref()
                == Some(owner)
        {
            exposed = outside_static_read(node, &ancestors);
        }
        if dynamic && exposed {
            return ControlFlow::Break(());
        }
        ancestors.push(node);
        ControlFlow::Continue(Walk::Descend)
    })
    .is_break()
}

fn outside_static_read<'a>(
    mut node: tree_sitter::Node<'a>,
    ancestors: &[tree_sitter::Node<'a>],
) -> bool {
    let mut selected_member = false;
    for &parent in ancestors.iter().rev() {
        match parent.kind() {
            "variable_declarator" if parent.child_by_field_name("name") == Some(node) => {
                return false;
            }
            "import_specifier" => return parent.child_by_field_name("alias").is_some(),
            "pair" if parent.child_by_field_name("key") == Some(node) => return false,
            "member_expression" if parent.child_by_field_name("property") == Some(node) => {}
            "member_expression" if parent.child_by_field_name("object") == Some(node) => {
                selected_member = true;
            }
            "subscript_expression" if parent.child_by_field_name("object") == Some(node) => {
                if parent
                    .child_by_field_name("index")
                    .and_then(static_index_key)
                    .is_none()
                {
                    return true;
                }
                selected_member = true;
            }
            "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
            | "parenthesized_expression" => {}
            _ => break,
        }
        node = parent;
    }
    !selected_member
}
