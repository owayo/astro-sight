use super::super::source_pair::OldNewSources;
use super::*;

pub(super) struct Removal {
    pub(super) kept: BTreeSet<String>,
    pub(super) removed: BTreeSet<String>,
}
pub(super) enum Change {
    NotApplicable,
    Unsupported,
    Removal(Removal),
}

pub(super) fn classify(site: &CompatibleModSite<'_>, pair: &OldNewSources) -> Change {
    let Some(lang) = site.lang_id else {
        return Change::NotApplicable;
    };
    if pair.old.len() > inventory::MAX_SOURCE {
        return Change::NotApplicable;
    }
    let Ok(old) = crate::engine::parser::parse_source(&pair.old, lang) else {
        return Change::NotApplicable;
    };
    let Ok(old_source) = std::str::from_utf8(&pair.old) else {
        return Change::NotApplicable;
    };
    let Some(old_candidate) =
        root_alias(old.root_node(), old_source, site.name).filter(|candidate| checked(*candidate))
    else {
        return Change::NotApplicable;
    };
    let Some(old_values) = old_candidate
        .child_by_field_name("value")
        .and_then(|value| values(value, old_source))
    else {
        return Change::NotApplicable;
    };
    // 旧側の純 string 候補を認識した後、新側の不明を既存の行判定へ戻さない。
    if pair.new.len() > inventory::MAX_SOURCE {
        return Change::Unsupported;
    }
    let Ok(new) = crate::engine::parser::parse_source(&pair.new, lang) else {
        return Change::Unsupported;
    };
    let Ok(new_source) = std::str::from_utf8(&pair.new) else {
        return Change::Unsupported;
    };
    let old_set = old_values
        .iter()
        .map(|(value, _)| value.clone())
        .collect::<BTreeSet<_>>();
    let new_literal_set = root_alias(new.root_node(), new_source, site.name)
        .filter(|candidate| checked(*candidate))
        .and_then(|declaration| declaration.child_by_field_name("value"))
        .map(|value| direct_strings(value, new_source))
        .unwrap_or_default();
    // 追加だけ・並べ替えだけは削除証明の適用外とし、既存の互換/関数型判定へ戻す。
    if old_set.is_subset(&new_literal_set) {
        return Change::NotApplicable;
    }
    if !checked(old.root_node()) || !checked(new.root_node()) || !plain_name(site.name) {
        return Change::Unsupported;
    }
    let result = (|| {
        let old_decl = declaration(old.root_node(), old_source, site.name)?;
        let new_decl = declaration(new.root_node(), new_source, site.name)?;
        let old_value = old_decl.child_by_field_name("value")?;
        let new_value = new_decl.child_by_field_name("value")?;
        let new_values = values(new_value, new_source)?;
        let new_set = new_values
            .iter()
            .map(|(value, _)| value.clone())
            .collect::<BTreeSet<_>>();
        if old_values
            .iter()
            .chain(new_values.iter())
            .any(|(_, raw)| raw.contains('\\'))
            || old_set.len() != old_values.len()
            || new_set.len() != new_values.len()
            || new_set.len() >= old_set.len()
            || !new_set.is_subset(&old_set)
            || header(old_decl, old_value, old_source)? != header(new_decl, new_value, new_source)?
            || old_values
                .iter()
                .filter(|(value, _)| new_set.contains(value))
                .collect::<Vec<_>>()
                != new_values.iter().collect::<Vec<_>>()
        {
            return None;
        }
        Some(Change::Removal(Removal {
            removed: old_set.difference(&new_set).cloned().collect(),
            kept: new_set,
        }))
    })();
    result.unwrap_or(Change::Unsupported)
}

fn root_alias<'t>(root: Node<'t>, source: &str, name: &str) -> Option<Node<'t>> {
    children(root)
        .into_iter()
        .filter_map(|statement| {
            if statement.kind() == "export_statement" {
                statement.child_by_field_name("declaration")
            } else {
                Some(statement)
            }
        })
        .find(|declaration| {
            declaration.kind() == "type_alias_declaration"
                && declaration
                    .child_by_field_name("name")
                    .and_then(|node| text(node, source))
                    == Some(name)
        })
}
fn direct_strings(root: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "union_type" | "parenthesized_type" => pending.extend(children(node)),
            "literal_type" => {
                if let Some(literal) = only(node).filter(|node| node.kind() == "string")
                    && let Some(value) = decode::string(literal, source)
                {
                    values.insert(value);
                }
            }
            _ => {}
        }
    }
    values
}

