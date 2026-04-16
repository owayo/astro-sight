use anyhow::Result;
use rayon::prelude::*;
use std::path::Path;
use tree_sitter::Node;

use crate::engine::parser;
use crate::language::{LangId, normalize_identifier};
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

    // ファイル言語を拡張子から先読みし、CI 言語ではバイト事前フィルタを skip
    // (memchr は case-sensitive のため Xojo の `MyVar`/`myvar` 一致を取りこぼす)。
    let ext_lang = LangId::from_path(path).ok();
    let is_ci = ext_lang.is_some_and(|l| l.is_case_insensitive());
    if !is_ci && memchr::memmem::find(&source, symbol_name.as_bytes()).is_none() {
        return Ok(Vec::new());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();

    let mut refs = Vec::new();
    let definition_kinds = definition_node_kinds(lang_id);
    let target = normalize_identifier(lang_id, symbol_name);
    collect_identifier_refs(
        root,
        &source,
        target.as_ref(),
        path.as_str(),
        definition_kinds,
        lang_id,
        &mut refs,
    );

    Ok(refs)
}

/// AST を再帰走査し、指定シンボル名に一致する identifier ノードを収集する。
/// `symbol_name` は言語に応じて正規化済みであることが前提。
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
        && normalize_identifier(lang_id, text).as_ref() == symbol_name
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
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn definition_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &[
            "function_item",
            "function_signature_item", // trait メソッド宣言（ボディなし）
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "const_item",
            "static_item",
            "type_item",
            "mod_item",
        ],
        LangId::C => &["function_definition", "struct_specifier", "enum_specifier"],
        LangId::Cpp => &[
            "function_definition",
            "struct_specifier",
            "class_specifier",
            "enum_specifier",
            "namespace_definition",
        ],
        LangId::Python => &["function_definition", "class_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "variable_declarator",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "variable_declarator",
        ],
        LangId::Go => &[
            "package_clause",
            "function_declaration",
            "method_declaration",
            "type_spec",
        ],
        LangId::Php => &[
            "function_definition",
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "trait_declaration",
        ],
        LangId::Java => &[
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Kotlin => &[
            "function_declaration",
            "class_declaration",
            "object_declaration",
        ],
        LangId::Swift => &[
            "function_declaration",
            "class_declaration",
            "protocol_declaration",
        ],
        LangId::CSharp => &[
            "namespace_declaration",
            "method_declaration",
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Bash => &["function_definition"],
        LangId::Ruby => &[
            "method",
            "singleton_method",
            "class",
            "module",
            "assignment",
        ],
        LangId::Zig => &[
            "function_declaration",
            "variable_declaration",
            "test_declaration",
            "struct_declaration",
            "enum_declaration",
            "union_declaration",
        ],
        LangId::Xojo => &[
            "class_declaration",
            "module_declaration",
            "interface_declaration",
            "structure_declaration",
            "enum_declaration",
            "sub_declaration",
            "function_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "event_declaration",
            "delegate_declaration",
            "simple_property_declaration",
            "computed_property_declaration",
            "const_declaration",
            "field_declaration",
            "declare_declaration",
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
/// minified/生成コードの巨大行によるメモリ爆発を防ぐため 256B で切り詰める。
fn extract_line_context(source: &[u8], row: usize) -> String {
    const MAX_CTX: usize = 256;
    let text = std::str::from_utf8(source).unwrap_or("");
    let line = text.lines().nth(row).unwrap_or("").trim();
    if line.len() <= MAX_CTX {
        line.to_string()
    } else {
        // UTF-8 境界で安全に切り詰める
        let truncated = &line[..line.floor_char_boundary(MAX_CTX)];
        format!("{truncated}...")
    }
}

// ---------------------------------------------------------------------------
// Batch reference search: O(N + S) instead of O(S × N)
// ---------------------------------------------------------------------------

/// 全シンボル名の参照を1回のディレクトリウォークで検索する。
/// シンボル名→参照リストのマップを返す。
/// Aho-Corasick オートマトンによる効率的なマルチパターン事前フィルタを使用。
///
/// fold/reduce でワーカー局所バケットに直接統合し、
/// per_file Vec + merged HashMap の二重保持を回避する。
pub fn find_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    // AC は ASCII CI で構築: CI 言語 (Xojo) で case 違いを事前フィルタで取りこぼさないため。
    // 非 CI 言語では多少の false positive (大文字小文字違い) が発生するが、AST 比較で弾く。
    let ac = aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

    let files = collect_files(dir, glob_pattern)?;

    // fold/reduce: ワーカーごとに Vec<Vec<SymbolReference>> を持ち、直接統合
    let mut buckets: Vec<Vec<SymbolReference>> = files
        .into_par_iter()
        .fold(
            || vec![Vec::new(); symbol_names.len()],
            |mut local, path| {
                let Some(path_str) = path.to_str() else {
                    return local;
                };
                let utf8_path = camino::Utf8Path::new(path_str);
                if let Ok(per_file) = find_refs_batch_in_file_indexed(symbol_names, &ac, utf8_path)
                {
                    for (ix, mut refs) in per_file.into_iter().enumerate() {
                        local[ix].append(&mut refs);
                    }
                }
                local
            },
        )
        .reduce(
            || vec![Vec::new(); symbol_names.len()],
            |mut acc, mut local| {
                for (acc_refs, local_refs) in acc.iter_mut().zip(local.iter_mut()) {
                    acc_refs.append(local_refs);
                }
                acc
            },
        );

    let mut merged = HashMap::with_capacity(symbol_names.len());
    for (i, name) in symbol_names.iter().enumerate() {
        let mut refs = std::mem::take(&mut buckets[i]);
        // ソート: 定義を先頭に、その後パス/行番号順
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
        if !refs.is_empty() {
            merged.insert(name.clone(), refs);
        }
    }

    Ok(merged)
}

/// dead-code 判定用: フル SymbolReference を作らず非 Definition 参照の件数のみ返す。
/// SymbolReference のヒープ確保を全排除し、メモリ消費を大幅に削減する。
pub fn count_non_definition_refs_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, usize>> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    let ac = aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

    let files = collect_files(dir, glob_pattern)?;

    let counts: Vec<usize> = files
        .into_par_iter()
        .fold(
            || vec![0usize; symbol_names.len()],
            |mut local, path| {
                let Some(path_str) = path.to_str() else {
                    return local;
                };
                let utf8_path = camino::Utf8Path::new(path_str);
                if let Ok(per_file) = count_refs_in_file(symbol_names, &ac, utf8_path) {
                    for (ix, cnt) in per_file.into_iter().enumerate() {
                        local[ix] += cnt;
                    }
                }
                local
            },
        )
        .reduce(
            || vec![0usize; symbol_names.len()],
            |mut acc, local| {
                for (a, b) in acc.iter_mut().zip(local) {
                    *a += b;
                }
                acc
            },
        );

    Ok(symbol_names.iter().cloned().zip(counts).collect())
}

/// 単一ファイル内で複数シンボルの参照を index ベースの Vec に格納する。
/// find_references_batch の fold/reduce から呼ばれる。
fn find_refs_batch_in_file_indexed(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<Vec<SymbolReference>>> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    // マルチパターン事前フィルタ (AC は ASCII CI で構築済、超集合フィルタ)
    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }

    if present_indices.is_empty() {
        return Ok(vec![Vec::new(); num]);
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    // 言語別に正規化済みキーで name_to_ix を再構築する (Xojo では case 違いを吸収)。
    let name_to_ix: std::collections::HashMap<std::borrow::Cow<'_, str>, usize> = present_indices
        .iter()
        .map(|&i| (normalize_identifier(lang_id, symbol_names[i].as_str()), i))
        .collect();

    let mut result = vec![Vec::new(); num];
    collect_identifier_refs_indexed(
        root,
        &source,
        &name_to_ix,
        path.as_str(),
        definition_kinds,
        lang_id,
        &mut result,
    );

    Ok(result)
}

/// AST を再帰走査し、シンボル index ベースの Vec に参照を格納する。
fn collect_identifier_refs_indexed(
    node: Node<'_>,
    source: &[u8],
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, usize>,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut [Vec<SymbolReference>],
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(&ix) = name_to_ix.get(&normalize_identifier(lang_id, text))
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context(source, node.start_position().row);

        refs[ix].push(SymbolReference {
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

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs_indexed(
            child,
            source,
            name_to_ix,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

/// 単一ファイル内の非 Definition 参照件数をカウントする（SymbolReference を確保しない）。
fn count_refs_in_file(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<usize>> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }

    if present_indices.is_empty() {
        return Ok(vec![0; num]);
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    // 言語別に正規化キーで name_to_ix を構築
    let name_to_ix: std::collections::HashMap<std::borrow::Cow<'_, str>, usize> = present_indices
        .iter()
        .map(|&i| (normalize_identifier(lang_id, symbol_names[i].as_str()), i))
        .collect();

    let mut counts = vec![0usize; num];
    count_identifier_refs(
        root,
        &source,
        &name_to_ix,
        definition_kinds,
        lang_id,
        &mut counts,
    );

    Ok(counts)
}

/// AST を再帰走査し、非 Definition 参照の件数のみカウントする。
fn count_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, usize>,
    definition_kinds: &[&str],
    lang_id: LangId,
    counts: &mut [usize],
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(&ix) = name_to_ix.get(&normalize_identifier(lang_id, text))
        && !is_definition_context(node, definition_kinds, lang_id)
    {
        counts[ix] += 1;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_identifier_refs(child, source, name_to_ix, definition_kinds, lang_id, counts);
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
