use anyhow::Result;
use rayon::prelude::*;
use std::path::Path;
use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;
use crate::models::reference::{RefKind, SymbolReference};

/// Search for references to `symbol_name` across files in `dir`.
/// Optionally filter by a glob pattern (e.g., "**/*.rs").
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
    // Sort: definitions first, then by path/line
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

/// Collect files using the `ignore` crate (.gitignore aware).
pub fn collect_files(dir: &Path, glob_pattern: Option<&str>) -> Result<Vec<std::path::PathBuf>> {
    use ignore::WalkBuilder;

    let mut builder = WalkBuilder::new(dir);
    builder.hidden(true).git_ignore(true).git_global(true);

    // Apply glob filter via override
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
            // Only include files we can parse
            if LangId::from_path(camino::Utf8Path::new(path.to_str().unwrap_or(""))).is_ok() {
                files.push(path);
            }
        }
    }

    Ok(files)
}

/// Find references to `symbol_name` in a single file.
fn find_refs_in_file(symbol_name: &str, path: &camino::Utf8Path) -> Result<Vec<SymbolReference>> {
    let source = parser::read_file(path)?;

    // Quick text check: skip if symbol name not in source
    let source_str = std::str::from_utf8(&source).unwrap_or("");
    if !source_str.contains(symbol_name) {
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
        &mut refs,
    );

    Ok(refs)
}

/// Recursively walk the tree and collect identifier nodes matching `symbol_name`.
fn collect_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    path: &str,
    definition_kinds: &[&str],
    refs: &mut Vec<SymbolReference>,
) {
    let kind = node.kind();
    let is_identifier = kind == "identifier"
        || kind == "type_identifier"
        || kind == "field_identifier"
        || kind == "property_identifier"
        || kind == "simple_identifier"
        || kind == "namespace_identifier"
        || kind == "package_identifier"
        || kind == "name"
        || kind == "word";

    if is_identifier
        && let Ok(text) = node.utf8_text(source)
        && text == symbol_name
    {
        let is_def = is_definition_context(node, definition_kinds);
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

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs(child, source, symbol_name, path, definition_kinds, refs);
    }
}

/// Check if this identifier node is in a definition context.
fn is_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    if let Some(parent) = node.parent() {
        // Check if the parent is a definition node
        if definition_kinds.contains(&parent.kind()) {
            return true;
        }
        // Check grandparent (e.g., function_declarator > identifier)
        if let Some(grandparent) = parent.parent()
            && definition_kinds.contains(&grandparent.kind())
        {
            return true;
        }
    }
    false
}

/// Get node kinds that represent definitions for a language.
fn definition_node_kinds(lang_id: LangId) -> Vec<&'static str> {
    match lang_id {
        LangId::Rust => vec![
            "function_item",
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
    }
}

/// Extract the source line at a given row for context.
fn extract_line_context(source: &[u8], row: usize) -> String {
    let text = std::str::from_utf8(source).unwrap_or("");
    text.lines().nth(row).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Batch reference search: O(N + S) instead of O(S × N)
// ---------------------------------------------------------------------------

/// Search for references to *all* symbol names in a single directory walk.
/// Returns a map from symbol name to its references.
pub fn find_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    let files = collect_files(dir, glob_pattern)?;

    let per_file: Vec<HashMap<String, Vec<SymbolReference>>> = files
        .par_iter()
        .filter_map(|path| {
            let utf8_path = camino::Utf8Path::new(path.to_str()?);
            find_refs_batch_in_file(symbol_names, utf8_path).ok()
        })
        .collect();

    // Merge per-file results
    let mut merged: HashMap<String, Vec<SymbolReference>> = HashMap::new();
    for file_map in per_file {
        for (name, refs) in file_map {
            merged.entry(name).or_default().extend(refs);
        }
    }

    // Sort each symbol's refs: definitions first, then by path/line
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

/// Find references to multiple symbols in a single file (one parse).
fn find_refs_batch_in_file(
    symbol_names: &[String],
    path: &camino::Utf8Path,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::HashMap;

    let source = parser::read_file(path)?;
    let source_str = std::str::from_utf8(&source).unwrap_or("");

    // Quick text filter: only keep symbols that appear in this file
    let present: Vec<&String> = symbol_names
        .iter()
        .filter(|name| source_str.contains(name.as_str()))
        .collect();

    if present.is_empty() {
        return Ok(HashMap::new());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    let mut result: HashMap<String, Vec<SymbolReference>> = HashMap::new();

    // Walk the tree once, checking all present symbols at each identifier node
    collect_identifier_refs_batch(
        root,
        &source,
        &present,
        path.as_str(),
        &definition_kinds,
        &mut result,
    );

    Ok(result)
}

/// Recursively walk the tree and collect identifier nodes matching any of the given symbols.
fn collect_identifier_refs_batch(
    node: Node<'_>,
    source: &[u8],
    symbol_names: &[&String],
    path: &str,
    definition_kinds: &[&str],
    refs: &mut std::collections::HashMap<String, Vec<SymbolReference>>,
) {
    let kind = node.kind();
    let is_identifier = kind == "identifier"
        || kind == "type_identifier"
        || kind == "field_identifier"
        || kind == "property_identifier"
        || kind == "simple_identifier"
        || kind == "namespace_identifier"
        || kind == "package_identifier"
        || kind == "name"
        || kind == "word";

    if is_identifier && let Ok(text) = node.utf8_text(source) {
        for name in symbol_names {
            if text == name.as_str() {
                let is_def = is_definition_context(node, definition_kinds);
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
                break; // Each identifier matches at most one symbol name
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs_batch(child, source, symbol_names, path, definition_kinds, refs);
    }
}
