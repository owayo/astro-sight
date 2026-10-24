//! 名前空間内の同名 export に隠れた、TS/TSX の一意な直下宣言の変更を回復する。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::*;
use crate::language::LangId;

/// 順序だけの差・不変の overload 名は参照検索へ追加しない。
pub(super) fn changed_duplicate_names(
    old: &[(String, String, String)],
    new: &[(String, String, String)],
) -> HashSet<String> {
    fn grouped(symbols: &[(String, String, String)]) -> HashMap<&str, Vec<(&str, &str)>> {
        let mut groups: HashMap<_, Vec<_>> = HashMap::new();
        for (name, kind, sig) in symbols {
            groups
                .entry(name.as_str())
                .or_default()
                .push((kind.as_str(), sig.as_str()));
        }
        for group in groups.values_mut() {
            group.sort_unstable();
        }
        groups
    }
    let old = grouped(old);
    let new = grouped(new);
    old.iter()
        .filter_map(|(name, signatures)| {
            // 回復対象は直下のbare名だけ。Class.methodを識別子索引へ足さない。
            if name.contains('.') {
                return None;
            }
            let after = new.get(name)?;
            ((signatures.len() > 1 || after.len() > 1) && signatures != after)
                .then(|| (*name).to_owned())
        })
        .collect()
}

pub(super) struct RootChange {
    pub(super) kind: String,
    pub(super) old_sig: String,
    pub(super) new_sig: String,
}

pub(super) fn recover_root_changes(
    inputs: &DetectionInputs<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
) -> HashMap<String, RootChange> {
    let Some(lang) = recovery_language(&df.old_path, &df.new_path) else {
        return HashMap::new();
    };
    let names = changed_duplicate_names(facts.old_syms, facts.new_syms);
    if names.is_empty() {
        return HashMap::new();
    }
    let Some(sources) = load_new_source(inputs.dir, &df.new_path).and_then(|new| {
        load_old_source_with_new(inputs.base_blobs, &df.old_path, &df.new_path, new)
    }) else {
        return HashMap::new();
    };
    let Some((old_tree, new_tree)) = sources.parse_pair(lang) else {
        return HashMap::new();
    };
    let old = root_declarations(old_tree.root_node(), &sources.old, lang, &names);
    let new = root_declarations(new_tree.root_node(), &sources.new, lang, &names);
    old.into_iter()
        .filter_map(|(name, (kind, old_sig))| {
            let (new_kind, new_sig) = new.get(&name)?;
            (kind == *new_kind && old_sig != *new_sig).then(|| {
                (
                    name,
                    RootChange {
                        kind,
                        old_sig,
                        new_sig: new_sig.clone(),
                    },
                )
            })
        })
        .collect()
}

pub(super) fn recovery_language(old_path: &str, new_path: &str) -> Option<LangId> {
    let lang @ (LangId::Typescript | LangId::Tsx) =
        LangId::from_path(camino::Utf8Path::new(new_path)).ok()?
    else {
        return None;
    };
    (LangId::from_path(camino::Utf8Path::new(old_path)).ok() == Some(lang)).then_some(lang)
}