pub(super) fn declaration<'t>(root: Node<'t>, source: &str, name: &str) -> Option<Node<'t>> {
    if root.has_error() || any_node(root, Children::All, |node| node.is_missing()) {
        return None;
    }
    let mut found = None;
    for statement in children(root) {
        let declaration = if statement.kind() == "export_statement" {
            statement.child_by_field_name("declaration")
        } else {
            Some(statement)
        };
        let Some(declaration) = declaration else {
            continue;
        };
        if declaration
            .child_by_field_name("name")
            .and_then(|node| text(node, source))
            != Some(name)
        {
            continue;
        }
        if found.is_some()
            || declaration.kind() != "type_alias_declaration"
            || !exported(declaration)
            || token(statement, "default")
            || declaration.child_by_field_name("type_parameters").is_some()
        {
            return None;
        }
        found = Some(declaration);
    }
    let decl = found?;
    if super::super::ts_const_arg::collect_bindings_named(root, source.as_bytes(), name).len() != 1
    {
        return None;
    }
    Some(decl)
}
fn header(decl: Node<'_>, value: Node<'_>, source: &str) -> Option<(SigTokens, SigTokens)> {
    let outer = decl.parent()?;
    let clean = |mut tokens: SigTokens| {
        tokens.0.retain(|(kind, _)| kind != "comment");
        tokens
    };
    Some((
        clean(signature_tokens_in_range(
            outer,
            source.as_bytes(),
            outer.start_byte(),
            value.start_byte(),
        )?),
        clean(signature_tokens_in_range(
            outer,
            source.as_bytes(),
            value.end_byte(),
            outer.end_byte(),
        )?),
    ))
}
fn values(root: Node<'_>, source: &str) -> Option<Vec<(String, String)>> {
    let mut values = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "union_type" | "parenthesized_type" => pending.extend(children(node).into_iter().rev()),
            "literal_type" => {
                let literal = only(node)?;
                let raw = text(literal, source)?;
                if literal.kind() != "string" {
                    return None;
                }
                values.push((decode::string(literal, source)?, raw.to_owned()));
            }
            _ => return None,
        }
        if values.len() + pending.len() > 256 {
            return None;
        }
    }
    (!values.is_empty()).then_some(values)
}

