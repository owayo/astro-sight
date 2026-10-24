//! 削除キーの残存を、型の名前を出さない別の値・型の利用も含めて保守的に監査する。

use super::*;

impl Prover<'_, '_> {
    pub(super) fn prove_removed_keys_absent(&self, caches: &mut ApiClosureCaches) -> bool {
        let names: Vec<&str> = self
            .delta
            .removed_names()
            .iter()
            .map(String::as_str)
            .collect();
        if super::super::ref_index::embedded_js_sources_may_hide_removed_keys(
            self.scope.dir,
            &names,
            caches,
        ) {
            return false;
        }
        let files = caches
            .ts_checked_files
            .get_or_insert_with(|| {
                crate::engine::refs::collect_files_scan(
                    Path::new(self.scope.dir),
                    Some(NAME_REFS_GLOB),
                    crate::engine::refs::FileScanOptions::default(),
                )
                .ok()
                .map(|collection| Rc::new(collection.files))
            })
            .clone();
        let Some(files) = files else {
            return false;
        };
        !files.par_iter().any(|path| {
            let Some(lang) = path
                .to_str()
                .and_then(|path| LangId::from_path(camino::Utf8Path::new(path)).ok())
            else {
                return true;
            };
            let Some(text) =
                crate::engine::bounded_read::read_utf8_file_limited(path, MAX_SOURCE_SCAN_BYTES)
            else {
                return true;
            };
            // escape だけで書いた識別子・文字列も AST へ渡す。名前の生文字列だけで落とさない。
            if !text.contains('\\')
                && !names
                    .iter()
                    .any(|name| super::super::ref_index::contains_js_word(&text, name))
            {
                return false;
            }
            let Ok(tree) = crate::engine::parser::parse_source(text.as_bytes(), lang) else {
                return true;
            };
            tree.root_node().has_error()
                || contains_removed_key(tree.root_node(), text.as_bytes(), lang, &names)
        })
    }
}

fn contains_removed_key(root: Node<'_>, source: &[u8], lang: LangId, names: &[&str]) -> bool {
    any_node(root, Children::Named, |node| match node.kind() {
        "identifier" | "type_identifier" => {
            node.utf8_text(source).is_ok_and(|text| text.contains('\\'))
        }
        "property_identifier"
        | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern" => node
            .utf8_text(source)
            .is_ok_and(|text| text.contains('\\') || names.contains(&text)),
        "string" | "template_string" | "template_literal_type" => {
            removed_string_key(node) && string_may_equal(node, source, names)
        }
        "comment" if lang == LangId::Javascript => node.utf8_text(source).is_ok_and(|text| {
            text.starts_with("/**")
                && (text.contains('\\')
                    || names
                        .iter()
                        .any(|name| super::super::ref_index::contains_js_word(text, name)))
        }),
        _ => false,
    })
}

fn removed_string_key(node: Node<'_>) -> bool {
    if is_string_access_key(node) || node.kind() == "template_literal_type" {
        return true;
    }
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        if value_flows_to_parent(current, parent) {
            current = parent;
            continue;
        }
        return match parent.kind() {
            // 型引数・lookup の文字列以外も、同名の literal 型はキーとして利用され得る。
            "literal_type" | "computed_property_name" => true,
            "pair" => parent.child_by_field_name("key").map(|n| n.id()) == Some(current.id()),
            "property_signature" | "method_signature" | "method_definition" => {
                parent.child_by_field_name("name").map(|n| n.id()) == Some(current.id())
            }
            _ => false,
        };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_key_nodes_exist_and_match_in_both_typescript_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for query in [
                "(identifier)",
                "(type_identifier)",
                "(property_identifier)",
                "(string)",
                "(template_string)",
                "(extends_type_clause)",
                "(interface_declaration type_parameters: (type_parameters))",
                "(type_alias_declaration type_parameters: (type_parameters))",
                "(property_signature name: (_) type: (type_annotation))",
                "(interface_declaration body: (interface_body))",
                "(type_alias_declaration value: (object_type))",
                "(member_expression object: (_) property: (property_identifier))",
                "(subscript_expression object: (_) index: (_))",
                "(pair key: (_) value: (_))",
                "(pair_pattern key: (_) value: (_))",
                "(shorthand_property_identifier)",
                "(shorthand_property_identifier_pattern)",
                "(lookup_type)",
                "(literal_type)",
                "(template_literal_type)",
                "(method_definition name: (_))",
                "(method_signature name: (_))",
                "(computed_property_name)",
                "(comment)",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
            for (source, expected) in [
                ("value.extra;", true),
                ("const other = { extra: 1 };", true),
                ("const other = { extra };", true),
                ("const { extra } = value;", true),
                ("const { extra: renamed } = value;", true),
                ("const other = { [('extra' as const)]: 1 };", true),
                ("interface Other { 'extra'(): void }", true),
                ("value['extra'];", true),
                ("type Other = Lookup['extra'];", true),
                ("type Other = Pick<Lookup, 'extra'>;", true),
                ("type Other = `extra`;", true),
                (r#"value["\u0065xtra"];"#, true),
                (r#"value.e\u0078tra;"#, true),
                ("const key = 'extra'; value[key];", false),
                ("const word = 'extra'; value.id;", false),
                ("// extra\nvalue.id;", false),
            ] {
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{lang}: {source}");
                assert_eq!(
                    contains_removed_key(tree.root_node(), source.as_bytes(), lang, &["extra"]),
                    expected,
                    "{lang}: {source}"
                );
            }
        }
    }
}
