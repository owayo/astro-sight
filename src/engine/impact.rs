use anyhow::Result;
use camino::Utf8Path;
use std::path::Path;

use crate::engine::{calls, diff, parser, refs, symbols};
use crate::language::LangId;
use crate::models::impact::{
    AffectedSymbol, ContextResult, FileImpact, ImpactedCaller, SignatureChange,
};
use crate::models::reference::RefKind;
use crate::models::symbol::SymbolKind;

/// Analyze the impact of a unified diff within a workspace directory.
///
/// Uses a 2-pass approach for cross-file references:
///   Pass 1: Parse changed files, collect affected symbols per file.
///   Pass 2: Batch-search all affected symbol names in one directory walk.
pub fn analyze_impact(diff_input: &str, dir: &Path) -> Result<ContextResult> {
    let diff_files = diff::parse_unified_diff(diff_input);

    // --- Pass 1: Parse changed files and collect affected symbols ---
    struct FileContext {
        new_path: String,
        lang_id: LangId,
        affected: Vec<AffectedSymbol>,
        sig_changes: Vec<SignatureChange>,
        hunks: Vec<crate::models::impact::HunkInfo>,
        call_edges: Vec<crate::models::call::CallEdge>,
    }

    let mut file_contexts = Vec::new();
    let mut all_symbol_names: Vec<String> = Vec::new();
    let mut method_parent_types: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for df in &diff_files {
        // Reject diff paths that attempt path traversal (e.g. "../../etc/passwd")
        if !is_safe_diff_path(&df.new_path) {
            continue;
        }

        let file_path = dir.join(&df.new_path);
        if !file_path.exists() {
            continue;
        }

        // Verify resolved path stays within workspace
        if let Ok(canonical) = std::fs::canonicalize(&file_path)
            && let Ok(canonical_dir) = std::fs::canonicalize(dir)
            && !canonical.starts_with(&canonical_dir)
        {
            continue;
        }

        let utf8_path = Utf8Path::new(file_path.to_str().unwrap_or(""));
        let source = match parser::read_file(utf8_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (tree, lang_id) = match parser::parse_file(utf8_path, &source) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let root = tree.root_node();

        let syms = symbols::extract_symbols(root, &source, lang_id).unwrap_or_default();
        let affected = find_affected_symbols(&syms, &df.hunks);
        let sig_changes = detect_signature_changes(diff_input, &df.new_path, &affected);
        let call_edges = calls::extract_calls(root, &source, lang_id, None).unwrap_or_default();

        // Filter: exclude non-exported symbols and body-only changes from cross-file search
        for sym in &affected {
            if all_symbol_names.contains(&sym.name) {
                continue;
            }
            // Skip impl block type names (e.g. "Foo" from `impl Foo { ... }`).
            // The struct/enum definition itself is captured separately; the impl
            // type name changing only means the impl body changed, not the type's API.
            if sym.kind == "type" {
                continue;
            }
            // Skip symbols in test context (#[cfg(test)] modules, #[test] functions, etc.)
            let in_test = syms.iter().any(|s| {
                s.name == sym.name
                    && symbol_overlaps_hunks(s, &df.hunks)
                    && is_in_test_context(root, &source, &s.range, lang_id)
            });
            if in_test {
                continue;
            }
            // For functions/methods, only include if the signature actually changed.
            // Body-only changes (e.g. modified logic, added logging) don't affect callers.
            if (sym.kind == "function" || sym.kind == "method")
                && !sig_changes.iter().any(|sc| sc.name == sym.name)
            {
                continue;
            }
            // Check export status only for symbols that overlap with changed hunks
            // (avoid using a different same-named symbol's export status)
            let any_affected_exported = syms.iter().any(|s| {
                s.name == sym.name
                    && symbol_overlaps_hunks(s, &df.hunks)
                    && symbols::is_symbol_exported(root, &source, lang_id, &s.range)
            });
            if !any_affected_exported {
                continue;
            }
            // Skip if symbol name doesn't appear in any changed line (body-only change)
            if !is_symbol_in_changed_lines(diff_input, &df.new_path, &sym.name) {
                continue;
            }
            // For methods, collect parent type for cross-file scoping
            if let Some(orig) = syms
                .iter()
                .find(|s| s.name == sym.name && symbol_overlaps_hunks(s, &df.hunks))
                && let Some(parent_type) =
                    find_parent_type_name(root, &source, &orig.range, lang_id)
            {
                method_parent_types.insert(sym.name.clone(), parent_type.clone());
                if !all_symbol_names.contains(&parent_type) {
                    all_symbol_names.push(parent_type);
                }
            }
            all_symbol_names.push(sym.name.clone());
        }

        let hunks = df
            .hunks
            .iter()
            .map(|h| crate::models::impact::HunkInfo {
                old_start: h.old_start,
                old_count: h.old_count,
                new_start: h.new_start,
                new_count: h.new_count,
            })
            .collect();

        file_contexts.push(FileContext {
            new_path: df.new_path.clone(),
            lang_id,
            affected,
            sig_changes,
            hunks,
            call_edges,
        });
    }

    // --- Pass 2: Batch cross-file reference search (one walk for all symbols) ---
    let batch_refs = if all_symbol_names.is_empty() {
        std::collections::HashMap::new()
    } else {
        refs::find_references_batch(&all_symbol_names, dir, None).unwrap_or_default()
    };

    // --- Assemble results ---
    let mut changes = Vec::new();
    // Cache parsed target files for test-context checks (avoid re-parsing)
    let mut target_file_cache: std::collections::HashMap<String, Option<ParsedFile>> =
        std::collections::HashMap::new();

    for ctx in file_contexts {
        let mut impacted_callers = Vec::new();

        // Cross-file callers from batch results (only References, not Definitions)
        let source_lang_group = lang_compat_group(ctx.lang_id);
        for sym in &ctx.affected {
            if let Some(caller_refs) = batch_refs.get(&sym.name) {
                for r in caller_refs {
                    // Skip definitions — we only want call-site references
                    if r.kind == Some(RefKind::Definition) {
                        continue;
                    }
                    // Skip same-file refs (use exact path comparison via canonical paths)
                    if r.path.ends_with(&ctx.new_path) {
                        continue;
                    }
                    // Skip cross-language false positives (e.g. Rust `command` vs Bash builtin)
                    if let Ok(ref_lang) = LangId::from_path(Utf8Path::new(&r.path))
                        && lang_compat_group(ref_lang) != source_lang_group
                    {
                        continue;
                    }
                    // Method type scoping: for methods inside impl/class blocks,
                    // only include refs from files that also reference the parent type.
                    // This prevents e.g. StopHookFilter::execute from matching RmFilter::execute.
                    if let Some(parent_type) = method_parent_types.get(&sym.name) {
                        let type_in_ref_file = batch_refs
                            .get(parent_type.as_str())
                            .is_some_and(|type_refs| type_refs.iter().any(|tr| tr.path == r.path));
                        if !type_in_ref_file {
                            continue;
                        }
                    }
                    // Skip refs in test context in the target file
                    if is_ref_in_target_test_context(
                        &r.path,
                        r.line,
                        r.column,
                        &mut target_file_cache,
                    ) {
                        continue;
                    }
                    impacted_callers.push(ImpactedCaller {
                        path: r.path.clone(),
                        name: r
                            .context
                            .as_deref()
                            .and_then(extract_function_from_context)
                            .unwrap_or_else(|| sym.name.clone()),
                        line: r.line,
                    });
                }
            }
        }

        // Same-file callers from call graph
        for sym in &ctx.affected {
            for edge in &ctx.call_edges {
                if edge.callee.name == sym.name {
                    let caller_line = edge.call_site.line;
                    if !ctx.affected.iter().any(|a| a.name == edge.caller.name) {
                        impacted_callers.push(ImpactedCaller {
                            path: ctx.new_path.clone(),
                            name: edge.caller.name.clone(),
                            line: caller_line,
                        });
                    }
                }
            }
        }

        impacted_callers.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
        impacted_callers.dedup_by(|a, b| a.path == b.path && a.line == b.line);

        changes.push(FileImpact {
            path: ctx.new_path,
            hunks: ctx.hunks,
            affected_symbols: ctx.affected,
            signature_changes: ctx.sig_changes,
            impacted_callers,
        });
    }

    Ok(ContextResult { changes })
}

/// Match hunks against symbol ranges to find affected symbols.
fn find_affected_symbols(
    syms: &[crate::models::symbol::Symbol],
    hunks: &[crate::models::impact::HunkInfo],
) -> Vec<AffectedSymbol> {
    let mut affected = Vec::new();

    for sym in syms {
        for hunk in hunks {
            let hunk_start = hunk.new_start.saturating_sub(1); // 1-indexed to 0-indexed
            let hunk_end = hunk_start + hunk.new_count;
            let sym_start = sym.range.start.line;
            let sym_end = sym.range.end.line;

            // Check overlap
            if hunk_start < sym_end && hunk_end > sym_start {
                let change_type = if hunk.old_count == 0 {
                    "added"
                } else if hunk.new_count == 0 {
                    "removed"
                } else {
                    "modified"
                };

                affected.push(AffectedSymbol {
                    name: sym.name.clone(),
                    kind: symbol_kind_str(sym.kind),
                    change_type: change_type.to_string(),
                });
                break; // Don't double-count
            }
        }
    }

    affected
}

/// Check if a symbol's range overlaps with any of the given hunks.
fn symbol_overlaps_hunks(
    sym: &crate::models::symbol::Symbol,
    hunks: &[crate::models::impact::HunkInfo],
) -> bool {
    hunks.iter().any(|h| {
        let hunk_start = h.new_start.saturating_sub(1);
        let hunk_end = hunk_start + h.new_count;
        hunk_start < sym.range.end.line && hunk_end > sym.range.start.line
    })
}

fn symbol_kind_str(kind: SymbolKind) -> String {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::Module => "module",
        SymbolKind::Import => "import",
        SymbolKind::Type => "type",
        SymbolKind::Field => "field",
        SymbolKind::Parameter => "parameter",
    }
    .to_string()
}

