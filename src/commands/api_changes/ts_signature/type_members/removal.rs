//! 単純な property signature の削除だけを分類する (Issue #141)。

use super::*;
use crate::commands::api_changes::ts_required_members::{
    MemberChangeMode, RequiredMemberChange, RequiredMemberDelta, TypeAtom, simple_type,
};

pub(crate) fn classify_member_property_removal(
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
        return if narrowing::root_has_object_type(old_tree.root_node(), site.name, &src.old)
            || narrowing::root_has_object_type(new_tree.root_node(), site.name, &src.new)
        {
            RequiredMemberChange::Unsupported
        } else {
            RequiredMemberChange::NotApplicable
        };
    };
    let (Some(old_body), Some(new_body)) = (type_member_body(old_decl), type_member_body(new_decl))
    else {
        return RequiredMemberChange::NotApplicable;
    };
    let (Some(old), Some(new)) = (
        normalized_members(old_body, &src.old),
        normalized_members(new_body, &src.new),
    ) else {
        return RequiredMemberChange::Unsupported;
    };
    // 型変更は縮小側へ渡す。名前を比べられない既存メンバーの消失は未証明とする。
    let removed = old
        .iter()
        .filter(|(signature, member)| {
            !new.iter().any(|(next, _)| next == signature)
                && member_name(*member, &src.old).flatten().is_none_or(|name| {
                    !new.iter().any(|(_, next)| {
                        member_name(*next, &src.new).flatten() == Some(name.clone())
                    })
                })
        })
        .collect::<Vec<_>>();
    if removed.is_empty() {
        return if old.len() > new.len() {
            RequiredMemberChange::Unsupported
        } else {
            RequiredMemberChange::NotApplicable
        };
    }
    if old_tree.root_node().has_error()
        || new_tree.root_node().has_error()
        || old_decl.kind() != new_decl.kind()
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
        || new
            .iter()
            .any(|(signature, _)| !old.iter().any(|(previous, _)| previous == signature))
    {
        return RequiredMemberChange::Unsupported;
    }
    for (decl, members, source) in [
        (old_decl, &old, src.old.as_slice()),
        (new_decl, &new, src.new.as_ref()),
    ] {
        let mut names = HashSet::new();
        if crate::engine::tree_walk::any_node(
            decl,
            crate::engine::tree_walk::Children::Named,
            |node| {
                node.kind() == "extends_type_clause"
                    || (node.kind() == "type_identifier"
                        && node.utf8_text(source).ok() == Some(site.name)
                        && node.parent().is_some_and(|parent| parent.id() != decl.id()))
            },
        ) {
            return RequiredMemberChange::Unsupported;
        }
        for (_, member) in members {
            let Some(Some(name)) = member_name(*member, source) else {
                return RequiredMemberChange::Unsupported;
            };
            let scalar = member
                .child_by_field_name("type")
                .and_then(|annotation| {
                    let mut cursor = annotation.walk();
                    let mut children = annotation
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() != "comment");
                    let ty = children.next()?;
                    children
                        .next()
                        .is_none()
                        .then(|| simple_type(ty, source))
                        .flatten()
                })
                .is_some_and(|ty| {
                    ty.iter().all(|atom| {
                        !matches!(
                            atom,
                            TypeAtom::Array(_)
                                | TypeAtom::Any
                                | TypeAtom::Unknown
                                | TypeAtom::StrLit(None)
                        )
                    })
                });
            if member.kind() != "property_signature"
                || name.contains('\\')
                || !names.insert(name)
                || !scalar
            {
                return RequiredMemberChange::Unsupported;
            }
        }
    }
    let retained = new
        .iter()
        .filter(|(_, member)| !ts_property_signature_is_optional(*member))
        .map(|(_, member)| {
            let name = member_name(*member, &src.new).flatten()?;
            let annotation = member.child_by_field_name("type")?;
            let mut cursor = annotation.walk();
            let ty = annotation
                .named_children(&mut cursor)
                .find(|n| n.kind() != "comment")?;
            Some(
                crate::commands::api_changes::ts_required_members::RequiredProperty {
                    name,
                    ty: simple_type(ty, &src.new)?,
                },
            )
        })
        .collect::<Option<Vec<_>>>();
    let Some(retained) = retained else {
        return RequiredMemberChange::Unsupported;
    };
    RequiredMemberChange::Supported(RequiredMemberDelta {
        added: retained,
        mode: MemberChangeMode::Removal(
            removed
                .iter()
                .filter_map(|(_, member)| member_name(*member, &src.old).flatten())
                .collect(),
        ),
    })
}