/// 署名の構築は通常の API 抽出と共有し、宣言の対応だけを位置で確認する。
fn root_declarations(
    root: Node<'_>,
    source: &[u8],
    lang: LangId,
    names: &HashSet<String>,
) -> HashMap<String, (String, String)> {
    if root.has_error() {
        return HashMap::new();
    }
    let Ok(symbols) = crate::engine::symbols::extract_symbols(root, source, lang) else {
        return HashMap::new();
    };
    let Ok(text) = std::str::from_utf8(source) else {
        return HashMap::new();
    };
    let lines: Vec<_> = text.lines().collect();
    symbols
        .iter()
        .filter_map(|symbol| {
            if !names.contains(&symbol.name) {
                return None;
            }
            let declaration = root_export_declaration(root, source, symbol)?;
            let bindings = super::ts_const_arg::collect_bindings_named(root, source, &symbol.name);
            let mut bindings = bindings.into_iter().filter(|binding| {
                !inside_namespace(*binding)
                    || matches!(
                        binding.kind(),
                        "assignment_expression"
                            | "augmented_assignment_expression"
                            | "for_in_statement"
                    )
            });
            if bindings.next()?.id() != declaration.id()
                || bindings.next().is_some()
                || crate::engine::tree_walk::any_node(
                    root,
                    crate::engine::tree_walk::Children::Named,
                    |node| {
                        node.kind() == "update_expression"
                            && node
                                .child_by_field_name("argument")
                                .is_some_and(|argument| {
                                    super::js_ts_shadow::pattern_binds_name(
                                        argument,
                                        source,
                                        &symbol.name,
                                    )
                                })
                    },
                )
            {
                return None;
            }
            Some((
                symbol.name.clone(),
                (
                    format!("{:?}", symbol.kind).to_lowercase(),
                    extract_api_signature(symbol, root, source, &lines, lang),
                ),
            ))
        })
        .collect()
}

fn inside_namespace(node: Node<'_>) -> bool {
    // 名前空間自身は残す。`namespace run {}` と関数 run の merge を許さない。
    let mut current = node.parent();
    while let Some(node) = current {
        if node.kind() == "internal_module" {
            return true;
        }
        current = node.parent();
    }
    false
}