/// Detect signature changes by looking at removed (-) and added (+) lines in the diff
/// that contain function signatures for affected symbols.
fn detect_signature_changes(
    diff_input: &str,
    file_path: &str,
    affected: &[AffectedSymbol],
) -> Vec<SignatureChange> {
    let mut changes = Vec::new();
    let mut in_file = false;
    let mut removed_lines = Vec::new();
    let mut added_lines = Vec::new();

    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            let path = line.strip_prefix("+++ b/").unwrap_or("");
            in_file = path == file_path;
            if in_file {
                removed_lines.clear();
                added_lines.clear();
            }
        } else if line.starts_with("--- ") {
            // Will be followed by +++ line
        } else if in_file {
            if let Some(content) = line.strip_prefix('-') {
                removed_lines.push(content.to_string());
            } else if let Some(content) = line.strip_prefix('+') {
                added_lines.push(content.to_string());
            }
        }
    }

    for sym in affected {
        if sym.kind != "function" && sym.kind != "method" {
            continue;
        }

        let old_sig = find_signature_in_lines(&removed_lines, &sym.name);
        let new_sig = find_signature_in_lines(&added_lines, &sym.name);

        if let (Some(old), Some(new)) = (old_sig, new_sig)
            && old != new
        {
            changes.push(SignatureChange {
                name: sym.name.clone(),
                old_signature: old,
                new_signature: new,
            });
        }
    }

    changes
}