fn checked(root: Node<'_>) -> bool {
    let mut count = 0;
    !root.has_error()
        && crate::engine::tree_walk::walk_preorder(root, Children::All, |node, depth| {
            count += 1;
            if count > 100_000 || depth > MAX_DEPTH || node.is_error() || node.is_missing() {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            }
        })
        .is_continue()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn classify_pair(old: &str, new: &str, lang: LangId) -> Change {
        classify_named_pair(old, new, "Mode", lang)
    }
    fn classify_named_pair(old: &str, new: &str, name: &str, lang: LangId) -> Change {
        let site = CompatibleModSite {
            dir: ".",
            base: "HEAD",
            old_path: "mode.ts",
            new_path: "mode.ts",
            name,
            kind: "type",
            old_sig: "",
            new_sig: "",
            lang_id: Some(lang),
        };
        let pair = OldNewSources {
            old: old.as_bytes().to_vec(),
            new: crate::engine::parser::SourceBuf::Vec(new.as_bytes().to_vec()),
        };
        classify(&site, &pair)
    }
    #[test]
    fn literal_union_shape_requires_ordered_pure_removal_in_both_grammars() {
        let old = "export type Mode = 'all' | 'legacy' | 'recent';";
        for lang in [LangId::Typescript, LangId::Tsx] {
            let Change::Removal(removal) =
                classify_pair(old, "export type Mode = 'all' | /* kept */ 'recent';", lang)
            else {
                panic!("pure removal");
            };
            assert_eq!(removal.removed, BTreeSet::from(["legacy".into()]));
            for new in [
                "export type Mode = 'recent' | 'all';",
                "export type Mode = 'all' | 'new';",
                "export type Mode<T> = 'all' | 'recent';",
                "export type Mode = 'all' | 'all';",
                "export type Mode = 'all' | Other;",
                "export type Mode = 'all' | 1;",
                "export type Mode = 'all' | `recent`;",
                "export type Mode = 'all' | 're\\u0063ent';",
                "type Mode = 'all' | 'recent'; export { Mode };",
            ] {
                assert!(
                    matches!(classify_pair(old, new, lang), Change::Unsupported),
                    "{new}"
                );
            }
            assert!(matches!(
                classify_pair(
                    "export type Mode = { id: string };",
                    "export type Mode = { id: string; note: string };",
                    lang
                ),
                Change::NotApplicable
            ));
        }
    }
    #[test]
    fn literal_union_shape_blocks_unsupported_names_only_for_union_candidates() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for name in ["方式", r"Mo\u0064e"] {
                let old = format!("export type {name} = 'all' | 'legacy' | 'recent';");
                let new = format!("export type {name} = 'all' | 'recent';");
                assert!(
                    matches!(
                        classify_named_pair(&old, &new, name, lang),
                        Change::Unsupported
                    ),
                    "{name}"
                );
                let old = format!("export type {name} = {{ id: string }};");
                let new = format!("export type {name} = {{ id: string; note: string }};");
                assert!(
                    matches!(
                        classify_named_pair(&old, &new, name, lang),
                        Change::NotApplicable
                    ),
                    "{name}"
                );
            }
        }
    }
    #[test]
    fn literal_union_shape_leaves_other_unions_and_changes_to_existing_classifiers() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (old, new) in [
                (
                    "export type Mode = ((value: string) => void) | null;",
                    "export type Mode = ((value: number) => void) | null;",
                ),
                (
                    "export type Mode = string | number;",
                    "export type Mode = string | boolean;",
                ),
                (
                    "export type Mode = Add | Remove;",
                    "export type Mode = Add | Remove | Reset;",
                ),
                ("export type Mode = true;", "export type Mode = false;"),
                (
                    "export type Mode = 'all' | 'recent';",
                    "export type Mode = 'recent' | 'all';",
                ),
                (
                    "export type Mode = 'all' | 'recent';",
                    "export type Mode = 'all' | 'recent' | 'other';",
                ),
                (
                    "namespace Box { export type Mode = 'all' | 'recent'; }",
                    "namespace Box { export type Mode = 'all'; }",
                ),
            ] {
                assert!(
                    matches!(classify_pair(old, new, lang), Change::NotApplicable),
                    "{old} -> {new}"
                );
            }
            assert!(matches!(
                classify_pair(
                    "export type Mode = 'all' | 'legacy' | 're\\u0063ent';",
                    "export type Mode = 'all' | 'recent';",
                    lang
                ),
                Change::Unsupported
            ));
        }
    }
    #[test]
    fn literal_union_shape_does_not_claim_unparsed_or_oversized_type_sites() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            assert!(matches!(
                classify_pair(
                    "export type Mode = { id: string }; broken syntax ?",
                    "export type Mode = { id: string; note: string };",
                    lang
                ),
                Change::NotApplicable
            ));
            let old = format!(
                "{}export type Mode = {{ id: string }};",
                " ".repeat(inventory::MAX_SOURCE)
            );
            assert!(matches!(
                classify_pair(
                    &old,
                    "export type Mode = { id: string; note: string };",
                    lang
                ),
                Change::NotApplicable
            ));
        }
    }
    #[test]
    fn literal_union_shape_keeps_recognized_old_strings_blocking_when_new_is_unknown() {
        let old = "export type Mode = 'all' | 'legacy' | 'recent';";
        for lang in [LangId::Typescript, LangId::Tsx] {
            let deep = format!(
                "export type Mode = 'all' | 'recent'; const deep={}1{};",
                "(".repeat(MAX_DEPTH + 1),
                ")".repeat(MAX_DEPTH + 1)
            );
            let large = format!(
                "export type Mode = 'all' | 'recent';{}",
                " ".repeat(inventory::MAX_SOURCE)
            );
            for new in [
                "export type Mode = ;",
                "export type Mode = 'all' | 'recent'; const broken = ;",
                &deep,
                &large,
            ] {
                assert!(matches!(classify_pair(old, new, lang), Change::Unsupported));
                assert!(matches!(
                    classify_pair("export type Mode = string | number;", new, lang),
                    Change::NotApplicable
                ));
                assert!(matches!(
                    classify_pair("export type Mode = { id: string };", new, lang),
                    Change::NotApplicable
                ));
            }
        }
        let jsx = format!(
            "export type Mode = 'all' | 'recent'; const deep={}text{};",
            "<section>".repeat(MAX_DEPTH + 1),
            "</section>".repeat(MAX_DEPTH + 1)
        );
        assert!(matches!(
            classify_pair(old, &jsx, LangId::Tsx),
            Change::Unsupported
        ));
    }
    #[test]
    fn literal_union_shape_recognizes_deletion_before_shared_file_parse_barriers() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let deep = format!(
                "const deep={}1{};",
                "(".repeat(MAX_DEPTH + 1),
                ")".repeat(MAX_DEPTH + 1)
            );
            for suffix in ["const broken = ;", &deep] {
                let old = format!("export type Mode='all'|'legacy'|'recent'; {suffix}");
                let new = format!("export type Mode='all'|'recent'; {suffix}");
                assert!(matches!(
                    classify_pair(&old, &new, lang),
                    Change::Unsupported
                ));
                let added = format!("export type Mode='all'|'legacy'|'recent'|'other'; {suffix}");
                assert!(matches!(
                    classify_pair(&old, &added, lang),
                    Change::NotApplicable
                ));
                let reordered = format!("export type Mode='recent'|'legacy'|'all'; {suffix}");
                assert!(matches!(
                    classify_pair(&old, &reordered, lang),
                    Change::NotApplicable
                ));
                let old_other = format!("export type Mode=string|number; {suffix}");
                assert!(matches!(
                    classify_pair(&old_other, &new, lang),
                    Change::NotApplicable
                ));
            }
            assert!(matches!(
                classify_pair(
                    "export type Mode = 'all' | ;",
                    "export type Mode = 'all';",
                    lang
                ),
                Change::NotApplicable
            ));
        }
        let suffix = format!(
            "const deep={}text{};",
            "<section>".repeat(MAX_DEPTH + 1),
            "</section>".repeat(MAX_DEPTH + 1)
        );
        let old = format!("export type Mode='all'|'legacy'|'recent'; {suffix}");
        let new = format!("export type Mode='all'|'recent'; {suffix}");
        assert!(matches!(
            classify_pair(&old, &new, LangId::Tsx),
            Change::Unsupported
        ));
        let added = format!("export type Mode='all'|'legacy'|'recent'|'other'; {suffix}");
        assert!(matches!(
            classify_pair(&old, &added, LangId::Tsx),
            Change::NotApplicable
        ));
    }
}
