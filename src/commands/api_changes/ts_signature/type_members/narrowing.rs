//! 既存 property の scalar union から型を除く変更だけを分類する (Issue #139)。

use super::*;
use crate::commands::api_changes::ts_required_members::{
    RequiredMemberChange, RequiredMemberDelta, RequiredProperty, TypeAtom, simple_type,
};

pub(crate) fn classify_member_type_narrowing(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> RequiredMemberChange {
    let Some(lang) = site.lang_in(TS_ONLY_LANGS) else {
        return RequiredMemberChange::NotApplicable;
    };
    if !matches!(site.kind, "type" | "trait" | "interface") {
        return RequiredMemberChange::NotApplicable;
    }
    let Some(src) = sources.get(site) else {
        return RequiredMemberChange::Unsupported;
    };
    let Some((old_tree, new_tree)) = src.parse_pair(lang) else {
        return RequiredMemberChange::Unsupported;
    };
    let (Some((old_decl, old_outer)), Some((new_decl, new_outer))) = (
        unique_type_declaration(old_tree.root_node(), site.name, &src.old),
        unique_type_declaration(new_tree.root_node(), site.name, &src.new),
    ) else {
        return if root_has_object_type(old_tree.root_node(), site.name, &src.old)
            || root_has_object_type(new_tree.root_node(), site.name, &src.new)
        {
            RequiredMemberChange::Unsupported
        } else {
            RequiredMemberChange::NotApplicable
        };
    };
    let (old_body, new_body) = (type_member_body(old_decl), type_member_body(new_decl));
    let (old_body, new_body) = match (old_body, new_body) {
        (Some(old), Some(new)) => (old, new),
        (None, None) => return RequiredMemberChange::NotApplicable,
        _ => return RequiredMemberChange::Unsupported,
    };
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return RequiredMemberChange::Unsupported;
    }
    let (Some(old), Some(new)) = (
        normalized_members(old_body, &src.old),
        normalized_members(new_body, &src.new),
    ) else {
        return RequiredMemberChange::Unsupported;
    };
    // 既存の同名メンバーが変わった場合だけ引き受ける。対応できない変更も行の証拠へ戻さない。
    let changed_existing = old.iter().any(|(signature, member)| {
        let Some(Some(name)) = member_name(*member, &src.old) else {
            return false;
        };
        new.iter().any(|(next, member)| {
            member_name(*member, &src.new) == Some(Some(name.clone())) && next != signature
        })
    });
    let unknown_member_changed = old
        .iter()
        .any(|(_, member)| member_name(*member, &src.old).flatten().is_none())
        || new
            .iter()
            .any(|(_, member)| member_name(*member, &src.new).flatten().is_none());
    let unknown_member_changed = unknown_member_changed
        && canonical_members(&old, &src.old) != canonical_members(&new, &src.new);
    if !changed_existing && !unknown_member_changed {
        return RequiredMemberChange::NotApplicable;
    }
    if old_tree.root_node().has_error()
        || new_tree.root_node().has_error()
        || old_decl.kind() != new_decl.kind()
        || old.len() != new.len()
        || old_decl.child_by_field_name("type_parameters").is_some()
        || new_decl.child_by_field_name("type_parameters").is_some()
        || normalized_range(
            old_outer,
            &src.old,
            old_outer.start_byte(),
            old_body.start_byte(),
        ) != normalized_range(
            new_outer,
            &src.new,
            new_outer.start_byte(),
            new_body.start_byte(),
        )
        || normalized_range(
            old_outer,
            &src.old,
            old_body.end_byte(),
            old_outer.end_byte(),
        ) != normalized_range(
            new_outer,
            &src.new,
            new_body.end_byte(),
            new_outer.end_byte(),
        )
        || crate::engine::tree_walk::any_node(
            new_decl,
            crate::engine::tree_walk::Children::Named,
            |node| node.kind() == "extends_type_clause",
        )
    {
        return RequiredMemberChange::Unsupported;
    }
    let mut old_by_name = HashMap::new();
    for (signature, member) in old {
        let Some(Some(name)) = member_name(member, &src.old) else {
            return RequiredMemberChange::Unsupported;
        };
        if member.kind() != "property_signature"
            || name.contains('\\')
            || old_by_name.insert(name, (signature, member)).is_some()
        {
            return RequiredMemberChange::Unsupported;
        }
    }
    let mut seen = HashSet::new();
    let mut narrowed = Vec::new();
    for (signature, member) in new {
        let Some(Some(name)) = member_name(member, &src.new) else {
            return RequiredMemberChange::Unsupported;
        };
        if member.kind() != "property_signature"
            || name.contains('\\')
            || !seen.insert(name.clone())
        {
            return RequiredMemberChange::Unsupported;
        }
        // 未変更の関数値・未知の型も this 経由の書込みを隠し得るため、縮小では型全体を限定する。
        let mut type_cursor = member.walk();
        let scalar_properties = member
            .child_by_field_name("type")
            .and_then(|annotation| {
                let ty = annotation
                    .named_children(&mut type_cursor)
                    .find(|n| n.kind() != "comment")?;
                simple_type(ty, &src.new)
            })
            .is_some_and(|atoms| {
                atoms.iter().all(|atom| {
                    !matches!(
                        atom,
                        TypeAtom::Array(_)
                            | TypeAtom::Any
                            | TypeAtom::Unknown
                            | TypeAtom::StrLit(None)
                    )
                })
            });
        if !scalar_properties {
            return RequiredMemberChange::Unsupported;
        }
        let Some((previous, old_member)) = old_by_name.get(&name) else {
            return RequiredMemberChange::Unsupported;
        };
        if previous == &signature {
            continue;
        }
        if ts_property_signature_is_optional(member)
            || ts_property_signature_is_optional(*old_member)
        {
            return RequiredMemberChange::Unsupported;
        }
        let (Some(old_annotation), Some(new_annotation)) = (
            old_member.child_by_field_name("type"),
            member.child_by_field_name("type"),
        ) else {
            return RequiredMemberChange::Unsupported;
        };
        if normalized_range(
            *old_member,
            &src.old,
            old_member.start_byte(),
            old_annotation.start_byte(),
        ) != normalized_range(
            member,
            &src.new,
            member.start_byte(),
            new_annotation.start_byte(),
        ) || normalized_range(
            *old_member,
            &src.old,
            old_annotation.end_byte(),
            old_member.end_byte(),
        ) != normalized_range(
            member,
            &src.new,
            new_annotation.end_byte(),
            member.end_byte(),
        ) {
            return RequiredMemberChange::Unsupported;
        }
        let read = |annotation: Node<'_>, source: &[u8]| {
            let mut cursor = annotation.walk();
            let mut children = annotation
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment");
            let node = children.next()?;
            if children.next().is_some() {
                return None;
            }
            simple_type(node, source)
        };
        let (Some(before), Some(after)) = (
            read(old_annotation, &src.old),
            read(new_annotation, &src.new),
        ) else {
            return RequiredMemberChange::Unsupported;
        };
        let scalar = |atom: &TypeAtom| {
            !matches!(
                atom,
                TypeAtom::Array(_) | TypeAtom::Any | TypeAtom::Unknown | TypeAtom::StrLit(None)
            )
        };
        if !before.iter().all(scalar)
            || !after.iter().all(scalar)
            || after.len() >= before.len()
            || !after.iter().all(|atom| before.contains(atom))
        {
            return RequiredMemberChange::Unsupported;
        }
        narrowed.push(RequiredProperty { name, ty: after });
    }
    if narrowed.is_empty() {
        RequiredMemberChange::Unsupported
    } else {
        RequiredMemberChange::Supported(RequiredMemberDelta {
            added: narrowed,
            narrowing: true,
        })
    }
}

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
            ) && super::super::super::node_field_name_eq(node, name, source)
                && type_member_body(node).is_some()
        })
    })
}
