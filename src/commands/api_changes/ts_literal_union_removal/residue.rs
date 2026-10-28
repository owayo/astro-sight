//! 削除値の残存だけを調べる。結果は型名や carrier の計画には依存しない。

use super::*;

pub(super) fn absent(inventory: &inventory::Inventory, removal: &shape::Removal) -> bool {
    let removed = removal
        .removed
        .iter()
        .map(|value| value.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    // 数値 key は 1e3/0x... など原文と ToPropertyKey の綴りが異なるため、
    // 数字として読める削除名では全ソースの静的数値 key を保守的に止める。
    let numeric_keys = removal
        .removed
        .iter()
        .any(|value| value.parse::<f64>().is_ok());
    for (path, file) in &inventory.files {
        if !numeric_keys
            && !file.source.contains('\\')
            && !removal
                .removed
                .iter()
                .any(|value| file.source.contains(value))
        {
            continue;
        }
        match file.kind {
            inventory::Kind::Embedded => return false,
            inventory::Kind::Json => {
                let Some(json) = inventory.json_root(path) else {
                    return false;
                };
                if json
                    .strings
                    .iter()
                    .any(|value| removal.removed.contains(value))
                {
                    return false;
                }
            }
            inventory::Kind::Source(lang) => {
                let Some(root) = inventory.root(path) else {
                    return false;
                };
                for node in slots::nodes(root) {
                    let Some(raw) = text(node, &file.source) else {
                        return false;
                    };
                    if numeric_keys
                        && (node.kind() == "computed_property_name"
                            || (node.kind() == "number"
                                && node.parent().is_some_and(|parent| {
                                    ["key", "name"].into_iter().any(|field| {
                                        parent.child_by_field_name(field).map(span)
                                            == Some(span(node))
                                    })
                                })))
                    {
                        return false;
                    }
                    match node.kind() {
                        "string" | "template_string" | "template_literal_type" => {
                            if any_node(node, Children::Named, |node| {
                                matches!(node.kind(), "template_substitution" | "template_type")
                            }) {
                                continue;
                            }
                            let Some(inner) = raw.get(1..raw.len().saturating_sub(1)) else {
                                return false;
                            };
                            let Some(value) = decode::units(inner) else {
                                return false;
                            };
                            if removed.contains(&value) {
                                return false;
                            }
                        }
                        "property_identifier"
                        | "shorthand_property_identifier"
                        | "shorthand_property_identifier_pattern" => {
                            let Some(value) = decode::identifier(raw) else {
                                return false;
                            };
                            if removal.removed.contains(&value) {
                                return false;
                            }
                        }
                        "comment"
                            if lang == LangId::Javascript
                                && raw.starts_with("/**")
                                && (raw.contains('\\')
                                    || removal.removed.iter().any(|value| {
                                        super::super::ref_index::contains_js_word(raw, value)
                                    })) =>
                        {
                            return false;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_union_residue_decodes_values_and_keys_without_rejecting_unrelated_escapes() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for (path, source, expected) in [
            (
                "src/value.ts",
                r#"export const text = "le\u0067acy";"#,
                false,
            ),
            (
                "src/value.tsx",
                r#"export const value = { le\u0067acy: 1 };"#,
                false,
            ),
            ("data.json", r#"{"le\u0067acy":1}"#, false),
            ("data.json", r#"{"value":"lega\u0063y"}"#, false),
            ("data.json", r#"{"value":"legacy","value":"all"}"#, false),
            (
                "src/value.ts",
                r#"export const text = "prefix-legacy"; export const line = "\n";"#,
                true,
            ),
            (
                "src/value.tsx",
                "export const text = `unrelated ${value}`;",
                true,
            ),
            (
                "view.vue",
                "<script>const value = 'legacy';</script>",
                false,
            ),
        ] {
            let (_directory, inventory) = inventory::fixture(&[(path, source)]);
            assert_eq!(absent(&inventory, &removal), expected, "{path}/{source}");
        }
    }
    #[test]
    fn literal_union_residue_retains_numeric_property_keys_in_both_grammars() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into()]),
            removed: BTreeSet::from(["1000".into()]),
        };
        for (extension, lang) in [("ts", LangId::Typescript), ("tsx", LangId::Tsx)] {
            for query in [
                "(pair key: (number))",
                "(property_signature name: (number))",
                "(computed_property_name)",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
            for source in [
                "export const values = {1e3: 'other'};",
                "export interface Values { 0x3e8: string; }",
                "export const values = {[1e3]: 'other'};",
            ] {
                let path = format!("src/value.{extension}");
                let (_directory, inventory) = inventory::fixture(&[(&path, source)]);
                assert!(!absent(&inventory, &removal), "{extension}/{source}");
            }
            let path = format!("src/value.{extension}");
            let (_directory, inventory) =
                inventory::fixture(&[(&path, "export const value = 1000;")]);
            assert!(absent(&inventory, &removal));
        }
    }
}
