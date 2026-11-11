//! null だけの入力拡大と、その利用方向の証拠。

use tree_sitter::Node;

use super::removed_params::{
    OptionalRemovalCache, UseRule, prove_repository_uses, unique_function_declaration,
};
use super::type_dependencies::{DependencyEnvironment, type_dependencies_unchanged};
use super::*;
use crate::models::impact::DiffFile;
use crate::models::review::{
    ApiChanges, NullableTypeChange, TypeChangeDirection, TypeChangeEvidence, TypeChangePosition,
    TypeChangeUsage,
};

pub(crate) fn detect_nullable_input_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
    diff_files: &[DiffFile],
) -> Option<CompatibleApiModification> {
    let lang = nullable_site(site)?;
    let src = sources.get(site)?;
    let (old, new) = src.parse_pair(lang)?;
    let analysis = analyze(
        old.root_node(),
        &src.old,
        new.root_node(),
        &src.new,
        site.name,
    )?;
    if !analysis.same_contract
        || analysis.changes.iter().any(|change| {
            change.position != TypeChangePosition::Input
                || change.direction != TypeChangeDirection::Widening
        })
    {
        return None;
    }
    let refs = index.complete_refs_for(site.name)?;
    if refs.is_empty() {
        return None;
    }
    {
        let evidence = cache.evidence(site.dir)?;
        let packages = evidence.relaxation()?;
        if !packages.consumers_are_local() {
            return None;
        }
        let changed_paths = diff_files
            .iter()
            .flat_map(|file| [&file.old_path, &file.new_path])
            .filter(|path| *path != "/dev/null")
            .map(|path| crate::git_support::normalize_workspace_separators(path))
            .collect();
        if !type_dependencies_unchanged(
            &DependencyEnvironment {
                target_path: site.new_path,
                changed_paths: &changed_paths,
                registry_dependency: &|specifier| packages.is_registry_dependency(specifier),
                declared_globally: &|name| evidence.declares_global(name),
            },
            old.root_node(),
            &src.old,
            new.root_node(),
            &src.new,
            &analysis.type_roots,
        ) {
            return None;
        }
    }
    let (inventory, evidence) = cache.inventory(site.dir, site.name, site.new_path)?;
    if !inventory.has_source(site.new_path, src.original_new()) {
        return None;
    }
    prove_repository_uses(
        refs,
        &inventory,
        evidence.relaxation(),
        site.new_path,
        site.name,
        UseRule::CallArity(analysis.arity),
    )?;
    Some(site.compatible("nullable_input_widening"))
}

fn nullable_site(site: &CompatibleModSite<'_>) -> Option<crate::language::LangId> {
    if site.kind != "function" || site.name.contains('.') || site.old_path != site.new_path {
        return None;
    }
    site.lang_in(TS_ONLY_LANGS)
}

struct Analysis<'tree> {
    changes: Vec<NullableTypeChange>,
    same_contract: bool,
    arity: usize,
    type_roots: Vec<Node<'tree>>,
}

fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