/// Find a function signature line containing the given function name.
fn find_signature_in_lines(lines: &[String], func_name: &str) -> Option<String> {
    for line in lines {
        let trimmed = line.trim();
        if trimmed.contains(func_name) && is_signature_line(trimmed) {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Heuristic: a line is a signature if it contains "fn ", "def ", "function ", "func ", etc.
fn is_signature_line(line: &str) -> bool {
    let keywords = [
        "fn ",
        "def ",
        "function ",
        "func ",
        "fun ",
        "void ",
        "int ",
        "string ",
        "bool ",
        "public ",
        "private ",
        "protected ",
        "static ",
        "async ",
    ];
    keywords.iter().any(|kw| line.contains(kw))
}

/// Try to extract a function name from a context line like "    symbols::extract_symbols(...)".
fn extract_function_from_context(context: &str) -> Option<String> {
    // Look for "fn name" pattern
    if let Some(pos) = context.find("fn ") {
        let rest = &context[pos + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Check if a symbol name appears in any changed (+/-) line for the given file in the diff.
///
/// If the symbol name is absent from all changed lines, the change is body-only
/// (e.g. internal JSX/logic change) and callers are not affected.
fn is_symbol_in_changed_lines(diff_input: &str, file_path: &str, symbol_name: &str) -> bool {
    let mut in_file = false;

    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            in_file = line.strip_prefix("+++ b/").unwrap_or("") == file_path;
        } else if in_file
            && ((line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---")))
            && line[1..].contains(symbol_name)
        {
            return true;
        }
    }

    false
}

/// Language compatibility group for cross-file reference filtering.
///
/// Languages in the same group can reference each other's symbols
/// (e.g. JS/TS/TSX share imports, C/C++ share headers, Java/Kotlin share JVM).
/// Cross-group matches (e.g. Rust `command` in a Bash script) are false positives.
fn lang_compat_group(lang: LangId) -> u8 {
    match lang {
        LangId::Rust => 0,
        LangId::C | LangId::Cpp => 1,
        LangId::Python => 2,
        LangId::Javascript | LangId::Typescript | LangId::Tsx => 3,
        LangId::Go => 4,
        LangId::Java | LangId::Kotlin => 5,
        LangId::Swift => 6,
        LangId::CSharp => 7,
        LangId::Php => 8,
        LangId::Ruby => 9,
        LangId::Bash => 10,
    }
}

/// Cached parse result: (tree, source bytes, language).
type ParsedFile = (tree_sitter::Tree, Vec<u8>, LangId);

/// Check if a reference at the given line/column in a target file is inside test context.
///
/// Parses the target file on-demand and caches the result to avoid re-parsing.
/// This filters out impacted callers that are in `#[cfg(test)]` modules or `#[test]` functions.
fn is_ref_in_target_test_context(
    path: &str,
    line: usize,
    column: usize,
    cache: &mut std::collections::HashMap<String, Option<ParsedFile>>,
) -> bool {
    let entry = cache.entry(path.to_string()).or_insert_with(|| {
        let utf8_path = Utf8Path::new(path);
        let source = parser::read_file(utf8_path).ok()?;
        let source_vec = source.as_bytes().to_vec();
        let (tree, lang_id) = parser::parse_file(utf8_path, &source).ok()?;
        Some((tree, source_vec, lang_id))
    });

    let Some((tree, source, lang_id)) = entry else {
        return false;
    };

    let range = crate::models::location::Range {
        start: crate::models::location::Point { line, column },
        end: crate::models::location::Point { line, column },
    };

    is_in_test_context(tree.root_node(), source, &range, *lang_id)
}

/// Check if a symbol is inside a test context (e.g. `#[cfg(test)]` module, `#[test]` function).
///
/// Test symbols should not propagate cross-file impacts because:
/// - Test functions are not called from production code
/// - Changes to test helpers only affect the test module
fn is_in_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
    lang_id: LangId,
) -> bool {
    match lang_id {
        LangId::Rust => is_rust_test_context(root, source, symbol_range),
        _ => false,
    }
}

/// Rust-specific test context detection via AST.
///
/// Checks if the symbol is inside a `#[cfg(test)]` module or has a `#[test]` attribute.
fn is_rust_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    let Some(node) = root.descendant_for_point_range(start, end) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        // Check if this function has #[test] attribute
        if n.kind() == "function_item" && has_attribute_text(n, source, "test") {
            return true;
        }
        // Check if inside a #[cfg(test)] module
        if n.kind() == "mod_item" && has_attribute_text(n, source, "cfg(test)") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Check if a node has a preceding attribute_item sibling containing the given text.
fn has_attribute_text(node: tree_sitter::Node, source: &[u8], pattern: &str) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "attribute_item" => {
                if let Ok(text) = p.utf8_text(source)
                    && text.contains(pattern)
                {
                    return true;
                }
                prev = p.prev_named_sibling();
            }
            "line_comment" | "block_comment" => {
                prev = p.prev_named_sibling();
            }
            _ => break,
        }
    }
    false
}

/// Find the parent type name for a method inside an impl/class block.
///
/// For Rust `impl Foo { fn bar() {} }` → returns `Some("Foo")`
/// For Rust `impl Trait for Foo { fn bar() {} }` → returns `Some("Foo")`
/// For class-based languages → returns the class name
fn find_parent_type_name(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
    lang_id: LangId,
) -> Option<String> {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    let node = root.descendant_for_point_range(start, end)?;

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "impl_item" && lang_id == LangId::Rust {
            return n
                .child_by_field_name("type")
                .and_then(|t| extract_type_name(t, source));
        }
        if matches!(
            n.kind(),
            "class_declaration" | "class_definition" | "class_specifier"
        ) {
            return n
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .map(|s| s.to_string());
        }
        current = n.parent();
    }
    None
}

/// Extract a type name from a tree-sitter type node, handling generics and scoped types.
fn extract_type_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => node.utf8_text(source).ok().map(|s| s.to_string()),
        "generic_type" => node
            .child_by_field_name("type")
            .and_then(|t| extract_type_name(t, source)),
        "scoped_type_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(|s| s.to_string()),
        _ => node.utf8_text(source).ok().map(|s| s.to_string()),
    }
}

/// Validate that a diff path is safe (no absolute paths or traversal components).
fn is_safe_diff_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    for component in path.split(['/', '\\']) {
        if component == ".." {
            return false;
        }
    }
    true
}