fn root_export_declaration<'tree>(
    root: Node<'tree>,
    source: &[u8],
    symbol: &Symbol,
) -> Option<Node<'tree>> {
    let range = symbol.identity_range();
    let mut node = root.descendant_for_point_range(
        tree_sitter::Point {
            row: range.start.line,
            column: range.start.column,
        },
        tree_sitter::Point {
            row: range.end.line,
            column: range.end.column,
        },
    )?;
    loop {
        if matches!(
            node.kind(),
            "function_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "variable_declarator"
        ) {
            if node.child_by_field_name("name")?.utf8_text(source).ok()? != symbol.name {
                return None;
            }
            let outer = if node.kind() == "variable_declarator" {
                node.parent().filter(|parent| {
                    matches!(
                        parent.kind(),
                        "lexical_declaration" | "variable_declaration"
                    )
                })?
            } else {
                node
            };
            let export = outer
                .parent()
                .filter(|parent| parent.kind() == "export_statement")?;
            let mut cursor = export.walk();
            if export.parent()?.id() != root.id()
                || export
                    .children(&mut cursor)
                    .any(|child| child.kind() == "default")
                || export.child_by_field_name("declaration")?.id() != outer.id()
            {
                return None;
            }
            return Some(node);
        }
        node = node.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_namespace_recovery_grammar_nodes_and_language_boundary() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for query in [
                "(internal_module name: (_) body: (statement_block))",
                "(function_declaration name: (identifier))",
                "(interface_declaration name: (type_identifier))",
                "(type_alias_declaration name: (type_identifier))",
                "(variable_declarator name: (identifier))",
                "(lexical_declaration (variable_declarator))",
                "(variable_declaration (variable_declarator))",
                "(export_statement declaration: (_))",
                "(export_statement \"default\")",
                "(assignment_expression left: (_))",
                "(augmented_assignment_expression left: (_))",
                "(for_in_statement left: (_))",
                "(update_expression argument: (_))",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
        }
        assert_eq!(
            recovery_language("old.ts", "new.ts"),
            Some(LangId::Typescript)
        );
        assert_eq!(recovery_language("old.tsx", "new.tsx"), Some(LangId::Tsx));
        for (old, new) in [
            ("old.js", "new.ts"),
            ("old.rs", "new.tsx"),
            ("old.ts", "new.js"),
            ("old.ts", "new.tsx"),
        ] {
            assert_eq!(recovery_language(old, new), None, "{old} -> {new}");
        }
    }

    fn roots(source: &str, lang: LangId, name: &str) -> HashMap<String, (String, String)> {
        let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
        root_declarations(
            tree.root_node(),
            source.as_bytes(),
            lang,
            &HashSet::from([name.to_owned()]),
        )
    }

    #[test]
    fn root_declarations_keep_namespace_scope_and_existing_signatures() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (name, root, nested) in [
                (
                    "run",
                    "export function run(value: string): void {}\n",
                    "export function run(value: number): void {}",
                ),
                (
                    "Row",
                    "export interface Row { id: string; note?: number }\n",
                    "export interface Row { other: number }",
                ),
                (
                    "Row",
                    "export type Row = { id: string; note?: number };\n",
                    "export type Row = { other: number };",
                ),
                (
                    "LIMIT",
                    "export const LIMIT: number = 1, OTHER = true;\n",
                    "export const LIMIT = 'nested';",
                ),
            ] {
                let plain = roots(root, lang, name);
                assert_eq!(plain.len(), 1, "{root}");
                for source in [
                    format!("{root}namespace N {{ {nested} }}\n"),
                    format!("export namespace N {{ {nested} }}\n{root}"),
                ] {
                    assert_eq!(roots(&source, lang, name), plain, "{source}");
                }
            }
        }
    }

    #[test]
    fn root_declarations_do_not_prove_ambiguous_or_unsupported_bindings() {
        for (name, source) in [
            (
                "run",
                "export function run(value: string): void; export function run(value: unknown): void {}",
            ),
            (
                "Row",
                "export interface Row { id: string } export interface Row { note: string }",
            ),
            (
                "run",
                "export function run(): void {} namespace run { export const item = 1; }",
            ),
            (
                "run",
                "export function run(): void {} namespace run.Legacy { export const item = 1; }",
            ),
            ("run", "export function run(): void {} run = replacement;"),
            (
                "run",
                "export function run(): void {} namespace N { run = replacement; }",
            ),
            ("LIMIT", "export let LIMIT = 1; LIMIT++;"),
            ("LIMIT", "export const { LIMIT } = source;"),
            ("run", "export default function run(): void {}"),
            ("run", "function run(): void {} export { run };"),
            ("run", "export function run(): void {} export const bad = ;"),
        ] {
            assert!(
                roots(source, LangId::Typescript, name).is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn changed_duplicate_names_compare_signature_multisets_and_index_hidden_changes() {
        let symbol = |name: &str, kind: &str, signature: &str| {
            (name.to_owned(), kind.to_owned(), signature.to_owned())
        };
        let old = vec![
            symbol("run", "function", "one"),
            symbol("run", "function", "two"),
            symbol("plain", "function", "before"),
        ];
        let reordered = vec![
            old[1].clone(),
            old[0].clone(),
            symbol("plain", "function", "after"),
        ];
        assert!(changed_duplicate_names(&old, &reordered).is_empty());
        let new = vec![old[1].clone(), old[1].clone(), old[2].clone()];
        assert_eq!(
            changed_duplicate_names(&old, &new),
            HashSet::from(["run".to_owned()])
        );
        for (lang, expected) in [
            (LangId::Typescript, true),
            (LangId::Tsx, true),
            (LangId::Rust, false),
            (LangId::Cpp, false),
        ] {
            let mut names = HashSet::new();
            collect_modified_file_index_names(
                &old,
                &new,
                &HashSet::new(),
                &HashSet::new(),
                false,
                Some(lang),
                &mut names,
            );
            assert_eq!(names.contains("run"), expected, "{lang:?}: {names:?}");
        }
        let changed_kind = vec![
            symbol("run", "variable", "two"),
            old[1].clone(),
            old[2].clone(),
        ];
        assert!(changed_duplicate_names(&old, &changed_kind).contains("run"));
        let old_methods = vec![
            symbol("Class.run", "method", "one"),
            symbol("Class.run", "method", "two"),
        ];
        let new_methods = vec![old_methods[1].clone(), old_methods[1].clone()];
        assert!(changed_duplicate_names(&old_methods, &new_methods).is_empty());
    }
}