fn inner(mut node: Node<'_>) -> Option<Node<'_>> {
    while matches!(node.kind(), "type_annotation" | "parenthesized_type") {
        let children = named(node);
        let [child] = children.as_slice() else {
            return None;
        };
        node = *child;
    }
    Some(node)
}

fn same_range(
    old: Node<'_>,
    a: &[u8],
    old_range: std::ops::Range<usize>,
    new: Node<'_>,
    b: &[u8],
    new_range: std::ops::Range<usize>,
) -> bool {
    signature_tokens_in_range(old, a, old_range.start, old_range.end)
        .zip(signature_tokens_in_range(
            new,
            b,
            new_range.start,
            new_range.end,
        ))
        .is_some_and(|(old, new)| old == new)
}

fn shell(
    old: Node<'_>,
    a: &[u8],
    old_type: Node<'_>,
    new: Node<'_>,
    b: &[u8],
    new_type: Node<'_>,
) -> bool {
    old.kind() == new.kind()
        && same_range(
            old,
            a,
            old.start_byte()..old_type.start_byte(),
            new,
            b,
            new.start_byte()..new_type.start_byte(),
        )
        && same_range(
            old,
            a,
            old_type.end_byte()..old.end_byte(),
            new,
            b,
            new_type.end_byte()..new.end_byte(),
        )
}

fn analyze<'new>(
    old_root: Node<'_>,
    a: &[u8],
    new_root: Node<'new>,
    b: &[u8],
    name: &str,
) -> Option<Analysis<'new>> {
    let (old, old_statement) = unique_function_declaration(old_root, a, name)?;
    let (new, new_statement) = unique_function_declaration(new_root, b, name)?;
    let old_params = old.child_by_field_name("parameters")?;
    let new_params = new.child_by_field_name("parameters")?;
    let old_return = old.child_by_field_name("return_type")?;
    let new_return = new.child_by_field_name("return_type")?;
    let old_body = old.child_by_field_name("body")?;
    let new_body = new.child_by_field_name("body")?;
    let old_parameters = named(old_params);
    let new_parameters = named(new_params);
    if old_parameters.len() != new_parameters.len() {
        return None;
    }
    let same_contract = same_range(
        old_statement,
        a,
        old_statement.start_byte()..old_params.start_byte(),
        new_statement,
        b,
        new_statement.start_byte()..new_params.start_byte(),
    ) && same_range(
        old,
        a,
        old_params.end_byte()..old_return.start_byte(),
        new,
        b,
        new_params.end_byte()..new_return.start_byte(),
    ) && same_range(
        old,
        a,
        old_return.end_byte()..old_body.start_byte(),
        new,
        b,
        new_return.end_byte()..new_body.start_byte(),
    ) && node_signature_tokens(old_body, a)
        == node_signature_tokens(new_body, b);
    let mut changes = Vec::new();
    let mut roots = Vec::new();
    for (parameter, (old, new)) in old_parameters.iter().zip(&new_parameters).enumerate() {
        let old_type = old.child_by_field_name("type")?;
        let new_type = new.child_by_field_name("type")?;
        if !shell(*old, a, old_type, *new, b, new_type) {
            return None;
        }
        compare_types(
            old_type,
            a,
            new_type,
            b,
            TypeChangePosition::Input,
            Some(parameter),
            &mut changes,
        )?;
        roots.push(new_type);
    }
    compare_types(
        old_return,
        a,
        new_return,
        b,
        TypeChangePosition::Return,
        None,
        &mut changes,
    )?;
    roots.push(new_return);
    if changes.is_empty() {
        return None;
    }
    Some(Analysis {
        changes,
        same_contract,
        arity: new_parameters.len(),
        type_roots: roots,
    })
}

fn compare_types(
    old: Node<'_>,
    a: &[u8],
    new: Node<'_>,
    b: &[u8],
    position: TypeChangePosition,
    parameter: Option<usize>,
    changes: &mut Vec<NullableTypeChange>,
) -> Option<()> {
    let mut pending = vec![(inner(old)?, inner(new)?, Vec::<String>::new())];
    while let Some((old, new, path)) = pending.pop() {
        if node_signature_tokens(old, a) == node_signature_tokens(new, b) {
            continue;
        }
        if let Some(direction) = null_difference(old, a, new, b) {
            changes.push(NullableTypeChange {
                position,
                parameter,
                property_path: path,
                direction,
                member: "null".to_owned(),
            });
            continue;
        }
        if old.kind() != "object_type" || new.kind() != "object_type" {
            return None;
        }
        let old_members = named(old);
        let new_members = named(new);
        if old_members.len() != new_members.len() {
            return None;
        }
        let mut names = HashSet::new();
        for (old, new) in old_members.into_iter().zip(new_members).rev() {
            if old.kind() != "property_signature" || new.kind() != "property_signature" {
                return None;
            }
            let key = old.child_by_field_name("name")?;
            if key.kind() != "property_identifier" {
                return None;
            }
            let name = key.utf8_text(a).ok()?;
            if name.contains('\\') || !names.insert(name.to_owned()) {
                return None;
            }
            let old_type = old.child_by_field_name("type")?;
            let new_type = new.child_by_field_name("type")?;
            if !shell(old, a, old_type, new, b, new_type) {
                return None;
            }
            let mut path = path.clone();
            path.push(name.to_owned());
            pending.push((inner(old_type)?, inner(new_type)?, path));
        }
    }
    Some(())
}

type UnionMembers = Vec<Vec<(String, String)>>;

