use anyhow::Result;
use rayon::prelude::*;
use std::path::Path;
use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;
use crate::models::reference::{RefKind, SymbolReference};

/// 指定シンボルへの参照をディレクトリ内のファイルから検索する。
/// glob パターン（例: "**/*.rs"）によるフィルタも可能。
pub fn find_references(
    symbol_name: &str,
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<Vec<SymbolReference>> {
    let files = collect_files(dir, glob_pattern)?;

    let refs: Vec<Vec<SymbolReference>> = files
        .par_iter()
        .filter_map(|path| {
            let utf8_path = camino::Utf8Path::new(path.to_str()?);
            find_refs_in_file(symbol_name, utf8_path).ok()
        })
        .collect();

    let mut all_refs: Vec<SymbolReference> = refs.into_iter().flatten().collect();
    // ソート: 定義を先頭に、その後パス/行番号順
    all_refs.sort_by(|a, b| {
        let def_order = |k: &Option<RefKind>| match k {
            Some(RefKind::Definition) => 0,
            _ => 1,
        };
        def_order(&a.kind)
            .cmp(&def_order(&b.kind))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    Ok(all_refs)
}

/// ignore クレートでファイルを収集する（.gitignore 対応）。
pub fn collect_files(dir: &Path, glob_pattern: Option<&str>) -> Result<Vec<std::path::PathBuf>> {
    use ignore::WalkBuilder;

    let mut builder = WalkBuilder::new(dir);
    builder.hidden(true).git_ignore(true).git_global(true);

    // glob フィルタを override で適用
    if let Some(pattern) = glob_pattern {
        let mut overrides = ignore::overrides::OverrideBuilder::new(dir);
        overrides.add(pattern)?;
        builder.overrides(overrides.build()?);
    }

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let path = entry.into_path();
            // パース可能なファイルのみ対象
            if LangId::from_path(camino::Utf8Path::new(path.to_str().unwrap_or(""))).is_ok() {
                files.push(path);
            }
        }
    }

    Ok(files)
}

/// 単一ファイル内でシンボル参照を検索する。
fn find_refs_in_file(symbol_name: &str, path: &camino::Utf8Path) -> Result<Vec<SymbolReference>> {
    let source = parser::read_file(path)?;

    // バイトレベルの高速チェック: シンボル名がソースに含まれなければスキップ（SIMD 加速）
    if memchr::memmem::find(&source, symbol_name.as_bytes()).is_none() {
        return Ok(Vec::new());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();

    let mut refs = Vec::new();
    let definition_kinds = definition_node_kinds(lang_id);
    collect_identifier_refs(
        root,
        &source,
        symbol_name,
        path.as_str(),
        &definition_kinds,
        lang_id,
        &mut refs,
    );

    Ok(refs)
}

/// AST を再帰走査し、指定シンボル名に一致する identifier ノードを収集する。
fn collect_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut Vec<SymbolReference>,
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && text == symbol_name
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context(source, node.start_position().row);

        refs.push(SymbolReference {
            path: path.to_string(),
            line: node.start_position().row,
            column: node.start_position().column,
            context: Some(context),
            kind: Some(if is_def {
                RefKind::Definition
            } else {
                RefKind::Reference
            }),
        });
    }

    // 子ノードを再帰走査
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs(
            child,
            source,
            symbol_name,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

/// この identifier ノードが定義コンテキストにあるかを判定する。
fn is_definition_context(node: Node<'_>, definition_kinds: &[&str], lang_id: LangId) -> bool {
    if lang_id == LangId::Ruby {
        return is_ruby_definition_context(node);
    }

    if let Some(parent) = node.parent() {
        // 親ノードが定義ノードかチェック
        if definition_kinds.contains(&parent.kind()) {
            return true;
        }
        // 祖父ノードもチェック（例: function_declarator > identifier）
        if let Some(grandparent) = parent.parent()
            && definition_kinds.contains(&grandparent.kind())
        {
            return true;
        }
    }
    false
}

fn is_ruby_definition_context(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    match parent.kind() {
        "method" | "singleton_method" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "class" | "module" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "scope_resolution" => {
            let is_name = parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id());
            if !is_name {
                return false;
            }

            if let Some(grandparent) = parent.parent() {
                match grandparent.kind() {
                    "assignment" => grandparent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == parent.id()),
                    "class" | "module" => grandparent
                        .child_by_field_name("name")
                        .is_some_and(|name| name.id() == parent.id()),
                    _ => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

/// 言語ごとの定義ノード種別を返す。
fn definition_node_kinds(lang_id: LangId) -> Vec<&'static str> {
    match lang_id {
        LangId::Rust => vec![
            "function_item",
            "function_signature_item", // trait method declarations (no body)
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "const_item",
            "static_item",
            "type_item",
            "mod_item",
        ],
        LangId::C => vec!["function_definition", "struct_specifier", "enum_specifier"],
        LangId::Cpp => vec![
            "function_definition",
            "struct_specifier",
            "class_specifier",
            "enum_specifier",
            "namespace_definition",
        ],
        LangId::Python => vec!["function_definition", "class_definition"],
        LangId::Javascript => vec![
            "function_declaration",
            "class_declaration",
            "method_definition",
            "variable_declarator",
        ],
        LangId::Typescript | LangId::Tsx => vec![
            "function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "variable_declarator",
        ],
        LangId::Go => vec![
            "package_clause",
            "function_declaration",
            "method_declaration",
            "type_spec",
        ],
        LangId::Php => vec![
            "function_definition",
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "trait_declaration",
        ],
        LangId::Java => vec![
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Kotlin => vec![
            "function_declaration",
            "class_declaration",
            "object_declaration",
        ],
        LangId::Swift => vec![
            "function_declaration",
            "class_declaration",
            "protocol_declaration",
        ],
        LangId::CSharp => vec![
            "namespace_declaration",
            "method_declaration",
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Bash => vec!["function_definition"],
        LangId::Ruby => vec![
            "method",
            "singleton_method",
            "class",
            "module",
            "assignment",
        ],
    }
}

/// identifier ノードかどうかを判定する。
fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "simple_identifier"
            | "namespace_identifier"
            | "package_identifier"
            | "name"
            | "qualified_name"
            | "word"
            | "constant"
    )
}

/// 指定行のソース行をコンテキストとして抽出する。
fn extract_line_context(source: &[u8], row: usize) -> String {
    let text = std::str::from_utf8(source).unwrap_or("");
    text.lines().nth(row).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Batch reference search: O(N + S) instead of O(S × N)
// ---------------------------------------------------------------------------

/// 全シンボル名の参照を1回のディレクトリウォークで検索する。
/// シンボル名→参照リストのマップを返す。
/// Aho-Corasick オートマトンによる効率的なマルチパターン事前フィルタを使用。
pub fn find_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    // Aho-Corasick オートマトンを1回構築し、全ファイルで共有する
    let ac = aho_corasick::AhoCorasick::new(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

    let files = collect_files(dir, glob_pattern)?;

    let per_file: Vec<HashMap<String, Vec<SymbolReference>>> = files
        .par_iter()
        .filter_map(|path| {
            let utf8_path = camino::Utf8Path::new(path.to_str()?);
            find_refs_batch_in_file(symbol_names, &ac, utf8_path).ok()
        })
        .collect();

    // ファイルごとの結果をマージ
    let mut merged: HashMap<String, Vec<SymbolReference>> = HashMap::new();
    for file_map in per_file {
        for (name, refs) in file_map {
            merged.entry(name).or_default().extend(refs);
        }
    }

    // 各シンボルの参照をソート: 定義を先頭に、その後パス/行番号順
    for refs in merged.values_mut() {
        refs.sort_by(|a, b| {
            let def_order = |k: &Option<RefKind>| match k {
                Some(RefKind::Definition) => 0,
                _ => 1,
            };
            def_order(&a.kind)
                .cmp(&def_order(&b.kind))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
    }

    Ok(merged)
}

/// 単一ファイル内で複数シンボルの参照を検索する（1回のパースで処理）。
/// 事前構築した Aho-Corasick オートマトンで O(file_size) のマルチパターンフィルタを使用。
fn find_refs_batch_in_file(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::{HashMap, HashSet};

    let source = parser::read_file(path)?;

    // マルチパターン事前フィルタ: 1パスでこのファイルに含まれるシンボルを特定
    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == symbol_names.len() {
            break; // 全パターン検出済み
        }
    }

    if present_indices.is_empty() {
        return Ok(HashMap::new());
    }

    let present: Vec<&String> = present_indices.iter().map(|&i| &symbol_names[i]).collect();

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    let mut result: HashMap<String, Vec<SymbolReference>> = HashMap::new();

    // AST を1回走査し、含まれるシンボルを全てチェック
    collect_identifier_refs_batch(
        root,
        &source,
        &present,
        path.as_str(),
        &definition_kinds,
        lang_id,
        &mut result,
    );

    Ok(result)
}

/// AST を再帰走査し、指定シンボル群に一致する identifier ノードを収集する。
fn collect_identifier_refs_batch(
    node: Node<'_>,
    source: &[u8],
    symbol_names: &[&String],
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut std::collections::HashMap<String, Vec<SymbolReference>>,
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
    {
        for name in symbol_names {
            if text == name.as_str() {
                let is_def = is_definition_context(node, definition_kinds, lang_id);
                let context = extract_line_context(source, node.start_position().row);

                refs.entry(name.to_string())
                    .or_default()
                    .push(SymbolReference {
                        path: path.to_string(),
                        line: node.start_position().row,
                        column: node.start_position().column,
                        context: Some(context),
                        kind: Some(if is_def {
                            RefKind::Definition
                        } else {
                            RefKind::Reference
                        }),
                    });
                break; // 各 identifier は最大1つのシンボルに一致
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs_batch(
            child,
            source,
            symbol_names,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 既知の identifier ノード種別が true を返すことを検証
    #[test]
    fn is_identifier_kind_matches() {
        assert!(is_identifier_kind("identifier"));
        assert!(is_identifier_kind("type_identifier"));
        assert!(is_identifier_kind("field_identifier"));
        assert!(is_identifier_kind("property_identifier"));
        assert!(is_identifier_kind("constant"));
        assert!(is_identifier_kind("name"));
        assert!(is_identifier_kind("word"));
    }

    /// 非 identifier ノード種別が false を返すことを検証
    #[test]
    fn is_identifier_kind_rejects_non_identifier() {
        assert!(!is_identifier_kind("function_definition"));
        assert!(!is_identifier_kind("block"));
        assert!(!is_identifier_kind("string"));
        assert!(!is_identifier_kind("comment"));
    }

    /// 指定行のソースが正しく抽出され、前後の空白が除去されることを検証
    #[test]
    fn extract_line_context_basic() {
        let source = b"line0\n  line1  \nline2";
        let ctx = extract_line_context(source, 1);
        assert_eq!(ctx, "line1");
    }

    /// 範囲外の行に対して空文字を返すことを検証
    #[test]
    fn extract_line_context_out_of_range() {
        let source = b"only one line";
        let ctx = extract_line_context(source, 5);
        assert_eq!(ctx, "");
    }

    /// Rust の定義ノード種別に function_item と struct_item が含まれることを検証
    #[test]
    fn definition_node_kinds_rust() {
        let kinds = definition_node_kinds(LangId::Rust);
        assert!(kinds.contains(&"function_item"));
        assert!(kinds.contains(&"struct_item"));
        assert!(kinds.contains(&"enum_item"));
        assert!(kinds.contains(&"trait_item"));
    }

    /// Python の定義ノード種別に function_definition と class_definition が含まれることを検証
    #[test]
    fn definition_node_kinds_python() {
        let kinds = definition_node_kinds(LangId::Python);
        assert!(kinds.contains(&"function_definition"));
        assert!(kinds.contains(&"class_definition"));
    }
}