fn union_parts(node: Node<'_>, source: &[u8]) -> Option<(bool, UnionMembers)> {
    let mut pending = vec![node];
    let mut null = false;
    let mut members = Vec::new();
    while let Some(node) = pending.pop() {
        let node = inner(node)?;
        if node.kind() == "union_type" {
            pending.extend(named(node));
            continue;
        }
        if node.kind() == "literal_type" && node.utf8_text(source).ok()? == "null" {
            if null {
                return None;
            }
            null = true;
        } else {
            if !non_function_type(node, source) {
                return None;
            }
            members.push(node_signature_tokens(node, source)?.0);
        }
    }
    members.sort();
    if members.is_empty() || members.windows(2).any(|pair| pair[0] == pair[1]) {
        return None;
    }
    Some((null, members))
}

fn null_difference(
    old: Node<'_>,
    a: &[u8],
    new: Node<'_>,
    b: &[u8],
) -> Option<TypeChangeDirection> {
    let (old_null, old_members) = union_parts(old, a)?;
    let (new_null, new_members) = union_parts(new, b)?;
    if old_members != new_members || old_null == new_null {
        return None;
    }
    Some(if new_null {
        TypeChangeDirection::Widening
    } else {
        TypeChangeDirection::Narrowing
    })
}

fn non_function_type(node: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "predefined_type"
                if matches!(
                    node.utf8_text(source).ok(),
                    Some("string" | "number" | "boolean" | "bigint" | "symbol")
                ) => {}
            "literal_type"
                if named(node).first().is_some_and(|value| {
                    matches!(value.kind(), "string" | "number" | "true" | "false")
                }) => {}
            "object_type" => {
                for member in named(node) {
                    if member.kind() != "property_signature"
                        || member
                            .child_by_field_name("name")
                            .is_none_or(|name| name.kind() != "property_identifier")
                    {
                        return false;
                    }
                    let Some(ty) = member.child_by_field_name("type").and_then(inner) else {
                        return false;
                    };
                    pending.push(ty);
                }
            }
            "union_type" | "parenthesized_type" => pending.extend(named(node)),
            _ => return false,
        }
    }
    true
}

pub(crate) fn collect_nullable_type_evidence(
    dir: &str,
    base: &str,
    api: &ApiChanges,
    blobs: &crate::commands::git_input::GitBlobBatch,
    strip: &super::super::ts_strip_internal::StripInternal,
) -> Vec<TypeChangeEvidence> {
    let modified = api
        .modified
        .iter()
        .chain(&api.modified_closed_in_diff)
        .map(|change| {
            (
                &change.file,
                &change.name,
                &change.kind,
                &change.old_signature,
                &change.new_signature,
                false,
            )
        });
    let compatible = api.compatible_modified.iter().map(|change| {
        (
            &change.file,
            &change.name,
            &change.kind,
            &change.old_signature,
            &change.new_signature,
            change.reason == "nullable_input_widening",
        )
    });
    let mut result = Vec::new();
    for (file, name, kind, old, new, proven) in modified.chain(compatible) {
        let (Some(old), Some(new)) = (old, new) else {
            continue;
        };
        let site = CompatibleModSite {
            dir,
            base,
            old_path: file,
            new_path: file,
            name,
            kind,
            old_sig: old,
            new_sig: new,
            lang_id: crate::language::LangId::from_path(camino::Utf8Path::new(file)).ok(),
        };
        let Some(lang) = nullable_site(&site) else {
            continue;
        };
        let mut sources = SignatureSourceCache::with_base_blobs(blobs, strip);
        let Some(src) = sources.get(&site) else {
            continue;
        };
        let Some((old, new)) = src.parse_pair(lang) else {
            continue;
        };
        let Some(analysis) = analyze(old.root_node(), &src.old, new.root_node(), &src.new, name)
        else {
            continue;
        };
        result.push(TypeChangeEvidence {
            file: file.clone(),
            name: name.clone(),
            kind: kind.clone(),
            changes: analysis.changes,
            usage: if proven {
                TypeChangeUsage::DirectCallsProven
            } else {
                TypeChangeUsage::Unproven
            },
            scope: "repository".to_owned(),
        });
    }
    result.sort_by(|a, b| (&a.file, &a.name, &a.kind).cmp(&(&b.file, &b.name, &b.kind)));
    result
}
